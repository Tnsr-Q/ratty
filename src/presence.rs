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
//! ## Honest limitations
//!
//! - Leases advance on `Res<Time>`, so on a hidden wasm tab expiry
//!   stalls with everything else (the sensor/avatar stance) — durations
//!   stretch in wall time, nothing ever fires while unobserved.
//! - Cursors and notes pin to **live grid cells** (the viewport): they
//!   do **not** scroll with the text they were placed beside.
//! - Name labels and note text render through the vello stroke font:
//!   uppercase letters, digits, and limited punctuation; other glyphs
//!   render as hollow boxes. The full text stays queryable either way.
//! - A plain PTY is one effective principal: every local writer shares
//!   namespace 0. Distinct participants under one namespace are distinct
//!   *ids*, honestly labeled, not authenticated identities.

use std::collections::HashMap;
use std::time::Duration;

use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::prelude::*;
use serde_json::{Value, json};

use crate::ai::{AiCommand, CommandOrigin};
use crate::osc::RattyAiCommand;
use crate::query::codes;
use crate::query_channel::{AckOutcome, AiDiagnostics, ack_commit, reject};
use crate::terminal::TerminalRedrawState;

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

/// Registers the presence registry and its applier.
///
/// Ordering: `apply_presence_commands` runs after `pump_pty_output` (it
/// owns the `user.*`/`note` acks); the render systems land in the next
/// slice ordered after the applier. `answer_queries` is ordered after
/// the applier in [`crate::ai::RattyAiPlugin`] so a same-chunk "join
/// then read" observes the join.
pub struct PresencePlugin;

impl Plugin for PresencePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PresenceRegistry>().add_systems(
            Update,
            apply_presence_commands.after(crate::systems::pump_pty_output),
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
    mut redraw: ResMut<TerminalRedrawState>,
    mut acks: MessageWriter<AckOutcome>,
    mut diagnostics: ResMut<AiDiagnostics>,
) {
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

    // ── App tier: the applier over the message stream ──

    fn app_test() -> App {
        let mut app = App::new();
        app.init_resource::<PresenceRegistry>();
        app.init_resource::<AiDiagnostics>();
        app.init_resource::<TerminalRedrawState>();
        app.init_resource::<Time>();
        app.add_message::<AiCommand>();
        app.add_message::<AckOutcome>();
        app.add_systems(Update, apply_presence_commands);
        app
    }

    fn send(app: &mut App, ack: Option<&str>, origin: CommandOrigin, command: RattyAiCommand) {
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .write(AiCommand {
                source: IngressSource::Local,
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
}
