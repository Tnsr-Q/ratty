//! Inline object state and APC handling.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use vt100::Callbacks;

use crate::kitty::{KittyOperation, KittyParserState, refresh_kitty_placeholder_anchors};
use crate::model::{
    ObjectLoadOptions, load_embedded_object_source, load_object_source_from_bytes_with_options,
};
use crate::rgp::{
    RgpOperation, RgpPlacementStyle, RgpPlacementUpdate, RgpRegisterSource, RgpStageUpdate,
    consume_sequence as consume_rgp_sequence, support_reply,
};
const APC_START: &[u8] = b"\x1b_";
const ST: &[u8] = b"\x1b\\";
const C1_ST: u8 = 0x9c;

/// Integrated built-in animation state for an RGP object root entity.
///
/// The angles integrate per-frame so per-object animation rates can change
/// mid-flight without snapping. Objects using only the global config rates
/// keep the v1 absolute-time expressions, with this state refreshed in
/// lockstep so a later switch to per-object rates stays continuous. Tilt is
/// derived as `0.7 * spin`, preserving the v1 coupling.
#[derive(Component, Default, Clone, Copy)]
pub struct RgpAnimationState {
    /// Accumulated spin angle in radians.
    pub spin_angle: f32,
    /// Accumulated bob phase in radians.
    pub bob_phase: f32,
}

/// Marker for 2D inline object sprites.
#[derive(Component)]
pub struct TerminalInlineObjectSprite;

/// Marker for 3D inline object planes.
#[derive(Component)]
pub struct TerminalInlineObjectPlane;

/// Layout data used to animate Kitty image planes on the warped terminal surface.
#[derive(Component, Clone, Copy)]
pub(crate) struct InlineKittyPlaneLayout {
    /// Normalized horizontal center within the terminal plane.
    pub local_x: f32,
    /// Normalized vertical center within the terminal plane.
    pub local_y: f32,
    /// Normalized width within the terminal plane.
    pub local_width: f32,
    /// Normalized height within the terminal plane.
    pub local_height: f32,
    /// Horizontal mesh subdivision count.
    pub x_segments: u32,
    /// Vertical mesh subdivision count.
    pub y_segments: u32,
}

/// Cached GPU assets for a Kitty image plane attached to the terminal surface.
pub(crate) struct KittyPlaneCache {
    /// Cached horizontal mesh subdivision count.
    pub x_segments: u32,
    /// Cached vertical mesh subdivision count.
    pub y_segments: u32,
    /// Cached plane mesh handle.
    pub mesh: Handle<Mesh>,
    /// Cached plane material handle.
    pub material: Handle<StandardMaterial>,
}

/// Marker for RGP-backed inline objects.
#[derive(Component)]
pub struct TerminalRgpObject {
    /// Registered object identifier.
    pub object_id: u32,
}

/// Inline object registry and anchor state.
#[derive(Resource, Default)]
pub struct TerminalInlineObjects {
    pending_bytes: Vec<u8>,
    pending_rgp_payloads: HashMap<u32, PendingRgpPayload>,
    pending_stage: Vec<RgpStageUpdate>,
    kitty: KittyParserState,
    dirty: bool,
    rebuild_objects: HashSet<u32>,
    restyle_objects: HashSet<u32>,
    last_viewport_size: Vec2,
    last_cols: u16,
    last_rows: u16,
    pub(crate) objects: HashMap<u32, InlineObject>,
    pub(crate) anchors: HashMap<u32, InlineAnchor>,
    revisions: HashMap<u32, u64>,
    mutation_seq: u64,
    osc_guard: OscGuard,
}

/// Upper bound on the payload of a single OSC sequence that reaches the
/// vt100 parser (see [`OscGuard`]).
///
/// This is an OSC-protocol-wide memory bound, deliberately well above the
/// 8 KiB query-acceptance bound ([`crate::query::MAX_QUERY_SEQUENCE_BYTES`])
/// so a legitimate max-size OSC 778 query reaches the query parser intact
/// and is answered `too-large` there rather than being silently truncated.
/// Ratty handles no OSC that legitimately exceeds this (titles, 778
/// queries, and every other OSC code — all far smaller), so truncation
/// only ever affects pathological or hostile input.
const MAX_OSC_SEQUENCE_BYTES: usize = 64 * 1024;

// The watchdog cap must never sit below the query-acceptance bound, or a
// valid-but-large 778 query would be truncated before it could be parsed.
const _: () = assert!(MAX_OSC_SEQUENCE_BYTES >= crate::query::MAX_QUERY_SEQUENCE_BYTES);

/// Streaming guard that bounds how many bytes of a single OSC sequence
/// reach the vt100 parser.
///
/// vt100 0.16 pulls vte 0.15 with its default `std` feature, under which
/// vte accumulates OSC payload bytes in an unbounded `Vec` until the
/// sequence terminates — the `MAX_OSC_RAW` cap only exists in `no_std`
/// builds. A never-terminated or gigabyte-long OSC in untrusted terminal
/// output (e.g. `cat` of a hostile file emitting `ESC ] 778 ; <gigabytes>`
/// with no ST/BEL) would grow that buffer without bound, and ratty's own
/// size checks in [`crate::query`] only run at the OSC terminator, too
/// late to matter.
///
/// This guard sits on the byte stream just before it reaches the parser
/// and mirrors vte's OSC entry and exit exactly (verified against vte
/// 0.15's `advance_esc`/`advance_osc_string`): OSC is entered only by the
/// 7-bit `ESC ]` introducer — vte is always UTF-8, so the C1 introducer
/// `0x9d` is executed as a control, never an OSC start — and ends on BEL,
/// CAN, SUB, or ESC. Once a single OSC's payload exceeds
/// [`MAX_OSC_SEQUENCE_BYTES`], the guard stops forwarding that payload
/// (still forwarding the eventual terminator so vte closes the sequence
/// in sync), so vte can never buffer more than the cap. State persists
/// across chunks because an OSC may span many PTY reads.
#[derive(Default)]
struct OscGuard {
    state: OscGuardState,
    /// Payload bytes counted since the current OSC's introducer; frozen at
    /// the cap once the guard begins dropping.
    osc_len: usize,
}

#[derive(Default, PartialEq, Eq, Clone, Copy, Debug)]
enum OscGuardState {
    /// Outside any escape sequence.
    #[default]
    Ground,
    /// The previous byte was a lone `ESC` (vte's `Escape` state); the next
    /// byte decides whether an OSC begins.
    Escape,
    /// Inside an OSC string, forwarding its payload.
    Osc,
    /// Inside an oversized OSC, suppressing payload until it terminates.
    OscDropping,
}

impl OscGuard {
    const ESC: u8 = 0x1b;
    const BEL: u8 = 0x07;
    const CAN: u8 = 0x18;
    const SUB: u8 = 0x1a;
    const OSC_INTRODUCER: u8 = 0x5d; // `]`

    /// Forwards `bytes` to the parser, eliding the payload of any single
    /// OSC sequence past the cap.
    fn forward<CB: Callbacks>(&mut self, parser: &mut vt100::Parser<CB>, bytes: &[u8]) {
        self.for_each_run(bytes, |run| parser.process(run));
    }

    /// Walks `bytes`, invoking `emit` on each contiguous run that should
    /// reach the parser. Factored out so the state machine can be tested
    /// without a live parser.
    fn for_each_run(&mut self, bytes: &[u8], mut emit: impl FnMut(&[u8])) {
        let mut run_start = 0;
        for (i, &byte) in bytes.iter().enumerate() {
            if !self.step(byte) {
                if run_start < i {
                    emit(&bytes[run_start..i]);
                }
                run_start = i + 1;
            }
        }
        if run_start < bytes.len() {
            emit(&bytes[run_start..]);
        }
    }

    /// Advances the state machine by one byte, returning whether the byte
    /// should be forwarded to the parser. Suppression happens only inside
    /// an oversized OSC's payload, so sequences under the cap — and every
    /// non-OSC byte — pass through untouched.
    fn step(&mut self, byte: u8) -> bool {
        match self.state {
            OscGuardState::Ground => {
                if byte == Self::ESC {
                    self.state = OscGuardState::Escape;
                }
                true
            }
            OscGuardState::Escape => {
                self.state = match byte {
                    Self::OSC_INTRODUCER => {
                        self.osc_len = 0;
                        OscGuardState::Osc
                    }
                    // The bytes vte's `advance_esc` executes in place or
                    // ignores *without leaving its escape state* (its
                    // execute, `ESC`, and catch-all arms): C0 controls
                    // except CAN/SUB, DEL, and 0x80..=0xFF. Mirroring this
                    // is load-bearing — vte's next `]` still opens an OSC,
                    // so collapsing these to Ground would let a one-byte
                    // prefix (`ESC <c> ] <gigabytes>`) slip an unbounded
                    // OSC straight past the guard.
                    0x00..=0x17 | 0x19 | 0x1b..=0x1f | 0x7f..=0xff => OscGuardState::Escape,
                    // Everything else advances vte out of Escape (CSI, DCS,
                    // escape intermediates, single-byte dispatches, and
                    // CAN/SUB) — none of which opens an OSC on a later `]`
                    // without a fresh ESC.
                    _ => OscGuardState::Ground,
                };
                true
            }
            OscGuardState::Osc => match byte {
                Self::BEL | Self::CAN | Self::SUB => {
                    self.state = OscGuardState::Ground;
                    true
                }
                Self::ESC => {
                    self.state = OscGuardState::Escape;
                    true
                }
                _ => {
                    self.osc_len += 1;
                    if self.osc_len > MAX_OSC_SEQUENCE_BYTES {
                        self.state = OscGuardState::OscDropping;
                        false
                    } else {
                        true
                    }
                }
            },
            OscGuardState::OscDropping => match byte {
                // The terminator is always forwarded so vte closes the
                // (bounded) OSC and does not swallow following output.
                Self::BEL | Self::CAN | Self::SUB => {
                    self.state = OscGuardState::Ground;
                    true
                }
                Self::ESC => {
                    self.state = OscGuardState::Escape;
                    true
                }
                _ => false,
            },
        }
    }
}

impl TerminalInlineObjects {
    /// Consumes PTY output and extracts inline object control sequences.
    pub fn consume_pty_output<CB: Callbacks>(
        &mut self,
        chunk: &[u8],
        parser: &mut vt100::Parser<CB>,
    ) -> Vec<Vec<u8>> {
        self.pending_bytes.extend_from_slice(chunk);
        let mut replies = Vec::new();
        // The OSC watchdog state persists across chunks; take it so the
        // `pending_bytes` slices below can still be borrowed. Every path
        // out of this function restores it.
        let mut osc_guard = std::mem::take(&mut self.osc_guard);

        let mut cursor = 0;
        loop {
            let Some(start_offset) = self.pending_bytes[cursor..]
                .windows(APC_START.len())
                .position(|window| window == APC_START)
            else {
                let pending_len = self.pending_bytes.len();
                let keep_from = pending_apc_prefix_start(&self.pending_bytes, cursor);
                if cursor < keep_from {
                    osc_guard.forward(
                        parser,
                        &normalize_hvp_sequences(&self.pending_bytes[cursor..keep_from]),
                    );
                }
                if keep_from < pending_len {
                    self.pending_bytes.drain(..keep_from);
                } else {
                    self.pending_bytes.clear();
                }
                self.osc_guard = osc_guard;
                return replies;
            };
            let start = cursor + start_offset;
            if cursor < start {
                osc_guard.forward(
                    parser,
                    &normalize_hvp_sequences(&self.pending_bytes[cursor..start]),
                );
            }

            let payload_start = start + APC_START.len();
            let Some(end) = apc_end(&self.pending_bytes, payload_start) else {
                self.pending_bytes.drain(..start);
                self.osc_guard = osc_guard;
                return replies;
            };
            let sequence = self.pending_bytes[start..end].to_vec();
            let (handled, reply) =
                self.handle_apc_sequence(&sequence, parser.screen().cursor_position());
            if let Some(reply) = reply {
                replies.push(reply);
            }
            if !handled {
                osc_guard.forward(parser, &sequence);
            }
            cursor = end;
        }
    }

    /// Test-only view of the OSC watchdog's engagement after a
    /// [`Self::consume_pty_output`] call.
    #[cfg(test)]
    pub(crate) fn osc_guard_state(&self) -> (bool, usize) {
        (
            self.osc_guard.state == OscGuardState::OscDropping,
            self.osc_guard.osc_len,
        )
    }

    /// Returns whether inline objects need synchronization.
    pub fn needs_sync(&self, viewport_size: Vec2, cols: u16, rows: u16) -> bool {
        self.dirty
            || self.last_viewport_size != viewport_size
            || self.last_cols != cols
            || self.last_rows != rows
    }

    /// Marks synchronization as complete.
    ///
    /// A full rebuild spawns every object from its current style, so any
    /// queued per-object rebuilds and restyles are subsumed and cleared.
    pub fn finish_sync(&mut self, viewport_size: Vec2, cols: u16, rows: u16) {
        self.dirty = false;
        self.rebuild_objects.clear();
        self.restyle_objects.clear();
        self.last_viewport_size = viewport_size;
        self.last_cols = cols;
        self.last_rows = rows;
    }

    /// Drains object ids whose entities must be despawned and respawned
    /// (`depth` changes re-extrude meshes; glTF styles live in the scene).
    pub fn take_rebuild_objects(&mut self) -> HashSet<u32> {
        std::mem::take(&mut self.rebuild_objects)
    }

    /// Drains object ids whose materials can be rewritten in place
    /// (`color`/`brightness` changes on mesh-backed objects).
    pub fn take_restyle_objects(&mut self) -> HashSet<u32> {
        std::mem::take(&mut self.restyle_objects)
    }

    /// Returns whether any in-place material restyles are queued.
    pub fn has_restyle_objects(&self) -> bool {
        !self.restyle_objects.is_empty()
    }

    /// Mesh-backed RGP objects derive their materials entirely from
    /// [`InlineStyle`], so those materials can be rewritten in place. glTF
    /// scenes own their materials and Kitty images have none.
    fn supports_restyle(&self, object_id: u32) -> bool {
        matches!(
            self.objects.get(&object_id),
            Some(InlineObject::RgpObject(
                RgpInlineObject::Obj { .. } | RgpInlineObject::Stl { .. }
            ))
        )
    }

    /// Applies upward scroll to anchored objects.
    pub fn apply_scroll(&mut self, rows_scrolled: u16) {
        if rows_scrolled == 0 || self.anchors.is_empty() {
            return;
        }

        self.anchors.retain(|object_id, anchor| {
            if self
                .objects
                .get(object_id)
                .is_some_and(|object| !object.scrolls_with_text())
            {
                return true;
            }
            let new_row = anchor.row as i32 - rows_scrolled as i32;
            if new_row + anchor.rows as i32 <= 0 {
                return false;
            }
            anchor.row = new_row.max(0) as u16;
            true
        });
        self.dirty = true;
    }

    /// Returns whether any anchors need scroll tracking.
    pub fn has_scroll_tracked_anchors(&self) -> bool {
        self.anchors.keys().any(|object_id| {
            self.objects
                .get(object_id)
                .is_some_and(InlineObject::scrolls_with_text)
        })
    }

    /// Refreshes placeholder-derived Kitty anchors.
    pub fn refresh_placeholder_anchors(&mut self, screen: &vt100::Screen) {
        if refresh_kitty_placeholder_anchors(&self.objects, &mut self.anchors, screen) {
            self.dirty = true;
        }
    }

    fn set_anchor(&mut self, object_id: u32, anchor: InlineAnchor) {
        self.anchors.insert(object_id, anchor);
        self.dirty = true;
        self.bump_revision(object_id);
    }

    /// Stamps a fresh revision on an object record. Revisions are drawn
    /// from one monotonic per-session counter, so they also order mutations
    /// across objects. Only explicit record mutations (spawn, restyle,
    /// re-anchor, replace) bump revisions; derived visibility changes
    /// (scrolling) do not.
    fn bump_revision(&mut self, object_id: u32) {
        self.mutation_seq += 1;
        self.revisions.insert(object_id, self.mutation_seq);
    }

    /// The object's current revision, or 0 when the id has no live record.
    pub(crate) fn revision(&self, object_id: u32) -> u64 {
        self.revisions.get(&object_id).copied().unwrap_or(0)
    }

    fn remove_object(&mut self, object_id: u32) {
        // The transmission/system surface never removes AI-owned objects;
        // the AI id partition is theirs alone (see `is_ai_object_id`).
        if is_ai_object_id(object_id) {
            return;
        }
        self.objects.remove(&object_id);
        self.anchors.remove(&object_id);
        self.pending_rgp_payloads.remove(&object_id);
        self.revisions.remove(&object_id);
        self.dirty = true;
    }

    /// Clears the transmission/system partition (RGP/Kitty "delete all"),
    /// leaving AI-owned objects untouched — those are removed only through
    /// the AI channel's own `object.clear`/`reset`, which emit removal
    /// events.
    fn clear_objects(&mut self) {
        self.objects.retain(|id, _| is_ai_object_id(*id));
        self.anchors.retain(|id, _| is_ai_object_id(*id));
        self.pending_rgp_payloads
            .retain(|id, _| is_ai_object_id(*id));
        self.revisions.retain(|id, _| is_ai_object_id(*id));
        self.dirty = true;
    }

    /// Returns whether stage updates parsed from `c` sequences are queued.
    pub fn has_pending_stage(&self) -> bool {
        !self.pending_stage.is_empty()
    }

    /// Drains queued stage updates in arrival order.
    pub fn take_stage_updates(&mut self) -> Vec<RgpStageUpdate> {
        std::mem::take(&mut self.pending_stage)
    }

    // ── AI-channel (OSC 777) mutations ──
    //
    // These ride the per-object rebuild path, never the scene-wide `dirty`
    // flag: an agent placing or removing its own object must not respawn a
    // transmission's scene.

    /// Returns whether an object payload is registered under `object_id`.
    pub(crate) fn contains_object(&self, object_id: u32) -> bool {
        self.objects.contains_key(&object_id)
    }

    /// Number of live objects whose id lies in the given AI namespace.
    pub(crate) fn ai_namespace_len(&self, namespace: u8) -> usize {
        self.objects
            .keys()
            .filter(|id| crate::osc::ai_object_namespace(**id) == Some(namespace))
            .count()
    }

    /// Inserts (or replaces) an AI-owned object anchored at the centered
    /// cell `(x, y)` with the default AI footprint, queuing a per-object
    /// spawn.
    pub(crate) fn ai_insert_object(
        &mut self,
        object_id: u32,
        object: InlineObject,
        x: u16,
        y: u16,
        style: InlineStyle,
    ) {
        let anchor = InlineAnchor {
            row: ai_anchor_component(y, AI_OBJECT_ROWS),
            col: ai_anchor_component(x, AI_OBJECT_COLUMNS),
            columns: AI_OBJECT_COLUMNS,
            rows: AI_OBJECT_ROWS,
            style,
        };
        self.objects.insert(object_id, object);
        self.anchors.insert(object_id, anchor);
        self.restyle_objects.remove(&object_id);
        self.rebuild_objects.insert(object_id);
        self.bump_revision(object_id);
    }

    /// Applies an `object.update`: x/y re-anchor the object (a discrete
    /// relocation — scrolling and hit-testing follow the new cell), while
    /// scale/spin mutate the live style fields and brightness routes through
    /// the same restyle/rebuild triage as RGP updates.
    pub(crate) fn ai_update_object(
        &mut self,
        object_id: u32,
        x: Option<u16>,
        y: Option<u16>,
        scale: Option<f32>,
        spin: Option<f32>,
        brightness: Option<f32>,
    ) -> AiUpdateOutcome {
        if !self.objects.contains_key(&object_id) {
            return AiUpdateOutcome::UnknownId;
        }
        let reanchored = x.is_some() || y.is_some();
        let Some(anchor) = self.anchors.get_mut(&object_id) else {
            // The object scrolled off the top and lost its anchor. A full
            // re-anchor (both coordinates) relocates it into the scene; its
            // style resets because the scroll discarded the old anchor.
            let (Some(col), Some(row)) = (x, y) else {
                return AiUpdateOutcome::NoAnchor;
            };
            let mut style = InlineStyle::default();
            if let Some(scale) = scale {
                style.scale = scale;
            }
            if let Some(spin) = spin {
                style.spin = Some(spin);
                style.animate = spin != 0.0;
            }
            if let Some(brightness) = brightness {
                style.brightness = brightness;
            }
            self.anchors.insert(
                object_id,
                InlineAnchor {
                    row: ai_anchor_component(row, AI_OBJECT_ROWS),
                    col: ai_anchor_component(col, AI_OBJECT_COLUMNS),
                    columns: AI_OBJECT_COLUMNS,
                    rows: AI_OBJECT_ROWS,
                    style,
                },
            );
            self.restyle_objects.remove(&object_id);
            self.rebuild_objects.insert(object_id);
            self.bump_revision(object_id);
            return AiUpdateOutcome::Applied;
        };
        if let Some(col) = x {
            anchor.col = ai_anchor_component(col, anchor.columns);
        }
        if let Some(row) = y {
            anchor.row = ai_anchor_component(row, anchor.rows);
        }
        if let Some(scale) = scale {
            anchor.style.scale = scale;
        }
        if let Some(spin) = spin {
            anchor.style.spin = Some(spin);
            anchor.style.animate = spin != 0.0 || anchor.style.bob.is_some();
        }
        let restyled = brightness.is_some();
        if let Some(brightness) = brightness {
            anchor.style.brightness = brightness;
        }
        // A re-anchor is a discrete relocation, so respawn the entity: an
        // object that was off-screen (never spawned) becomes visible, and
        // one moved off-screen is despawned by the granular pass.
        // scale/spin remain live per-frame fields (zero-cost).
        if reanchored {
            self.restyle_objects.remove(&object_id);
            self.rebuild_objects.insert(object_id);
        } else if restyled {
            if self.supports_restyle(object_id) {
                self.restyle_objects.insert(object_id);
            } else {
                self.rebuild_objects.insert(object_id);
                self.restyle_objects.remove(&object_id);
            }
        }
        self.bump_revision(object_id);
        AiUpdateOutcome::Applied
    }

    /// Removes an AI-owned object, queuing a per-object despawn. Returns
    /// whether the object existed.
    pub(crate) fn ai_remove_object(&mut self, object_id: u32) -> bool {
        let existed = self.objects.remove(&object_id).is_some();
        self.anchors.remove(&object_id);
        self.pending_rgp_payloads.remove(&object_id);
        self.revisions.remove(&object_id);
        if existed {
            self.restyle_objects.remove(&object_id);
            // The id is no longer renderable, so the granular sync pass
            // despawns its entity without respawning anything.
            self.rebuild_objects.insert(object_id);
        }
        existed
    }

    /// Removes every live object in the given AI namespace, returning the
    /// removed ids. Idempotent: an empty namespace removes nothing.
    pub(crate) fn ai_clear_namespace(&mut self, namespace: u8) -> Vec<u32> {
        let ids = self
            .objects
            .keys()
            .copied()
            .filter(|id| crate::osc::ai_object_namespace(*id) == Some(namespace))
            .collect::<Vec<_>>();
        for id in &ids {
            self.ai_remove_object(*id);
        }
        ids
    }

    /// Removes every AI-range object across all namespaces (the `reset`
    /// command), returning the removed ids.
    pub(crate) fn ai_clear_all(&mut self) -> Vec<u32> {
        let ids = self
            .objects
            .keys()
            .copied()
            .filter(|id| crate::osc::ai_object_namespace(*id).is_some())
            .collect::<Vec<_>>();
        for id in &ids {
            self.ai_remove_object(*id);
        }
        ids
    }

    fn handle_apc_sequence(
        &mut self,
        sequence: &[u8],
        cursor_position: (u16, u16),
    ) -> (bool, Option<Vec<u8>>) {
        if let Some(reply) = self.handle_rgp_sequence(sequence) {
            return (true, reply);
        }

        let Some(operation) = self.kitty.consume_sequence(sequence, cursor_position) else {
            return (false, None);
        };

        match operation {
            KittyOperation::Pending | KittyOperation::Ignored => (true, None),
            // The AI id partition is off-limits to the Kitty surface: a
            // cat'd file cannot squat, mutate, or evict an AI object.
            KittyOperation::TransmitOnly { object_id, .. }
            | KittyOperation::TransmitAndPlace { object_id, .. }
            | KittyOperation::PlaceExisting { object_id, .. }
                if is_ai_object_id(object_id) =>
            {
                warn!("Kitty object id {object_id} is in the AI-owned range; ignoring");
                (true, None)
            }
            KittyOperation::TransmitOnly { object_id, image } => {
                self.objects
                    .insert(object_id, InlineObject::KittyImage(image.rasterize()));
                self.dirty = true;
                self.bump_revision(object_id);
                (true, None)
            }
            KittyOperation::TransmitAndPlace {
                object_id,
                image,
                anchor,
            } => {
                self.remove_objects_at(&InlineAnchor {
                    row: anchor.row,
                    col: anchor.col,
                    columns: anchor.columns,
                    rows: anchor.rows,
                    style: InlineStyle::default(),
                });
                self.objects
                    .insert(object_id, InlineObject::KittyImage(image.rasterize()));
                self.set_anchor(
                    object_id,
                    InlineAnchor {
                        row: anchor.row,
                        col: anchor.col,
                        columns: anchor.columns,
                        rows: anchor.rows,
                        style: InlineStyle::default(),
                    },
                );
                (true, None)
            }
            KittyOperation::PlaceExisting { object_id, anchor } => {
                if self.objects.contains_key(&object_id) {
                    self.set_anchor(
                        object_id,
                        InlineAnchor {
                            row: anchor.row,
                            col: anchor.col,
                            columns: anchor.columns,
                            rows: anchor.rows,
                            style: InlineStyle::default(),
                        },
                    );
                }
                (true, None)
            }
            KittyOperation::Delete { object_id } => {
                if let Some(object_id) = object_id {
                    self.remove_object(object_id);
                } else {
                    self.clear_objects();
                }
                (true, None)
            }
        }
    }

    fn handle_rgp_sequence(&mut self, sequence: &[u8]) -> Option<Option<Vec<u8>>> {
        let operation = consume_rgp_sequence(sequence)?;
        Some(match operation {
            RgpOperation::SupportQuery => Some(support_reply()),
            // The AI id partition is off-limits to the RGP surface: a
            // transmission cannot register, place, or restyle an AI object.
            RgpOperation::Register { object_id, .. }
            | RgpOperation::Place { object_id, .. }
            | RgpOperation::Update { object_id, .. }
                if is_ai_object_id(object_id) =>
            {
                warn!("RGP object id {object_id} is in the AI-owned range; ignoring");
                None
            }
            RgpOperation::Register {
                object_id,
                format,
                options,
                source,
            } => {
                let load_options = ObjectLoadOptions {
                    normalize: options.normalize,
                };
                if format != "obj" && format != "glb" && format != "stl" {
                    warn!("unsupported RGP object format `{format}` for object {object_id}");
                    None
                } else {
                    match source {
                        RgpRegisterSource::Path { path } => {
                            self.pending_rgp_payloads.remove(&object_id);
                            // The `path=` register resolves embedded ratty
                            // assets only — never a filesystem path. The byte
                            // stream is untrusted, so a printed escape must not
                            // be able to read an arbitrary file from disk.
                            match load_embedded_object_source(&path, load_options) {
                                Ok((source, source_data)) => {
                                    info!("registered RGP object {} from {}", object_id, source);
                                    self.objects.insert(object_id, source_data.into());
                                    self.dirty = true;
                                    self.bump_revision(object_id);
                                    None
                                }
                                Err(error) => {
                                    warn!("failed to load RGP object {object_id}: {error:#}");
                                    None
                                }
                            }
                        }
                        RgpRegisterSource::Payload { name, more, data } => self
                            .handle_rgp_payload_chunk(
                                object_id,
                                &format,
                                name,
                                more,
                                data,
                                load_options,
                            ),
                    }
                }
            }
            RgpOperation::Place { object_id, anchor } => {
                if self.objects.contains_key(&object_id) {
                    let row = anchor
                        .row
                        .saturating_sub(anchor.rows.saturating_sub(1).div_ceil(2) as u16);
                    let col = anchor
                        .col
                        .saturating_sub(anchor.columns.saturating_sub(1).div_ceil(2) as u16);
                    self.set_anchor(
                        object_id,
                        InlineAnchor {
                            row,
                            col,
                            columns: anchor.columns,
                            rows: anchor.rows,
                            style: anchor.style.into(),
                        },
                    );
                }
                None
            }
            RgpOperation::Update { object_id, update } => {
                let mut mutated = false;
                if let Some(anchor) = self.anchors.get_mut(&object_id) {
                    let needs_rebuild = update.depth.is_some();
                    let needs_restyle = update.color.is_some() || update.brightness.is_some();
                    apply_rgp_update(&mut anchor.style, update);
                    mutated = true;
                    if needs_rebuild || needs_restyle {
                        if !matches!(
                            self.objects.get(&object_id),
                            Some(InlineObject::RgpObject(_))
                        ) {
                            // Kitty images have no per-object entity mapping;
                            // keep the conservative full rebuild for them.
                            self.dirty = true;
                        } else if !needs_rebuild && self.supports_restyle(object_id) {
                            self.restyle_objects.insert(object_id);
                        } else {
                            self.rebuild_objects.insert(object_id);
                            self.restyle_objects.remove(&object_id);
                        }
                    }
                }
                if mutated {
                    self.bump_revision(object_id);
                }
                None
            }
            RgpOperation::Delete { object_id } => {
                if let Some(object_id) = object_id {
                    self.remove_object(object_id);
                } else {
                    self.clear_objects();
                }
                None
            }
            // Stage changes never touch `dirty`: dirty despawns and respawns
            // inline objects, and a camera move must not do that.
            RgpOperation::Stage { update } => {
                self.pending_stage.push(update);
                None
            }
            RgpOperation::Ignored => None,
        })
    }

    fn remove_objects_at(&mut self, new_anchor: &InlineAnchor) {
        let row_start = new_anchor.row as i32;
        let row_end = row_start + new_anchor.rows as i32;
        let col_start = new_anchor.col as i32;
        let col_end = col_start + new_anchor.columns as i32;

        let overlapping_ids = self
            .anchors
            .iter()
            .filter_map(|(object_id, anchor)| {
                // Kitty placement never evicts AI-owned objects.
                if is_ai_object_id(*object_id) {
                    return None;
                }
                let anchor_row_start = anchor.row as i32;
                let anchor_row_end = anchor_row_start + anchor.rows as i32;
                let anchor_col_start = anchor.col as i32;
                let anchor_col_end = anchor_col_start + anchor.columns as i32;

                (anchor_row_start < row_end
                    && anchor_row_end > row_start
                    && anchor_col_start < col_end
                    && anchor_col_end > col_start)
                    .then_some(*object_id)
            })
            .collect::<Vec<_>>();

        for object_id in overlapping_ids {
            self.objects.remove(&object_id);
            self.anchors.remove(&object_id);
            self.revisions.remove(&object_id);
        }
    }

    // Buffers chunked payload registrations until the final chunk arrives, then loads and registers the object.
    fn handle_rgp_payload_chunk(
        &mut self,
        object_id: u32,
        format: &str,
        name: Option<String>,
        more: bool,
        data: Vec<u8>,
        options: ObjectLoadOptions,
    ) -> Option<Vec<u8>> {
        let pending = self
            .pending_rgp_payloads
            .entry(object_id)
            .or_insert_with(|| PendingRgpPayload {
                format: format.to_string(),
                name: name.clone(),
                data: Vec::new(),
                options,
            });
        if pending.format != format {
            warn!(
                "ignoring RGP payload chunk for object {} due to format mismatch ({} vs {})",
                object_id, pending.format, format
            );
            return None;
        }
        if pending.name.is_none() {
            pending.name = name;
        }
        pending.data.extend_from_slice(&data);
        info!(
            "received RGP payload chunk for object {} (format={}, accumulated={} bytes, more={})",
            object_id,
            pending.format,
            pending.data.len(),
            more
        );
        if more {
            return None;
        }

        let pending = self.pending_rgp_payloads.remove(&object_id)?;
        info!(
            "finalizing RGP payload for object {} (format={}, total={} bytes)",
            object_id,
            pending.format,
            pending.data.len()
        );
        match load_object_source_from_bytes_with_options(
            &pending.format,
            pending.name.as_deref(),
            &pending.data,
            pending.options,
        ) {
            Ok((source, source_data)) => {
                info!("registered RGP object {} from {}", object_id, source);
                self.objects.insert(object_id, source_data.into());
                self.dirty = true;
                self.bump_revision(object_id);
                None
            }
            Err(error) => {
                warn!("failed to load RGP object {object_id}: {error:#}");
                None
            }
        }
    }
}

struct PendingRgpPayload {
    format: String,
    name: Option<String>,
    data: Vec<u8>,
    options: ObjectLoadOptions,
}

fn normalize_hvp_sequences(bytes: &[u8]) -> Cow<'_, [u8]> {
    // vt100 handles CUP (`H`) but not HVP (`f`), so normalize cursor-positioning sequences.
    let mut normalized = None;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 2 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && matches!(bytes[j], b'0'..=b'9' | b';') {
                j += 1;
            }

            if j < bytes.len() && bytes[j] == b'f' && j > i + 2 {
                let out = normalized.get_or_insert_with(|| {
                    let mut out = Vec::with_capacity(bytes.len());
                    out.extend_from_slice(&bytes[..i]);
                    out
                });
                out.extend_from_slice(&bytes[i..j]);
                out.push(b'H');
                i = j + 1;
                continue;
            }
        }

        if let Some(out) = normalized.as_mut() {
            out.push(bytes[i]);
        }
        i += 1;
    }

    match normalized {
        Some(bytes) => Cow::Owned(bytes),
        None => Cow::Borrowed(bytes),
    }
}

fn pending_apc_prefix_start(bytes: &[u8], cursor: usize) -> usize {
    let start = cursor.min(bytes.len());
    if bytes[start..].ends_with(&APC_START[..1]) {
        bytes.len() - 1
    } else {
        bytes.len()
    }
}

fn apc_end(bytes: &[u8], payload_start: usize) -> Option<usize> {
    let mut index = payload_start;
    loop {
        if index >= bytes.len() {
            return None;
        }
        if bytes[index] == C1_ST {
            return Some(index + 1);
        }
        if index + 1 < bytes.len() && bytes[index] == ST[0] && bytes[index + 1] == ST[1] {
            return Some(index + 2);
        }
        index += 1;
    }
}

/// Registered inline object.
pub enum InlineObject {
    /// Kitty image object.
    KittyImage(KittyInlineObject),
    /// Ratty graphics object.
    RgpObject(RgpInlineObject),
}

/// Raster image payload.
pub struct RasterObject {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// RGBA image bytes.
    pub rgba: Vec<u8>,
    /// Uploaded image handle.
    pub handle: Option<Handle<Image>>,
}

/// Kitty-backed inline object.
pub struct KittyInlineObject {
    /// Raster image payload.
    pub raster: RasterObject,
    /// Indicates placeholder-driven placement.
    pub uses_placeholders: bool,
    /// Cached plane mesh and material for 3D presentation.
    pub(crate) plane: Option<KittyPlaneCache>,
}

/// RGP-backed inline object.
pub enum RgpInlineObject {
    /// STL mesh payload.
    Stl {
        /// The loaded mesh
        mesh: Mesh,
        /// Cached extruded mesh handle keyed by extrusion depth.
        handle: Option<(u32, Handle<Mesh>)>,
    },
    /// OBJ mesh payload.
    Obj {
        /// Loaded mesh parts.
        meshes: Vec<Mesh>,
        /// Cached mesh handles keyed by depth.
        handles: Option<(u32, Vec<Handle<Mesh>>)>,
    },
    /// glTF scene payload.
    Gltf {
        /// Scene asset path.
        asset_path: String,
        /// Cached scene handle.
        handle: Option<Handle<WorldAsset>>,
    },
}

impl InlineObject {
    fn scrolls_with_text(&self) -> bool {
        match self {
            InlineObject::KittyImage(object) => !object.uses_placeholders,
            InlineObject::RgpObject(_) => true,
        }
    }
}

/// Default anchor footprint (in cells) for AI-spawned objects. `object.add`
/// carries no extent parameters, so every AI object uses this footprint and
/// `scale` sizes it from there.
const AI_OBJECT_COLUMNS: u32 = 12;
const AI_OBJECT_ROWS: u32 = 6;

/// Whether an object id belongs to the AI-owned partition.
///
/// The id space is split: the AI channel owns ids at or above
/// [`crate::osc::AI_OBJECT_ID_MIN`], and the transmission/system surfaces
/// (RGP registrations, Kitty images) own the rest. Each surface refuses to
/// create, mutate, or remove ids in the other's partition, so ownership is
/// enforced mechanically at every wire ingress — not just in the AI
/// lowering layer.
fn is_ai_object_id(id: u32) -> bool {
    crate::osc::ai_object_namespace(id).is_some()
}

/// Result of an AI-channel object update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AiUpdateOutcome {
    /// The update was applied.
    Applied,
    /// No object is registered under the id.
    UnknownId,
    /// The object exists but its anchor scrolled away; `object.add` with
    /// `replace=true` re-anchors it.
    NoAnchor,
}

/// Converts a centered anchor coordinate to the stored top-left component,
/// mirroring the RGP `p` placement rule.
fn ai_anchor_component(center: u16, extent: u32) -> u16 {
    center.saturating_sub(extent.saturating_sub(1).div_ceil(2) as u16)
}

/// Inline object anchor.
pub struct InlineAnchor {
    /// Anchor row.
    pub row: u16,
    /// Anchor column.
    pub col: u16,
    /// Object width in cells.
    pub columns: u32,
    /// Object height in cells.
    pub rows: u32,
    /// Inline styling.
    pub style: InlineStyle,
}

/// Inline object style.
#[derive(Clone, Copy, Default)]
pub struct InlineStyle {
    /// Enables default animation.
    pub animate: bool,
    /// Scale multiplier.
    pub scale: f32,
    /// Extrusion depth.
    pub depth: f32,
    /// Optional object color.
    pub color: Option<[u8; 3]>,
    /// Brightness multiplier.
    pub brightness: f32,
    /// Translation offset relative to the anchor.
    pub offset: Vec3,
    /// Rotation in degrees.
    pub rotation: Vec3,
    /// Non-uniform scale multiplier.
    pub scale3: Vec3,
    /// Spin speed in radians per second; `None` uses the configured speed.
    pub spin: Option<f32>,
    /// Bob speed in radians per second; `None` uses the configured speed.
    pub bob: Option<f32>,
    /// Bob amplitude as a fraction of the cell height; `None` uses the
    /// configured amplitude.
    pub bob_amplitude: Option<f32>,
    /// Constant phase offset in radians applied to spin and bob.
    pub phase: f32,
}

impl From<RgpPlacementStyle> for InlineStyle {
    fn from(value: RgpPlacementStyle) -> Self {
        Self {
            animate: value.animate,
            scale: value.scale,
            depth: value.depth,
            color: value.color,
            brightness: value.brightness,
            offset: Vec3::from_array(value.offset),
            rotation: Vec3::from_array(value.rotation),
            scale3: Vec3::from_array(value.scale3),
            spin: value.spin,
            bob: value.bob,
            bob_amplitude: value.bob_amplitude,
            phase: value.phase,
        }
    }
}

fn apply_rgp_update(style: &mut InlineStyle, update: RgpPlacementUpdate) {
    if let Some(animate) = update.animate {
        style.animate = animate;
    }
    if let Some(scale) = update.scale {
        style.scale = scale;
    }
    if let Some(depth) = update.depth {
        style.depth = depth;
    }
    if let Some(color) = update.color {
        style.color = Some(color);
    }
    if let Some(brightness) = update.brightness {
        style.brightness = brightness;
    }
    apply_vec3_update(&mut style.offset, update.offset);
    apply_vec3_update(&mut style.rotation, update.rotation);
    apply_vec3_update(&mut style.scale3, update.scale3);
    // Like `color`, the animation rates are set-only: an update can change
    // them but not clear them back to the configured globals.
    if let Some(spin) = update.spin {
        style.spin = Some(spin);
    }
    if let Some(bob) = update.bob {
        style.bob = Some(bob);
    }
    if let Some(bob_amplitude) = update.bob_amplitude {
        style.bob_amplitude = Some(bob_amplitude);
    }
    if let Some(phase) = update.phase {
        style.phase = phase;
    }
}

fn apply_vec3_update(target: &mut Vec3, update: [Option<f32>; 3]) {
    if let Some(x) = update[0] {
        target.x = x;
    }
    if let Some(y) = update[1] {
        target.y = y;
    }
    if let Some(z) = update[2] {
        target.z = z;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgp_sequence(content: &str) -> Vec<u8> {
        format!("\x1b_ratty;g;{content}\x1b\\").into_bytes()
    }

    fn stl_object() -> InlineObject {
        InlineObject::RgpObject(RgpInlineObject::Stl {
            mesh: Mesh::new(
                bevy::mesh::PrimitiveTopology::TriangleList,
                bevy::asset::RenderAssetUsages::default(),
            ),
            handle: None,
        })
    }

    fn gltf_object() -> InlineObject {
        InlineObject::RgpObject(RgpInlineObject::Gltf {
            asset_path: "objects/x.glb".into(),
            handle: None,
        })
    }

    const AI_ID: u32 = 0x8000_0005;

    #[test]
    fn ai_insert_and_remove_stay_per_object() {
        let mut inline = TerminalInlineObjects::default();
        inline.ai_insert_object(AI_ID, stl_object(), 20, 10, InlineStyle::default());
        assert!(
            !inline.dirty,
            "AI mutations must never trigger the scene-wide respawn"
        );
        assert!(inline.rebuild_objects.contains(&AI_ID));
        assert!(inline.contains_object(AI_ID));

        assert!(inline.ai_remove_object(AI_ID));
        assert!(!inline.dirty);
        assert!(!inline.contains_object(AI_ID));
        assert!(
            inline.rebuild_objects.contains(&AI_ID),
            "removal queues a granular despawn"
        );
        assert!(
            !inline.ai_remove_object(AI_ID),
            "second removal reports absence"
        );
    }

    #[test]
    fn ai_update_routes_brightness_by_object_kind() {
        let mut inline = TerminalInlineObjects::default();
        inline.ai_insert_object(AI_ID, stl_object(), 0, 0, InlineStyle::default());
        inline.rebuild_objects.clear();
        assert_eq!(
            inline.ai_update_object(AI_ID, None, None, None, None, Some(2.0)),
            AiUpdateOutcome::Applied
        );
        assert!(
            inline.restyle_objects.contains(&AI_ID),
            "STL brightness restyles in place"
        );
        assert!(!inline.rebuild_objects.contains(&AI_ID));

        let gltf_id = 0x8000_0006;
        inline.ai_insert_object(gltf_id, gltf_object(), 0, 0, InlineStyle::default());
        inline.rebuild_objects.clear();
        assert_eq!(
            inline.ai_update_object(gltf_id, None, None, None, None, Some(2.0)),
            AiUpdateOutcome::Applied
        );
        assert!(
            inline.rebuild_objects.contains(&gltf_id),
            "glTF brightness needs a per-object rebuild"
        );

        assert_eq!(
            inline.ai_update_object(0x8000_0099, None, None, None, None, None),
            AiUpdateOutcome::UnknownId
        );
    }

    #[test]
    fn ai_update_reanchor_respawns_but_live_fields_do_not() {
        let mut inline = TerminalInlineObjects::default();
        inline.ai_insert_object(AI_ID, stl_object(), 20, 10, InlineStyle::default());

        // A scale/spin-only update keeps the object live — no respawn.
        inline.rebuild_objects.clear();
        assert_eq!(
            inline.ai_update_object(AI_ID, None, None, Some(2.5), Some(3.0), None),
            AiUpdateOutcome::Applied
        );
        assert!(
            inline.rebuild_objects.is_empty() && !inline.dirty,
            "scale/spin are live per-frame fields"
        );
        let anchor = inline.anchors.get(&AI_ID).expect("anchor exists");
        assert_eq!(anchor.style.scale, 2.5);
        assert_eq!(anchor.style.spin, Some(3.0));
        assert!(anchor.style.animate);

        // A re-anchor is a discrete relocation: it respawns so an off-screen
        // object can appear (or an on-screen one move off and despawn).
        inline.rebuild_objects.clear();
        assert_eq!(
            inline.ai_update_object(AI_ID, Some(40), Some(4), None, None, None),
            AiUpdateOutcome::Applied
        );
        let anchor = inline.anchors.get(&AI_ID).expect("anchor exists");
        // Centered on (40, 4) with the default 12x6 footprint.
        assert_eq!(anchor.col, 34);
        assert_eq!(anchor.row, 1);
        assert!(
            inline.rebuild_objects.contains(&AI_ID) && !inline.dirty,
            "re-anchor queues a per-object respawn, never a scene rebuild"
        );
    }

    #[test]
    fn ai_update_recovers_a_scrolled_away_object() {
        let mut inline = TerminalInlineObjects::default();
        inline.ai_insert_object(AI_ID, stl_object(), 10, 2, InlineStyle::default());
        // Scroll it off the top: apply_scroll drops the anchor, keeps the
        // payload.
        inline.apply_scroll(50);
        assert!(inline.contains_object(AI_ID));
        assert!(!inline.anchors.contains_key(&AI_ID), "anchor scrolled away");

        // A single coordinate cannot fully re-place it.
        assert_eq!(
            inline.ai_update_object(AI_ID, Some(30), None, None, None, None),
            AiUpdateOutcome::NoAnchor
        );
        // Both coordinates rebuild the anchor and requeue a spawn.
        inline.rebuild_objects.clear();
        assert_eq!(
            inline.ai_update_object(AI_ID, Some(30), Some(6), None, None, None),
            AiUpdateOutcome::Applied
        );
        assert!(inline.anchors.contains_key(&AI_ID), "anchor recreated");
        assert!(inline.rebuild_objects.contains(&AI_ID));
    }

    #[test]
    fn wire_surfaces_cannot_touch_the_ai_partition() {
        let mut inline = TerminalInlineObjects::default();
        inline.ai_insert_object(AI_ID, stl_object(), 5, 5, InlineStyle::default());

        // RGP register on an AI-range id is refused.
        inline.handle_rgp_sequence(&rgp_sequence(&format!("r;id={AI_ID};fmt=obj;path=x.obj")));
        // The AI object is untouched (still the STL we inserted).
        assert!(matches!(
            inline.objects.get(&AI_ID),
            Some(InlineObject::RgpObject(RgpInlineObject::Stl { .. }))
        ));

        // RGP delete-all clears only the transmission partition.
        inline.objects.insert(3, gltf_object());
        inline.handle_rgp_sequence(&rgp_sequence("d"));
        assert!(
            inline.contains_object(AI_ID),
            "transmission clear-all spares AI objects"
        );
        assert!(
            !inline.contains_object(3),
            "it still clears its own objects"
        );
    }

    #[test]
    fn ai_clear_scopes_to_namespace_and_spares_low_ids() {
        let mut inline = TerminalInlineObjects::default();
        // A transmission-owned object (below the AI range).
        inline.objects.insert(7, stl_object());
        inline.ai_insert_object(0x8000_0001, stl_object(), 0, 0, InlineStyle::default());
        // A different agent namespace.
        inline.ai_insert_object(0x8100_0001, gltf_object(), 0, 0, InlineStyle::default());

        assert_eq!(inline.ai_clear_namespace(0), vec![0x8000_0001]);
        assert!(inline.contains_object(7));
        assert!(inline.contains_object(0x8100_0001));
        assert!(
            inline.ai_clear_namespace(0).is_empty(),
            "clear is idempotent"
        );

        assert_eq!(inline.ai_clear_all(), vec![0x8100_0001]);
        assert!(
            inline.contains_object(7),
            "reset spares transmission objects"
        );
    }

    #[test]
    fn rgp_path_register_resolves_embedded_assets_only() {
        // Write a real, loadable OBJ to disk. The wire `path=` register must
        // NOT read it — a program printing to the terminal cannot make ratty
        // load an arbitrary file. (On the pre-fix disk-first loader this file
        // would register, so this assertion is the regression guard.)
        // Unique per process *and* per run: concurrent `cargo test` invocations
        // (two worktrees sharing $TMPDIR, a background run plus a foreground
        // one) must not have one run's cleanup delete another run's directory
        // between create_dir_all and write.
        static NEXT_DIR: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = NEXT_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ratty_rgp_path_register_test_{}_{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let disk_asset = dir.join("disk_only.obj");
        std::fs::write(&disk_asset, "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n")
            .expect("write disk asset");

        // The file genuinely loads through the trusted config loader — so the
        // wire register's refusal below is the embedded-only gate, not an
        // unloadable asset.
        assert!(
            crate::model::load_object_source_with_options(
                &disk_asset,
                crate::model::ObjectLoadOptions::default(),
            )
            .is_ok(),
            "the disk asset is loadable through the trusted path"
        );

        let mut inline = TerminalInlineObjects::default();
        inline.handle_rgp_sequence(&rgp_sequence(&format!(
            "r;id=1;fmt=obj;path={}",
            disk_asset.display()
        )));
        assert!(
            !inline.objects.contains_key(&1),
            "an absolute filesystem path must not load: the wire cannot read disk"
        );

        // A traversal path is refused for the same reason.
        inline.handle_rgp_sequence(&rgp_sequence("r;id=2;fmt=obj;path=../../etc/passwd.obj"));
        assert!(
            !inline.objects.contains_key(&2),
            "traversal paths resolve to a non-embedded name and are refused"
        );

        // An embedded ratty asset still registers by name.
        inline.handle_rgp_sequence(&rgp_sequence("r;id=3;fmt=obj;path=CairoSpinyMouse.obj"));
        assert!(
            inline.objects.contains_key(&3),
            "embedded assets still resolve by name"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stage_sequences_queue_in_order_without_dirtying_objects() {
        let mut inline = TerminalInlineObjects::default();
        let first = inline.handle_rgp_sequence(&rgp_sequence("c;warp=0.1"));
        let second = inline.handle_rgp_sequence(&rgp_sequence("c;warp=0.9;dur=2"));
        assert_eq!(first, Some(None), "stage sequences produce no reply");
        assert_eq!(second, Some(None));
        assert!(!inline.dirty, "stage sequences must not respawn objects");
        assert!(inline.has_pending_stage());

        let updates = inline.take_stage_updates();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].warp, Some(0.1));
        assert_eq!(updates[1].warp, Some(0.9));
        assert_eq!(updates[1].dur, Some(2.0));
        assert!(!inline.has_pending_stage());
    }

    #[test]
    fn v1_place_style_converts_field_identically() {
        let sequence = rgp_sequence(
            "p;id=1;row=13;col=74;w=28;h=16;animate=1;scale=1.15;depth=0.2;color=aabbcc;\
             brightness=1.1;px=0.1;py=0.2;pz=0.3;rx=180;ry=90;rz=45;sx=1;sy=2;sz=3",
        );
        let Some(RgpOperation::Place { anchor, .. }) = consume_rgp_sequence(&sequence) else {
            panic!("place sequence did not parse");
        };
        let style: InlineStyle = anchor.style.into();
        assert!(style.animate);
        assert_eq!(style.scale, 1.15);
        assert_eq!(style.depth, 0.2);
        assert_eq!(style.color, Some([0xaa, 0xbb, 0xcc]));
        assert_eq!(style.brightness, 1.1);
        assert_eq!(style.offset, Vec3::new(0.1, 0.2, 0.3));
        assert_eq!(style.rotation, Vec3::new(180.0, 90.0, 45.0));
        assert_eq!(style.scale3, Vec3::new(1.0, 2.0, 3.0));
        // v2 animation fields stay neutral when a v1 sequence places.
        assert!(style.spin.is_none());
        assert!(style.bob.is_none());
        assert!(style.bob_amplitude.is_none());
        assert_eq!(style.phase, 0.0);
    }

    fn inline_with_anchor(object_id: u32) -> TerminalInlineObjects {
        let mut inline = TerminalInlineObjects::default();
        inline.anchors.insert(
            object_id,
            InlineAnchor {
                row: 4,
                col: 6,
                columns: 8,
                rows: 4,
                style: InlineStyle {
                    animate: true,
                    scale: 1.0,
                    brightness: 1.0,
                    scale3: Vec3::ONE,
                    ..Default::default()
                },
            },
        );
        inline
    }

    fn register_mesh_object(inline: &mut TerminalInlineObjects, object_id: u32) {
        inline.objects.insert(
            object_id,
            InlineObject::RgpObject(RgpInlineObject::Obj {
                meshes: Vec::new(),
                handles: None,
            }),
        );
    }

    fn register_gltf_object(inline: &mut TerminalInlineObjects, object_id: u32) {
        inline.objects.insert(
            object_id,
            InlineObject::RgpObject(RgpInlineObject::Gltf {
                asset_path: "test.glb".to_string(),
                handle: None,
            }),
        );
    }

    #[test]
    fn animation_updates_apply_live_without_respawning() {
        let mut inline = inline_with_anchor(1);
        register_mesh_object(&mut inline, 1);
        inline.dirty = false;
        inline.handle_rgp_sequence(&rgp_sequence("u;id=1;spin=2.0;phase=0.5"));
        let style = inline.anchors[&1].style;
        assert_eq!(style.spin, Some(2.0));
        assert_eq!(style.phase, 0.5);
        assert!(!inline.dirty, "animation fields are live updates");
        assert!(inline.rebuild_objects.is_empty());
        assert!(inline.restyle_objects.is_empty());
    }

    #[test]
    fn depth_updates_rebuild_only_their_object() {
        let mut inline = inline_with_anchor(1);
        register_mesh_object(&mut inline, 1);
        inline.dirty = false;
        inline.handle_rgp_sequence(&rgp_sequence("u;id=1;depth=1.0;spin=2.0"));
        let style = inline.anchors[&1].style;
        assert_eq!(style.depth, 1.0);
        assert_eq!(style.spin, Some(2.0));
        assert!(!inline.dirty, "depth must not respawn the whole scene");
        assert_eq!(inline.take_rebuild_objects(), HashSet::from([1]));
        assert!(inline.restyle_objects.is_empty());
    }

    #[test]
    fn color_and_brightness_updates_restyle_in_place() {
        let mut inline = inline_with_anchor(1);
        register_mesh_object(&mut inline, 1);
        inline.dirty = false;
        inline.handle_rgp_sequence(&rgp_sequence("u;id=1;color=ff8844"));
        inline.handle_rgp_sequence(&rgp_sequence("u;id=1;brightness=1.5"));
        let style = inline.anchors[&1].style;
        assert_eq!(style.color, Some([0xff, 0x88, 0x44]));
        assert_eq!(style.brightness, 1.5);
        assert!(!inline.dirty, "mesh restyles must not respawn anything");
        assert!(inline.rebuild_objects.is_empty());
        assert!(inline.has_restyle_objects());
        assert_eq!(inline.take_restyle_objects(), HashSet::from([1]));
    }

    #[test]
    fn depth_supersedes_a_pending_restyle() {
        let mut inline = inline_with_anchor(1);
        register_mesh_object(&mut inline, 1);
        inline.dirty = false;
        inline.handle_rgp_sequence(&rgp_sequence("u;id=1;color=ff8844"));
        inline.handle_rgp_sequence(&rgp_sequence("u;id=1;depth=1.0"));
        assert!(!inline.dirty);
        assert!(
            !inline.has_restyle_objects(),
            "the rebuild respawns from current style, covering the restyle"
        );
        assert_eq!(inline.take_rebuild_objects(), HashSet::from([1]));
    }

    #[test]
    fn gltf_styles_take_the_rebuild_path() {
        let mut inline = inline_with_anchor(1);
        register_gltf_object(&mut inline, 1);
        inline.dirty = false;
        inline.handle_rgp_sequence(&rgp_sequence("u;id=1;color=ff8844"));
        assert!(!inline.dirty);
        assert!(
            !inline.has_restyle_objects(),
            "glTF scenes own their materials; style cannot rewrite them"
        );
        assert_eq!(inline.take_rebuild_objects(), HashSet::from([1]));
    }

    #[test]
    fn updates_without_an_object_mapping_respawn_globally() {
        let mut inline = inline_with_anchor(1);
        inline.dirty = false;
        inline.handle_rgp_sequence(&rgp_sequence("u;id=1;color=ff8844"));
        assert!(
            inline.dirty,
            "no per-object entity mapping exists; keep the full rebuild"
        );
        assert!(inline.rebuild_objects.is_empty());
        assert!(inline.restyle_objects.is_empty());
    }

    #[test]
    fn finish_sync_clears_pending_granular_work() {
        let mut inline = inline_with_anchor(1);
        register_mesh_object(&mut inline, 1);
        inline.handle_rgp_sequence(&rgp_sequence("u;id=1;color=ff8844"));
        inline.handle_rgp_sequence(&rgp_sequence("u;id=2;depth=1.0"));
        inline.rebuild_objects.insert(7);
        inline.finish_sync(Vec2::new(800.0, 600.0), 104, 32);
        assert!(!inline.dirty);
        assert!(inline.rebuild_objects.is_empty());
        assert!(inline.restyle_objects.is_empty());
    }

    /// Runs bytes through the OSC guard, returning the bytes it forwarded
    /// to the parser plus the guard's final state.
    fn guard_forward(bytes: &[u8]) -> (Vec<u8>, OscGuard) {
        let mut guard = OscGuard::default();
        let mut out = Vec::new();
        guard.for_each_run(bytes, |run| out.extend_from_slice(run));
        (out, guard)
    }

    #[test]
    fn osc_guard_passes_bounded_sequences_untouched() {
        // A normal OSC (title) and surrounding text are forwarded verbatim.
        let title = b"before\x1b]0;a window title\x07after";
        let (out, guard) = guard_forward(title);
        assert_eq!(out, title);
        assert_eq!(guard.state, OscGuardState::Ground);

        // Non-OSC escapes must never be mistaken for OSC: a CSI, an ST
        // (`ESC \`), and a bare `]` in ground all pass through.
        let mixed = b"\x1b[1;2mhi\x1b\\a ] bracket\x1b]0;t\x07";
        let (out, _) = guard_forward(mixed);
        assert_eq!(out, mixed);
    }

    #[test]
    fn osc_guard_bounds_an_oversized_osc_payload() {
        let mut seq = b"\x1b]52;".to_vec();
        seq.resize(seq.len() + MAX_OSC_SEQUENCE_BYTES * 2, b'x'); // no terminator
        let (out, guard) = guard_forward(&seq);

        // vte receives the introducer plus at most the cap of payload —
        // never the full oversized run.
        assert!(out.starts_with(b"\x1b]52;"));
        assert!(out.len() <= MAX_OSC_SEQUENCE_BYTES + 8);
        assert_eq!(guard.state, OscGuardState::OscDropping);
        assert_eq!(
            guard.osc_len,
            MAX_OSC_SEQUENCE_BYTES + 1,
            "the counter freezes one past the cap"
        );
    }

    #[test]
    fn osc_guard_forwards_the_terminator_and_recovers() {
        let mut guard = OscGuard::default();
        let mut out = Vec::new();

        // An oversized OSC ending in its BEL terminator, with no trailing
        // bytes, so the only BEL in the output is the sequence's own
        // terminator — a dropped terminator would leave zero.
        let mut seq = b"\x1b]0;".to_vec();
        seq.resize(seq.len() + MAX_OSC_SEQUENCE_BYTES + 100, b'x');
        seq.push(OscGuard::BEL);
        guard.for_each_run(&seq, |run| out.extend_from_slice(run));

        assert_eq!(
            guard.state,
            OscGuardState::Ground,
            "the terminator ended the OSC"
        );
        assert_eq!(
            out.iter().filter(|&&byte| byte == OscGuard::BEL).count(),
            1,
            "the oversized OSC's own terminator is forwarded, not dropped"
        );
        assert_eq!(*out.last().expect("nonempty"), OscGuard::BEL);
        assert!(out.len() <= MAX_OSC_SEQUENCE_BYTES + 8);

        // The same guard recovers: a following normal OSC passes through
        // intact, proving vte was left in sync (not stuck mid-OSC).
        let recovered_at = out.len();
        guard.for_each_run(b"\x1b]0;short\x07", |run| out.extend_from_slice(run));
        assert_eq!(&out[recovered_at..], b"\x1b]0;short\x07");
        assert_eq!(guard.state, OscGuardState::Ground);
    }

    #[test]
    fn osc_guard_tracks_osc_after_an_intervening_escape_byte() {
        // vte stays in its escape state after executing a C0 control (bar
        // CAN/SUB), DEL, or a 0x80..=0xFF byte, so `ESC <c> ]` still opens
        // an OSC. The guard must engage too, or a one-byte prefix bypasses
        // the cap entirely.
        for prefix in [0x00u8, 0x05, 0x17, 0x19, 0x1b, 0x1f, 0x7f, 0x80, 0xff] {
            let mut seq = vec![OscGuard::ESC, prefix, OscGuard::OSC_INTRODUCER];
            seq.resize(seq.len() + MAX_OSC_SEQUENCE_BYTES * 2, b'x');
            let (out, guard) = guard_forward(&seq);
            assert_eq!(
                guard.state,
                OscGuardState::OscDropping,
                "ESC {prefix:#04x} ] must still be tracked as an OSC",
            );
            assert!(out.len() <= MAX_OSC_SEQUENCE_BYTES + 8);
        }

        // But a byte that advances vte OUT of Escape (a CSI `[`, or CAN)
        // must not be treated as still-in-escape: `ESC [ ]` is a CSI then
        // printable text, never an OSC.
        let (_, guard) = guard_forward(b"\x1b[]xxxxx");
        assert_eq!(guard.state, OscGuardState::Ground);
        let (_, guard) = guard_forward(b"\x1b\x18]xxxxx");
        assert_eq!(guard.state, OscGuardState::Ground);
    }

    #[test]
    fn osc_guard_recovers_on_every_terminator_kind_while_dropping() {
        // CAN, SUB, and ESC all terminate an OSC in vte; each must be
        // forwarded so the guard leaves its dropping state in sync.
        for terminator in [OscGuard::CAN, OscGuard::SUB, OscGuard::ESC] {
            let mut seq = b"\x1b]0;".to_vec();
            seq.resize(seq.len() + MAX_OSC_SEQUENCE_BYTES + 50, b'x');
            seq.push(terminator);
            let (_, guard) = guard_forward(&seq);
            assert_ne!(
                guard.state,
                OscGuardState::OscDropping,
                "terminator {terminator:#04x} must end the dropped OSC",
            );
        }
    }

    #[test]
    fn oversized_osc_stays_bounded_across_chunks_and_keeps_vte_in_sync() {
        use crate::runtime::TerminalParserCallbacks;

        let mut parser =
            vt100::Parser::new_with_callbacks(4, 40, 0, TerminalParserCallbacks::default());
        let mut inline = TerminalInlineObjects::default();

        // A multi-megabyte unterminated OSC arriving in realistic 16 KiB
        // PTY reads: the introducer in the first chunk, payload after.
        let mut first = b"\x1b]52;".to_vec();
        first.resize(16 * 1024, b'x');
        inline.consume_pty_output(&first, &mut parser);
        for _ in 0..256 {
            inline.consume_pty_output(&vec![b'x'; 16 * 1024], &mut parser);
        }

        let (dropping, osc_len) = inline.osc_guard_state();
        assert!(dropping, "the guard engaged on the oversized OSC");
        assert!(
            osc_len <= MAX_OSC_SEQUENCE_BYTES + 1,
            "vte received at most the cap, not the multi-megabyte stream"
        );

        // Terminate the OSC and print visible text: it must land on screen,
        // proving the forwarded terminator kept vte's parser in sync.
        inline.consume_pty_output(b"\x07hello", &mut parser);
        assert!(parser.screen().contents().contains("hello"));
    }
}
