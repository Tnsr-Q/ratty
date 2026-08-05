//! Direct Vello-to-Bevy texture rendering for the terminal surfaces.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use bevy::asset::RenderAssetUsages;
use bevy::image::ImageSampler;
use bevy::platform::cell::SyncCell;
use bevy::prelude::*;
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::{
    CommandEncoderDescriptor, Extent3d, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};
use bevy::render::renderer::{RenderDevice, RenderGraph, RenderGraphSystems, RenderQueue};
use bevy::render::texture::GpuImage;
use bevy::render::{Extract, ExtractSchedule, RenderApp};
use parley_ratatui::ratatui::buffer::Buffer;
use parley_ratatui::ratatui::layout::Position;
use parley_ratatui::vello::Scene;
use parley_ratatui::vello::peniko::Color as PenikoColor;
use parley_ratatui::{GpuRenderer, TerminalRenderer};

use crate::identity::{TerminalId, TerminalIdentity};

/// Plugin that renders terminal Vello scenes directly into Bevy GPU textures.
pub(crate) struct DirectTerminalRenderPlugin;

impl Plugin for DirectTerminalRenderPlugin {
    fn build(&self, app: &mut App) {
        // The terminal exchange is a per-seat Component since M4.3: no
        // main-world resource, no render-world clone, no shared Arc — a
        // cloned singleton here is exactly #54's one-mailbox trap. The
        // avatar overlay deliberately stays a screen-global resource (one
        // avatar, no wire, no N).
        app.init_resource::<AvatarOverlayExchange>();
        let avatar_exchange = app.world().resource::<AvatarOverlayExchange>().clone();

        let render_app = app.sub_app_mut(RenderApp);
        render_app.init_resource::<DirectTerminalRenderState>();
        render_app.world_mut().insert_resource(avatar_exchange);
        render_app.init_resource::<ExtractedDirectTerminalFrames>();
        render_app.init_resource::<ExtractedAvatarOverlayFrame>();
        render_app.add_systems(
            ExtractSchedule,
            (extract_terminal_frames, extract_avatar_overlay_frame),
        );
        // Render inside the render-graph schedule so Vello's submissions are
        // ordered with the frame's GPU work: `Begin` runs before the camera
        // passes that sample the terminal textures. The avatar overlay
        // shares the one GpuRenderer; N terminal submissions (ascending
        // TerminalId) then the avatar, sequentially, per frame.
        render_app.add_systems(
            RenderGraph,
            (
                render_terminal_frames,
                render_avatar_overlay_frame.after(render_terminal_frames),
            )
                .in_set(RenderGraphSystems::Begin),
        );
    }
}

/// The pair of textures a terminal frame is rendered through: Vello rasterizes
/// into `render` (a plain `Rgba8Unorm` storage texture) and it is copied into
/// `present` (sampled via an sRGB view) for the materials to sample.
pub(crate) struct TerminalImages {
    pub render: Handle<Image>,
    pub present: Handle<Image>,
}

struct DirectTerminalFrame {
    /// Plain `Rgba8Unorm` storage texture Vello rasterizes into.
    render_image: Handle<Image>,
    /// `Rgba8Unorm` texture (sampled via an sRGB view) the render image is
    /// copied into for the materials to sample.
    present_image: Handle<Image>,
    width: u32,
    height: u32,
    base_color: PenikoColor,
    scene: Scene,
}

/// One terminal seat's bounded frame exchange with the render world.
///
/// A Component on the seat since M4.3, born with the seat's dress: one
/// mailbox per terminal. The pre-M4.3 shape — a single Resource slot
/// shared by every publisher — was #54's false-PASS mechanism (N
/// publishers, one slot, one terminal drawn, looks fine). `Clone` clones
/// the `Arc`, NOT the slot: cloning an existing exchange onto a second
/// seat recreates that trap, which is why the seat tests assert slot
/// identity, not just component presence.
#[derive(Component, Clone, Default)]
pub(crate) struct DirectTerminalSceneExchange {
    inner: Arc<DirectTerminalSceneExchangeInner>,
}

#[derive(Default)]
struct DirectTerminalSceneExchangeInner {
    pending: Mutex<Option<DirectTerminalFrame>>,
    recycled: Mutex<Option<Scene>>,
}

/// Render-world Vello renderer state.
///
/// `vello::Renderer` is `Send` but not `Sync`, so the renderer lives in a
/// [`SyncCell`] to qualify as a regular [`Resource`]. A non-send resource
/// would pin [`render_terminal_frame`] to a specific thread, which deadlocks
/// the pipelined render app on its final update during shutdown.
#[derive(Resource)]
struct DirectTerminalRenderState {
    renderer: SyncCell<Option<GpuRenderer>>,
}

impl Default for DirectTerminalRenderState {
    fn default() -> Self {
        Self {
            renderer: SyncCell::new(None),
        }
    }
}

/// One terminal's retained render-world state: its exchange (the
/// recycle-back channel to that seat's mailbox) and the frame retained
/// across render frames until its GPU images are prepared.
struct RetainedTerminalFrame {
    exchange: DirectTerminalSceneExchange,
    frame: Option<DirectTerminalFrame>,
}

/// The render world's keyed retention map, one entry per live terminal.
///
/// A `BTreeMap` so the N Vello submissions order by ascending
/// [`TerminalId`], deterministic across runs — a `HashMap` would make
/// submission order nondeterministic and quietly taint every future N
/// measurement. Keyed on `TerminalId`, never the namespace (#56
/// decision 17: persisted stamps key on `TerminalId`).
#[derive(Resource, Default)]
struct ExtractedDirectTerminalFrames(BTreeMap<TerminalId, RetainedTerminalFrame>);

impl DirectTerminalSceneExchange {
    fn take_recycled_scene(&self) -> Scene {
        self.inner
            .recycled
            .lock()
            .expect("direct terminal recycled scene lock")
            .take()
            .unwrap_or_default()
    }

    fn recycle_scene(&self, mut scene: Scene) {
        scene.reset();

        let mut recycled = self
            .inner
            .recycled
            .lock()
            .expect("direct terminal recycled scene lock");
        if recycled.is_none() {
            *recycled = Some(scene);
        }
    }

    fn publish_frame(&self, frame: DirectTerminalFrame) {
        let previous = self
            .inner
            .pending
            .lock()
            .expect("direct terminal pending frame lock")
            .replace(frame);

        if let Some(previous) = previous {
            self.recycle_scene(previous.scene);
        }
    }

    fn take_pending_frame(&self) -> Option<DirectTerminalFrame> {
        self.inner
            .pending
            .lock()
            .expect("direct terminal pending frame lock")
            .take()
    }

    /// Whether two exchange values share one inner slot. Exists solely to
    /// make the clone-shaped false PASS impossible in tests: inserting
    /// `exchange.clone()` onto a second seat compiles, runs, and passes
    /// every count test while reproducing #54's one-terminal-drawn trap
    /// exactly.
    #[cfg(test)]
    pub(crate) fn same_slot(a: &Self, b: &Self) -> bool {
        Arc::ptr_eq(&a.inner, &b.inner)
    }

    /// Test-only publish of a minimal frame distinguishable by `width`
    /// (default image handles, empty scene) — the seat tests publish
    /// through the real slot without a renderer.
    #[cfg(test)]
    pub(crate) fn publish_test_frame(&self, width: u32) {
        self.publish_frame(DirectTerminalFrame {
            render_image: Handle::default(),
            present_image: Handle::default(),
            width,
            height: 1,
            base_color: PenikoColor::TRANSPARENT,
            scene: Scene::default(),
        });
    }

    /// Test-only take reporting the pending frame's width, `None` when
    /// the slot is empty.
    #[cfg(test)]
    pub(crate) fn take_pending_width(&self) -> Option<u32> {
        self.take_pending_frame().map(|frame| frame.width)
    }
}

/// Creates the terminal **present** texture that the plane material and sprite
/// sample.
///
/// Vello renders into the separate [`new_terminal_render_image`] storage
/// texture and writes sRGB-encoded, display-ready bytes; those bytes are copied
/// here each frame (same `Rgba8Unorm` format) and sampled through an
/// `Rgba8UnormSrgb` view so they are decoded on sample instead of being
/// re-encoded at the swapchain (which washes out colors). This texture has no
/// `STORAGE_BINDING`, so the sRGB view is valid on every backend — wgpu rejects
/// an sRGB view of a storage texture. The data is zero-filled so a frame
/// sampled before the first copy shows transparent black rather than
/// uninitialized memory.
pub(crate) fn new_terminal_image(width: u32, height: u32, label: &'static str) -> Image {
    let mut image = Image::new_fill(
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &[0, 0, 0, 0],
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
    );
    image.texture_descriptor.label = Some(label);
    image.texture_descriptor.usage = TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST;
    image.texture_descriptor.view_formats = &[TextureFormat::Rgba8UnormSrgb];
    image.texture_view_descriptor = Some(TextureViewDescriptor {
        format: Some(TextureFormat::Rgba8UnormSrgb),
        ..Default::default()
    });
    // Nearest (point) filtering: the texture is authored at physical resolution
    // and the flat 2D view presents it 1:1 via `textureLoad` (no sampler), so the
    // 3D plane modes use point sampling too for a consistent, crisp pixel grid.
    // Cell-edge seams are prevented at authoring time in parley_ratatui
    // (pixel-snapped cell fills), not by linear blending.
    image.sampler = ImageSampler::nearest();
    image
}

/// Creates the terminal **render** texture that Vello rasterizes into.
///
/// Vello binds it as a compute storage target, so it must be a plain
/// `Rgba8Unorm` texture with no sRGB view: wgpu rejects an sRGB view of a
/// storage texture (`STORAGE_BINDING`), which crashes bind-group creation on
/// strict backends. Its sRGB-encoded contents are copied into the
/// [`new_terminal_image`] present texture for sampling, so this texture is only
/// ever a copy source and is never sampled directly.
pub(crate) fn new_terminal_render_image(width: u32, height: u32, label: &'static str) -> Image {
    let mut image = Image::new_fill(
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &[0, 0, 0, 0],
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
    );
    image.texture_descriptor.label = Some(label);
    image.texture_descriptor.usage =
        TextureUsages::STORAGE_BINDING | TextureUsages::COPY_SRC | TextureUsages::COPY_DST;
    image
}

pub(crate) fn resize_terminal_image(image: &mut Image, width: u32, height: u32) {
    if image.texture_descriptor.size.width == width
        && image.texture_descriptor.size.height == height
    {
        return;
    }

    image.resize(Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    });
}

/// Builds and publishes one terminal frame. Chart-family visualizations
/// hand in their vello underlays (pixel rect + resolution-independent
/// ops); they append after the text so charts draw over the cells their
/// anchored footprint covers, exactly like inline objects do.
#[allow(clippy::too_many_arguments)]
pub(crate) fn update_direct_terminal_frame(
    exchange: &DirectTerminalSceneExchange,
    images: TerminalImages,
    terminal_renderer: &mut TerminalRenderer,
    buffer: &Buffer,
    cursor: Option<Position>,
    cursor_visible: bool,
    elapsed_seconds: f32,
    viz_underlays: &[(
        crate::viz_draw::UnderlayRect,
        Vec<crate::viz_draw::VizDrawOp>,
    )],
) {
    let build_scene = exchange.take_recycled_scene();
    let spare_scene = terminal_renderer.replace_scene(build_scene);
    let (width, height) = terminal_renderer.texture_size_for_buffer(buffer);
    let base_color = terminal_renderer.theme().background.to_peniko();
    terminal_renderer.build_scene_with_elapsed(buffer, cursor, cursor_visible, elapsed_seconds);
    let mut scene = terminal_renderer.replace_scene(spare_scene);
    for (rect, ops) in viz_underlays {
        crate::viz_draw::append_viz_underlay(&mut scene, *rect, ops);
    }

    exchange.publish_frame(DirectTerminalFrame {
        render_image: images.render,
        present_image: images.present,
        width,
        height,
        base_color,
        scene,
    });
}

/// Extract-phase body, factored out of the ECS so it is testable without
/// a `RenderApp` (the root CI job runs with no display): for each live
/// seat, drains the published frame into its keyed entry, recycling any
/// displaced scene into THAT seat's exchange; then sweeps entries whose
/// terminal no longer exists (decision 17's corollary: scene-global
/// structures keyed by a terminal are swept on despawn).
///
/// The sweep is load-bearing for GPU memory, not hygiene — retained
/// frames hold strong `Handle<Image>` clones, so an unswept entry keeps a
/// despawned terminal's texture pair alive forever, and that leak renders
/// fine (#54's lesson). A frame published after the last extract before a
/// despawn is dropped with its seat, never rendered: correct, and not to
/// be "fixed" with a global fallback slot — a fallback slot IS the
/// singleton mailbox again.
fn drain_published_frames<'a>(
    live: impl Iterator<Item = (TerminalId, &'a DirectTerminalSceneExchange)>,
    frames: &mut BTreeMap<TerminalId, RetainedTerminalFrame>,
) {
    let mut live_ids = Vec::new();
    for (id, exchange) in live {
        live_ids.push(id);
        let entry = frames.entry(id).or_insert_with(|| RetainedTerminalFrame {
            exchange: exchange.clone(),
            frame: None,
        });
        if let Some(next_frame) = exchange.take_pending_frame()
            && let Some(previous_frame) = entry.frame.replace(next_frame)
        {
            exchange.recycle_scene(previous_frame.scene);
        }
    }
    frames.retain(|id, _| live_ids.contains(id));
}

/// Drains every live seat's mailbox into the keyed retention map. The ECS
/// itself is the registry: a despawned seat simply stops matching the
/// query, so the sweep inside [`drain_published_frames`] needs no
/// parallel bookkeeping.
fn extract_terminal_frames(
    mut frames: ResMut<ExtractedDirectTerminalFrames>,
    seats: Extract<Query<(&TerminalIdentity, &DirectTerminalSceneExchange)>>,
) {
    drain_published_frames(
        seats
            .iter()
            .map(|(identity, exchange)| (identity.id(), exchange)),
        &mut frames.0,
    );
}

fn render_terminal_frames(
    mut state: ResMut<DirectTerminalRenderState>,
    mut frames: ResMut<ExtractedDirectTerminalFrames>,
    gpu_images: Res<RenderAssets<GpuImage>>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
) {
    // BTreeMap iteration is ascending-TerminalId: deterministic submission
    // order with no sort. Frames are self-describing (each carries its own
    // texture pair), so no per-terminal lookup happens here.
    for retained in frames.0.values_mut() {
        let Some(current) = retained.frame.take() else {
            continue;
        };
        // Retain undrawable frames and retry on the next render frame; a newer
        // published frame supersedes a retained one during extraction.
        let (Some(render_image), Some(present_image)) = (
            gpu_images.get(&current.render_image),
            gpu_images.get(&current.present_image),
        ) else {
            debug!("retaining terminal frame: GPU image not yet prepared");
            retained.frame = Some(current);
            continue;
        };

        let render_size = render_image.texture_descriptor.size;
        let present_size = present_image.texture_descriptor.size;
        if render_size.width != current.width
            || render_size.height != current.height
            || present_size.width != current.width
            || present_size.height != current.height
        {
            debug!(
                "retaining terminal frame: render {}x{}, present {}x{}, frame {}x{}",
                render_size.width,
                render_size.height,
                present_size.width,
                present_size.height,
                current.width,
                current.height
            );
            retained.frame = Some(current);
            continue;
        }

        let device = render_device.wgpu_device();
        let renderer = state
            .renderer
            .get()
            .get_or_insert_with(|| GpuRenderer::new(device).expect("vello renderer"));

        // Vello rasterizes into the plain `Rgba8Unorm` storage texture; its default
        // view is storage-compatible (no sRGB reinterpretation).
        renderer
            .render_scene_to_texture_view(
                device,
                &render_queue,
                &render_image.texture_view,
                current.width,
                current.height,
                current.base_color,
                &current.scene,
            )
            .expect("render terminal scene into Bevy texture");

        // Copy the rendered, sRGB-encoded bytes into the present texture, which the
        // materials sample through an `Rgba8UnormSrgb` view to decode them. Both
        // textures are `Rgba8Unorm`, so this is a plain same-format texel copy.
        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
            label: Some("terminal present copy"),
        });
        encoder.copy_texture_to_texture(
            render_image.texture.as_image_copy(),
            present_image.texture.as_image_copy(),
            Extent3d {
                width: current.width,
                height: current.height,
                depth_or_array_layers: 1,
            },
        );
        render_queue.submit([encoder.finish()]);

        // Recycle into THIS seat's exchange — the retained entry is the
        // recycle-back channel.
        retained.exchange.recycle_scene(current.scene);
    }
}

// ── Avatar overlay (#23): a second vello scene, screen-space ──
//
// The bubble/typewriter overlay renders through the exact terminal
// pipeline shape — its own render/present texture pair, the same shared
// GpuRenderer, the same retention-on-mismatch guard — but composites via
// a screen-space quad on a dedicated overlay camera, so it can never
// inherit the plane warp. The scene's base color is always TRANSPARENT.

/// One published avatar overlay frame.
pub(crate) struct AvatarOverlayFrame {
    /// Plain `Rgba8Unorm` storage texture Vello rasterizes into.
    pub(crate) render_image: Handle<Image>,
    /// The present texture the overlay quad samples (sRGB view).
    pub(crate) present_image: Handle<Image>,
    /// Frame extent, physical px.
    pub(crate) width: u32,
    /// Frame extent, physical px.
    pub(crate) height: u32,
    /// The overlay scene (bubble chrome, text, glow).
    pub(crate) scene: Scene,
}

/// Shared bounded exchange between the main world and the render world,
/// the [`DirectTerminalSceneExchange`] shape.
#[derive(Resource, Clone, Default)]
pub(crate) struct AvatarOverlayExchange {
    inner: Arc<AvatarOverlayExchangeInner>,
}

#[derive(Default)]
struct AvatarOverlayExchangeInner {
    pending: Mutex<Option<AvatarOverlayFrame>>,
    recycled: Mutex<Option<Scene>>,
}

impl AvatarOverlayExchange {
    pub(crate) fn take_recycled_scene(&self) -> Scene {
        self.inner
            .recycled
            .lock()
            .expect("avatar overlay recycled scene lock")
            .take()
            .unwrap_or_default()
    }

    fn recycle_scene(&self, mut scene: Scene) {
        scene.reset();
        let mut recycled = self
            .inner
            .recycled
            .lock()
            .expect("avatar overlay recycled scene lock");
        if recycled.is_none() {
            *recycled = Some(scene);
        }
    }

    pub(crate) fn publish_frame(&self, frame: AvatarOverlayFrame) {
        let previous = self
            .inner
            .pending
            .lock()
            .expect("avatar overlay pending frame lock")
            .replace(frame);
        if let Some(previous) = previous {
            self.recycle_scene(previous.scene);
        }
    }

    fn take_pending_frame(&self) -> Option<AvatarOverlayFrame> {
        self.inner
            .pending
            .lock()
            .expect("avatar overlay pending frame lock")
            .take()
    }
}

#[derive(Resource, Default)]
struct ExtractedAvatarOverlayFrame(Option<AvatarOverlayFrame>);

fn extract_avatar_overlay_frame(
    mut frame: ResMut<ExtractedAvatarOverlayFrame>,
    exchange: Extract<Res<AvatarOverlayExchange>>,
) {
    if let Some(next_frame) = exchange.take_pending_frame()
        && let Some(previous_frame) = frame.0.replace(next_frame)
    {
        exchange.recycle_scene(previous_frame.scene);
    }
}

fn render_avatar_overlay_frame(
    mut state: ResMut<DirectTerminalRenderState>,
    exchange: Res<AvatarOverlayExchange>,
    mut frame: ResMut<ExtractedAvatarOverlayFrame>,
    gpu_images: Res<RenderAssets<GpuImage>>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
) {
    let Some(current) = frame.0.take() else {
        return;
    };
    let (Some(render_image), Some(present_image)) = (
        gpu_images.get(&current.render_image),
        gpu_images.get(&current.present_image),
    ) else {
        debug!("retaining avatar overlay frame: GPU image not yet prepared");
        frame.0 = Some(current);
        return;
    };

    let render_size = render_image.texture_descriptor.size;
    let present_size = present_image.texture_descriptor.size;
    if render_size.width != current.width
        || render_size.height != current.height
        || present_size.width != current.width
        || present_size.height != current.height
    {
        debug!(
            "retaining avatar overlay frame: render {}x{}, present {}x{}, frame {}x{}",
            render_size.width,
            render_size.height,
            present_size.width,
            present_size.height,
            current.width,
            current.height
        );
        frame.0 = Some(current);
        return;
    }

    let device = render_device.wgpu_device();
    let renderer = state
        .renderer
        .get()
        .get_or_insert_with(|| GpuRenderer::new(device).expect("vello renderer"));

    renderer
        .render_scene_to_texture_view(
            device,
            &render_queue,
            &render_image.texture_view,
            current.width,
            current.height,
            PenikoColor::TRANSPARENT,
            &current.scene,
        )
        .expect("render avatar overlay scene into Bevy texture");

    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
        label: Some("avatar overlay present copy"),
    });
    encoder.copy_texture_to_texture(
        render_image.texture.as_image_copy(),
        present_image.texture.as_image_copy(),
        Extent3d {
            width: current.width,
            height: current.height,
            depth_or_array_layers: 1,
        },
    );
    render_queue.submit([encoder.finish()]);

    exchange.recycle_scene(current.scene);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::TerminalId;

    #[test]
    fn exchange_last_writer_wins_is_per_terminal() {
        let a = DirectTerminalSceneExchange::default();
        let b = DirectTerminalSceneExchange::default();
        assert!(
            !DirectTerminalSceneExchange::same_slot(&a, &b),
            "two default exchanges are two slots"
        );
        a.publish_test_frame(101);
        a.publish_test_frame(102);
        assert_eq!(
            a.take_pending_width(),
            Some(102),
            "last writer wins within one seat's slot"
        );
        assert!(
            a.inner
                .recycled
                .lock()
                .expect("recycled scene lock")
                .is_some(),
            "the displaced scene came home to A's recycle slot"
        );
        assert!(
            b.inner
                .recycled
                .lock()
                .expect("recycled scene lock")
                .is_none(),
            "recycling never crosses seats"
        );
        assert_eq!(
            a.take_pending_width(),
            None,
            "single-slot semantics per terminal: a second take is empty"
        );
    }

    #[test]
    fn extract_keying_routes_by_terminal_id() {
        let a = DirectTerminalSceneExchange::default();
        let b = DirectTerminalSceneExchange::default();
        let id_a = TerminalId::from_raw(1);
        let id_b = TerminalId::from_raw(2);
        let mut frames = BTreeMap::new();

        a.publish_test_frame(101);
        drain_published_frames([(id_a, &a), (id_b, &b)].into_iter(), &mut frames);
        assert_eq!(frames.len(), 2, "two live seats, two entries — exactly");
        assert_eq!(
            frames[&id_a].frame.as_ref().map(|frame| frame.width),
            Some(101),
            "A's publish landed under A's key"
        );
        assert!(
            frames[&id_b].frame.is_none(),
            "B exists but has not published"
        );

        b.publish_test_frame(202);
        drain_published_frames([(id_a, &a), (id_b, &b)].into_iter(), &mut frames);
        assert_eq!(frames.len(), 2, "still exactly two entries");
        assert_eq!(
            frames[&id_a].frame.as_ref().map(|frame| frame.width),
            Some(101),
            "A's retained frame survives a drain with no new publish (retention)"
        );
        assert_eq!(
            frames[&id_b].frame.as_ref().map(|frame| frame.width),
            Some(202),
            "B's publish landed under B's key"
        );
    }

    #[test]
    fn retained_frames_swept_on_despawn() {
        let a = DirectTerminalSceneExchange::default();
        let b = DirectTerminalSceneExchange::default();
        let id_a = TerminalId::from_raw(1);
        let id_b = TerminalId::from_raw(2);
        let mut frames = BTreeMap::new();
        a.publish_test_frame(101);
        b.publish_test_frame(202);
        drain_published_frames([(id_a, &a), (id_b, &b)].into_iter(), &mut frames);
        assert_eq!(frames.len(), 2, "both seats retained while both live");

        // B despawns: it stops matching the live query, and its entry —
        // holding strong Handle<Image> clones — must go with it.
        drain_published_frames([(id_a, &a)].into_iter(), &mut frames);
        assert_eq!(
            frames.keys().copied().collect::<Vec<_>>(),
            vec![id_a],
            "the dead terminal's entry is swept; exactly the live seat remains"
        );
        assert_eq!(
            frames[&id_a].frame.as_ref().map(|frame| frame.width),
            Some(101),
            "the surviving seat's retained frame is intact"
        );
    }
}
