//! The collaboration-presence organ (#25, M3.11): the source-agnostic
//! participant registry — remote humans and agent swarms embodied on the
//! shared scene as cursors, display names, and floating notes.
//!
//! **Naming**: this organ is *collaboration* presence. The effects
//! Think/Confidence/Mood family that older docs call "AI presence" is a
//! different thing — recordable choreography about one agent's internal
//! state. This organ is keyed identity with leases and ownership: *who
//! else is here, and where*.
//!
//! ## Identity is ingress truth
//!
//! A participant or note is keyed by **(ingress source namespace,
//! caller-local id)**. The namespace comes from the transport at apply
//! time, never from the byte stream, so a stream can only ever populate
//! its own roster — ownership is enforced by construction, not by an
//! owner check. Display names and colors are rendering metadata, never
//! authentication. Ratty performs no networking: presence *rendering* is
//! tier A of #25; relays and bridges live outside this process and speak
//! ordinary OSC 777 when they arrive.
//!
//! Because presence identity is ingress truth — a participant exists
//! because a live stream said so *now* — the whole family is
//! control-plane (never macro-recorded, see [`crate::osc`]) and this
//! applier additionally refuses any command whose [`CommandOrigin`] is
//! not `Wire`: a macro/bookmark/rule-replayed `user.join` would forge
//! liveness, and a replayed `user.leave` would evict a real participant.
//!
//! ## Leases (#21, the sensor model verbatim)
//!
//! Every record stores `updated` (`Time::elapsed` at mutation) and a
//! TTL; `fresh(now) = now − updated ≤ ttl`. **No sweep ever deletes a
//! record** — expiry is computed lazily and stays *visible*: expired
//! rows remain queryable with `fresh: false` (never ghosting, never a
//! silent vanish), and rendering simply hides them. Removal happens only
//! through `user.leave` / `note.remove` / `reset`. Any successful
//! mutation of one's own record refreshes its lease, and `user.renew`
//! exists to refresh without moving anything; renewing an expired
//! participant revives it — honest lease semantics, the row was still
//! there.
//!
//! ## Collisions and revisions (#16)
//!
//! An existing id — fresh *or* expired — rejects `already-exists` unless
//! `replace=true`; a replace overwrites the record but the revision
//! continues (bumps), so observers can tell "same identity, new state"
//! from "brand new". Every record carries a `revision` starting at 1,
//! bumped on every successful mutation, and the registry keeps a
//! [`PresenceRegistry::mutation_seq`] for cheap change detection by the
//! render systems.
//!
//! ## Read side (#18 posture)
//!
//! `state.presence` (paginated) projects rosters under the three-tier
//! read scope: the caller's own namespace in full **including expired
//! rows**, foreign namespaces as fresh rows only — rendered = public,
//! and an expired foreign row's existence must not leak.
//! `state.namespaces` gains fresh participant/note counts per namespace.
//! Everything rendered (name, color, cursor, note text) is public by
//! definition — presence is the embodiment substrate.
//!
//! ## Rendering (the embodiment substrate)
//!
//! Fresh rows render, expired rows render nothing — that is the visible
//! expiry on the scene. Cursor markers are small unlit caret meshes in
//! the main scene (screen-space in the flat view; pinned to the warped
//! surface through the RGP projection in the 3D modes). Name labels and
//! note panels are vello stroke-font underlays drawn into the terminal
//! texture beside the viz underlays, so they warp with the plane. Every
//! registry mutation *and* every fresh→expired flip requests a terminal
//! redraw — an expired note disappears even on an otherwise idle
//! terminal.
//!
//! ## Honest limitations
//!
//! - Leases advance on `Res<Time>`, so on a hidden wasm tab expiry
//!   stalls with everything else (the sensor/avatar stance) — durations
//!   stretch in wall time, nothing ever fires while unobserved.
//! - Cursors and notes pin to **live grid cells** (the viewport): they
//!   do **not** scroll with the text they were placed beside.
//!   Out-of-grid cells clamp to the nearest edge cell at render; the
//!   stored wire value is untouched.
//! - Name labels and note text render through the vello stroke font:
//!   uppercase letters, digits, and limited punctuation; other glyphs
//!   render as hollow boxes. The full text stays queryable either way.
//! - Notes render one line anchored at their cell, truncated at the
//!   grid's right edge; labels cap at 16 glyphs. The full text and name
//!   stay queryable via `state.presence`.
//! - A participant without a reported cursor renders nothing — marker
//!   and label both hang off the cursor cell — and the wire carries no
//!   color on `note`, so note borders are the fixed presence accent,
//!   not per-participant.
//! - A plain PTY is one effective principal: every local writer shares
//!   namespace 0. Distinct participants under one namespace are distinct
//!   *ids*, honestly labeled, not authenticated identities.

use std::collections::HashMap;
use std::time::Duration;

use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use serde_json::{Value, json};

use crate::ai::{AiCommand, CommandOrigin};
use crate::config::CURSOR_DEPTH;
use crate::osc::RattyAiCommand;
use crate::query::codes;
use crate::query_channel::{AckOutcome, AiDiagnostics, ack_commit, reject};
use crate::scene::{
    MobiusTransition, TerminalPlane, TerminalPlaneWarp, TerminalPresentation,
    TerminalPresentationMode, TerminalViewport,
};
use crate::systems::{active_mobius_progress, plane_surface_point};
use crate::terminal::{TerminalRedrawState, TerminalSurface};
use crate::viz_draw::{GLYPH_ASPECT, TextAlign, UnderlayRect, VizDrawOp};

/// Upper bound on participants per agent namespace: an honest failure
/// instead of an unbounded roster driven by untrusted output. Expired
/// rows still occupy their slot (they are queryable state; only
/// `user.leave` frees it).
pub const MAX_PRESENCE_PARTICIPANTS_PER_NAMESPACE: usize = 16;

/// Upper bound on notes per agent namespace (the participant posture).
pub const MAX_PRESENCE_NOTES_PER_NAMESPACE: usize = 16;

/// Upper bound, in bytes, on a participant or note id (the sensor-suffix
/// bound).
pub const MAX_PRESENCE_ID_BYTES: usize = 48;

/// Upper bound, in bytes, on a display name (any UTF-8; the macro-name
/// bound).
pub const MAX_PRESENCE_NAME_BYTES: usize = 64;

/// Upper bound, in bytes, on a note's text.
pub const MAX_PRESENCE_NOTE_TEXT_BYTES: usize = 256;

/// Default participant lease TTL, in seconds.
pub const DEFAULT_PRESENCE_TTL_SECS: f32 = 60.0;

/// Lower clamp on a presence TTL, in seconds (participants and notes).
pub const MIN_PRESENCE_TTL_SECS: f32 = 1.0;

/// Upper clamp on a presence TTL, in seconds (participants and notes).
pub const MAX_PRESENCE_TTL_SECS: f32 = 3600.0;

/// Default note lease TTL, in seconds — notes are annotations, expected
/// to outlive the cursor twitch that placed them.
pub const DEFAULT_PRESENCE_NOTE_TTL_SECS: f32 = 300.0;

/// A rejection: the wire code plus a human message for the
/// `state.errors` ring (the `ReactiveReject` convention).
type PresenceReject = (&'static str, String);

/// One collaboration participant, keyed (namespace, id) in the registry.
#[derive(Debug, Clone)]
struct ParticipantRecord {
    /// Display name (defaults to the id at parse). Rendering metadata,
    /// never authentication.
    name: String,
    /// Cursor color, validated strict `#rrggbb` at apply.
    color: String,
    /// Last reported cursor cell `(col, row)`, if any.
    cursor: Option<(u16, u16)>,
    /// `Time::elapsed` at the last successful mutation (the lease clock).
    updated: Duration,
    /// Freshness lifetime; past it the row renders nothing but stays
    /// queryable (`fresh: false`).
    ttl: Duration,
    /// Bumped on every successful mutation; continues across replace.
    revision: u64,
}

impl ParticipantRecord {
    fn fresh(&self, now: Duration) -> bool {
        now.saturating_sub(self.updated) <= self.ttl
    }
}

/// One floating note, keyed (namespace, id) in the registry.
#[derive(Debug, Clone)]
struct NoteRecord {
    /// The note text (byte-capped at apply).
    text: String,
    /// Anchor column (cell).
    x: u16,
    /// Anchor row (cell).
    y: u16,
    /// `Time::elapsed` at the last successful mutation (the lease clock).
    updated: Duration,
    /// Freshness lifetime.
    ttl: Duration,
    /// Bumped on every successful mutation; continues across replace.
    revision: u64,
}

impl NoteRecord {
    fn fresh(&self, now: Duration) -> bool {
        now.saturating_sub(self.updated) <= self.ttl
    }
}

/// The participant and note registries — one resource, the presence
/// analog of [`crate::reactive::ReactiveRegistry`]. All methods are pure
/// over an injected `now` so the lease semantics are unit-testable
/// without an app.
#[derive(Resource, Default)]
pub struct PresenceRegistry {
    /// Participants by (owner namespace, caller-local id).
    participants: HashMap<(u8, String), ParticipantRecord>,
    /// Notes by (owner namespace, caller-local id).
    notes: HashMap<(u8, String), NoteRecord>,
    /// Bumped on any successful mutation (including removal and reset)
    /// for cheap change detection by render systems.
    mutation_seq: u64,
}

impl PresenceRegistry {
    /// The change-detection sequence: bumped on any successful mutation.
    pub fn mutation_seq(&self) -> u64 {
        self.mutation_seq
    }

    /// Whether the registry holds no rows at all (the render systems'
    /// run condition).
    pub fn is_empty(&self) -> bool {
        self.participants.is_empty() && self.notes.is_empty()
    }

    /// Participants stored in `namespace` — fresh *and* expired (expired
    /// rows occupy their cap slot until `user.leave`).
    fn participant_count(&self, namespace: u8) -> usize {
        self.participants
            .keys()
            .filter(|(entry_namespace, _)| *entry_namespace == namespace)
            .count()
    }

    /// Notes stored in `namespace` — fresh and expired.
    fn note_count(&self, namespace: u8) -> usize {
        self.notes
            .keys()
            .filter(|(entry_namespace, _)| *entry_namespace == namespace)
            .count()
    }

    /// Per-namespace fresh row counts as `(participants, notes)`, for
    /// the `state.namespaces` aggregate. Fresh rows only — public =
    /// rendered — and a namespace with nothing fresh is absent entirely,
    /// so expired foreign existence never leaks through the aggregate.
    pub fn fresh_counts(&self, now: Duration) -> HashMap<u8, (usize, usize)> {
        let mut counts: HashMap<u8, (usize, usize)> = HashMap::new();
        for ((namespace, _), record) in &self.participants {
            if record.fresh(now) {
                counts.entry(*namespace).or_default().0 += 1;
            }
        }
        for ((namespace, _), record) in &self.notes {
            if record.fresh(now) {
                counts.entry(*namespace).or_default().1 += 1;
            }
        }
        counts
    }

    /// `user.join`: creates (or `replace=true`-overwrites) the
    /// participant (namespace, id). An existing id — fresh *or* expired —
    /// without replace is `already-exists` (#16); a replace overwrites
    /// every field (the cursor clears — a fresh join states complete new
    /// state) but the revision continues. New ids respect the namespace
    /// cap; replacing is never a new slot.
    #[allow(clippy::too_many_arguments)]
    fn join(
        &mut self,
        namespace: u8,
        id: &str,
        name: &str,
        color: &str,
        ttl: Option<f32>,
        replace: bool,
        now: Duration,
    ) -> Result<(), PresenceReject> {
        validate_id(id)?;
        validate_name(name)?;
        validate_color(color)?;
        let ttl = pin_ttl(ttl, DEFAULT_PRESENCE_TTL_SECS)?;
        let key = (namespace, id.to_string());
        let existing = self.participants.get(&key);
        if existing.is_some() && !replace {
            return Err((
                codes::ALREADY_EXISTS,
                format!("participant '{id}' exists (pass replace=true to overwrite it)"),
            ));
        }
        if existing.is_none()
            && self.participant_count(namespace) >= MAX_PRESENCE_PARTICIPANTS_PER_NAMESPACE
        {
            return Err((
                codes::NAMESPACE_CAP,
                format!(
                    "namespace {namespace} is at its \
                     {MAX_PRESENCE_PARTICIPANTS_PER_NAMESPACE}-participant limit"
                ),
            ));
        }
        let revision = existing.map(|record| record.revision).unwrap_or(0) + 1;
        self.participants.insert(
            key,
            ParticipantRecord {
                name: name.to_string(),
                color: color.to_string(),
                cursor: None,
                updated: now,
                ttl,
                revision,
            },
        );
        self.mutation_seq += 1;
        Ok(())
    }

    /// `user.renew`: refreshes the lease clock, optionally re-pinning
    /// the TTL. Renewing an expired participant revives it — expiry
    /// never deleted the row, so it is still there to revive (#21).
    fn renew(
        &mut self,
        namespace: u8,
        id: &str,
        ttl: Option<f32>,
        now: Duration,
    ) -> Result<(), PresenceReject> {
        let pinned = match ttl {
            None => None,
            Some(_) => Some(pin_ttl(ttl, DEFAULT_PRESENCE_TTL_SECS)?),
        };
        let record = self
            .participants
            .get_mut(&(namespace, id.to_string()))
            .ok_or_else(|| unknown_participant(id))?;
        if let Some(ttl) = pinned {
            record.ttl = ttl;
        }
        record.updated = now;
        record.revision += 1;
        self.mutation_seq += 1;
        Ok(())
    }

    /// `user.cursor`: updates the cursor cell and refreshes the lease
    /// (any successful own-participant mutation does). The wire value is
    /// stored as given; rendering clamps into the live grid.
    fn set_cursor(
        &mut self,
        namespace: u8,
        id: &str,
        x: u16,
        y: u16,
        now: Duration,
    ) -> Result<(), PresenceReject> {
        let record = self
            .participants
            .get_mut(&(namespace, id.to_string()))
            .ok_or_else(|| unknown_participant(id))?;
        record.cursor = Some((x, y));
        record.updated = now;
        record.revision += 1;
        self.mutation_seq += 1;
        Ok(())
    }

    /// `user.leave`: removes the participant — with `note.remove` and
    /// `reset`, the only ways a row leaves the registry. Works on
    /// expired rows too (leaving is not gated on freshness).
    fn leave(&mut self, namespace: u8, id: &str) -> Result<(), PresenceReject> {
        self.participants
            .remove(&(namespace, id.to_string()))
            .map(|_| self.mutation_seq += 1)
            .ok_or_else(|| unknown_participant(id))
    }

    /// `note`: creates (or `replace=true`-overwrites) the note
    /// (namespace, id) — `user.join`'s exact collision, cap, and
    /// revision semantics on the note map.
    #[allow(clippy::too_many_arguments)]
    fn set_note(
        &mut self,
        namespace: u8,
        id: &str,
        text: &str,
        x: u16,
        y: u16,
        ttl: Option<f32>,
        replace: bool,
        now: Duration,
    ) -> Result<(), PresenceReject> {
        validate_id(id)?;
        if text.is_empty() {
            return Err((codes::BAD_PAYLOAD, "text= must be non-empty".to_string()));
        }
        if text.len() > MAX_PRESENCE_NOTE_TEXT_BYTES {
            return Err((
                codes::TOO_LARGE,
                format!("note text exceeds {MAX_PRESENCE_NOTE_TEXT_BYTES} bytes"),
            ));
        }
        let ttl = pin_ttl(ttl, DEFAULT_PRESENCE_NOTE_TTL_SECS)?;
        let key = (namespace, id.to_string());
        let existing = self.notes.get(&key);
        if existing.is_some() && !replace {
            return Err((
                codes::ALREADY_EXISTS,
                format!("note '{id}' exists (pass replace=true to overwrite it)"),
            ));
        }
        if existing.is_none() && self.note_count(namespace) >= MAX_PRESENCE_NOTES_PER_NAMESPACE {
            return Err((
                codes::NAMESPACE_CAP,
                format!(
                    "namespace {namespace} is at its {MAX_PRESENCE_NOTES_PER_NAMESPACE}-note limit"
                ),
            ));
        }
        let revision = existing.map(|record| record.revision).unwrap_or(0) + 1;
        self.notes.insert(
            key,
            NoteRecord {
                text: text.to_string(),
                x,
                y,
                updated: now,
                ttl,
                revision,
            },
        );
        self.mutation_seq += 1;
        Ok(())
    }

    /// `note.remove`: removes the note (expired or fresh).
    fn remove_note(&mut self, namespace: u8, id: &str) -> Result<(), PresenceReject> {
        self.notes
            .remove(&(namespace, id.to_string()))
            .map(|_| self.mutation_seq += 1)
            .ok_or_else(|| {
                (
                    codes::UNKNOWN_ID,
                    format!("no note '{id}' in the caller's namespace"),
                )
            })
    }

    /// Full session reset: clear every roster. Presence is wire-tier
    /// state with no trusted tier, so everything goes; called from the
    /// `reset` tap, which acks elsewhere.
    fn reset(&mut self) {
        if self.is_empty() {
            return;
        }
        self.participants.clear();
        self.notes.clear();
        self.mutation_seq += 1;
    }
}

/// The flat `unknown-id` for a missing participant.
fn unknown_participant(id: &str) -> PresenceReject {
    (
        codes::UNKNOWN_ID,
        format!("no participant '{id}' in the caller's namespace"),
    )
}

/// Validates a participant/note id: ASCII alphanumerics plus `_`, `-`,
/// `.` (the sensor-component charset), non-empty, byte-capped.
fn validate_id(id: &str) -> Result<(), PresenceReject> {
    let well_formed = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.');
    if !well_formed {
        return Err((
            codes::BAD_PAYLOAD,
            "presence ids are ascii [A-Za-z0-9_.-], non-empty".to_string(),
        ));
    }
    if id.len() > MAX_PRESENCE_ID_BYTES {
        return Err((
            codes::TOO_LARGE,
            format!("id exceeds {MAX_PRESENCE_ID_BYTES} bytes"),
        ));
    }
    Ok(())
}

/// Validates a display name: any UTF-8, non-empty (the parser defaults
/// it to the id, so emptiness means a caller sent an explicitly empty
/// `name=`), byte-capped.
fn validate_name(name: &str) -> Result<(), PresenceReject> {
    if name.is_empty() {
        return Err((codes::BAD_PAYLOAD, "name= must be non-empty".to_string()));
    }
    if name.len() > MAX_PRESENCE_NAME_BYTES {
        return Err((
            codes::TOO_LARGE,
            format!("name exceeds {MAX_PRESENCE_NAME_BYTES} bytes"),
        ));
    }
    Ok(())
}

/// Validates a cursor color: strict `#rrggbb`. Colors are
/// identity-adjacent — a participant is recognized by their color — so
/// no lenient effects-style coercion to a default.
fn validate_color(color: &str) -> Result<(), PresenceReject> {
    let bytes = color.as_bytes();
    let well_formed =
        bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(u8::is_ascii_hexdigit);
    if !well_formed {
        return Err((
            codes::BAD_PAYLOAD,
            format!("color must be strict #rrggbb (got '{color}')"),
        ));
    }
    Ok(())
}

/// Pins a lease TTL: absent picks the family default; present must be
/// finite and above 0, clamped to the documented range — the exact
/// sensor TTL pattern.
fn pin_ttl(ttl: Option<f32>, default_secs: f32) -> Result<Duration, PresenceReject> {
    match ttl {
        None => Ok(Duration::from_secs_f32(default_secs)),
        Some(value) if value.is_finite() && value > 0.0 => Ok(Duration::from_secs_f32(
            value.clamp(MIN_PRESENCE_TTL_SECS, MAX_PRESENCE_TTL_SECS),
        )),
        Some(_) => Err((
            codes::BAD_PAYLOAD,
            "ttl= must be a finite value above 0".to_string(),
        )),
    }
}

/// `state.presence`: the roster rows under the three-tier read scope —
/// the caller's own namespace in full **including expired rows**
/// (`fresh: false` visible, #21), foreign namespaces as fresh rows only
/// (rendered = public; an expired foreign row's existence must not
/// leak). Participant rows precede note rows, (namespace, id)-ordered
/// within each kind; the stable enumeration keys pagination (the
/// macros.rs shape).
pub fn presence_state_items(
    registry: &PresenceRegistry,
    namespace: u8,
    now: Duration,
) -> Vec<(u64, Value)> {
    let mut rows: Vec<((u8, u8, &str), Value)> = Vec::new();
    for ((entry_namespace, id), record) in &registry.participants {
        let fresh = record.fresh(now);
        if *entry_namespace != namespace && !fresh {
            continue;
        }
        rows.push((
            (0, *entry_namespace, id.as_str()),
            json!({
                "kind": "participant",
                "ns": entry_namespace,
                "id": id,
                "name": record.name,
                "color": record.color,
                "cursor": record
                    .cursor
                    .map_or(Value::Null, |(x, y)| json!({ "x": x, "y": y })),
                "fresh": fresh,
                "age_secs": now.saturating_sub(record.updated).as_secs_f32(),
                "ttl_secs": record.ttl.as_secs_f32(),
                "revision": record.revision,
            }),
        ));
    }
    for ((entry_namespace, id), record) in &registry.notes {
        let fresh = record.fresh(now);
        if *entry_namespace != namespace && !fresh {
            continue;
        }
        rows.push((
            (1, *entry_namespace, id.as_str()),
            json!({
                "kind": "note",
                "ns": entry_namespace,
                "id": id,
                "text": record.text,
                "x": record.x,
                "y": record.y,
                "fresh": fresh,
                "age_secs": now.saturating_sub(record.updated).as_secs_f32(),
                "ttl_secs": record.ttl.as_secs_f32(),
                "revision": record.revision,
            }),
        ));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter()
        .enumerate()
        .map(|(index, (_, value))| (index as u64, value))
        .collect()
}

// ── Rendering: markers on the scene, notes and labels in the texture ──

/// Caret width and thickness as fractions of the cell width.
const MARKER_WIDTH_FRACTION: f32 = 0.28;

/// Caret height as a fraction of the cell height.
const MARKER_HEIGHT_FRACTION: f32 = 0.85;

/// Plane-local depth offset for markers in the 3D modes: above the
/// warped surface, below the RGP object baseline of 8.
const MARKER_PLANE_OFFSET: f32 = 6.0;

/// Note text glyph height as a fraction of the cell height.
const NOTE_GLYPH_HEIGHT_FRACTION: f32 = 0.62;

/// Note panel padding as a fraction of the note glyph height.
const NOTE_PAD_FRACTION: f32 = 0.40;

/// Note border stroke width as a fraction of the panel height.
const NOTE_BORDER_STROKE: f32 = 0.06;

/// Label glyph height as a fraction of the cell height.
const LABEL_GLYPH_HEIGHT_FRACTION: f32 = 0.55;

/// Gap between the cursor cell and its name label, in cell widths.
const LABEL_GAP_FRACTION: f32 = 0.35;

/// Displayed name-label glyph cap; the full name stays queryable.
const LABEL_MAX_GLYPHS: usize = 16;

/// Note panel fill: a subtle scrim so the text under it stays hinted.
const NOTE_FILL: [f32; 4] = [0.05, 0.05, 0.08, 0.82];

/// Note panel border: the fixed presence accent (the avatar-bubble hue).
/// The wire carries no color on `note`, so this cannot be
/// per-participant — an honest limitation, not a default.
const NOTE_BORDER: [f32; 4] = [0.35, 0.76, 1.0, 0.9];

/// Note text color.
const NOTE_TEXT: [f32; 4] = [0.91, 0.93, 0.96, 1.0];

/// Label scrim fill: the note scrim's translucent posture, so a name
/// over occupied cells reads against a backing instead of superimposing
/// on the text beneath (#78). No border — labels are short where notes
/// are prose.
const LABEL_FILL: [f32; 4] = [0.05, 0.05, 0.08, 0.82];

/// One fresh, cursor-bearing participant queued for rendering: the
/// (namespace, id) key, the record, and its reported cursor cell.
type LabeledParticipant<'a> = ((u8, &'a str), &'a ParticipantRecord, (u16, u16));

/// One participant's cursor caret in the main scene — presence's own
/// marker component, deliberately not [`crate::inline::TerminalRgpObject`]:
/// `sync_inline_objects` full-syncs by despawning every RGP entity, and
/// a Kitty upload would silently evict every cursor if markers shared
/// the component.
#[derive(Component)]
pub(crate) struct PresenceCursorMarker {
    /// Owner namespace of the participant this caret embodies.
    namespace: u8,
    /// Caller-local participant id.
    id: String,
    /// Last-applied color channels, for material change detection.
    color: [u8; 3],
}

/// Clamps a wire cursor/note cell into the live grid (clamp-to-edge;
/// the stored wire value is untouched).
fn clamped_cell(x: u16, y: u16, cols: u16, rows: u16) -> (u16, u16) {
    (x.min(cols.saturating_sub(1)), y.min(rows.saturating_sub(1)))
}

/// Parses a strict `#rrggbb` into its channels. Stored colors are
/// validated at apply time ([`validate_color`]), so `None` is
/// unreachable for registry records — the render side stays a total
/// function instead of panicking on a broken invariant.
fn hex_rgb(color: &str) -> Option<[u8; 3]> {
    let bytes = color.as_bytes();
    if bytes.len() != 7 || bytes[0] != b'#' {
        return None;
    }
    let channel = |index: usize| -> Option<u8> {
        let high = char::from(bytes[index]).to_digit(16)?;
        let low = char::from(bytes[index + 1]).to_digit(16)?;
        Some((high * 16 + low) as u8)
    };
    Some([channel(1)?, channel(3)?, channel(5)?])
}

/// The presence underlays for one frame: fresh note panels first, then
/// fresh participants' name labels, each batch (namespace, id)-sorted so
/// overlaps stack deterministically (the viz ascending-id posture).
/// Rendered = public, so every namespace draws. Coordinates are physical
/// texture pixels, computed where the cell metrics live
/// ([`crate::terminal::TerminalSurface::sync_image`]).
pub(crate) fn presence_underlays(
    registry: &PresenceRegistry,
    now: Duration,
    cols: u16,
    rows: u16,
    cell_width: f32,
    cell_height: f32,
) -> Vec<(UnderlayRect, Vec<VizDrawOp>)> {
    let mut underlays = Vec::new();
    if cell_width <= 0.0 || cell_height <= 0.0 {
        return underlays;
    }
    let mut notes: Vec<(&(u8, String), &NoteRecord)> = registry
        .notes
        .iter()
        .filter(|(_, record)| record.fresh(now))
        .collect();
    notes.sort_by(|a, b| a.0.cmp(b.0));
    for (_, record) in notes {
        underlays.push(note_underlay(record, cols, rows, cell_width, cell_height));
    }
    let mut labeled: Vec<LabeledParticipant<'_>> = registry
        .participants
        .iter()
        .filter_map(|((namespace, id), record)| {
            let cursor = record.cursor?;
            record
                .fresh(now)
                .then_some(((*namespace, id.as_str()), record, cursor))
        })
        .collect();
    labeled.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, record, cursor) in labeled {
        underlays.extend(label_underlay(
            record,
            cursor,
            cols,
            rows,
            cell_width,
            cell_height,
        ));
    }
    underlays
}

/// One note panel: a subtle filled rect, the accent border, and the text
/// on a single line — anchored at the (clamped) cell, one cell tall,
/// truncated at the grid's right edge.
fn note_underlay(
    record: &NoteRecord,
    cols: u16,
    rows: u16,
    cell_width: f32,
    cell_height: f32,
) -> (UnderlayRect, Vec<VizDrawOp>) {
    let (col, row) = clamped_cell(record.x, record.y, cols, rows);
    let glyph_height = cell_height * NOTE_GLYPH_HEIGHT_FRACTION;
    let advance = glyph_height * GLYPH_ASPECT;
    let pad = glyph_height * NOTE_PAD_FRACTION;
    let x = f32::from(col) * cell_width;
    let glyphs = record.text.chars().count() as f32;
    let width = (pad * 2.0 + advance * glyphs).min(f32::from(cols) * cell_width - x);
    let rect = UnderlayRect {
        x,
        y: f32::from(row) * cell_height,
        width,
        height: cell_height,
    };
    let pad_fraction = pad / width;
    let ops = vec![
        VizDrawOp::Fill {
            min: (0.0, 0.0),
            max: (1.0, 1.0),
            color: NOTE_FILL,
        },
        VizDrawOp::Polyline {
            points: vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)],
            width: NOTE_BORDER_STROKE,
            color: NOTE_BORDER,
        },
        VizDrawOp::Text {
            pos: (pad_fraction, (1.0 - NOTE_GLYPH_HEIGHT_FRACTION) * 0.5),
            height: NOTE_GLYPH_HEIGHT_FRACTION,
            text: record.text.clone(),
            color: NOTE_TEXT,
            align: TextAlign::Left,
            max_width: Some((1.0 - pad_fraction * 2.0).max(0.0)),
        },
    ];
    (rect, ops)
}

/// One participant name label: a subtle scrim, then participant-colored
/// stroke text beside the (clamped) cursor cell, shifted left at the
/// grid's right edge so it stays on the texture. `None` only for an
/// unparseable stored color, which apply-time validation makes
/// unreachable.
fn label_underlay(
    record: &ParticipantRecord,
    cursor: (u16, u16),
    cols: u16,
    rows: u16,
    cell_width: f32,
    cell_height: f32,
) -> Option<(UnderlayRect, Vec<VizDrawOp>)> {
    let rgb = hex_rgb(&record.color)?;
    let (col, row) = clamped_cell(cursor.0, cursor.1, cols, rows);
    let glyph_height = cell_height * LABEL_GLYPH_HEIGHT_FRACTION;
    let text: String = record.name.chars().take(LABEL_MAX_GLYPHS).collect();
    let width = glyph_height * GLYPH_ASPECT * text.chars().count() as f32;
    let x = ((f32::from(col) + 1.0) * cell_width + cell_width * LABEL_GAP_FRACTION)
        .min((f32::from(cols) * cell_width - width).max(0.0));
    let ops = vec![
        VizDrawOp::Fill {
            min: (0.0, 0.0),
            max: (1.0, 1.0),
            color: LABEL_FILL,
        },
        VizDrawOp::Text {
            pos: (0.0, (1.0 - LABEL_GLYPH_HEIGHT_FRACTION) * 0.5),
            height: LABEL_GLYPH_HEIGHT_FRACTION,
            text,
            color: [
                f32::from(rgb[0]) / 255.0,
                f32::from(rgb[1]) / 255.0,
                f32::from(rgb[2]) / 255.0,
                1.0,
            ],
            align: TextAlign::Left,
            max_width: None,
        },
    ];
    Some((
        UnderlayRect {
            x,
            y: f32::from(row) * cell_height,
            width,
            height: cell_height,
        },
        ops,
    ))
}

/// The world pose (translation, rotation) for a marker at `cursor`:
/// screen-space above the terminal in the flat view (the RGP depth
/// neighborhood), pinned to the warped surface through
/// [`plane_surface_point`] in the 3D modes. `None` when the 3D plane is
/// missing (the sync hides the marker, the RGP posture).
#[allow(clippy::too_many_arguments)]
fn marker_pose(
    cursor: (u16, u16),
    cols: u16,
    rows: u16,
    viewport: &TerminalViewport,
    mode: TerminalPresentationMode,
    plane_transform: Option<&Transform>,
    warp_amount: f32,
    elapsed_secs: f32,
    mobius_progress: f32,
) -> Option<(Vec3, Quat)> {
    let (col, row) = clamped_cell(cursor.0, cursor.1, cols, rows);
    let cols_f = f32::from(cols.max(1));
    let rows_f = f32::from(rows.max(1));
    let center_col = f32::from(col) + 0.5;
    let center_row = f32::from(row) + 0.5;
    match mode {
        TerminalPresentationMode::Flat2d => Some((
            Vec3::new(
                viewport.center.x - viewport.size.x * 0.5 + center_col * (viewport.size.x / cols_f),
                viewport.center.y + viewport.size.y * 0.5 - center_row * (viewport.size.y / rows_f),
                CURSOR_DEPTH,
            ),
            Quat::IDENTITY,
        )),
        TerminalPresentationMode::Plane3d | TerminalPresentationMode::Mobius3d => {
            let plane_transform = plane_transform?;
            let local = plane_surface_point(
                mode,
                center_col / cols_f - 0.5,
                0.5 - center_row / rows_f,
                warp_amount,
                elapsed_secs,
                MARKER_PLANE_OFFSET,
                mobius_progress,
            );
            Some((
                plane_transform.transform_point(local),
                plane_transform.rotation,
            ))
        }
    }
}

/// The material a caret marker renders with: unlit, so the participant
/// color reads exactly as sent regardless of scene lighting.
fn marker_material(rgb: [u8; 3]) -> StandardMaterial {
    StandardMaterial {
        base_color: Color::srgb_u8(rgb[0], rgb[1], rgb[2]),
        unlit: true,
        cull_mode: None,
        ..default()
    }
}

/// Query type for the live markers (`Without<TerminalPlane>` keeps the
/// mutable `Transform` access provably disjoint from the plane read).
type PresenceMarkerQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut PresenceCursorMarker,
        &'static mut Transform,
        &'static mut Visibility,
        &'static MeshMaterial3d<StandardMaterial>,
    ),
    Without<TerminalPlane>,
>;

/// Parameters for [`sync_presence_cursor_markers`].
#[derive(SystemParam)]
pub(crate) struct PresenceMarkerParams<'w, 's> {
    commands: Commands<'w, 's>,
    time: Res<'w, Time>,
    registry: Res<'w, PresenceRegistry>,
    terminal: Query<'w, 's, &'static TerminalSurface>,
    viewport: Query<'w, 's, &'static TerminalViewport>,
    presentation: Res<'w, TerminalPresentation>,
    mobius_transition: Res<'w, MobiusTransition>,
    plane_warp: Query<'w, 's, &'static TerminalPlaneWarp>,
    plane_query: Query<'w, 's, &'static Transform, With<TerminalPlane>>,
    markers: PresenceMarkerQuery<'w, 's>,
    meshes: ResMut<'w, Assets<Mesh>>,
    materials: ResMut<'w, Assets<StandardMaterial>>,
    /// The unit-cube mesh every caret shares, created on first need.
    marker_mesh: Local<'s, Option<Handle<Mesh>>>,
}

/// Synchronizes the cursor caret markers with the participant roster:
/// spawn on first sight, despawn on expiry / leave / cursor loss (a
/// join-replace clears the cursor), and pose every survivor at its
/// clamped cell through the active presentation projection each frame
/// (the warped surface animates, so poses are per-frame like RGP; the
/// writes go through `set_if_neq` so change detection only fires on
/// actual movement). Materials are only touched when the participant's
/// color actually changed.
pub(crate) fn sync_presence_cursor_markers(mut params: PresenceMarkerParams) {
    let PresenceMarkerParams {
        commands,
        time,
        registry,
        terminal,
        viewport,
        presentation,
        mobius_transition,
        plane_warp,
        plane_query,
        markers,
        meshes,
        materials,
        marker_mesh,
    } = &mut params;
    let terminal = match terminal.single() {
        Ok(terminal) => terminal,
        Err(err) => {
            // Latched once per process: the surface lives on THE terminal
            // seat, and the miss must name its system (#54's silent-.single()
            // finding).
            warn_once!(
                "sync_presence_cursor_markers: the surface needs exactly one terminal seat: {err}"
            );
            return;
        }
    };
    let viewport = match viewport.single() {
        Ok(viewport) => viewport,
        Err(err) => {
            // Latched once per process: the viewport lives on THE terminal
            // seat, and the miss must name its system (#54's silent-.single()
            // finding).
            warn_once!(
                "sync_presence_cursor_markers: the viewport needs exactly one terminal seat: {err}"
            );
            return;
        }
    };
    let plane_warp = match plane_warp.single() {
        Ok(plane_warp) => plane_warp,
        Err(err) => {
            // Latched once per process: the warp lives on THE terminal
            // seat, and the miss must name its system (#54's silent-.single()
            // finding).
            warn_once!(
                "sync_presence_cursor_markers: the plane warp needs exactly one terminal seat: {err}"
            );
            return;
        }
    };
    let now = time.elapsed();
    let elapsed_secs = time.elapsed_secs();
    let cols = terminal.cols;
    let rows = terminal.rows;
    let cell_width = viewport.size.x / f32::from(cols.max(1));
    let cell_height = viewport.size.y / f32::from(rows.max(1));
    let scale = Vec3::new(
        cell_width * MARKER_WIDTH_FRACTION,
        cell_height * MARKER_HEIGHT_FRACTION,
        cell_width * MARKER_WIDTH_FRACTION,
    );
    let mobius_progress = active_mobius_progress(presentation.mode, mobius_transition);
    let plane_transform = match plane_query.single() {
        Ok(plane_transform) => Some(plane_transform),
        Err(err) => {
            // Latched once per process: the 3D projections need THE plane;
            // markers hide there until it is back (Flat2d poses never
            // consult it).
            warn_once!(
                "sync_presence_cursor_markers: presence markers need exactly one terminal plane: {err}"
            );
            None
        }
    };

    // The desired marker set: fresh participants with a reported cursor.
    // A Vec + linear claim keeps this allocation-light — the roster is
    // 16-capped per namespace and this runs every presence-active frame.
    let mut desired: Vec<LabeledParticipant<'_>> = registry
        .participants
        .iter()
        .filter_map(|((namespace, id), record)| {
            let cursor = record.cursor?;
            record
                .fresh(now)
                .then_some(((*namespace, id.as_str()), record, cursor))
        })
        .collect();

    for (entity, mut marker, mut transform, mut visibility, material_handle) in markers.iter_mut() {
        let claimed = desired
            .iter()
            .position(|((namespace, id), _, _)| *namespace == marker.namespace && *id == marker.id)
            .map(|index| desired.swap_remove(index));
        let Some((_, record, cursor)) = claimed else {
            commands.entity(entity).despawn();
            continue;
        };
        match marker_pose(
            cursor,
            cols,
            rows,
            viewport,
            presentation.mode,
            plane_transform,
            plane_warp.amount,
            elapsed_secs,
            mobius_progress,
        ) {
            Some((translation, rotation)) => {
                transform.set_if_neq(Transform {
                    translation,
                    rotation,
                    scale,
                });
                visibility.set_if_neq(Visibility::Visible);
            }
            None => {
                visibility.set_if_neq(Visibility::Hidden);
            }
        }
        if let Some(rgb) = hex_rgb(&record.color)
            && marker.color != rgb
        {
            if let Some(mut material) = materials.get_mut(&material_handle.0) {
                material.base_color = Color::srgb_u8(rgb[0], rgb[1], rgb[2]);
            }
            marker.color = rgb;
        }
    }

    // Spawn the newcomers, key-sorted so entity order is deterministic
    // (the claim pass swap-removes, which shuffles the survivors).
    desired.sort_by(|a, b| a.0.cmp(&b.0));
    for ((namespace, id), record, cursor) in desired {
        let Some(rgb) = hex_rgb(&record.color) else {
            continue; // Unreachable: colors are validated at apply.
        };
        let mesh = marker_mesh
            .get_or_insert_with(|| meshes.add(Cuboid::new(1.0, 1.0, 1.0)))
            .clone();
        let (transform, visibility) = match marker_pose(
            cursor,
            cols,
            rows,
            viewport,
            presentation.mode,
            plane_transform,
            plane_warp.amount,
            elapsed_secs,
            mobius_progress,
        ) {
            Some((translation, rotation)) => (
                Transform {
                    translation,
                    rotation,
                    scale,
                },
                Visibility::Visible,
            ),
            None => (Transform::default(), Visibility::Hidden),
        };
        commands.spawn((
            PresenceCursorMarker {
                namespace,
                id: id.to_string(),
                color: rgb,
            },
            Mesh3d(mesh),
            MeshMaterial3d(materials.add(marker_material(rgb))),
            transform,
            visibility,
        ));
    }
}

/// The texture-facing state stamp: the mutation sequence plus the count
/// of fresh in-texture rows (note panels and labeled cursors). Between
/// two equal sequences freshness can only decay, so an equal stamp
/// proves the drawn texture is still exact — no per-row set tracking
/// needed.
fn presence_texture_stamp(registry: &PresenceRegistry, now: Duration) -> (u64, usize) {
    let fresh_notes = registry
        .notes
        .values()
        .filter(|record| record.fresh(now))
        .count();
    let fresh_labels = registry
        .participants
        .values()
        .filter(|record| record.cursor.is_some() && record.fresh(now))
        .count();
    (registry.mutation_seq, fresh_notes + fresh_labels)
}

/// Requests a terminal redraw whenever the presence underlays would
/// change: on every registry mutation and — crucially — on every
/// fresh→expired flip, so an expired note or label disappears from an
/// otherwise idle terminal (#21's visible expiry, applied to pixels).
pub(crate) fn request_presence_expiry_redraw(
    time: Res<Time>,
    registry: Res<PresenceRegistry>,
    mut redraw: Query<&mut TerminalRedrawState>,
    mut drawn_stamp: Local<Option<(u64, usize)>>,
) {
    let mut redraw = match redraw.single_mut() {
        Ok(redraw) => redraw,
        Err(err) => {
            // Latched once per process: the redraw flag lives on THE terminal
            // seat, and the miss must name its system (#54's silent-.single()
            // finding).
            warn_once!(
                "request_presence_expiry_redraw: the redraw flag needs exactly one terminal seat: {err}"
            );
            return;
        }
    };
    let stamp = presence_texture_stamp(&registry, time.elapsed());
    if *drawn_stamp == Some(stamp) {
        return;
    }
    *drawn_stamp = Some(stamp);
    redraw.request();
}

/// Registers the presence registry, its applier, and the render side.
///
/// Ordering: `apply_presence_commands` runs after `pump_pty_output` (it
/// owns the `user.*`/`note` acks). The marker sync runs after the
/// applier and after `handle_window_resize` (it positions against the
/// live viewport), gated on there being anything to sync or clean up.
/// The expiry-redraw coupling runs between the applier and the redraw
/// pipeline so a request lands in the same frame's texture pass, gated
/// on a non-empty registry (an emptying `user.leave`/`reset` already
/// requests its redraw on the mutation path). `answer_queries` is
/// ordered after the applier in [`crate::ai::RattyAiPlugin`] so a
/// same-chunk "join then read" observes the join.
pub struct PresencePlugin;

impl Plugin for PresencePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PresenceRegistry>()
            .add_systems(
                Update,
                apply_presence_commands.after(crate::systems::pump_pty_output),
            )
            .add_systems(
                Update,
                sync_presence_cursor_markers
                    .after(apply_presence_commands)
                    .after(crate::systems::handle_window_resize)
                    .run_if(
                        |registry: Res<PresenceRegistry>,
                         markers: Query<(), With<PresenceCursorMarker>>| {
                            !registry.is_empty() || !markers.is_empty()
                        },
                    ),
            )
            .add_systems(
                Update,
                request_presence_expiry_redraw
                    .after(apply_presence_commands)
                    .before(crate::systems::TerminalRedrawSet)
                    .run_if(|registry: Res<PresenceRegistry>| !registry.is_empty()),
            );
    }
}

/// Handles the `user.*`/`note` presence commands (owning their acks)
/// from the shared [`AiCommand`] stream, plus the silent `reset` tap.
///
/// Every presence command must arrive as [`CommandOrigin::Wire`]:
/// presence identity is ingress truth, so a replayed join would forge
/// liveness (the reactive chain-depth guard, #25's forged-presence
/// trap). Ownership needs no check — commands only ever touch
/// `source.namespace()`'s own keys, by construction.
pub fn apply_presence_commands(
    time: Res<Time>,
    mut commands: MessageReader<AiCommand>,
    mut registry: ResMut<PresenceRegistry>,
    mut redraw: Query<&mut TerminalRedrawState>,
    mut acks: MessageWriter<AckOutcome>,
    mut diagnostics: ResMut<AiDiagnostics>,
) {
    let mut redraw = match redraw.single_mut() {
        Ok(redraw) => redraw,
        Err(err) => {
            // Latched once per process: the redraw flag lives on THE terminal
            // seat, and the miss must name its system (#54's silent-.single()
            // finding).
            warn_once!(
                "apply_presence_commands: the redraw flag needs exactly one terminal seat: {err}"
            );
            return;
        }
    };
    let now = time.elapsed();
    for AiCommand {
        source,
        ack_token,
        origin,
        command,
    } in commands.read()
    {
        macro_rules! reject {
            ($action:literal, $code:expr, $message:expr) => {{
                let message = $message;
                warn!("ratty-presence: {} rejected: {message}", $action);
                reject(
                    &mut diagnostics,
                    &mut acks,
                    *source,
                    ack_token,
                    $action,
                    $code,
                    message,
                );
            }};
        }
        macro_rules! guard_wire_origin {
            ($action:literal) => {
                if *origin != CommandOrigin::Wire {
                    reject!(
                        $action,
                        codes::NOT_PERMITTED,
                        "presence must originate from live ingress (a replayed join would \
                         forge liveness)"
                            .to_string()
                    );
                    continue;
                }
            };
        }
        macro_rules! commit {
            ($action:literal, $result:expr) => {
                match $result {
                    Ok(()) => {
                        redraw.request();
                        ack_commit(&mut acks, *source, ack_token);
                    }
                    Err((code, message)) => reject!($action, code, message),
                }
            };
        }
        match command {
            RattyAiCommand::UserJoin {
                id,
                name,
                color,
                ttl,
                replace,
            } => {
                guard_wire_origin!("user.join");
                commit!(
                    "user.join",
                    registry.join(source.namespace(), id, name, color, *ttl, *replace, now)
                );
            }
            RattyAiCommand::UserRenew { id, ttl } => {
                guard_wire_origin!("user.renew");
                commit!(
                    "user.renew",
                    registry.renew(source.namespace(), id, *ttl, now)
                );
            }
            RattyAiCommand::UserCursor { id, x, y } => {
                guard_wire_origin!("user.cursor");
                commit!(
                    "user.cursor",
                    registry.set_cursor(source.namespace(), id, *x, *y, now)
                );
            }
            RattyAiCommand::UserLeave { id } => {
                guard_wire_origin!("user.leave");
                commit!("user.leave", registry.leave(source.namespace(), id));
            }
            RattyAiCommand::Note {
                id,
                text,
                x,
                y,
                ttl,
                replace,
            } => {
                guard_wire_origin!("note");
                commit!(
                    "note",
                    registry.set_note(source.namespace(), id, text, *x, *y, *ttl, *replace, now)
                );
            }
            RattyAiCommand::NoteRemove { id } => {
                guard_wire_origin!("note.remove");
                commit!("note.remove", registry.remove_note(source.namespace(), id));
            }
            RattyAiCommand::Reset => {
                // Full session reset. Reset's single ack belongs to
                // apply_ai_commands; the presence registry clears its
                // rosters silently (there is no trusted tier here).
                registry.reset();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::IngressSource;
    use bevy::ecs::message::Messages;

    fn t(secs: f32) -> Duration {
        Duration::from_secs_f32(secs)
    }

    /// A minimal well-formed join for namespace `ns`.
    fn join(registry: &mut PresenceRegistry, ns: u8, id: &str, now: Duration) {
        registry
            .join(ns, id, id, "#00ff00", None, false, now)
            .expect("registry op ok");
    }

    #[test]
    fn limits_are_pinned() {
        assert_eq!(MAX_PRESENCE_PARTICIPANTS_PER_NAMESPACE, 16);
        assert_eq!(MAX_PRESENCE_NOTES_PER_NAMESPACE, 16);
        assert_eq!(MAX_PRESENCE_ID_BYTES, 48);
        assert_eq!(MAX_PRESENCE_NAME_BYTES, 64);
        assert_eq!(MAX_PRESENCE_NOTE_TEXT_BYTES, 256);
        assert_eq!(DEFAULT_PRESENCE_TTL_SECS, 60.0);
        assert_eq!(MIN_PRESENCE_TTL_SECS, 1.0);
        assert_eq!(MAX_PRESENCE_TTL_SECS, 3600.0);
        assert_eq!(DEFAULT_PRESENCE_NOTE_TTL_SECS, 300.0);
    }

    #[test]
    fn join_validation_rejects_bad_ids_names_colors_and_ttls() {
        let mut registry = PresenceRegistry::default();
        // Ids: charset, emptiness, length — each with its code.
        for bad_id in ["", "bad id", "café", "a/b", "a&b"] {
            let (code, _) = registry
                .join(0, bad_id, "n", "#00ff00", None, false, t(0.0))
                .expect_err("malformed id");
            assert_eq!(code, codes::BAD_PAYLOAD, "id '{bad_id}'");
        }
        let long_id = "x".repeat(MAX_PRESENCE_ID_BYTES + 1);
        let (code, _) = registry
            .join(0, &long_id, "n", "#00ff00", None, false, t(0.0))
            .expect_err("long id");
        assert_eq!(code, codes::TOO_LARGE);
        // Names: non-empty (the parser defaults absent names to the id,
        // so only an explicitly empty name= reaches this), byte-capped.
        let (code, _) = registry
            .join(0, "a", "", "#00ff00", None, false, t(0.0))
            .expect_err("empty name");
        assert_eq!(code, codes::BAD_PAYLOAD);
        let long_name = "x".repeat(MAX_PRESENCE_NAME_BYTES + 1);
        let (code, _) = registry
            .join(0, "a", &long_name, "#00ff00", None, false, t(0.0))
            .expect_err("long name");
        assert_eq!(code, codes::TOO_LARGE);
        // Colors: strict #rrggbb only.
        for bad_color in ["00ff00", "#00ff0", "#00ff000", "#00gg00", "green", "#"] {
            let (code, _) = registry
                .join(0, "a", "a", bad_color, None, false, t(0.0))
                .expect_err("malformed color");
            assert_eq!(code, codes::BAD_PAYLOAD, "color '{bad_color}'");
        }
        // TTLs: finite and above 0 or bust (the sensor pattern).
        for bad_ttl in [f32::NAN, f32::INFINITY, 0.0, -1.0] {
            let (code, _) = registry
                .join(0, "a", "a", "#00ff00", Some(bad_ttl), false, t(0.0))
                .expect_err("bad ttl");
            assert_eq!(code, codes::BAD_PAYLOAD, "ttl {bad_ttl}");
        }
        // Atomic: nothing above partially applied.
        assert!(registry.is_empty(), "a rejected join applies nothing");
        assert_eq!(registry.mutation_seq(), 0);
        // In-range values clamp rather than reject; unicode names pass.
        registry
            .join(0, "a", "Ålice W", "#AbCdEf", Some(10_000.0), false, t(0.0))
            .expect("registry op ok");
        let record = registry
            .participants
            .get(&(0, "a".to_string()))
            .expect("stored");
        assert_eq!(record.ttl, t(MAX_PRESENCE_TTL_SECS), "ttl clamps");
    }

    #[test]
    fn note_validation_and_collision_mirror_the_participant_rules() {
        let mut registry = PresenceRegistry::default();
        let (code, _) = registry
            .set_note(0, "n1", "", 1, 2, None, false, t(0.0))
            .expect_err("empty text");
        assert_eq!(code, codes::BAD_PAYLOAD);
        let long = "x".repeat(MAX_PRESENCE_NOTE_TEXT_BYTES + 1);
        let (code, _) = registry
            .set_note(0, "n1", &long, 1, 2, None, false, t(0.0))
            .expect_err("long text");
        assert_eq!(code, codes::TOO_LARGE);
        registry
            .set_note(0, "n1", "hi", 1, 2, None, false, t(0.0))
            .expect("registry op ok");
        let (code, _) = registry
            .set_note(0, "n1", "again", 1, 2, None, false, t(0.0))
            .expect_err("collision");
        assert_eq!(code, codes::ALREADY_EXISTS);
        registry
            .set_note(0, "n1", "again", 3, 4, None, true, t(1.0))
            .expect("replace overwrites");
        let record = registry.notes.get(&(0, "n1".to_string())).expect("stored");
        assert_eq!((record.x, record.y, record.revision), (3, 4, 2));
        // Default note TTL is the note default, not the participant one.
        assert_eq!(record.ttl, t(DEFAULT_PRESENCE_NOTE_TTL_SECS));
        registry.remove_note(0, "n1").expect("registry op ok");
        let (code, _) = registry.remove_note(0, "n1").expect_err("gone");
        assert_eq!(code, codes::UNKNOWN_ID);
    }

    #[test]
    fn collision_replace_and_revive_matrix() {
        let mut registry = PresenceRegistry::default();
        registry
            .join(0, "a", "a", "#00ff00", Some(10.0), false, t(0.0))
            .expect("registry op ok");
        // Fresh × no-replace: already-exists.
        let (code, _) = registry
            .join(0, "a", "a", "#00ff00", None, false, t(1.0))
            .expect_err("fresh collision");
        assert_eq!(code, codes::ALREADY_EXISTS);
        // Fresh × replace: overwrite, revision continues.
        registry
            .join(0, "a", "renamed", "#ff0000", Some(10.0), true, t(2.0))
            .expect("replace overwrites");
        {
            let record = registry
                .participants
                .get(&(0, "a".to_string()))
                .expect("stored");
            assert_eq!(record.revision, 2, "revision continues across replace");
            assert_eq!(record.name, "renamed");
            assert!(
                record.cursor.is_none(),
                "a replace states complete new state"
            );
        }
        // Expired × no-replace: STILL already-exists — expiry never
        // frees the id, only user.leave does.
        let (code, _) = registry
            .join(0, "a", "a", "#00ff00", None, false, t(100.0))
            .expect_err("expired collision");
        assert_eq!(code, codes::ALREADY_EXISTS);
        // Expired × replace: overwrite revives, revision continues.
        registry
            .join(0, "a", "back", "#00ff00", Some(10.0), true, t(100.0))
            .expect("replace revives");
        {
            let record = registry
                .participants
                .get(&(0, "a".to_string()))
                .expect("stored");
            assert_eq!(record.revision, 3);
            assert!(record.fresh(t(100.0)));
        }
        // Renew revives an expired participant too (#21 honest leases)…
        assert!(
            !registry
                .participants
                .get(&(0, "a".to_string()))
                .expect("stored")
                .fresh(t(200.0))
        );
        registry.renew(0, "a", None, t(200.0)).expect("revives");
        assert!(
            registry
                .participants
                .get(&(0, "a".to_string()))
                .expect("stored")
                .fresh(t(200.0))
        );
        // …but never a missing one.
        let (code, _) = registry
            .renew(0, "ghost", None, t(0.0))
            .expect_err("missing");
        assert_eq!(code, codes::UNKNOWN_ID);
        // Leave actually frees the id; a re-join starts a new lineage.
        registry.leave(0, "a").expect("registry op ok");
        let (code, _) = registry.leave(0, "a").expect_err("gone");
        assert_eq!(code, codes::UNKNOWN_ID);
        registry
            .join(0, "a", "a", "#00ff00", None, false, t(300.0))
            .expect("freed id rejoins");
        assert_eq!(
            registry
                .participants
                .get(&(0, "a".to_string()))
                .expect("stored")
                .revision,
            1,
            "a freed id starts a fresh lineage"
        );
    }

    #[test]
    fn namespace_caps_fail_honestly_and_replace_at_cap_succeeds() {
        let mut registry = PresenceRegistry::default();
        for index in 0..MAX_PRESENCE_PARTICIPANTS_PER_NAMESPACE {
            join(&mut registry, 0, &format!("p{index}"), t(0.0));
        }
        let (code, _) = registry
            .join(0, "overflow", "o", "#00ff00", None, false, t(0.0))
            .expect_err("at the cap");
        assert_eq!(code, codes::NAMESPACE_CAP);
        // Replacing and renewing an existing id is never a new slot.
        registry
            .join(0, "p0", "p0", "#123456", None, true, t(1.0))
            .expect("replace at cap");
        registry.renew(0, "p0", None, t(1.0)).expect("renew at cap");
        // Expired rows still occupy their slot: advance past every TTL
        // and a new id still rejects — only user.leave frees slots.
        let (code, _) = registry
            .join(0, "overflow", "o", "#00ff00", None, false, t(10_000.0))
            .expect_err("expired rows occupy slots");
        assert_eq!(code, codes::NAMESPACE_CAP);
        registry.leave(0, "p1").expect("registry op ok");
        registry
            .join(0, "overflow", "o", "#00ff00", None, false, t(10_000.0))
            .expect("a freed slot admits");
        // Notes have their own independent 16-cap.
        for index in 0..MAX_PRESENCE_NOTES_PER_NAMESPACE {
            registry
                .set_note(0, &format!("n{index}"), "hi", 0, 0, None, false, t(0.0))
                .expect("registry op ok");
        }
        let (code, _) = registry
            .set_note(0, "overflow", "hi", 0, 0, None, false, t(0.0))
            .expect_err("note cap");
        assert_eq!(code, codes::NAMESPACE_CAP);
        registry
            .set_note(0, "n0", "hi", 0, 0, None, true, t(0.0))
            .expect("note replace at cap");
    }

    #[test]
    fn lease_freshness_edges_are_exact() {
        let mut registry = PresenceRegistry::default();
        registry
            .join(0, "a", "a", "#00ff00", Some(10.0), false, Duration::ZERO)
            .expect("registry op ok");
        let record = registry
            .participants
            .get(&(0, "a".to_string()))
            .expect("stored");
        // Exactly at the TTL: still fresh (now − updated ≤ ttl).
        assert!(record.fresh(Duration::from_secs(10)));
        // Just past it: stale.
        assert!(!record.fresh(Duration::from_millis(10_001)));
        // Expiry is visible, never a deletion.
        assert_eq!(registry.participant_count(0), 1);
    }

    #[test]
    fn namespaces_are_isolated_by_construction() {
        let mut registry = PresenceRegistry::default();
        // The same caller-local id in two namespaces is two identities.
        join(&mut registry, 0, "alice", t(0.0));
        join(&mut registry, 1, "alice", t(0.0));
        assert_eq!(registry.participant_count(0), 1);
        assert_eq!(registry.participant_count(1), 1);
        // A mutation under ns 0 can never touch ns 1's record.
        registry.set_cursor(0, "alice", 3, 4, t(1.0)).expect("own");
        assert!(
            registry
                .participants
                .get(&(1, "alice".to_string()))
                .expect("stored")
                .cursor
                .is_none(),
            "ns 0's cursor update never crosses namespaces"
        );
        // An id that exists only in the other namespace is unknown here.
        registry
            .set_note(1, "n1", "hi", 0, 0, None, false, t(0.0))
            .expect("registry op ok");
        let (code, _) = registry.remove_note(0, "n1").expect_err("foreign note");
        assert_eq!(code, codes::UNKNOWN_ID);
        registry.leave(0, "alice").expect("registry op ok");
        let (code, _) = registry.leave(0, "alice").expect_err("own gone");
        assert_eq!(code, codes::UNKNOWN_ID);
        assert_eq!(
            registry.participant_count(1),
            1,
            "ns 1's roster is untouched"
        );
    }

    #[test]
    fn revisions_and_mutation_seq_advance_per_successful_mutation() {
        let mut registry = PresenceRegistry::default();
        join(&mut registry, 0, "a", t(0.0));
        let revision =
            |registry: &PresenceRegistry| registry.participants[&(0, "a".to_string())].revision;
        assert_eq!(revision(&registry), 1);
        assert_eq!(registry.mutation_seq(), 1);
        registry.set_cursor(0, "a", 1, 2, t(1.0)).expect("cursor");
        assert_eq!(revision(&registry), 2);
        registry.renew(0, "a", Some(30.0), t(2.0)).expect("renew");
        assert_eq!(revision(&registry), 3);
        assert_eq!(registry.mutation_seq(), 3);
        // A failed mutation bumps nothing.
        registry
            .set_cursor(0, "ghost", 1, 2, t(3.0))
            .expect_err("unknown id");
        assert_eq!(registry.mutation_seq(), 3);
        // Removal is a change render systems must observe.
        registry.leave(0, "a").expect("registry op ok");
        assert_eq!(registry.mutation_seq(), 4);
    }

    #[test]
    fn reset_clears_every_roster() {
        let mut registry = PresenceRegistry::default();
        join(&mut registry, 0, "a", t(0.0));
        join(&mut registry, 1, "b", t(0.0));
        registry
            .set_note(0, "n1", "hi", 0, 0, None, false, t(0.0))
            .expect("registry op ok");
        let seq = registry.mutation_seq();
        registry.reset();
        assert!(registry.is_empty(), "reset clears every namespace's rows");
        assert!(
            registry.mutation_seq() > seq,
            "reset is an observable change"
        );
        // Reset on an already-empty registry is not a change.
        let seq = registry.mutation_seq();
        registry.reset();
        assert_eq!(registry.mutation_seq(), seq);
    }

    #[test]
    fn read_scope_shows_own_expired_rows_but_hides_foreign_ones() {
        let mut registry = PresenceRegistry::default();
        registry
            .join(
                0,
                "own-fresh",
                "own-fresh",
                "#00ff00",
                Some(100.0),
                false,
                t(0.0),
            )
            .expect("registry op ok");
        registry
            .join(
                0,
                "own-stale",
                "own-stale",
                "#00ff00",
                Some(1.0),
                false,
                t(0.0),
            )
            .expect("registry op ok");
        registry
            .join(
                1,
                "foreign-fresh",
                "ff",
                "#00ff00",
                Some(100.0),
                false,
                t(0.0),
            )
            .expect("registry op ok");
        registry
            .join(
                1,
                "foreign-stale",
                "fs",
                "#00ff00",
                Some(1.0),
                false,
                t(0.0),
            )
            .expect("registry op ok");
        registry
            .set_note(1, "note-stale", "hi", 0, 0, Some(1.0), false, t(0.0))
            .expect("registry op ok");
        let now = t(50.0);
        let rows: Vec<Value> = presence_state_items(&registry, 0, now)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let ids: Vec<&str> = rows
            .iter()
            .map(|row| row["id"].as_str().expect("id"))
            .collect();
        // Own rows in full (expired visible, fresh: false); foreign rows
        // fresh-only — an expired foreign row's existence never leaks.
        assert_eq!(ids, vec!["own-fresh", "own-stale", "foreign-fresh"]);
        assert_eq!(rows[0]["fresh"], json!(true));
        assert_eq!(rows[1]["fresh"], json!(false));
        assert_eq!(rows[2]["ns"], json!(1));
        // The aggregate counts fresh rows only, and a namespace with
        // nothing fresh is absent entirely.
        let counts = registry.fresh_counts(now);
        assert_eq!(counts.get(&0), Some(&(1, 0)));
        assert_eq!(counts.get(&1), Some(&(1, 0)));
        let counts = registry.fresh_counts(t(500.0));
        assert!(counts.is_empty(), "all-expired namespaces vanish");
    }

    // ── Rendering: pure geometry over the registry ──

    #[test]
    fn note_and_label_underlays_pin_their_pixel_geometry() {
        let mut registry = PresenceRegistry::default();
        registry
            .set_note(0, "n1", "HI", 2, 3, None, false, t(0.0))
            .expect("registry op ok");
        registry
            .join(0, "alice", "AL", "#00ff00", None, false, t(0.0))
            .expect("registry op ok");
        registry
            .set_cursor(0, "alice", 5, 2, t(0.0))
            .expect("registry op ok");
        let underlays = presence_underlays(&registry, t(0.0), 80, 24, 10.0, 20.0);
        assert_eq!(underlays.len(), 2, "one note panel, one name label");

        // The note panel anchors at its cell, one cell tall, sized by
        // the stroke-font advance: fill, closed border, then the text.
        let (rect, ops) = &underlays[0];
        let glyph_height = 20.0 * NOTE_GLYPH_HEIGHT_FRACTION;
        let expected_width =
            glyph_height * NOTE_PAD_FRACTION * 2.0 + glyph_height * GLYPH_ASPECT * 2.0;
        assert!((rect.x - 20.0).abs() < 1e-4);
        assert!((rect.y - 60.0).abs() < 1e-4);
        assert!((rect.height - 20.0).abs() < 1e-4);
        assert!((rect.width - expected_width).abs() < 1e-3);
        assert_eq!(ops.len(), 3);
        assert!(matches!(ops[0], VizDrawOp::Fill { .. }));
        assert!(
            matches!(&ops[1], VizDrawOp::Polyline { points, .. } if points.first() == points.last()),
            "the border strokes a closed loop"
        );
        assert!(matches!(&ops[2], VizDrawOp::Text { text, .. } if text == "HI"));

        // The label sits one cell right of the cursor cell (plus the
        // gap), on the cursor row: the scrim, then the name in the
        // participant's color (#78 — bare glyphs over occupied cells
        // were illegible).
        let (rect, ops) = &underlays[1];
        assert!((rect.x - (60.0 + 10.0 * LABEL_GAP_FRACTION)).abs() < 1e-3);
        assert!((rect.y - 40.0).abs() < 1e-4);
        assert_eq!(ops.len(), 2);
        assert!(
            matches!(&ops[0], VizDrawOp::Fill { min, max, color }
                if *min == (0.0, 0.0) && *max == (1.0, 1.0) && *color == LABEL_FILL),
            "the scrim backs the whole label rect"
        );
        match &ops[1] {
            VizDrawOp::Text { text, color, .. } => {
                assert_eq!(text, "AL");
                assert!(color[0].abs() < 1e-6 && (color[1] - 1.0).abs() < 1e-6);
            }
            other => panic!("expected a text op, got {other:?}"),
        }
    }

    #[test]
    fn unclamped_note_panels_never_truncate_their_text() {
        // Regression for the f32 floor boundary: the panel is sized as
        // pad*2 + advance*N, and recovering N from the max_width
        // fraction lands a rounding hair below N at ~14% of realistic
        // cell metrics (a one-glyph note rendered an empty panel at
        // cell heights 14-40px). Sweep heights and glyph counts and
        // assert the recovered count survives the round-trip whenever
        // the grid does not clamp the panel.
        let cols: u16 = 2000;
        let cell_width = 10.0;
        for half_px in 12..=128u16 {
            let cell_height = f32::from(half_px) * 0.5;
            let glyph_height = cell_height * NOTE_GLYPH_HEIGHT_FRACTION;
            let advance = glyph_height * crate::viz_draw::GLYPH_ASPECT;
            for glyphs in 1..=256usize {
                let record = NoteRecord {
                    text: "X".repeat(glyphs),
                    x: 0,
                    y: 0,
                    updated: t(0.0),
                    ttl: t(60.0),
                    revision: 1,
                };
                let (rect, ops) = note_underlay(&record, cols, 24, cell_width, cell_height);
                if rect.width >= f32::from(cols) * cell_width - 1e-3 {
                    continue; // grid-edge clamp: truncation is the contract
                }
                let VizDrawOp::Text { max_width, .. } = &ops[2] else {
                    panic!("expected the text op third");
                };
                let budget = max_width.expect("note text always carries a budget") * rect.width;
                assert_eq!(
                    crate::viz_draw::glyphs_that_fit(budget, advance),
                    glyphs,
                    "cell_height {cell_height} glyphs {glyphs}"
                );
            }
        }
    }

    #[test]
    fn out_of_grid_cells_clamp_to_the_edge_at_render() {
        assert_eq!(clamped_cell(1000, 1000, 80, 24), (79, 23));
        assert_eq!(clamped_cell(0, 0, 80, 24), (0, 0));
        // Degenerate grids never underflow.
        assert_eq!(clamped_cell(5, 5, 0, 0), (0, 0));

        let mut registry = PresenceRegistry::default();
        registry
            .set_note(0, "n1", "A-LONG-NOTE-BODY", 1000, 1000, None, false, t(0.0))
            .expect("registry op ok");
        registry
            .join(
                0,
                "alice",
                "SIXTEEN-GLYPH-NAME!",
                "#00ff00",
                None,
                false,
                t(0.0),
            )
            .expect("registry op ok");
        registry
            .set_cursor(0, "alice", 79, 0, t(0.0))
            .expect("registry op ok");
        let underlays = presence_underlays(&registry, t(0.0), 80, 24, 10.0, 20.0);
        // The note clamps to the bottom-right cell and its panel
        // truncates at the grid's right edge instead of overflowing.
        let (rect, _) = &underlays[0];
        assert!((rect.x - 790.0).abs() < 1e-3);
        assert!((rect.y - 460.0).abs() < 1e-3);
        assert!(
            (rect.width - 10.0).abs() < 1e-3,
            "the panel clamps to the remaining grid width"
        );
        // The label caps at 16 glyphs and shifts left so it stays on
        // the texture despite the right-edge cursor.
        let (rect, ops) = &underlays[1];
        let glyph_height = 20.0 * LABEL_GLYPH_HEIGHT_FRACTION;
        let expected_width = glyph_height * GLYPH_ASPECT * 16.0;
        assert!((rect.width - expected_width).abs() < 1e-3);
        assert!((rect.x - (800.0 - expected_width)).abs() < 1e-3);
        match &ops[1] {
            VizDrawOp::Text { text, .. } => assert_eq!(text, "SIXTEEN-GLYPH-NA"),
            other => panic!("expected the text op after the scrim, got {other:?}"),
        }
    }

    #[test]
    fn only_fresh_rows_render_and_notes_precede_labels_deterministically() {
        let mut registry = PresenceRegistry::default();
        registry
            .set_note(1, "b", "B", 0, 0, Some(100.0), false, t(0.0))
            .expect("registry op ok");
        registry
            .set_note(0, "z", "Z", 0, 0, Some(100.0), false, t(0.0))
            .expect("registry op ok");
        registry
            .set_note(0, "gone", "GONE", 0, 0, Some(1.0), false, t(0.0))
            .expect("registry op ok");
        // Fresh but cursor-less: queryable state, no pixels.
        registry
            .join(0, "idle", "IDLE", "#0000ff", Some(100.0), false, t(0.0))
            .expect("registry op ok");
        // Expired with a cursor: no pixels either.
        registry
            .join(0, "stale", "STALE", "#0000ff", Some(1.0), false, t(0.0))
            .expect("registry op ok");
        registry
            .set_cursor(0, "stale", 1, 1, t(0.0))
            .expect("registry op ok");
        registry
            .join(1, "a", "A", "#ff0000", Some(100.0), false, t(0.0))
            .expect("registry op ok");
        registry
            .set_cursor(1, "a", 2, 2, t(0.0))
            .expect("registry op ok");
        registry
            .join(0, "m", "M", "#00ff00", Some(100.0), false, t(0.0))
            .expect("registry op ok");
        registry
            .set_cursor(0, "m", 3, 3, t(0.0))
            .expect("registry op ok");
        let texts: Vec<String> = presence_underlays(&registry, t(50.0), 80, 24, 10.0, 20.0)
            .iter()
            .filter_map(|(_, ops)| {
                ops.iter().find_map(|op| match op {
                    VizDrawOp::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
            })
            .collect();
        // Note panels first ((ns, id)-sorted), then labels ((ns,
        // id)-sorted); the expired note, the expired cursor, and the
        // cursor-less participant render nothing.
        assert_eq!(texts, vec!["Z", "B", "M", "A"]);
    }

    #[test]
    fn strict_hex_colors_parse_to_channels() {
        assert_eq!(hex_rgb("#00ff00"), Some([0, 255, 0]));
        assert_eq!(hex_rgb("#AbCdEf"), Some([0xab, 0xcd, 0xef]));
        assert_eq!(hex_rgb("00ff00"), None);
        assert_eq!(hex_rgb("#00ff0"), None);
        assert_eq!(hex_rgb("#00gg00"), None);
    }

    #[test]
    fn marker_poses_track_the_cell_center_in_every_projection() {
        let viewport = TerminalViewport {
            size: Vec2::new(800.0, 480.0),
            center: Vec2::ZERO,
        };
        // Flat view: cell (0, 0) centers half a cell in from the
        // top-left corner, at the cursor depth.
        let (translation, rotation) = marker_pose(
            (0, 0),
            80,
            24,
            &viewport,
            TerminalPresentationMode::Flat2d,
            None,
            0.0,
            0.0,
            0.0,
        )
        .expect("flat pose");
        assert!((translation.x - -395.0).abs() < 1e-3);
        assert!((translation.y - 230.0).abs() < 1e-3);
        assert!((translation.z - CURSOR_DEPTH).abs() < 1e-6);
        assert_eq!(rotation, Quat::IDENTITY);
        // Out-of-grid cursors clamp to the far edge cell.
        let (translation, _) = marker_pose(
            (1000, 1000),
            80,
            24,
            &viewport,
            TerminalPresentationMode::Flat2d,
            None,
            0.0,
            0.0,
            0.0,
        )
        .expect("flat pose");
        assert!((translation.x - 395.0).abs() < 1e-3);
        assert!((translation.y - -230.0).abs() < 1e-3);
        // The 3D modes need the plane; without one there is no pose
        // (the sync hides the marker instead).
        assert!(
            marker_pose(
                (0, 0),
                80,
                24,
                &viewport,
                TerminalPresentationMode::Plane3d,
                None,
                0.0,
                0.0,
                0.0,
            )
            .is_none()
        );
        // Through an identity plane at zero warp the marker floats
        // exactly the plane offset above the surface point.
        let plane = Transform::IDENTITY;
        let (translation, rotation) = marker_pose(
            (0, 0),
            80,
            24,
            &viewport,
            TerminalPresentationMode::Plane3d,
            Some(&plane),
            0.0,
            0.0,
            0.0,
        )
        .expect("3d pose");
        assert!((translation.x - -0.49375).abs() < 1e-5);
        assert!((translation.y - 0.479_166_7).abs() < 1e-4);
        assert!((translation.z - MARKER_PLANE_OFFSET).abs() < 1e-5);
        assert_eq!(rotation, Quat::IDENTITY);
    }

    // ── App tier: the applier over the message stream ──

    fn app_test() -> App {
        let mut app = App::new();
        app.world_mut().spawn(TerminalRedrawState::default());
        app.init_resource::<PresenceRegistry>();
        app.init_resource::<AiDiagnostics>();
        app.init_resource::<Time>();
        app.add_message::<AiCommand>();
        app.add_message::<AckOutcome>();
        app.add_systems(Update, apply_presence_commands);
        app
    }

    /// Takes the seat's redraw flag, the component-era `resource_mut().take()`.
    fn take_redraw(app: &mut App) -> bool {
        let world = app.world_mut();
        let mut query = world.query::<&mut TerminalRedrawState>();
        query
            .single_mut(world)
            .expect("exactly one terminal seat")
            .take()
    }

    fn send(app: &mut App, ack: Option<&str>, origin: CommandOrigin, command: RattyAiCommand) {
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .write(AiCommand {
                source: IngressSource::test_boot(),
                ack_token: ack.map(str::to_string),
                origin,
                command,
            });
        app.update();
    }

    fn drain_acks(app: &mut App) -> Vec<AckOutcome> {
        app.world_mut()
            .resource_mut::<Messages<AckOutcome>>()
            .drain()
            .collect()
    }

    fn wire_join(id: &str) -> RattyAiCommand {
        RattyAiCommand::UserJoin {
            id: id.to_string(),
            name: id.to_string(),
            color: "#00ff00".to_string(),
            ttl: None,
            replace: false,
        }
    }

    #[test]
    fn non_wire_origins_are_rejected_without_mutation() {
        let mut app = app_test();
        // A replayed join would forge liveness: every non-wire origin is
        // refused with not-permitted and mutates nothing.
        for origin in [
            CommandOrigin::Macro,
            CommandOrigin::Bookmark,
            CommandOrigin::Rule,
        ] {
            send(&mut app, Some("t"), origin, wire_join("alice"));
            let acks = drain_acks(&mut app);
            assert_eq!(acks.len(), 1);
            assert!(!acks[0].ok);
            assert_eq!(acks[0].code, Some(codes::NOT_PERMITTED));
        }
        send(
            &mut app,
            None,
            CommandOrigin::Macro,
            RattyAiCommand::Note {
                id: "n1".to_string(),
                text: "hi".to_string(),
                x: 1,
                y: 2,
                ttl: None,
                replace: false,
            },
        );
        assert!(
            app.world().resource::<PresenceRegistry>().is_empty(),
            "non-wire presence never reaches the registry"
        );
        // A replayed leave would evict a real participant: same guard.
        send(&mut app, None, CommandOrigin::Wire, wire_join("alice"));
        send(
            &mut app,
            None,
            CommandOrigin::Rule,
            RattyAiCommand::UserLeave {
                id: "alice".to_string(),
            },
        );
        assert_eq!(
            app.world()
                .resource::<PresenceRegistry>()
                .participant_count(0),
            1,
            "a rule-origin leave evicts nobody"
        );
    }

    #[test]
    fn acks_report_commit_and_rejection_codes_with_tok() {
        let mut app = app_test();
        send(
            &mut app,
            Some("j1"),
            CommandOrigin::Wire,
            wire_join("alice"),
        );
        let acks = drain_acks(&mut app);
        assert_eq!(acks.len(), 1);
        assert!(acks[0].ok, "the join committed");
        assert_eq!(acks[0].token, "j1");
        // Collision surfaces its code over the ack path.
        send(
            &mut app,
            Some("j2"),
            CommandOrigin::Wire,
            wire_join("alice"),
        );
        let acks = drain_acks(&mut app);
        assert!(!acks[0].ok);
        assert_eq!(acks[0].code, Some(codes::ALREADY_EXISTS));
        // Unknown ids likewise.
        send(
            &mut app,
            Some("c1"),
            CommandOrigin::Wire,
            RattyAiCommand::UserCursor {
                id: "ghost".to_string(),
                x: 1,
                y: 2,
            },
        );
        let acks = drain_acks(&mut app);
        assert!(!acks[0].ok);
        assert_eq!(acks[0].code, Some(codes::UNKNOWN_ID));
        // Token-less commands stay fire-and-forget in both directions.
        send(
            &mut app,
            None,
            CommandOrigin::Wire,
            RattyAiCommand::UserCursor {
                id: "alice".to_string(),
                x: 3,
                y: 4,
            },
        );
        assert!(drain_acks(&mut app).is_empty());
        // The reset tap clears silently: no ack from this applier.
        send(
            &mut app,
            Some("r1"),
            CommandOrigin::Wire,
            RattyAiCommand::Reset,
        );
        assert!(
            drain_acks(&mut app).is_empty(),
            "reset's single ack belongs to apply_ai_commands"
        );
        assert!(app.world().resource::<PresenceRegistry>().is_empty());
    }

    #[test]
    fn expiry_flips_redraw_an_otherwise_idle_terminal() {
        let mut app = App::new();
        app.world_mut().spawn(TerminalRedrawState::default());
        app.init_resource::<PresenceRegistry>();
        app.init_resource::<Time>();
        app.add_systems(Update, request_presence_expiry_redraw);
        app.world_mut()
            .resource_mut::<PresenceRegistry>()
            .set_note(0, "n1", "REVIEW", 2, 3, Some(1.0), false, Duration::ZERO)
            .expect("registry op ok");
        // Clear the boot-time pending redraw so requests are observable.
        let _ = take_redraw(&mut app);
        app.update();
        assert!(take_redraw(&mut app), "a fresh note is a texture change");
        app.update();
        assert!(
            !take_redraw(&mut app),
            "an unchanged fresh roster requests nothing"
        );
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(Duration::from_secs(2));
        app.update();
        assert!(
            take_redraw(&mut app),
            "the fresh→expired flip redraws the idle terminal"
        );
        app.update();
        assert!(
            !take_redraw(&mut app),
            "expiry redraws once, not every frame"
        );
    }

    /// Runs `f` with a thread-local WARN-capturing subscriber installed and
    /// returns the captured messages. Sound only because the system under
    /// test runs inline on this thread via `run_system_once` — an
    /// `app.update()` could run systems on task-pool threads this
    /// subscriber cannot see.
    fn capture_warns(f: impl FnOnce()) -> Vec<String> {
        use bevy::log::tracing::field::{Field, Visit};
        use bevy::log::tracing::{Event, Level, Metadata, Subscriber, span};
        use std::sync::{Arc, Mutex};

        struct WarnCollector(Arc<Mutex<Vec<String>>>);

        impl Subscriber for WarnCollector {
            fn enabled(&self, metadata: &Metadata<'_>) -> bool {
                *metadata.level() == Level::WARN
            }
            fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
                span::Id::from_u64(1)
            }
            fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}
            fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}
            fn event(&self, event: &Event<'_>) {
                struct MessageVisitor<'a>(&'a mut String);
                impl Visit for MessageVisitor<'_> {
                    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
                        if field.name() == "message" {
                            *self.0 = format!("{value:?}");
                        }
                    }
                }
                let mut message = String::new();
                event.record(&mut MessageVisitor(&mut message));
                if let Ok(mut messages) = self.0.lock() {
                    messages.push(message);
                }
            }
            fn enter(&self, _span: &span::Id) {}
            fn exit(&self, _span: &span::Id) {}
        }

        let messages = Arc::new(Mutex::new(Vec::new()));
        let collector = WarnCollector(Arc::clone(&messages));
        bevy::log::tracing::subscriber::with_default(collector, f);
        messages
            .lock()
            .expect("the warn collector lock should not be poisoned")
            .clone()
    }

    #[test]
    fn marker_sync_warns_once_when_the_plane_anchor_is_not_unique() {
        use crate::config::AppConfig;
        use bevy::ecs::system::RunSystemOnce;

        // Only this test may assert this site's warn: the warn_once latch
        // is process-global, so a second test driving this system into the
        // wrong-plane-count state would race it.
        let mut world = World::new();
        world.init_resource::<Time>();
        // The plane resolution runs before the roster loop, so an empty
        // registry reaches it (run_system_once also bypasses the run_if).
        world.init_resource::<PresenceRegistry>();
        world.spawn((
            TerminalSurface::new(&AppConfig::default()).expect("surface construction is CPU-only"),
            TerminalViewport {
                size: Vec2::new(800.0, 480.0),
                center: Vec2::ZERO,
            },
            TerminalPlaneWarp::default(),
        ));
        world.insert_resource(TerminalPresentation {
            mode: TerminalPresentationMode::Plane3d,
        });
        world.init_resource::<MobiusTransition>();
        world.init_resource::<Assets<Mesh>>();
        world.init_resource::<Assets<StandardMaterial>>();
        world.spawn((TerminalPlane, Transform::default()));
        world.spawn((TerminalPlane, Transform::default()));

        let warns = capture_warns(|| {
            world
                .run_system_once(sync_presence_cursor_markers)
                .expect("the system should run");
            world
                .run_system_once(sync_presence_cursor_markers)
                .expect("the system should run");
        });
        assert_eq!(
            warns
                .iter()
                .filter(|warn| warn.contains("sync_presence_cursor_markers"))
                .count(),
            1,
            "two wrong-plane frames must produce exactly one latched warn: {warns:?}"
        );
    }
}
