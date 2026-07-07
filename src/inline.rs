//! Inline object state and APC handling.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use bevy::prelude::*;
use vt100::Callbacks;

use crate::kitty::{KittyOperation, KittyParserState, refresh_kitty_placeholder_anchors};
use crate::model::{
    ObjectLoadOptions, load_object_source_from_bytes_with_options, load_object_source_with_options,
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

        let mut cursor = 0;
        loop {
            let Some(start_offset) = self.pending_bytes[cursor..]
                .windows(APC_START.len())
                .position(|window| window == APC_START)
            else {
                let pending_len = self.pending_bytes.len();
                let keep_from = pending_apc_prefix_start(&self.pending_bytes, cursor);
                if cursor < keep_from {
                    parser.process(&normalize_hvp_sequences(
                        &self.pending_bytes[cursor..keep_from],
                    ));
                }
                if keep_from < pending_len {
                    self.pending_bytes.drain(..keep_from);
                } else {
                    self.pending_bytes.clear();
                }
                return replies;
            };
            let start = cursor + start_offset;
            if cursor < start {
                parser.process(&normalize_hvp_sequences(&self.pending_bytes[cursor..start]));
            }

            let payload_start = start + APC_START.len();
            let Some(end) = apc_end(&self.pending_bytes, payload_start) else {
                self.pending_bytes.drain(..start);
                return replies;
            };
            let sequence = self.pending_bytes[start..end].to_vec();
            let (handled, reply) =
                self.handle_apc_sequence(&sequence, parser.screen().cursor_position());
            if let Some(reply) = reply {
                replies.push(reply);
            }
            if !handled {
                parser.process(&sequence);
            }
            cursor = end;
        }
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
    }

    fn remove_object(&mut self, object_id: u32) {
        self.objects.remove(&object_id);
        self.anchors.remove(&object_id);
        self.pending_rgp_payloads.remove(&object_id);
        self.dirty = true;
    }

    fn clear_objects(&mut self) {
        self.objects.clear();
        self.anchors.clear();
        self.pending_rgp_payloads.clear();
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
            KittyOperation::TransmitOnly { object_id, image } => {
                self.objects
                    .insert(object_id, InlineObject::KittyImage(image.rasterize()));
                self.dirty = true;
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
                            match load_object_source_with_options(Path::new(&path), load_options) {
                                Ok((source, source_data)) => {
                                    info!("registered RGP object {} from {}", object_id, source);
                                    self.objects.insert(object_id, source_data.into());
                                    self.dirty = true;
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
                if let Some(anchor) = self.anchors.get_mut(&object_id) {
                    let needs_rebuild = update.depth.is_some();
                    let needs_restyle = update.color.is_some() || update.brightness.is_some();
                    apply_rgp_update(&mut anchor.style, update);
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
}
