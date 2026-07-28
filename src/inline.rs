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
    /// Set while swallowing the remainder of an APC that overran
    /// [`MAX_APC_SEQUENCE_BYTES`]; persists across chunks.
    apc_discarding: bool,
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
pub(crate) const MAX_OSC_SEQUENCE_BYTES: usize = 64 * 1024;

// The watchdog cap must never sit below the query-acceptance bound, or a
// valid-but-large 778 query would be truncated before it could be parsed.
const _: () = assert!(MAX_OSC_SEQUENCE_BYTES >= crate::query::MAX_QUERY_SEQUENCE_BYTES);

/// Upper bound on a single *unterminated* APC sequence held in
/// `pending_bytes` while waiting for its `ESC \` terminator.
///
/// The APC sibling of [`MAX_OSC_SEQUENCE_BYTES`]: OSC payloads accumulate
/// inside vte, APC payloads accumulate here. An APC that never terminates
/// would otherwise be retained in full and re-extended by every following
/// PTY read (`cat` of a hostile file emitting `ESC _ ratty;g;` and then
/// gigabytes), growing `pending_bytes` without bound.
///
/// Sized well above anything legitimate. The canonical encoder
/// (`RattyGraphic::register_payload_sequences_with_name`) splits asset
/// payloads at `PAYLOAD_CHUNK_SIZE` = 3072 base64 characters per sequence,
/// so the largest single APC ratty's own tooling emits is ~3.2 KiB —
/// measured across every shipped transmission, the real maximum is 3157
/// bytes. Chunked RGP registrations are *many individually terminated*
/// APCs (the `more=1` flag in [`crate::rgp`]), never one enormous one, so
/// no legitimate single sequence approaches this cap. The headroom covers
/// unchunked Kitty graphics transmissions, which carry a whole image in
/// one APC: 8 MiB still exceeds the base64 of the largest asset shipped
/// here (`assets/objects/SpinyMouse.glb`, 1.68 MB → 2.25 MB encoded).
const MAX_APC_SEQUENCE_BYTES: usize = 8 * 1024 * 1024;

/// Upper bound on the combined size of all in-flight chunked RGP payloads
/// ([`TerminalInlineObjects::handle_rgp_payload_chunk`]).
///
/// [`MAX_APC_SEQUENCE_BYTES`] cannot bound this: every chunk in a run is a
/// well-formed, individually terminated APC that passes the per-sequence
/// cap, while the *accumulator* they feed grows with the run. Both the
/// length of a run (`more=1` forever) and the number of concurrent runs
/// (a fresh `id=` each time) are attacker-chosen, so one budget spans the
/// whole in-flight set rather than each object.
///
/// Decoded bytes, not base64 — 38x the largest asset shipped here
/// (`assets/objects/SpinyMouse.glb`, 1.68 MB).
const MAX_PENDING_RGP_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Upper bound on the number of concurrent chunked RGP payload runs, so a
/// stream of distinct `id=`s cannot grow the map itself without bound even
/// while each run stays tiny.
const MAX_PENDING_RGP_PAYLOADS: usize = 256;

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
    /// Spike #55 byte-level tee (TEST BUILDS ONLY). When `Some`, every byte
    /// actually handed to `vt100::Parser::process` is appended here, so a
    /// test can assert the charter's literal standard — "zero bytes may ever
    /// leak into pane 0" (docs/research/browser-story.md:507) — instead of
    /// the weaker screen-state standard. The screen oracle cannot see bytes
    /// that reach vte and are swallowed inside its APC state machine.
    ///
    /// `None` by default so the test-mode throughput benchmark measures the
    /// scanner, not the tee's memcpy.
    #[cfg(test)]
    forwarded: Option<Vec<u8>>,
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
        // TEST BUILDS ONLY: mirror the exact runs into the tee. `teed` is a
        // local because `for_each_run` already borrows `self` mutably; the
        // whole capture compiles away in non-test builds, leaving the body
        // byte-for-byte the original `for_each_run(bytes, |run| …)` call.
        #[cfg(test)]
        let mut teed: Option<Vec<u8>> = self.forwarded.as_ref().map(|_| Vec::new());
        self.for_each_run(bytes, |run| {
            #[cfg(test)]
            if let Some(teed) = teed.as_mut() {
                teed.extend_from_slice(run);
            }
            parser.process(run);
        });
        #[cfg(test)]
        if let (Some(teed), Some(sink)) = (teed, self.forwarded.as_mut()) {
            sink.extend_from_slice(&teed);
        }
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
    /// Spike #55 byte-level tee (TEST BUILDS ONLY): starts capturing every
    /// byte handed to `vt100::Parser::process`. See [`OscGuard::forwarded`].
    #[cfg(test)]
    fn enable_forward_tee(&mut self) {
        self.osc_guard.forwarded = Some(Vec::new());
    }

    /// Spike #55 byte-level tee (TEST BUILDS ONLY): drains and returns every
    /// byte captured since [`Self::enable_forward_tee`].
    #[cfg(test)]
    fn take_forwarded_bytes(&mut self) -> Vec<u8> {
        self.osc_guard
            .forwarded
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

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

        // An earlier chunk overran the APC cap: keep swallowing bytes until
        // this sequence's terminator, so its tail is never mistaken for
        // text and printed to the screen.
        if self.apc_discarding && !self.resync_after_overlong_apc() {
            self.osc_guard = osc_guard;
            return replies;
        }

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
                // Retain the in-progress APC so a later chunk can complete
                // it — but never past the cap, or a never-terminated
                // sequence grows this buffer by every following PTY read.
                self.pending_bytes.drain(..start);
                if self.pending_bytes.len() > MAX_APC_SEQUENCE_BYTES {
                    warn!(
                        "discarding a malformed APC sequence: unterminated past {MAX_APC_SEQUENCE_BYTES} bytes"
                    );
                    self.apc_discarding = true;
                    self.resync_after_overlong_apc();
                }
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

    /// Advances past an APC that overran [`MAX_APC_SEQUENCE_BYTES`] and is
    /// being discarded.
    ///
    /// Returns whether the sequence's terminator was found and dropped, so
    /// normal parsing can resume. When it was not, the buffer is emptied
    /// bar a trailing lone `ESC` — which may be the first half of an
    /// `ESC \` terminator split across two PTY reads — and discarding
    /// continues into the next chunk.
    fn resync_after_overlong_apc(&mut self) -> bool {
        if let Some(end) = apc_end(&self.pending_bytes, 0) {
            self.pending_bytes.drain(..end);
            self.apc_discarding = false;
            true
        } else {
            let keep = usize::from(self.pending_bytes.last() == Some(&ST[0]));
            let drop_to = self.pending_bytes.len() - keep;
            self.pending_bytes.drain(..drop_to);
            false
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

    /// Test-only view of the APC accumulator: retained bytes and whether
    /// an over-long sequence is being discarded.
    #[cfg(test)]
    pub(crate) fn apc_buffer_state(&self) -> (usize, bool) {
        (self.pending_bytes.len(), self.apc_discarding)
    }

    /// Test-only view of the in-flight chunked RGP payload accumulator:
    /// run count and combined decoded bytes.
    #[cfg(test)]
    pub(crate) fn pending_rgp_payload_state(&self) -> (usize, usize) {
        (
            self.pending_rgp_payloads.len(),
            self.pending_rgp_payloads
                .values()
                .map(|pending| pending.data.len())
                .sum(),
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
        // Every chunk in a run is its own terminated APC, so the
        // per-sequence cap never sees this accumulator grow. Bound the
        // whole in-flight set here instead: first the number of concurrent
        // runs, then their combined size. The sum walks at most
        // `MAX_PENDING_RGP_PAYLOADS` entries per ~3 KiB chunk, which is
        // cheaper than keeping a running total correct across every
        // insertion, finalization, and removal path.
        if self.pending_rgp_payloads.len() >= MAX_PENDING_RGP_PAYLOADS
            && !self.pending_rgp_payloads.contains_key(&object_id)
        {
            warn!(
                "ignoring RGP payload chunk for object {object_id}: \
                 {MAX_PENDING_RGP_PAYLOADS} chunk runs are already in flight"
            );
            return None;
        }
        let in_flight: usize = self
            .pending_rgp_payloads
            .values()
            .map(|pending| pending.data.len())
            .sum();
        if in_flight.saturating_add(data.len()) > MAX_PENDING_RGP_PAYLOAD_BYTES {
            warn!(
                "dropping the RGP payload chunk run for object {object_id}: in-flight \
                 payloads would exceed {MAX_PENDING_RGP_PAYLOAD_BYTES} bytes"
            );
            // The run can never finalize correctly now, so free it rather
            // than leaving it pinning the budget.
            self.pending_rgp_payloads.remove(&object_id);
            return None;
        }

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

    fn test_parser() -> vt100::Parser<crate::runtime::TerminalParserCallbacks> {
        vt100::Parser::new_with_callbacks(
            4,
            40,
            0,
            crate::runtime::TerminalParserCallbacks::default(),
        )
    }

    #[test]
    fn unterminated_apc_stays_bounded_across_chunks() {
        let mut parser = test_parser();
        let mut inline = TerminalInlineObjects::default();

        // An RGP register that opens and then never terminates, arriving in
        // realistic 64 KiB PTY reads until it is well past the cap.
        let mut first = b"\x1b_ratty;g;r;id=1;fmt=obj;source=payload;more=0;".to_vec();
        first.resize(64 * 1024, b'A');
        inline.consume_pty_output(&first, &mut parser);
        for _ in 0..192 {
            inline.consume_pty_output(&vec![b'A'; 64 * 1024], &mut parser);
            let (buffered, _) = inline.apc_buffer_state();
            assert!(
                buffered <= MAX_APC_SEQUENCE_BYTES + 64 * 1024,
                "the APC accumulator grew to {buffered} bytes, past the cap",
            );
        }

        // 12 MiB in: the cap engaged and the buffer collapsed to (at most)
        // the retained terminator prefix, rather than holding the stream.
        let (buffered, discarding) = inline.apc_buffer_state();
        assert!(discarding, "the over-long APC is being discarded");
        assert!(
            buffered <= 1,
            "a discarded APC retains at most a split terminator, held {buffered} bytes",
        );
    }

    #[test]
    fn discarded_apc_resyncs_at_its_terminator() {
        let mut parser = test_parser();
        let mut inline = TerminalInlineObjects::default();

        let mut seq = b"\x1b_ratty;g;r;id=1;fmt=obj;source=payload;more=0;".to_vec();
        seq.resize(MAX_APC_SEQUENCE_BYTES + 4096, b'A');
        inline.consume_pty_output(&seq, &mut parser);
        assert!(inline.apc_buffer_state().1, "the guard engaged");

        // The tail of the discarded APC must not reach the screen as text,
        // even though it is plain printable base64.
        inline.consume_pty_output(b"AAAAAAAA", &mut parser);
        assert!(!parser.screen().contents().contains("AAAA"));

        // Its terminator ends the discard; following output parses normally.
        inline.consume_pty_output(b"\x1b\\hello", &mut parser);
        let (buffered, discarding) = inline.apc_buffer_state();
        assert!(!discarding, "the terminator resynced the parser");
        assert_eq!(buffered, 0, "the buffer drained past the terminator");
        assert!(parser.screen().contents().contains("hello"));
    }

    #[test]
    fn discarded_apc_resyncs_on_a_split_terminator() {
        let mut parser = test_parser();
        let mut inline = TerminalInlineObjects::default();

        let mut seq = b"\x1b_ratty;g;".to_vec();
        seq.resize(MAX_APC_SEQUENCE_BYTES + 16, b'A');
        inline.consume_pty_output(&seq, &mut parser);
        assert!(inline.apc_buffer_state().1);

        // `ESC \` arriving split across two PTY reads must still terminate
        // the discard — the lone ESC has to survive the buffer purge.
        inline.consume_pty_output(b"\x1b", &mut parser);
        assert!(inline.apc_buffer_state().1, "still awaiting the ST");
        inline.consume_pty_output(b"\\hello", &mut parser);
        assert!(!inline.apc_buffer_state().1, "the split ST resynced");
        assert!(parser.screen().contents().contains("hello"));
    }

    #[test]
    fn large_terminated_rgp_register_still_parses() {
        let mut parser = test_parser();
        let mut inline = TerminalInlineObjects::default();

        // A single terminated APC far larger than anything ratty's own
        // encoder emits (PAYLOAD_CHUNK_SIZE is 3072 base64 chars, and the
        // largest sequence across all shipped transmissions is 3157 bytes)
        // but under the cap: it must be consumed whole, not truncated.
        let payload = "A".repeat(MAX_APC_SEQUENCE_BYTES / 2);
        let sequence = format!(
            "\x1b_ratty;g;r;id=7;fmt=obj;source=payload;more=1;name=big.obj;{payload}\x1b\\"
        );
        let bytes = sequence.into_bytes();
        assert!(bytes.len() > 4 * 1024 * 1024);

        // Delivered in 64 KiB PTY reads, the way a real stream arrives.
        for chunk in bytes.chunks(64 * 1024) {
            inline.consume_pty_output(chunk, &mut parser);
        }

        let (buffered, discarding) = inline.apc_buffer_state();
        assert!(
            !discarding,
            "a terminated sequence under the cap is not discarded"
        );
        assert_eq!(buffered, 0, "the whole sequence was consumed");
        // It reached the RGP payload accumulator intact rather than being
        // truncated or printed: `more=1` leaves the run open.
        let (runs, accumulated) = inline.pending_rgp_payload_state();
        assert_eq!(runs, 1, "the register opened a chunk run");
        assert!(
            accumulated >= 3 * 1024 * 1024,
            "decoded {accumulated} bytes"
        );
        assert!(
            !parser.screen().contents().contains("AAAA"),
            "a handled APC must never be echoed as text"
        );
    }

    #[test]
    fn one_rgp_payload_run_cannot_grow_without_bound() {
        let mut parser = test_parser();
        let mut inline = TerminalInlineObjects::default();

        // Every chunk here is a well-formed, individually terminated APC,
        // so MAX_APC_SEQUENCE_BYTES never sees it — only the accumulator
        // budget stops the run.
        let chunk = "A".repeat(4 * 1024 * 1024); // 3 MiB decoded per chunk
        let sequence =
            format!("\x1b_ratty;g;r;id=1;fmt=obj;source=payload;more=1;name=t.obj;{chunk}\x1b\\")
                .into_bytes();
        for _ in 0..32 {
            inline.consume_pty_output(&sequence, &mut parser);
            let (_, accumulated) = inline.pending_rgp_payload_state();
            assert!(
                accumulated <= MAX_PENDING_RGP_PAYLOAD_BYTES,
                "in-flight payloads reached {accumulated} bytes, past the cap",
            );
        }
    }

    #[test]
    fn concurrent_rgp_payload_runs_stay_bounded() {
        let mut parser = test_parser();
        let mut inline = TerminalInlineObjects::default();

        // A fresh id per chunk grows the map, not any single run.
        for id in 0..(MAX_PENDING_RGP_PAYLOADS as u32 * 4) {
            let sequence = format!(
                "\x1b_ratty;g;r;id={id};fmt=obj;source=payload;more=1;name=t.obj;QUFB\x1b\\"
            );
            inline.consume_pty_output(sequence.as_bytes(), &mut parser);
        }

        let (runs, _) = inline.pending_rgp_payload_state();
        assert!(
            runs <= MAX_PENDING_RGP_PAYLOADS,
            "{runs} concurrent chunk runs are in flight, past the cap",
        );
    }
}

/// Spike #55, charter item 3: the pane-0 corruption kill-condition fuzz.
///
/// THE QUESTION. Shape (b) of the browser-mux design puts pane-N content in
/// APC frames (`ESC _ ratty;m;<pane>;<payload>` + terminator) inside the same
/// byte stream as pane 0. Can a torn or malformed mux frame ever change what
/// pane 0 renders? The kill condition is absolute: zero divergence.
///
/// THE ORACLE is differential, never self-referential:
///   * baseline B — realistic pane-0 output (text, SGR, cursor moves, and a
///     real RGP APC so the scanner's claim path is exercised) — is fed
///     unchunked into a pristine `TerminalInlineObjects` + `vt100::Parser`.
///   * muxed S — byte-for-byte B with mux frames spliced in — is fed into a
///     fresh scanner+parser once per tearing.
///   * the two screens must be identical, in both `contents()` (the text
///     plane) and `contents_formatted()` (text + SGR attributes + cursor).
///
/// The oracle is SCREEN STATE, not intercepted bytes: `consume_pty_output`
/// takes a concrete `vt100::Parser`, so there is no seam to shim without
/// changing `inline.rs`. The screen is the ground truth for "did anything
/// reach pane 0", which is the question being asked.
#[cfg(test)]
mod spike55_pane0_fuzz {
    use super::*;
    use std::collections::BTreeSet;

    // ---------------------------------------------------------------- setup

    fn new_parser() -> vt100::Parser<crate::runtime::TerminalParserCallbacks> {
        vt100::Parser::new_with_callbacks(
            24,
            80,
            0,
            crate::runtime::TerminalParserCallbacks::default(),
        )
    }

    /// Screen state after feeding `chunks` through a fresh scanner+parser.
    /// `.0` is the text plane, `.1` is text + attributes + cursor.
    fn feed(chunks: &[&[u8]]) -> (String, String) {
        let mut parser = new_parser();
        let mut inline = TerminalInlineObjects::default();
        for chunk in chunks {
            inline.consume_pty_output(chunk, &mut parser);
        }
        let screen = parser.screen();
        (
            screen.contents(),
            String::from_utf8_lossy(&screen.contents_formatted()).into_owned(),
        )
    }

    /// Baseline pane-0 traffic, in the pieces a sequence-aware muxer would
    /// treat as atomic. Every token is unique so loss is detectable by name.
    fn baseline_parts() -> Vec<&'static [u8]> {
        vec![
            b"\x1b[2J\x1b[H",
            b"ALPHA0 plain text line\r\n",
            b"\x1b[31mBRAVO1 red\x1b[0m normal\r\n",
            b"\x1b[1;33;44mCHARLIE2 bold on blue\x1b[m\r\n",
            b"\x1b[5C\x1b[7mDELTA3\x1b[27m indented and reversed\r\n",
            // A legitimate RGP APC: claimed by the scanner, never echoed.
            b"\x1b_ratty;g;d\x1b\\",
            b"ECHO4 after the rgp delete\r\n",
            // An RGP support query: claimed, and emits a PTY reply.
            b"\x1b_ratty;g;s\x1b\\",
            b"FOXTROT5 after the rgp query\r\n",
            b"\x1b[12;4HGOLF6 cursor-addressed\r\n",
            b"\x1b[1;20r\x1b[15;1HHOTEL7 below a scroll region\r\n\x1b[r",
        ]
    }

    const BASELINE_TOKENS: &[&str] = &[
        "ALPHA0", "BRAVO1", "CHARLIE2", "DELTA3", "ECHO4", "FOXTROT5", "GOLF6", "HOTEL7",
    ];

    /// Bodies of pane 0's OWN escape sequences. If any of these render as
    /// text, pane 0's control stream was destroyed — a corruption distinct
    /// from mux payload bytes leaking through.
    const PANE0_ESCAPE_BODIES: &[&str] = &[
        "[2J",
        "[31m",
        "[1;33;44m",
        "[5C",
        "[7m",
        "[12;4H",
        "_ratty;g;d",
        "_ratty;g;s",
    ];

    fn baseline_bytes() -> Vec<u8> {
        baseline_parts().concat()
    }

    /// Offsets between whole baseline pieces — where an escape-sequence-aware
    /// muxer would splice.
    fn safe_offsets() -> Vec<usize> {
        let mut offsets = vec![0usize];
        let mut at = 0;
        for part in baseline_parts() {
            at += part.len();
            offsets.push(at);
        }
        offsets
    }

    /// Offsets strictly INSIDE the baseline's own escape sequences — where a
    /// byte-oblivious muxer would splice.
    fn adversarial_offsets() -> Vec<usize> {
        let bytes = baseline_bytes();
        let mut offsets = BTreeSet::new();
        for (i, byte) in bytes.iter().enumerate() {
            if *byte == 0x1b {
                offsets.insert(i + 1); // between ESC and its introducer
                offsets.insert(i + 2); // inside the sequence body
                if i + 4 <= bytes.len() {
                    offsets.insert(i + 4);
                }
            }
        }
        offsets.into_iter().filter(|o| *o <= bytes.len()).collect()
    }

    /// Splices `frame` into the baseline at every offset in `offsets`.
    fn muxed(frame: &[u8], offsets: &[usize]) -> Vec<u8> {
        let bytes = baseline_bytes();
        let mut out = Vec::new();
        let mut previous = 0usize;
        for offset in offsets {
            out.extend_from_slice(&bytes[previous..*offset]);
            out.extend_from_slice(frame);
            previous = *offset;
        }
        out.extend_from_slice(&bytes[previous..]);
        out
    }

    // ------------------------------------------------------------ mux frame

    /// `ESC _ ratty;m; <pane> ; <payload>` + terminator.
    fn mux_frame(pane: u32, payload: &[u8], c1_terminator: bool) -> Vec<u8> {
        let mut frame = format!("\x1b_ratty;m;{pane};").into_bytes();
        frame.extend_from_slice(payload);
        if c1_terminator {
            frame.push(0x9c);
        } else {
            frame.extend_from_slice(b"\x1b\\");
        }
        frame
    }

    fn base64(data: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for group in data.chunks(3) {
            let bytes = [
                group[0],
                *group.get(1).unwrap_or(&0),
                *group.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
            out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
            out.push(if group.len() > 1 {
                ALPHABET[((n >> 6) & 63) as usize] as char
            } else {
                '='
            });
            out.push(if group.len() > 2 {
                ALPHABET[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    // -------------------------------------------------------------- oracle

    #[derive(Default, Clone)]
    struct Outcome {
        tearings: usize,
        mismatches: usize,
        leaked: usize,
        lost: usize,
        mangled: usize,
        first_example: Option<String>,
    }

    struct Case {
        name: String,
        frame: Vec<u8>,
        /// Substrings that, if they appear on pane 0, are mux bytes.
        needles: Vec<String>,
    }

    /// Compares one muxed run against the baseline. Returns `None` when the
    /// screens are byte-identical, i.e. nothing about pane 0 changed.
    fn compare(
        base: &(String, String),
        got: &(String, String),
        needles: &[String],
        label: &str,
    ) -> Option<(bool, bool, bool, String)> {
        if base.0 == got.0 && base.1 == got.1 {
            return None;
        }
        let leaked = needles
            .iter()
            .any(|needle| got.0.contains(needle.as_str()) && !base.0.contains(needle.as_str()));
        let lost = BASELINE_TOKENS
            .iter()
            .any(|token| base.0.contains(token) && !got.0.contains(token));
        let missing: Vec<&str> = BASELINE_TOKENS
            .iter()
            .copied()
            .filter(|token| base.0.contains(token) && !got.0.contains(token))
            .collect();
        let found: Vec<&str> = needles
            .iter()
            .map(String::as_str)
            .filter(|needle| got.0.contains(needle) && !base.0.contains(needle))
            .collect();
        let mangled: Vec<&str> = PANE0_ESCAPE_BODIES
            .iter()
            .copied()
            .filter(|body| got.0.contains(body) && !base.0.contains(body))
            .collect();
        let kind = if base.0 != got.0 { "TEXT" } else { "ATTR-ONLY" };
        let detail = format!(
            "{label}: {kind} divergence; pane-0 tokens lost {missing:?}; mux needles on pane 0 {found:?}; \
             pane-0 escapes printed as text {mangled:?}\n\
             \x20   baseline text : {:?}\n\x20   muxed    text : {:?}",
            squash(&base.0),
            squash(&got.0),
        );
        Some((leaked, lost, !mangled.is_empty(), detail))
    }

    /// Collapses a screen dump to one line of non-blank content.
    fn squash(screen: &str) -> String {
        let joined = screen
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" | ");
        if joined.chars().count() > 300 {
            format!("{}…", joined.chars().take(300).collect::<String>())
        } else {
            joined
        }
    }

    fn record(outcome: &mut Outcome, verdict: Option<(bool, bool, bool, String)>) {
        outcome.tearings += 1;
        if let Some((leaked, lost, mangled, detail)) = verdict {
            outcome.mismatches += 1;
            if leaked {
                outcome.leaked += 1;
            }
            if lost {
                outcome.lost += 1;
            }
            if mangled {
                outcome.mangled += 1;
            }
            if outcome.first_example.is_none() {
                outcome.first_example = Some(detail);
            }
        }
    }

    /// Every tearing strategy the charter asks for, over one muxed stream.
    fn tear_all(stream: &[u8], base: &(String, String), needles: &[String]) -> Outcome {
        let mut outcome = Outcome::default();

        // 0-way: the whole stream in one write.
        record(
            &mut outcome,
            compare(base, &feed(&[stream]), needles, "whole"),
        );

        // 2-way: the chunk boundary at EVERY byte offset.
        for k in 0..=stream.len() {
            let got = feed(&[&stream[..k], &stream[k..]]);
            record(
                &mut outcome,
                compare(base, &got, needles, &format!("2-way@{k}")),
            );
        }

        // 3-way: sampled pairs of boundaries.
        for i in (0..stream.len()).step_by(29) {
            for j in (i..=stream.len()).step_by(31) {
                let got = feed(&[&stream[..i], &stream[i..j], &stream[j..]]);
                record(
                    &mut outcome,
                    compare(base, &got, needles, &format!("3-way@{i},{j}")),
                );
            }
        }

        // 1-byte drip: the pathological chunking, one write per byte.
        let drip: Vec<&[u8]> = (0..stream.len()).map(|i| &stream[i..i + 1]).collect();
        record(
            &mut outcome,
            compare(base, &feed(&drip), needles, "1-byte-drip"),
        );

        outcome
    }

    fn cases() -> Vec<Case> {
        let ascii = b"MUXPAYLOAD pane one says hi MUXTAIL".to_vec();
        // U+201C LEFT DOUBLE QUOTATION MARK is E2 80 9C - it carries a 0x9c
        // byte, which `apc_end` treats as a bare C1 ST terminator.
        let smart = "MUXPAYLOAD \u{201C}smart\u{201D} quotes MUXTAIL"
            .as_bytes()
            .to_vec();
        // Realistic raw pane-1 traffic: another terminal's output, escapes
        // and all.
        let ansi = b"MUXPAYLOAD \x1b[31mred\x1b[0m \x1b[2J MUXTAIL".to_vec();
        // The two hazards combined: a smart quote (embedded 0x9c, which
        // `apc_end` mistakes for a C1 ST) followed by real pane-1 escapes.
        let smart_ansi = "MUXPAYLOAD \u{201C}q\u{201D} \u{1b}[31mred\u{1b}[0m MUXTAIL"
            .as_bytes()
            .to_vec();
        // Pane-1 content that happens to contain a literal ST.
        let inner_st = b"MUXPAYLOAD\x1b\\MUXTAIL".to_vec();

        let mut cases = Vec::new();
        for (terminator_name, c1) in [("ESC-backslash", false), ("bare-0x9c", true)] {
            for (encoding, payload) in [
                ("base64(ascii)", base64(&ascii).into_bytes()),
                ("base64(ansi)", base64(&ansi).into_bytes()),
                ("raw-ascii", ascii.clone()),
                ("raw-utf8-smartquote", smart.clone()),
                ("raw-smartquote+ansi", smart_ansi.clone()),
                ("raw-ansi", ansi.clone()),
                ("raw-inner-ST", inner_st.clone()),
            ] {
                let needles = vec![
                    "MUXPAYLOAD".to_string(),
                    "MUXTAIL".to_string(),
                    "ratty;m".to_string(),
                    String::from_utf8_lossy(&payload).into_owned(),
                ];
                cases.push(Case {
                    name: format!("{encoding} / {terminator_name}"),
                    frame: mux_frame(1, &payload, c1),
                    needles,
                });
            }
        }

        // Back-to-back frames with no separator.
        let back_to_back = {
            let mut f = mux_frame(1, base64(&ascii).as_bytes(), false);
            f.extend_from_slice(&mux_frame(2, base64(&ascii).as_bytes(), false));
            f
        };
        cases.push(Case {
            name: "base64 x2 back-to-back / ESC-backslash".to_string(),
            frame: back_to_back,
            needles: vec!["MUXPAYLOAD".into(), "ratty;m".into(), base64(&ascii)],
        });

        // Near miss: one character off the claimed prefix. Still an APC.
        cases.push(Case {
            name: "near-miss prefix rattyX;m / ESC-backslash".to_string(),
            frame: b"\x1b_rattyX;m;1;MUXPAYLOAD near miss MUXTAIL\x1b\\".to_vec(),
            needles: vec!["MUXPAYLOAD".into(), "rattyX".into(), "MUXTAIL".into()],
        });

        cases
    }

    // ------------------------------------------------------- the main fuzz

    #[test]
    fn spike55_item3_pane0_corruption_report() {
        let base = feed(&[&baseline_bytes()[..]]);
        assert!(
            BASELINE_TOKENS.iter().all(|t| base.0.contains(t)),
            "the baseline itself must render every token; got {:?}",
            squash(&base.0),
        );
        assert!(
            !base.0.contains("ratty"),
            "the baseline's own RGP APCs must be claimed, not echoed; got {:?}",
            squash(&base.0),
        );

        let families: [(&str, Vec<usize>); 2] = [
            (
                "SAFE (frames spliced between whole pane-0 sequences)",
                safe_offsets(),
            ),
            (
                "ADVERSARIAL (frames spliced INSIDE pane-0 escape sequences)",
                adversarial_offsets(),
            ),
        ];

        println!("\n================ spike #55 item 3: pane-0 corruption fuzz ================");
        println!("baseline: {} bytes, screen 24x80", baseline_bytes().len());
        println!("oracle  : differential — vt100 contents() AND contents_formatted()\n");

        let mut grand_tearings = 0usize;
        let mut grand_leaked = 0usize;
        let mut grand_lost = 0usize;
        let mut grand_mangled = 0usize;

        for (family, offsets) in &families {
            println!("---- injection family: {family}");
            println!("     splice offsets: {} per stream\n", offsets.len());
            for case in cases() {
                let stream = muxed(&case.frame, offsets);
                let outcome = tear_all(&stream, &base, &case.needles);
                grand_tearings += outcome.tearings;
                grand_leaked += outcome.leaked;
                grand_lost += outcome.lost;
                grand_mangled += outcome.mangled;
                let verdict = if outcome.mismatches == 0 {
                    "PASS"
                } else {
                    "FAIL"
                };
                println!(
                    "  [{verdict}] {:<42} stream {:>5}B  tearings {:>6}  diverged {:>6}  leaked {:>6}  lost {:>6}  mangled {:>6}",
                    case.name,
                    stream.len(),
                    outcome.tearings,
                    outcome.mismatches,
                    outcome.leaked,
                    outcome.lost,
                    outcome.mangled,
                );
                if let Some(example) = &outcome.first_example {
                    println!("        first divergence — {example}");
                }
            }
            println!();
        }

        println!(
            "---- totals: {grand_tearings} tearings, {grand_leaked} leaked, {grand_lost} lost, \
             {grand_mangled} with pane-0 escapes printed as text"
        );
        println!("=========================================================================\n");
    }

    // ------------------------------------------- the specific hypothesis

    /// CHARTER HYPOTHESIS. `apc_end` (src/inline.rs:1160-1174) terminates on a
    /// BARE 0x9c. U+201C LEFT DOUBLE QUOTATION MARK encodes as E2 80 9C, so a
    /// mux frame carrying RAW UTF-8 pane content with a smart quote is cut
    /// short mid-character, and its remainder is handed to the parser as
    /// ordinary bytes. This test isolates that one frame and reports exactly
    /// what pane 0 rendered.
    #[test]
    fn spike55_hypothesis_raw_utf8_0x9c_early_termination() {
        println!("\n==== hypothesis probe: raw UTF-8 mux payload carrying U+201C ====");
        let base = feed(&[&baseline_bytes()[..]]);

        let variants: Vec<(&str, Vec<u8>, bool)> = vec![
            (
                "smart quote only, ESC-backslash terminator",
                "hello \u{201C}quoted\u{201D} world".into(),
                false,
            ),
            (
                "smart quote only, bare 0x9c terminator",
                "hello \u{201C}quoted\u{201D} world".into(),
                true,
            ),
            (
                "smart quote + real pane-1 ANSI, ESC-backslash terminator",
                "hello \u{201C}q\u{201D} \u{1b}[31mLEAKED\u{1b}[0m world".into(),
                false,
            ),
        ];

        let mut any_early = false;
        for (name, payload, c1) in variants {
            assert!(
                payload.windows(3).any(|w| w == [0xe2, 0x80, 0x9c]),
                "the payload must actually carry an embedded 0x9c",
            );
            let frame = mux_frame(1, &payload, c1);
            // Where does `apc_end` think the frame ends? (payload_start = +2)
            let end = apc_end(&frame, 2).expect("the frame terminates somewhere");
            let honest_end = frame.len();
            any_early |= end < honest_end;

            println!("\n-- {name}");
            println!(
                "   frame            : {:?}",
                String::from_utf8_lossy(&frame)
            );
            println!("   frame bytes      : {frame:02x?}");
            println!("   frame length     : {honest_end}");
            println!(
                "   apc_end() says   : {end}   ({} bytes early)",
                honest_end - end
            );
            println!(
                "   consumed as APC  : {:?}",
                String::from_utf8_lossy(&frame[..end])
            );
            println!(
                "   spilled remainder: {:?}",
                String::from_utf8_lossy(&frame[end..])
            );

            // End-to-end consequence, differentially, over every 2-way tear.
            let needles: Vec<String> = vec!["quoted".into(), "LEAKED".into(), "world".into()];
            let stream = muxed(&frame, &safe_offsets());
            let (mut diverged, mut leaked, mut lost) = (0usize, 0usize, 0usize);
            for k in 0..=stream.len() {
                let got = feed(&[&stream[..k], &stream[k..]]);
                if let Some((l, o, _, _)) = compare(&base, &got, &needles, "") {
                    diverged += 1;
                    leaked += usize::from(l);
                    lost += usize::from(o);
                }
            }
            println!(
                "   end-to-end       : {} tearings, {diverged} diverged, {leaked} leaked, {lost} lost",
                stream.len() + 1,
            );
            println!(
                "   pane 0 (muxed)   : {:?}",
                squash(&feed(&[&stream[..]]).0)
            );
        }

        println!("   pane 0 (baseline): {:?}", squash(&base.0));
        println!("=================================================================\n");

        assert!(
            any_early,
            "HYPOTHESIS REFUTED: apc_end consumed the whole frame",
        );
    }

    // ------------------------------------------- adversarial near misses

    #[test]
    fn spike55_near_miss_corpus() {
        let base_text = "ALPHA0 plain\r\n";
        println!("\n---- near-miss corpus (each fed after {base_text:?}) ----");

        let corpus: Vec<(&str, Vec<u8>)> = vec![
            ("dangling ESC _ at end of stream", b"\x1b_".to_vec()),
            ("truncated ESC _ ratt", b"\x1b_ratt".to_vec()),
            ("truncated ESC _ ratty;", b"\x1b_ratty;".to_vec()),
            ("truncated ESC _ ratty;m", b"\x1b_ratty;m".to_vec()),
            (
                "truncated ESC _ ratty;m;1; with payload, no ST",
                b"\x1b_ratty;m;1;MUXPAYLOAD".to_vec(),
            ),
            (
                "literal text, no APC intro",
                b"ratty;m;0;hello MUXPAYLOAD".to_vec(),
            ),
        ];

        for (name, tail) in corpus {
            let mut stream = base_text.as_bytes().to_vec();
            stream.extend_from_slice(&tail);
            let screen = feed(&[&stream]);
            let mut torn_worst = String::new();
            for k in 0..=stream.len() {
                let got = feed(&[&stream[..k], &stream[k..]]);
                if got.0 != screen.0 && torn_worst.is_empty() {
                    torn_worst = format!(" (tear@{k} differs: {:?})", squash(&got.0));
                }
            }
            println!("  {name:<44} pane 0 = {:?}{torn_worst}", squash(&screen.0));
        }

        // The literal-text near miss MUST reach pane 0 untouched.
        let literal = b"ALPHA0 plain\r\nratty;m;0;hello MUXPAYLOAD".to_vec();
        for k in 0..=literal.len() {
            let got = feed(&[&literal[..k], &literal[k..]]);
            assert!(
                got.0.contains("ratty;m;0;hello MUXPAYLOAD"),
                "tear@{k}: a literal, non-APC near miss must reach pane 0 verbatim; got {:?}",
                squash(&got.0),
            );
        }

        // A terminator torn across the chunk boundary (ESC | backslash).
        let frame = mux_frame(1, base64(b"MUXPAYLOAD").as_bytes(), false);
        let mut stream = base_text.as_bytes().to_vec();
        stream.extend_from_slice(&frame);
        stream.extend_from_slice(b"BRAVO1 after\r\n");
        let split = stream.len() - b"BRAVO1 after\r\n".len() - 1; // between ESC and '\'
        assert_eq!(
            stream[split - 1],
            0x1b,
            "the split really is mid-terminator"
        );
        let got = feed(&[&stream[..split], &stream[split..]]);
        println!(
            "  {:<44} pane 0 = {:?}",
            "ST torn across the chunk boundary",
            squash(&got.0)
        );
        assert!(
            got.0.contains("ALPHA0") && got.0.contains("BRAVO1") && !got.0.contains("MUX"),
            "a torn ST must not corrupt pane 0; got {:?}",
            squash(&got.0),
        );
        println!("-------------------------------------------------------------\n");
    }

    // --------------------------------------------------- locked-in verdicts

    /// The shape that SURVIVES: base64 payload, `ESC \` terminator. Zero
    /// divergence across every tearing, when frames are spliced only between
    /// whole pane-0 sequences.
    #[test]
    fn spike55_base64_esc_st_framing_never_touches_pane0() {
        let base = feed(&[&baseline_bytes()[..]]);
        let payload = base64(b"MUXPAYLOAD pane one says hi MUXTAIL");
        let frame = mux_frame(1, payload.as_bytes(), false);
        let stream = muxed(&frame, &safe_offsets());
        let needles = vec!["MUXPAYLOAD".to_string(), "ratty;m".to_string(), payload];

        for k in 0..=stream.len() {
            let got = feed(&[&stream[..k], &stream[k..]]);
            if let Some((_, _, _, detail)) = compare(&base, &got, &needles, &format!("2-way@{k}")) {
                panic!("base64 + ESC-backslash framing corrupted pane 0 — {detail}");
            }
        }
        let drip: Vec<&[u8]> = (0..stream.len()).map(|i| &stream[i..i + 1]).collect();
        if let Some((_, _, _, detail)) = compare(&base, &feed(&drip), &needles, "1-byte-drip") {
            panic!("base64 + ESC-backslash framing corrupted pane 0 under a drip — {detail}");
        }
    }

    /// KILL CONDITION 1. A bare 0x9c terminator destroys pane 0 even when the
    /// payload is pure base64. `apc_end` accepts 0x9c and hands it to vte,
    /// but vte's `SosPmApcString` state does NOT accept it as a terminator —
    /// the parser wedges and swallows pane-0 bytes until the next ESC.
    #[test]
    fn spike55_bare_c1_st_terminator_swallows_pane0() {
        let base = feed(&[&baseline_bytes()[..]]);
        let frame = mux_frame(1, base64(b"MUXPAYLOAD").as_bytes(), true);
        let stream = muxed(&frame, &safe_offsets());
        let got = feed(&[&stream[..]]);

        assert_ne!(
            base.0, got.0,
            "a bare-0x9c mux frame left pane 0 untouched — re-derive the finding",
        );
        let swallowed: Vec<&str> = BASELINE_TOKENS
            .iter()
            .copied()
            .filter(|token| base.0.contains(token) && !got.0.contains(token))
            .collect();
        assert!(
            !swallowed.is_empty(),
            "expected pane-0 text to be swallowed by the wedged parser",
        );
        assert!(
            !got.0.contains("MUX"),
            "the loss is a wedge, not a leak; pane 0 = {:?}",
            squash(&got.0),
        );
    }

    /// KILL CONDITION 2. A RAW (un-encoded) payload carrying real pane-1
    /// terminal output executes against pane 0. The payload's own `ESC`
    /// pulls vte out of the APC string, so the rest of the frame runs as
    /// control sequences and prints as text.
    ///
    /// NOTE ON WORDING: this does NOT make base64 specifically load-bearing.
    /// The invariant it proves is narrower — the payload alphabet must
    /// exclude `ESC` (0x1b), `0x9c`, `CAN` (0x18) and `SUB` (0x1a). base64,
    /// base64url and hex all satisfy it.
    #[test]
    fn spike55_raw_payload_executes_against_pane0() {
        let base = feed(&[&baseline_bytes()[..]]);
        let payload = b"BEFORE \x1b[2J LEAKED";
        let frame = mux_frame(1, payload, false);
        let stream = muxed(&frame, &safe_offsets());
        let got = feed(&[&stream[..]]);

        assert!(
            got.0.contains("LEAKED"),
            "expected raw mux payload text on pane 0; pane 0 = {:?}",
            squash(&got.0),
        );
        assert!(
            BASELINE_TOKENS.iter().all(|token| !got.0.contains(token)),
            "expected the payload's ED to have cleared pane 0; pane 0 = {:?}",
            squash(&got.0),
        );
        assert!(
            base.0 != got.0,
            "the baseline must not already look like this",
        );
    }

    // ==================================================================
    // SPIKE #55 ITEM 1 — THE BYTE-LEVEL ORACLE
    //
    // The charter's literal standard is BYTE-level: "zero bytes may ever
    // leak into pane 0" (docs/research/browser-story.md:507). Everything
    // above is a SCREEN-level oracle, which is strictly weaker: it cannot
    // see bytes that reach vte and are swallowed inside vte's APC state
    // machine. `OscGuard::forward` (src/inline.rs:224) now carries a
    // `#[cfg(test)]` tee of the exact bytes handed to
    // `vt100::Parser::process`, so the two standards can be compared.
    // ==================================================================

    /// Screen state AND the exact bytes handed to the vt100 parser.
    fn feed_teed(chunks: &[&[u8]]) -> (String, String, Vec<u8>) {
        let mut parser = new_parser();
        let mut inline = TerminalInlineObjects::default();
        inline.enable_forward_tee();
        for chunk in chunks {
            inline.consume_pty_output(chunk, &mut parser);
        }
        let forwarded = inline.take_forwarded_bytes();
        let screen = parser.screen();
        (
            screen.contents(),
            String::from_utf8_lossy(&screen.contents_formatted()).into_owned(),
            forwarded,
        )
    }

    /// Removes every leftmost non-overlapping occurrence of `needle`.
    fn strip_all(haystack: &[u8], needle: &[u8]) -> Vec<u8> {
        if needle.is_empty() {
            return haystack.to_vec();
        }
        let mut out = Vec::with_capacity(haystack.len());
        let mut i = 0;
        while i < haystack.len() {
            if haystack[i..].starts_with(needle) {
                i += needle.len();
            } else {
                out.push(haystack[i]);
                i += 1;
            }
        }
        out
    }

    #[derive(Default)]
    struct ByteOutcome {
        tearings: usize,
        screen_diverged: usize,
        strict_diverged: usize,
        elided_diverged: usize,
        extra_bytes_max: i64,
        first_elided_example: Option<String>,
    }

    fn byte_compare(
        outcome: &mut ByteOutcome,
        base_screen: &(String, String),
        base_forwarded: &[u8],
        got: &(String, String, Vec<u8>),
        frame: &[u8],
        label: &str,
    ) {
        outcome.tearings += 1;
        if base_screen.0 != got.0 || base_screen.1 != got.1 {
            outcome.screen_diverged += 1;
        }
        if base_forwarded != got.2.as_slice() {
            outcome.strict_diverged += 1;
        }
        let elided = strip_all(&got.2, frame);
        if elided != base_forwarded {
            outcome.elided_diverged += 1;
            outcome.extra_bytes_max = outcome
                .extra_bytes_max
                .max(elided.len() as i64 - base_forwarded.len() as i64);
            if outcome.first_elided_example.is_none() {
                outcome.first_elided_example = Some(format!(
                    "{label}: forwarded-after-eliding-frames ({} B) != baseline forwarded ({} B)",
                    elided.len(),
                    base_forwarded.len(),
                ));
            }
        }
    }

    /// ITEM 1 REPORT. Re-runs every frame shape from `cases()` under the
    /// BYTE-level differential oracle and prints, per shape, how the two
    /// standards disagree.
    ///
    /// Two byte oracles are reported because they answer different
    /// questions:
    ///   * STRICT — the bytes forwarded for the muxed stream must be
    ///     byte-identical to those forwarded for the baseline. This is the
    ///     charter sentence read literally.
    ///   * FRAME-ELIDED — the same, after deleting every verbatim copy of
    ///     the intact mux frame. This asks the narrower question "did
    ///     anything OTHER than an intact frame reach pane 0's parser".
    #[test]
    fn spike55_item1_byte_level_oracle_report() {
        let (base_text, base_fmt, base_forwarded) = feed_teed(&[&baseline_bytes()[..]]);
        let base_screen = (base_text, base_fmt);

        println!("\n============ spike #55 item 1: BYTE-level differential oracle ============");
        println!(
            "baseline: {} stream bytes; {} bytes forwarded to vt100",
            baseline_bytes().len(),
            base_forwarded.len(),
        );
        println!(
            "note    : the baseline's own RGP APCs ARE claimed, so fewer bytes reach vte\n\
             \x20         than were fed in ({} claimed).",
            baseline_bytes().len() - base_forwarded.len(),
        );
        println!("columns : screen = old (weaker) oracle; strict/elided = new byte oracle\n");

        let families: [(&str, Vec<usize>); 2] = [
            ("SAFE", safe_offsets()),
            ("ADVERSARIAL", adversarial_offsets()),
        ];

        for (family, offsets) in &families {
            println!(
                "---- injection family: {family} ({} splices)",
                offsets.len()
            );
            for case in cases() {
                let stream = muxed(&case.frame, offsets);
                let mut outcome = ByteOutcome::default();
                byte_compare(
                    &mut outcome,
                    &base_screen,
                    &base_forwarded,
                    &feed_teed(&[&stream]),
                    &case.frame,
                    "whole",
                );
                for k in 0..=stream.len() {
                    byte_compare(
                        &mut outcome,
                        &base_screen,
                        &base_forwarded,
                        &feed_teed(&[&stream[..k], &stream[k..]]),
                        &case.frame,
                        &format!("2-way@{k}"),
                    );
                }
                let drip: Vec<&[u8]> = (0..stream.len()).map(|i| &stream[i..i + 1]).collect();
                byte_compare(
                    &mut outcome,
                    &base_screen,
                    &base_forwarded,
                    &feed_teed(&drip),
                    &case.frame,
                    "1-byte-drip",
                );

                println!(
                    "  {:<42} tearings {:>5}  screen-diverged {:>5}  BYTE-strict-diverged {:>5}  BYTE-elided-diverged {:>5}",
                    case.name,
                    outcome.tearings,
                    outcome.screen_diverged,
                    outcome.strict_diverged,
                    outcome.elided_diverged,
                );
                if outcome.screen_diverged == 0 && outcome.strict_diverged > 0 {
                    println!(
                        "        ^^ PASSES the screen oracle, FAILS the byte oracle \
                         (mux frame bytes are handed verbatim to vt100 at src/inline.rs:425-427)"
                    );
                }
                if let Some(example) = &outcome.first_elided_example {
                    println!("        first frame-elided divergence — {example}");
                }
            }
            println!();
        }
        println!("=========================================================================\n");
    }

    /// ITEM 1's KEY RESULT, locked in. The one shape the screen oracle
    /// blessed — base64 payload + `ESC \` terminator, spliced only between
    /// whole pane-0 sequences — FAILS the byte-level oracle on every single
    /// tearing.
    ///
    /// CAUSE: `handle_apc_sequence` does not claim `ratty;m` today, so
    /// `consume_pty_output` forwards the WHOLE sequence to the vt100 parser
    /// (src/inline.rs:425-427). Every byte of every mux frame is handed to
    /// pane 0's parser; it is vte's APC state machine, not ratty, that
    /// decides they are harmless. That is exactly the dependency K1 and K2
    /// exploit.
    #[test]
    fn spike55_item1_byte_oracle_fails_the_shape_the_screen_oracle_passed() {
        let (base_text, base_fmt, base_forwarded) = feed_teed(&[&baseline_bytes()[..]]);
        let base_screen = (base_text, base_fmt);
        let payload = base64(b"MUXPAYLOAD pane one says hi MUXTAIL");
        let frame = mux_frame(1, payload.as_bytes(), false);
        let stream = muxed(&frame, &safe_offsets());

        let mut screen_diverged = 0usize;
        let mut strict_diverged = 0usize;
        let mut elided_diverged = 0usize;
        let mut tearings = 0usize;
        for k in 0..=stream.len() {
            let got = feed_teed(&[&stream[..k], &stream[k..]]);
            tearings += 1;
            if got.0 != base_screen.0 || got.1 != base_screen.1 {
                screen_diverged += 1;
            }
            if got.2 != base_forwarded {
                strict_diverged += 1;
            }
            if strip_all(&got.2, &frame) != base_forwarded {
                elided_diverged += 1;
            }
        }

        println!(
            "\nITEM 1 KEY RESULT (base64 + ESC-backslash, SAFE splices): {tearings} tearings, \
             screen-diverged {screen_diverged}, BYTE-strict-diverged {strict_diverged}, \
             BYTE-frame-elided-diverged {elided_diverged}\n"
        );

        assert_eq!(
            screen_diverged, 0,
            "the screen oracle is supposed to bless this shape",
        );
        assert_eq!(
            strict_diverged, tearings,
            "every tearing must fail the strict byte oracle: the frame is forwarded verbatim \
             to vt100 at src/inline.rs:425-427",
        );
        assert_eq!(
            elided_diverged, 0,
            "nothing OTHER than intact frames should reach vt100 for this shape",
        );
    }

    /// MUTATION-DETECTION POWER of the new oracle, item 2 of the charter.
    ///
    /// A verifier proved the SCREEN oracle has zero mutation-detection power
    /// over the chunk-boundary machinery: replacing the body of
    /// `pending_apc_prefix_start` (src/inline.rs:1196-1203) with a plain
    /// `bytes.len()` leaves all crate tests passing while silently breaking
    /// a split-across-the-boundary APC.
    ///
    /// This test pins the byte-level signal that mutation destroys: when the
    /// stream is torn BETWEEN the `ESC` and the `_` of a claimed RGP APC,
    /// the trailing-`ESC` retention keeps the two halves together, so the
    /// sequence is claimed and NOTHING is forwarded. Under the mutation the
    /// lone `ESC` is forwarded to vt100 immediately, so the tee sees an
    /// extra byte that the screen never shows.
    #[test]
    fn spike55_item2_byte_oracle_detects_trailing_esc_retention() {
        // A claimed RGP APC, torn between its ESC and its `_`.
        let stream = b"ALPHA0\r\n\x1b_ratty;g;d\x1b\\BRAVO1\r\n".to_vec();
        let split = stream
            .iter()
            .position(|b| *b == 0x1b)
            .expect("the stream contains an ESC")
            + 1;
        assert_eq!(stream[split - 1], 0x1b, "split really is after the ESC");
        assert_eq!(stream[split], b'_', "split really is before the `_`");

        let whole = feed_teed(&[&stream]);
        let torn = feed_teed(&[&stream[..split], &stream[split..]]);

        println!(
            "\nITEM 2 mutation probe:\n  whole forwarded : {:?}\n  torn  forwarded : {:?}",
            String::from_utf8_lossy(&whole.2),
            String::from_utf8_lossy(&torn.2),
        );

        assert_eq!(
            whole.0, torn.0,
            "the SCREEN is identical either way — this is why the screen oracle is blind",
        );
        assert_eq!(
            whole.2, torn.2,
            "BYTE ORACLE: tearing between ESC and `_` must not change the bytes vt100 sees. \
             If this fails, `pending_apc_prefix_start` (src/inline.rs:1196-1203) stopped \
             retaining the trailing ESC.",
        );
        assert!(
            !whole.2.contains(&0x1b),
            "the claimed RGP APC must contribute no bytes at all to pane 0; got {:?}",
            String::from_utf8_lossy(&whole.2),
        );
    }

    // ==================================================================
    // SPIKE #55 ITEM 3 — KILL CONDITIONS K4..K7, LOCKED IN
    //
    // Each test asserts the CURRENT (broken) behaviour so the evidence is
    // reproducible, and names the production line responsible.
    // ==================================================================

    /// K4 — A LOST TERMINATOR STALLS PANE 0, and it is ENCODING-INDEPENDENT:
    /// this frame is pure base64 with the canonical `ESC \` terminator, and
    /// only the terminator is missing.
    ///
    /// PRODUCTION CAUSE: src/inline.rs:404-417. When `apc_end` finds no
    /// terminator, `consume_pty_output` drains only up to `start` and
    /// RETAINS everything after it in `pending_bytes`, so every subsequent
    /// byte of pane-0 output is buffered and never forwarded. The loss
    /// window is bounded only by `MAX_APC_SEQUENCE_BYTES` = 8 MiB
    /// (src/inline.rs:139), and even at the cap the resync
    /// (`resync_after_overlong_apc`, src/inline.rs:440) keeps discarding
    /// until an `ESC \` or `0x9c` actually appears.
    ///
    /// The existing fuzz cannot see this: every unterminated frame in its
    /// corpus sits at END OF STREAM (src/inline.rs:2733-2800).
    #[test]
    fn spike55_k4_lost_terminator_stalls_pane0() {
        let mut truncated = mux_frame(1, base64(b"MUXPAYLOAD").as_bytes(), false);
        truncated.truncate(truncated.len() - 2); // drop the `ESC \`
        assert!(!truncated.ends_with(b"\x1b\\"));

        let (screen, _, forwarded) = feed_teed(&[
            b"ALPHA0 first\r\n",
            &truncated,
            b"BRAVO1 second\r\n",
            b"CHARLIE2 third\r\n",
        ]);

        println!(
            "\nK4: pane 0 = {:?}\n    forwarded to vt100 = {:?}",
            squash(&screen),
            String::from_utf8_lossy(&forwarded),
        );

        assert!(screen.contains("ALPHA0"), "pre-frame output must survive");
        assert!(
            !screen.contains("BRAVO1") && !screen.contains("CHARLIE2"),
            "K4 DID NOT REPRODUCE: post-frame pane-0 output reached the screen; got {:?}",
            squash(&screen),
        );
        assert!(
            !forwarded.windows(6).any(|w| w == b"BRAVO1"),
            "K4 byte oracle: post-frame bytes must never reach vt100 at all",
        );

        // …and the stall ends only when an ST byte finally appears.
        let (recovered, _, _) = feed_teed(&[
            b"ALPHA0 first\r\n",
            &truncated,
            b"BRAVO1 second\r\n",
            b"\x1b\\",
            b"CHARLIE2 third\r\n",
        ]);
        println!(
            "K4 after a late ST arrives: pane 0 = {:?}",
            squash(&recovered)
        );
        assert!(
            recovered.contains("CHARLIE2"),
            "a later ST should end the stall; got {:?}",
            squash(&recovered),
        );
        assert!(
            !recovered.contains("BRAVO1"),
            "the bytes buffered during the stall are consumed as APC payload, not printed; got {:?}",
            squash(&recovered),
        );
    }

    /// K5 — SPLICING MID-UTF-8-CODEPOINT CORRUPTS PANE 0, even for the
    /// "safe" shape (base64 payload + `ESC \` terminator) and even when the
    /// splice is nowhere near a pane-0 escape sequence.
    ///
    /// PRODUCTION CAUSE: the scanner is byte-oriented with no notion of
    /// UTF-8 (`windows(2)` search at src/inline.rs:375-377), so a frame may
    /// be spliced between the lead byte and the continuation bytes of one
    /// character. vte's UTF-8 decoder then sees `ESC` mid-character and
    /// abandons the partial codepoint.
    ///
    /// The existing corpus cannot express this: `baseline_parts`
    /// (src/inline.rs:2237-2253) is pure ASCII, and `adversarial_offsets`
    /// (src/inline.rs:2291-2304) only emits offsets adjacent to `ESC`.
    #[test]
    fn spike55_k5_mid_utf8_splice_corrupts_pane0() {
        const PREFIX: &[u8] = b"\x1b[2J\x1b[H";
        let text = "ALPHA0 café — naïve “smart” ✓ 日本語 BRAVO1\r\n";
        let mut baseline = PREFIX.to_vec();
        baseline.extend_from_slice(text.as_bytes());

        let frame = mux_frame(1, base64(b"MUXPAYLOAD").as_bytes(), false);
        let base = feed(&[&baseline]);

        let (mut boundary_total, mut boundary_bad) = (0usize, 0usize);
        let (mut mid_total, mut mid_bad) = (0usize, 0usize);
        let mut first_bad = String::new();

        for offset in 0..=text.len() {
            let mut stream = PREFIX.to_vec();
            stream.extend_from_slice(&text.as_bytes()[..offset]);
            stream.extend_from_slice(&frame);
            stream.extend_from_slice(&text.as_bytes()[offset..]);
            let got = feed(&[&stream]);
            let diverged = got.0 != base.0 || got.1 != base.1;
            if text.is_char_boundary(offset) {
                boundary_total += 1;
                boundary_bad += usize::from(diverged);
            } else {
                mid_total += 1;
                mid_bad += usize::from(diverged);
                if diverged && first_bad.is_empty() {
                    first_bad = format!(
                        "offset {offset}: baseline {:?} vs muxed {:?}",
                        squash(&base.0),
                        squash(&got.0),
                    );
                }
            }
        }

        println!(
            "\nK5: splices at CHAR BOUNDARIES {boundary_bad}/{boundary_total} corrupted pane 0; \
             splices MID-CODEPOINT {mid_bad}/{mid_total} corrupted pane 0"
        );
        if !first_bad.is_empty() {
            println!("    first mid-codepoint corruption — {first_bad}");
        }

        assert_eq!(
            boundary_bad, 0,
            "the control must be clean: splicing at a character boundary must not corrupt pane 0",
        );
        assert!(
            mid_bad > 0,
            "K5 DID NOT REPRODUCE: no mid-codepoint splice corrupted pane 0 \
             ({mid_bad}/{mid_total})",
        );
    }

    /// K6 — NESTED frames leak onto pane 0. Only ADJACENT frames were
    /// covered before ("base64 x2 back-to-back", src/inline.rs:2543-2548).
    ///
    /// PRODUCTION CAUSE: `apc_end` (src/inline.rs:1205-1219) scans for the
    /// FIRST terminator and has no notion of a nested APC introducer, so the
    /// outer frame is closed by the INNER frame's terminator and the outer
    /// frame's tail becomes ordinary text on pane 0.
    #[test]
    fn spike55_k6_nested_frames_leak_onto_pane0() {
        let mut nested = b"\x1b_ratty;m;1;QUFB".to_vec(); // outer, unterminated
        nested.extend_from_slice(b"\x1b_ratty;m;2;QkJC\x1b\\"); // inner, terminated
        nested.extend_from_slice(b"NESTEDTAIL"); // outer's tail
        nested.extend_from_slice(b"\x1b\\"); // outer's terminator

        let (screen, _, forwarded) =
            feed_teed(&[b"ALPHA0 first\r\n", &nested, b"BRAVO1 second\r\n"]);

        println!(
            "\nK6: pane 0 = {:?}\n    forwarded to vt100 = {:?}",
            squash(&screen),
            String::from_utf8_lossy(&forwarded),
        );

        assert!(
            screen.contains("NESTEDTAIL"),
            "K6 DID NOT REPRODUCE: the outer frame's tail did not print; pane 0 = {:?}",
            squash(&screen),
        );
    }

    /// K7 — **PRE-EXISTING SHIPPED BUG. NO MUX FRAME IS INVOLVED.** This is
    /// reachable from ordinary terminal output on today's single-pane
    /// product and is logically separate from the #55 mux question.
    ///
    /// PRODUCTION CAUSE: the APC scan at src/inline.rs:375-377 is a raw
    /// `windows(2)` byte search for `ESC _` with NO vt100 state awareness,
    /// so it claims an `ESC _` that occurs inside a pane-0 OSC or DCS
    /// PAYLOAD. `apc_end` then finds no `ESC \` (the OSC ends with BEL), so
    /// the retention path at src/inline.rs:404-417 buffers all subsequent
    /// output forever — the terminal stalls with no mux frame in sight.
    ///
    /// No test stream in the repo contains an OSC or DCS at all.
    #[test]
    fn spike55_k7_shipped_bug_esc_underscore_inside_osc_payload_stalls_terminal() {
        // An ordinary xterm title-set whose title text happens to contain
        // ESC _, terminated the ordinary way with BEL.
        let stream: Vec<u8> = [
            b"A0 before\r\n".as_slice(),
            b"\x1b]0;ti".as_slice(),
            b"\x1b_".as_slice(),
            b"tle".as_slice(),
            b"\x07".as_slice(),
            b"B1 after\r\n".as_slice(),
            b"C2 later\r\n".as_slice(),
        ]
        .concat();

        let (screen, _, forwarded) = feed_teed(&[&stream]);
        println!(
            "\nK7 (no mux frame anywhere): pane 0 = {:?}\n    forwarded to vt100 = {:?}",
            squash(&screen),
            String::from_utf8_lossy(&forwarded),
        );

        assert!(screen.contains("A0"), "output before the OSC must survive");
        assert!(
            !screen.contains("B1") && !screen.contains("C2"),
            "K7 DID NOT REPRODUCE: output after the OSC still reached pane 0; got {:?}",
            squash(&screen),
        );

        // SCOPE, measured rather than assumed. A DCS carrying `ESC _` is
        // ALSO mis-claimed, but a DCS ends with `ESC \`, which `apc_end`
        // accepts, so the fake APC closes at the DCS's own terminator and
        // pane 0 survives. The stall needs a host sequence that does NOT
        // end with `ESC \` or `0x9c` — i.e. a BEL-terminated OSC.
        let dcs: Vec<u8> = [
            b"A0 before\r\n".as_slice(),
            b"\x1bPtmux;\x1b_x\x1b\\".as_slice(),
            b"B1 after\r\n".as_slice(),
        ]
        .concat();
        let (dcs_screen, _, _) = feed_teed(&[&dcs]);
        println!(
            "K7 (DCS + ESC-backslash variant): pane 0 = {:?}",
            squash(&dcs_screen)
        );
        assert!(
            dcs_screen.contains("B1"),
            "an ST-terminated host sequence does NOT stall; got {:?}",
            squash(&dcs_screen),
        );

        // RECOVERY, measured rather than assumed.
        //
        // (a) A later `ESC \` ends the stall: ratty's fake APC closes, and
        //     everything buffered in between is eaten as its payload.
        let mut esc_st_recovery = stream.clone();
        esc_st_recovery.extend_from_slice(b"\x1b\\D3 recovered\r\n");
        let (esc_st_screen, _, _) = feed_teed(&[&esc_st_recovery]);
        println!(
            "K7 recovery via a later ESC-backslash: pane 0 = {:?}",
            squash(&esc_st_screen)
        );
        assert!(
            esc_st_screen.contains("D3"),
            "a later ESC-backslash should end the stall; got {:?}",
            squash(&esc_st_screen),
        );
        assert!(
            !esc_st_screen.contains("B1") && !esc_st_screen.contains("C2"),
            "everything buffered during the stall is eaten as APC payload; got {:?}",
            squash(&esc_st_screen),
        );

        // (b) A later bare `0x9c` (the third byte of U+2018..U+201F — an
        //     ordinary curly quote) ends ratty's stall but does NOT restore
        //     the screen: `apc_end` accepts 0x9c, the unclaimed sequence is
        //     forwarded to vte, and vte's `SosPmApcString` state has no
        //     0x9c arm, so the parser wedges instead. This is K1 firing on
        //     a stream that contains no mux frame at all.
        let mut curly_recovery = stream.clone();
        curly_recovery.extend_from_slice("D3 \u{201c}quoted\u{201d}\r\n".as_bytes());
        curly_recovery.extend_from_slice(b"E4 finally\r\n");
        let (curly_screen, _, _) = feed_teed(&[&curly_recovery]);
        println!(
            "K7 recovery attempt via a curly quote's 0x9c: pane 0 = {:?}",
            squash(&curly_screen)
        );
        assert!(
            !curly_screen.contains("E4"),
            "a bare 0x9c must NOT restore pane 0 — it wedges vte instead (K1); got {:?}",
            squash(&curly_screen),
        );

        // And the control: the identical title WITHOUT the ESC _ is fine.
        let control: Vec<u8> = [
            b"A0 before\r\n".as_slice(),
            b"\x1b]0;title\x07".as_slice(),
            b"B1 after\r\n".as_slice(),
            b"C2 later\r\n".as_slice(),
        ]
        .concat();
        let (control_screen, _, _) = feed_teed(&[&control]);
        println!(
            "K7 control (no ESC _ in the title): pane 0 = {:?}",
            squash(&control_screen)
        );
        assert!(
            control_screen.contains("B1") && control_screen.contains("C2"),
            "the control must be clean; got {:?}",
            squash(&control_screen),
        );
    }

    // ==================================================================
    // SPIKE #55 ITEM 4 — CRATE-SIDE DEMUX CPU
    //
    // NATIVE BUILD ONLY. These absolute numbers DO NOT TRANSFER TO WASM:
    // wasm has no threads and no pipelined rendering (the lesson of #54).
    // Read the RATIO, not the absolutes.
    // ==================================================================

    /// Realistic pane-0 traffic — plain text, SGR runs, EL and CR redraws —
    /// as a list of atomic pieces. The PANE-0 PAYLOAD IS HELD CONSTANT
    /// between the two corpora, so the mux corpus is strictly "the same
    /// work, plus frames" rather than "the same total bytes".
    fn bench_pane0_pieces(target: usize) -> Vec<Vec<u8>> {
        let mut pieces = Vec::new();
        let mut bytes = 0usize;
        let mut n = 0u32;
        while bytes < target {
            for piece in [
                format!("line {n:06} plain text output from a build log\r\n"),
                format!("\x1b[32m  ok\x1b[0m line {n:06} \x1b[1;33mwarning\x1b[m tail\r\n"),
                "\x1b[2Kprogress \x1b[7m####\x1b[27m 42%\r".to_string(),
            ] {
                bytes += piece.len();
                pieces.push(piece.into_bytes());
            }
            n += 1;
        }
        pieces
    }

    /// Splices `frame` after every `every`-th pane-0 piece (0 = no frames)
    /// and cuts the result into 8 KiB PTY-sized reads.
    fn bench_chunks(pieces: &[Vec<u8>], frame: &[u8], every: usize) -> Vec<Vec<u8>> {
        let mut stream = Vec::new();
        for (i, piece) in pieces.iter().enumerate() {
            stream.extend_from_slice(piece);
            if every > 0 && i % every == every - 1 {
                stream.extend_from_slice(frame);
            }
        }
        stream.chunks(8192).map(<[u8]>::to_vec).collect()
    }

    /// Best-of-`runs` wall time for pushing `chunks` through a fresh
    /// scanner + parser.
    fn bench_best_secs(chunks: &[Vec<u8>], runs: usize) -> f64 {
        // Warm up (and prove the corpus parses) before timing.
        {
            let mut parser = new_parser();
            let mut inline = TerminalInlineObjects::default();
            for chunk in chunks.iter().take(16) {
                inline.consume_pty_output(chunk, &mut parser);
            }
        }
        let mut best = f64::MAX;
        for _ in 0..runs {
            let mut parser = new_parser();
            let mut inline = TerminalInlineObjects::default();
            let started = std::time::Instant::now();
            for chunk in chunks {
                std::hint::black_box(
                    inline.consume_pty_output(std::hint::black_box(chunk), &mut parser),
                );
            }
            best = best.min(started.elapsed().as_secs_f64());
        }
        best
    }

    #[test]
    fn spike55_item4_crate_side_demux_throughput() {
        const TARGET: usize = 8 * 1024 * 1024;
        const RUNS: usize = 5;

        let pieces = bench_pane0_pieces(TARGET);
        let pane0_bytes: usize = pieces.iter().map(Vec::len).sum();
        let frame = mux_frame(
            1,
            base64(b"pane one output line with some colour and text").as_bytes(),
            false,
        );

        let variants: [(&str, usize); 3] = [
            ("(i)  pane-0 only, no mux frames", 0),
            ("(ii) + mux frame every 3 pieces", 3),
            ("(ii')+ mux frame every 12 pieces", 12),
        ];

        println!("\n=========== spike #55 item 4: crate-side demux throughput ===========");
        println!(
            "PROFILE: {}",
            if cfg!(debug_assertions) {
                "dev — opt-level=1 for this crate, 3 for deps, debug_assertions ON \
                 (Cargo.toml:129-134)"
            } else {
                "release — opt-level=3, lto=\"fat\", codegen-units=1 (Cargo.toml:136-140)"
            },
        );
        println!("NATIVE build. Absolutes DO NOT transfer to wasm (no threads, no pipelined");
        println!("rendering — the #54 lesson of issue #54). READ THE RATIO, not the MiB/s.");
        println!("Best of {RUNS} runs; identical pane-0 payload ({pane0_bytes} B) in every");
        println!("variant, fed in 8 KiB PTY-sized reads through consume_pty_output into a");
        println!("live 24x80 vt100::Parser. The #[cfg(test)] byte tee is DISABLED here.\n");

        let mut baseline_secs = 0.0f64;
        let mut baseline_ns_stream = 0.0f64;
        for (i, (name, every)) in variants.iter().enumerate() {
            let chunks = bench_chunks(&pieces, &frame, *every);
            let stream_bytes: usize = chunks.iter().map(Vec::len).sum();
            let secs = bench_best_secs(&chunks, RUNS);
            let mibps = (stream_bytes as f64 / (1024.0 * 1024.0)) / secs;
            let ns_stream = secs * 1e9 / stream_bytes as f64;
            let ns_pane0 = secs * 1e9 / pane0_bytes as f64;
            if i == 0 {
                baseline_secs = secs;
                baseline_ns_stream = ns_stream;
            }
            println!(
                "  {name:<34} stream {stream_bytes:>9} B ({:>5.1}% frames)  {secs:>7.4} s  \
                 {mibps:>6.1} MiB/s  {ns_stream:>5.2} ns/stream-B  {ns_pane0:>5.2} ns/pane0-B",
                (stream_bytes - pane0_bytes) as f64 * 100.0 / stream_bytes as f64,
            );
            if i > 0 {
                println!(
                    "        RATIO vs (i): {:.3}x wall time for the SAME pane-0 payload; \
                     {:.3}x cost per stream byte",
                    secs / baseline_secs,
                    ns_stream / baseline_ns_stream,
                );
            }
        }
        println!(
            "\n  READING: the per-stream-byte ratio near or below 1.0 says a mux frame byte is\n\
             \x20          NO MORE expensive than a pane-0 byte (frames are swallowed by vte's\n\
             \x20          APC state; pane-0 bytes reach the grid). The wall-time ratio is the\n\
             \x20          honest cost of carrying pane N in-band on the same stream."
        );
        println!("=====================================================================\n");

        assert!(pane0_bytes > 0);
    }
}
