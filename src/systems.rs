//! Runtime Bevy systems for terminal presentation.
//!
//! These systems are scheduled from [`crate::plugin::TerminalPlugin`] in a mostly linear flow:
//!
//! - [`pump_pty_output`]
//! - [`crate::keyboard::handle_keyboard_input`]
//! - [`crate::mouse::handle_mouse_input`]
//! - [`handle_window_resize`]
//! - [`apply_rgp_stage`]
//! - [`animate_stage_tween`]
//! - [`crate::scene::apply_terminal_presentation`]
//! - [`apply_inline_objects`]
//! - [`render_terminal_widget`]
//! - [`sync_inline_objects`]
//! - [`animate_inline_kitty_planes`]
//! - [`sync_rgp_objects`]
//! - [`rebuild_viz_objects`]
//! - [`sync_viz_objects`]
//! - [`animate_viz_effects`]
//! - [`apply_instance_brightness`]
//! - [`animate_terminal_plane_warp`]
//! - [`sync_asset_to_terminal_cursor`]
//!
//! The redraw path updates the terminal texture and presentation state first, then the inline
//! object systems rebuild or reposition scene entities that depend on the terminal grid.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::TryRecvError;

use crate::config::{AppConfig, CURSOR_DEPTH, CursorAnimationConfig};
use crate::direct_render::DirectTerminalSceneExchange;
use crate::inline::{
    InlineAnchor, InlineKittyPlaneLayout, InlineObject, InlineStyle, RgpAnimationState,
    TerminalInlineObjectPlane, TerminalInlineObjectSprite, TerminalInlineObjects,
    TerminalRgpObject,
};
use crate::model::{CursorModel, CursorSettings, spawn_cursor_model};
use crate::mouse::TerminalSelection;
use crate::present::TerminalPresentMaterial;
use crate::rendering::{sync_plane_texture, sync_terminal_debug_image};
use crate::rgp::RgpStageMode;
use crate::runtime::TerminalRuntime;
use crate::scene::{
    MobiusTransition, ModelLoadState, StageChannel, StageTween, TerminalPlane, TerminalPlaneBack,
    TerminalPlaneBackLayoutQuery, TerminalPlaneLayoutQuery, TerminalPlaneMeshes, TerminalPlaneView,
    TerminalPlaneWarp, TerminalPresentation, TerminalPresentationMode, TerminalViewport,
    apply_stage_mode_change, sync_terminal_layout,
};
use crate::terminal::{
    TerminalRedrawState, TerminalSurface, TerminalWidget, render_scale_for_window,
};
use crate::viz::{
    QueuedVizEffect, VIZ_EFFECT_SECONDS, VizChildRecord, VizChildSpec, VizEffectAnim,
    VizEffectKind, VizKeyedItem, VizObjectRoot, VizPaletteSlot, VizRegistry, viz_child_specs,
};
use bevy::app::AppExit;
use bevy::asset::AssetMut;
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::system::SystemParam;
use bevy::gltf::GltfAssetLabel;
use bevy::image::ImageSampler;
use bevy::mesh::{Indices, VertexAttributeValues};
use bevy::prelude::*;
use bevy::render::render_resource::PrimitiveTopology;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::window::{PrimaryWindow, Window, WindowCloseRequested, WindowResized};

struct InlineLayout {
    columns: u32,
    rows: u32,
    center_x: f32,
    center_y: f32,
    local_x: f32,
    local_y: f32,
    local_width: f32,
    local_height: f32,
    pixel_width: f32,
    pixel_height: f32,
}

struct KittyRenderContext<'a> {
    mode: TerminalPresentationMode,
    warp_amount: f32,
    elapsed_secs: f32,
    materials: &'a mut Assets<StandardMaterial>,
    images: &'a mut Assets<Image>,
    meshes: &'a mut Assets<Mesh>,
    plane_children: &'a mut Vec<Entity>,
}

struct CursorPoseContext<'a, 'w, 's> {
    runtime: &'a TerminalRuntime,
    terminal: &'a TerminalSurface,
    viewport: &'a TerminalViewport,
    mode: TerminalPresentationMode,
    plane_warp_amount: f32,
    mobius_progress: f32,
    elapsed_secs: f32,
    plane_query: &'a Query<'w, 's, &'static Transform, (With<TerminalPlane>, Without<CursorModel>)>,
}

/// Marker for objects that already had instance brightness applied.
#[derive(Component)]
pub struct BrightnessAdjusted;

type PlaneTransformQuery<'w, 's> =
    Query<'w, 's, &'static Transform, (With<TerminalPlane>, Without<TerminalRgpObject>)>;
type CursorTransformQuery<'w, 's> = Query<
    'w,
    's,
    (&'static mut Transform, &'static mut Visibility),
    (With<CursorModel>, Without<TerminalPlane>),
>;

/// Requests application exit as soon as the primary window is asked to close.
pub(crate) fn request_exit_on_primary_window_close(
    mut close_events: MessageReader<WindowCloseRequested>,
    primary_window: Query<Entity, With<PrimaryWindow>>,
    mut app_exit: MessageWriter<AppExit>,
    mut exit_requested: Local<bool>,
) {
    if *exit_requested {
        close_events.clear();
        return;
    }

    let Ok(primary_window) = primary_window.single() else {
        return;
    };

    if close_events
        .read()
        .any(|event| event.window == primary_window)
    {
        *exit_requested = true;
        app_exit.write(AppExit::Success);
    }
}

/// Shuts down the PTY runtime when Bevy begins exiting.
pub(crate) fn shutdown_terminal_runtime_on_exit(
    mut app_exit: MessageReader<AppExit>,
    mut runtime: ResMut<TerminalRuntime>,
    mut shutdown_started: Local<bool>,
) {
    if *shutdown_started {
        app_exit.clear();
        return;
    }

    if app_exit.read().next().is_some() {
        *shutdown_started = true;
        runtime.shutdown();
    }
}

/// Pumps PTY output into the terminal parser.
///
/// This runs early in the update schedule, before [`render_terminal_widget`]. It drains PTY output
/// from [`TerminalRuntime`], feeds it through [`TerminalInlineObjects::consume_pty_output`] and
/// requests a redraw through [`TerminalRedrawState`] when terminal state changed.
///
/// It also updates scroll-coupled inline anchors before the redraw and sync passes rebuild the
/// scene.
pub fn pump_pty_output(
    mut runtime: ResMut<TerminalRuntime>,
    mut inline_objects: ResMut<TerminalInlineObjects>,
    mut viz_registry: ResMut<crate::viz::VizRegistry>,
    mut app_exit: MessageWriter<AppExit>,
    mut ai_commands: MessageWriter<crate::ai::AiCommand>,
    mut queries: MessageWriter<crate::query_channel::QueryRequest>,
    mut redraw: ResMut<TerminalRedrawState>,
) {
    let screen_rows = |screen: &vt100::Screen| {
        let (_, cols) = screen.size();
        screen.rows(0, cols).collect::<Vec<_>>()
    };

    let mut processed_output = false;
    loop {
        match runtime.try_recv() {
            Ok(chunk) => {
                // Viz anchors always scroll with text, so any anchored
                // visualization keeps scroll inference alive too.
                let track_scroll =
                    inline_objects.has_scroll_tracked_anchors() || viz_registry.has_anchors();
                let prev_rows: Option<Vec<String>> = if track_scroll {
                    let (_, cols) = runtime.parser.screen().size();
                    Some(runtime.parser.screen().rows(0, cols).collect::<Vec<_>>())
                } else {
                    None
                };
                let mut replies = inline_objects.consume_pty_output(&chunk, &mut runtime.parser);
                replies.extend(runtime.parser.callbacks_mut().take_replies());
                for reply in replies {
                    runtime.write_input(&reply);
                }
                for (source, ack_token, command) in
                    runtime.parser.callbacks_mut().take_ai_commands()
                {
                    ai_commands.write(crate::ai::AiCommand {
                        source,
                        ack_token,
                        command,
                    });
                }
                for (source, envelope) in runtime.parser.callbacks_mut().take_queries() {
                    queries.write(crate::query_channel::QueryRequest {
                        source,
                        item: crate::query_channel::QueryItem::Query(envelope),
                    });
                }
                for (source, error) in runtime.parser.callbacks_mut().take_wire_errors() {
                    queries.write(crate::query_channel::QueryRequest {
                        source,
                        item: crate::query_channel::QueryItem::Error(error),
                    });
                }
                if let Some(prev_rows) = prev_rows {
                    let next_rows = screen_rows(runtime.parser.screen());
                    let scrolled = infer_upward_scroll(&prev_rows, &next_rows);
                    inline_objects.apply_scroll(scrolled);
                    viz_registry.apply_scroll(scrolled);
                }
                inline_objects.refresh_placeholder_anchors(runtime.parser.screen());
                processed_output = true;
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                if !runtime.pty_disconnected {
                    runtime.pty_disconnected = true;
                    app_exit.write(AppExit::Success);
                }
                break;
            }
        }
    }

    if processed_output {
        redraw.request();
    }
}

fn infer_upward_scroll(prev_rows: &[String], next_rows: &[String]) -> u16 {
    // Identical screens are no scroll. Without this, a fully blank screen
    // matches itself at every shift and the loop below infers a huge
    // spurious scroll, evicting every scroll-tracked anchor the moment any
    // output chunk arrives.
    if prev_rows == next_rows {
        return 0;
    }
    let max_shift = prev_rows.len().min(next_rows.len());
    for shift in (1..max_shift).rev() {
        if prev_rows
            .iter()
            .skip(shift)
            .zip(next_rows.iter())
            .all(|(prev, next)| prev == next)
        {
            return shift as u16;
        }
    }
    0
}

#[derive(SystemParam)]
pub(crate) struct ResizeParams<'w, 's> {
    primary_window: Query<'w, 's, (Entity, &'static Window), With<PrimaryWindow>>,
    runtime: ResMut<'w, TerminalRuntime>,
    terminal: ResMut<'w, TerminalSurface>,
    redraw: ResMut<'w, TerminalRedrawState>,
    viewport: ResMut<'w, TerminalViewport>,
    plane_query: TerminalPlaneLayoutQuery<'w, 's>,
    plane_back_query: TerminalPlaneBackLayoutQuery<'w, 's>,
}

/// Handles primary window resize events.
///
/// This updates both the PTY grid and the rendered scene dimensions. It resizes
/// [`TerminalRuntime`], [`TerminalSurface`], [`TerminalViewport`], the 2D terminal sprite and the
/// front and back terminal plane transforms.
///
/// The redraw system runs after this system and uploads the resized terminal image in the same
/// frame.
pub(crate) fn handle_window_resize(
    mut resize_events: MessageReader<WindowResized>,
    mut params: ResizeParams,
) {
    let ResizeParams {
        primary_window,
        runtime,
        terminal,
        redraw,
        viewport,
        plane_query,
        plane_back_query,
    } = &mut params;
    let Ok((primary_window, window)) = primary_window.single() else {
        return;
    };

    let mut latest_size = None;
    for event in resize_events.read() {
        if event.window == primary_window {
            latest_size = Some(Vec2::new(event.width, event.height));
        }
    }

    let Some(window_size) = latest_size else {
        return;
    };

    // Minimizing the window reports a 0x0 size. Skip it so the terminal keeps
    // its last good grid instead of collapsing to a degenerate size that the
    // vt100 parser can't safely process.
    if window_size.x < 1.0 || window_size.y < 1.0 {
        return;
    }

    let window_size = window_size.max(Vec2::ONE);
    let layout = terminal.resize_to_fit(window_size, render_scale_for_window(window));
    let pty_pixels = layout.pty_pixels();
    runtime.resize(
        layout.cols,
        layout.rows,
        pty_pixels.x as u16,
        pty_pixels.y as u16,
    );
    sync_terminal_layout(layout, viewport, plane_query, plane_back_query);
    redraw.request();
}

/// Applies inline object visibility for the current presentation mode.
///
/// This runs after [`crate::scene::apply_terminal_presentation`] and only flips scene visibility.
/// [`TerminalInlineObjectSprite`] entities are shown in [`TerminalPresentationMode::Flat2d`], while
/// [`TerminalInlineObjectPlane`] entities are shown in the 3D presentation modes.
pub fn apply_inline_objects(
    presentation: Res<TerminalPresentation>,
    mut sprite_query: Query<&mut Visibility, With<TerminalInlineObjectSprite>>,
    mut plane_query: Query<
        &mut Visibility,
        (
            With<TerminalInlineObjectPlane>,
            Without<TerminalInlineObjectSprite>,
        ),
    >,
) {
    let sprite_visibility = match presentation.mode {
        TerminalPresentationMode::Flat2d => Visibility::Visible,
        TerminalPresentationMode::Plane3d | TerminalPresentationMode::Mobius3d => {
            Visibility::Hidden
        }
    };
    let plane_visibility = match presentation.mode {
        TerminalPresentationMode::Flat2d => Visibility::Hidden,
        TerminalPresentationMode::Plane3d | TerminalPresentationMode::Mobius3d => {
            Visibility::Visible
        }
    };

    for mut visibility in &mut sprite_query {
        *visibility = sprite_visibility;
    }
    for mut visibility in &mut plane_query {
        *visibility = plane_visibility;
    }
}

/// Redraw system parameters.
/// Tracks whether the terminal frame was redrawn during the current update.
#[derive(Resource, Default)]
pub(crate) struct TerminalFrameDirty(pub bool);

/// Ordered terminal redraw pipeline:
/// [`render_terminal_widget`] → [`sync_terminal_materials`] →
/// [`finish_terminal_model_load`].
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TerminalRedrawSet;

/// Half-period of the fastest blink cadence the renderer supports (rapid
/// blink); slow blink (0.5s) is a multiple of it.
const BLINK_TICK_SECS: f32 = 0.25;

#[derive(SystemParam)]
pub(crate) struct RenderWidgetParams<'w, 's> {
    app_config: Res<'w, AppConfig>,
    cursor_settings: Res<'w, CursorSettings>,
    runtime: Res<'w, TerminalRuntime>,
    terminal: ResMut<'w, TerminalSurface>,
    selection: Res<'w, TerminalSelection>,
    time: Res<'w, Time>,
    redraw: ResMut<'w, TerminalRedrawState>,
    images: ResMut<'w, Assets<Image>>,
    direct_render: Res<'w, DirectTerminalSceneExchange>,
    model_load_state: Res<'w, ModelLoadState>,
    frame_dirty: ResMut<'w, TerminalFrameDirty>,
    blink_phase: Local<'s, u64>,
}

/// Redraws the Ratatui buffer and publishes the rendered terminal frame.
///
/// This runs after [`pump_pty_output`] and [`crate::mouse::handle_mouse_input`]. It records
/// whether the frame changed in [`TerminalFrameDirty`] so the rest of [`TerminalRedrawSet`]
/// can skip its work on clean frames.
pub(crate) fn render_terminal_widget(mut params: RenderWidgetParams) {
    let RenderWidgetParams {
        app_config,
        cursor_settings,
        runtime,
        terminal,
        selection,
        time,
        redraw,
        images,
        direct_render,
        model_load_state,
        frame_dirty,
        blink_phase,
    } = &mut params;
    let needs_redraw = redraw.take();
    // The texture content only changes with terminal state or blink phase;
    // warp and camera animations are mesh- and camera-side. Rebuilding on
    // blink ticks instead of every frame keeps idle scene builds at 4Hz.
    let phase = (time.elapsed_secs() / BLINK_TICK_SECS) as u64;
    let blink_ticked = **blink_phase != phase;
    **blink_phase = phase;
    frame_dirty.0 = needs_redraw || blink_ticked || !model_load_state.loaded;
    if !frame_dirty.0 {
        return;
    }

    let screen = runtime.parser.screen();
    let _ = terminal.tui.draw(|frame| {
        frame.render_widget(
            TerminalWidget {
                screen,
                selection,
                theme: &app_config.theme,
                font_style: app_config.font.style,
            },
            frame.area(),
        );

        // Draw the block cursor only when the 3D model is not showing it, so
        // the two never both appear (or both vanish). This reads the live
        // cursor state, which the `cursor` command can toggle at runtime.
        if !cursor_settings.visible && !screen.hide_cursor() {
            let (cursor_row, cursor_col) = screen.cursor_position();
            frame.set_cursor_position((cursor_col, cursor_row));
        }
    });

    let _ = terminal.sync_image(images, direct_render, time.elapsed_secs());
}

#[derive(SystemParam)]
pub(crate) struct SyncMaterialsParams<'w, 's> {
    runtime: Res<'w, TerminalRuntime>,
    terminal: Res<'w, TerminalSurface>,
    presentation: Res<'w, TerminalPresentation>,
    images: ResMut<'w, Assets<Image>>,
    materials: ResMut<'w, Assets<StandardMaterial>>,
    plane_materials: Query<'w, 's, &'static MeshMaterial3d<StandardMaterial>, With<TerminalPlane>>,
    plane_back_materials:
        Query<'w, 's, &'static MeshMaterial3d<StandardMaterial>, With<TerminalPlaneBack>>,
    present_materials: ResMut<'w, Assets<TerminalPresentMaterial>>,
    present_query: Query<'w, 's, &'static MeshMaterial2d<TerminalPresentMaterial>>,
    frame_dirty: Res<'w, TerminalFrameDirty>,
}

/// Refreshes the debug back texture and plane materials after a redraw.
pub(crate) fn sync_terminal_materials(mut params: SyncMaterialsParams) {
    let SyncMaterialsParams {
        runtime,
        terminal,
        presentation,
        images,
        materials,
        plane_materials,
        plane_back_materials,
        present_materials,
        present_query,
        frame_dirty,
    } = &mut params;
    if !frame_dirty.0 {
        return;
    }

    // The present texture's GpuImage is recreated when the terminal resizes (window
    // resize / font zoom), which invalidates the 2D present material's cached bind
    // group. Writing the texture handle — not merely touching the asset with
    // `get_mut` — advances the material's change tick so Bevy re-prepares the bind
    // group against the current GpuImage; a no-op touch leaves the quad sampling a
    // stale texture and the flat view freezes. Matches the plane handling.
    if let Some(present_image) = terminal.image_handle.as_ref() {
        for present_handle in present_query.iter() {
            if let Some(mut material) = present_materials.get_mut(&present_handle.0) {
                material.texture = present_image.clone();
            }
        }
    }

    let in_3d = matches!(
        presentation.mode,
        TerminalPresentationMode::Plane3d | TerminalPresentationMode::Mobius3d
    );
    if in_3d {
        sync_terminal_debug_image(terminal, images, runtime.parser.screen());
    }

    sync_plane_texture(terminal.image_handle.as_ref(), plane_materials, materials);
    if in_3d {
        sync_plane_texture(
            terminal.back_image_handle.as_ref(),
            plane_back_materials,
            materials,
        );
    }
}

#[derive(SystemParam)]
pub(crate) struct ModelLoadParams<'w, 's> {
    app_config: Res<'w, AppConfig>,
    cursor_settings: ResMut<'w, CursorSettings>,
    model_load_state: ResMut<'w, ModelLoadState>,
    redraw: ResMut<'w, TerminalRedrawState>,
    commands: Commands<'w, 's>,
    meshes: ResMut<'w, Assets<Mesh>>,
    materials: ResMut<'w, Assets<StandardMaterial>>,
    images: ResMut<'w, Assets<Image>>,
    asset_server: Res<'w, AssetServer>,
    frame_dirty: Res<'w, TerminalFrameDirty>,
}

/// Completes deferred cursor-model loading once the first frame is uploaded.
///
/// The first successful upload defers cursor-model spawning to the next frame. After that, it
/// ensures the cursor model exists so [`sync_asset_to_terminal_cursor`] can position it.
pub(crate) fn finish_terminal_model_load(mut params: ModelLoadParams) {
    let ModelLoadParams {
        app_config,
        cursor_settings,
        model_load_state,
        redraw,
        commands,
        meshes,
        materials,
        images,
        asset_server,
        frame_dirty,
    } = &mut params;
    if !frame_dirty.0 {
        return;
    }

    if !model_load_state.first_frame_uploaded {
        model_load_state.first_frame_uploaded = true;
        redraw.request();
        return;
    }

    if !model_load_state.loaded {
        // Spawn even when the model starts invisible: the `cursor` command
        // can toggle visibility at runtime, and visibility is applied per
        // frame by `sync_asset_to_terminal_cursor`.
        spawn_cursor_model(
            commands,
            meshes,
            materials,
            images,
            asset_server,
            app_config,
            cursor_settings,
        );
        cursor_settings.needs_respawn = false;
        model_load_state.loaded = true;
    }
}

/// Cursor-model respawn parameters.
#[derive(SystemParam)]
pub(crate) struct RespawnCursorParams<'w, 's> {
    commands: Commands<'w, 's>,
    cursor_settings: ResMut<'w, CursorSettings>,
    model_load_state: Res<'w, ModelLoadState>,
    roots: Query<'w, 's, Entity, With<CursorModel>>,
    meshes: ResMut<'w, Assets<Mesh>>,
    materials: ResMut<'w, Assets<StandardMaterial>>,
    images: ResMut<'w, Assets<Image>>,
    asset_server: Res<'w, AssetServer>,
    app_config: Res<'w, AppConfig>,
}

/// Rebuilds the cursor model when the `cursor` command swapped the model or
/// changed a baked property (brightness is baked into cloned materials).
pub(crate) fn respawn_cursor_model(mut params: RespawnCursorParams) {
    let RespawnCursorParams {
        commands,
        cursor_settings,
        model_load_state,
        roots,
        meshes,
        materials,
        images,
        asset_server,
        app_config,
    } = &mut params;
    // Before the deferred initial spawn, `finish_terminal_model_load` picks
    // up the latest settings on its own.
    if !cursor_settings.needs_respawn || !model_load_state.loaded {
        return;
    }
    for entity in roots.iter() {
        commands.entity(entity).despawn();
    }
    spawn_cursor_model(
        commands,
        meshes,
        materials,
        images,
        asset_server,
        app_config,
        cursor_settings,
    );
    cursor_settings.needs_respawn = false;
}

/// Synchronizes Kitty inline objects.
#[derive(SystemParam)]
pub(crate) struct SyncInlineParams<'w, 's> {
    commands: Commands<'w, 's>,
    inline_objects: ResMut<'w, TerminalInlineObjects>,
    terminal: Res<'w, TerminalSurface>,
    viewport: Res<'w, TerminalViewport>,
    presentation: Res<'w, TerminalPresentation>,
    plane_warp: Res<'w, TerminalPlaneWarp>,
    time: Res<'w, Time>,
    plane_query: Query<'w, 's, (Entity, &'static Transform), With<TerminalPlane>>,
    sprite_query: Query<'w, 's, Entity, With<TerminalInlineObjectSprite>>,
    plane_image_query: Query<'w, 's, Entity, With<TerminalInlineObjectPlane>>,
    rgp_query: Query<'w, 's, (Entity, &'static TerminalRgpObject)>,
    asset_server: Res<'w, AssetServer>,
    materials: ResMut<'w, Assets<StandardMaterial>>,
    images: ResMut<'w, Assets<Image>>,
    meshes: ResMut<'w, Assets<Mesh>>,
}

/// Synchronizes Kitty inline object entities.
///
/// This runs after [`render_terminal_widget`]. It rebuilds the scene entities for registered
/// [`InlineObject::KittyImage`] values and clears stale inline entities first so the scene matches
/// the latest terminal anchors exactly.
///
/// In 2D mode it spawns [`TerminalInlineObjectSprite`] entities. In 3D mode it also generates
/// plane-attached meshes under [`TerminalPlane`] so images follow the warped terminal surface.
/// Warp motion is handled in place by [`animate_inline_kitty_planes`].
pub(crate) fn sync_inline_objects(mut params: SyncInlineParams) {
    let SyncInlineParams {
        commands,
        inline_objects,
        terminal,
        viewport,
        presentation,
        plane_warp,
        time,
        plane_query,
        sprite_query,
        plane_image_query,
        rgp_query,
        asset_server,
        materials,
        images,
        meshes,
    } = &mut params;
    // Per-object rebuilds (queued by `depth` updates and glTF restyles) only
    // run when no full rebuild is due; a full rebuild subsumes them.
    let full_sync = inline_objects.needs_sync(viewport.size, terminal.cols, terminal.rows);
    let rebuild_ids = if full_sync {
        None
    } else {
        let ids = inline_objects.take_rebuild_objects();
        if ids.is_empty() {
            return;
        }
        Some(ids)
    };

    match &rebuild_ids {
        None => {
            for entity in sprite_query.iter() {
                commands.entity(entity).despawn();
            }
            for entity in plane_image_query.iter() {
                commands.entity(entity).despawn();
            }
            for (entity, _) in rgp_query.iter() {
                commands.entity(entity).despawn();
            }
        }
        Some(ids) => {
            for (entity, object) in rgp_query.iter() {
                if ids.contains(&object.object_id) {
                    commands.entity(entity).despawn();
                }
            }
        }
    }

    let Ok((plane_entity, _plane_transform)) = plane_query.single() else {
        return;
    };

    let cell_width = viewport.size.x / terminal.cols.max(1) as f32;
    let cell_height = viewport.size.y / terminal.rows.max(1) as f32;
    let elapsed_secs = time.elapsed_secs();
    let renderable_ids = inline_objects
        .anchors
        .iter()
        .filter_map(|(object_id, anchor)| {
            inline_objects.objects.get(object_id)?;
            if rebuild_ids
                .as_ref()
                .is_some_and(|ids| !ids.contains(object_id))
            {
                return None;
            }
            let start = anchor.row as i32;
            let end = start + anchor.rows as i32;
            (start < terminal.rows as i32 && end > 0).then_some(*object_id)
        })
        .collect::<Vec<_>>();

    let mut plane_children = Vec::new();
    for object_id in renderable_ids {
        let Some(anchor) = inline_objects.anchors.get(&object_id) else {
            continue;
        };
        let layout = inline_layout(anchor, terminal, viewport, cell_width, cell_height);
        let style = anchor.style;
        let Some(object) = inline_objects.objects.get_mut(&object_id) else {
            continue;
        };
        match object {
            InlineObject::KittyImage(object) => {
                let mut ctx = KittyRenderContext {
                    mode: presentation.mode,
                    warp_amount: plane_warp.amount,
                    elapsed_secs,
                    materials,
                    images,
                    meshes,
                    plane_children: &mut plane_children,
                };
                sync_kitty_inline_image(commands, object, &layout, &mut ctx);
            }
            InlineObject::RgpObject(object) => {
                spawn_rgp_object(
                    commands,
                    object_id,
                    object,
                    style,
                    materials,
                    meshes,
                    asset_server,
                );
            }
        }
    }

    if !plane_children.is_empty() {
        commands.entity(plane_entity).add_children(&plane_children);
    }

    if full_sync {
        inline_objects.finish_sync(viewport.size, terminal.cols, terminal.rows);
    }
}

fn inline_layout(
    anchor: &crate::inline::InlineAnchor,
    terminal: &TerminalSurface,
    viewport: &TerminalViewport,
    cell_width: f32,
    cell_height: f32,
) -> InlineLayout {
    let cols = terminal.cols.max(1) as f32;
    let rows = terminal.rows.max(1) as f32;
    let center_x = viewport.center.x - viewport.size.x * 0.5
        + (anchor.col as f32 + anchor.columns as f32 * 0.5) * cell_width;
    let center_y = viewport.center.y + viewport.size.y * 0.5
        - (anchor.row as f32 + anchor.rows as f32 * 0.5) * cell_height;

    InlineLayout {
        columns: anchor.columns,
        rows: anchor.rows,
        center_x,
        center_y,
        local_x: (anchor.col as f32 + anchor.columns as f32 * 0.5) / cols - 0.5,
        local_y: 0.5 - (anchor.row as f32 + anchor.rows as f32 * 0.5) / rows,
        local_width: anchor.columns as f32 / cols,
        local_height: anchor.rows as f32 / rows,
        pixel_width: anchor.columns as f32 * cell_width,
        pixel_height: anchor.rows as f32 * cell_height,
    }
}

fn sync_kitty_inline_image(
    commands: &mut Commands,
    object: &mut crate::inline::KittyInlineObject,
    layout: &InlineLayout,
    ctx: &mut KittyRenderContext<'_>,
) {
    let image_handle = if let Some(handle) = object.raster.handle.as_ref() {
        handle.clone()
    } else {
        let mut image = Image::new_fill(
            Extent3d {
                width: object.raster.width,
                height: object.raster.height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            &[0, 0, 0, 0],
            TextureFormat::Rgba8UnormSrgb,
            bevy::asset::RenderAssetUsages::default(),
        );
        image.sampler = ImageSampler::nearest();
        image.data = Some(std::mem::take(&mut object.raster.rgba));
        let handle = ctx.images.add(image);
        object.raster.handle = Some(handle.clone());
        handle
    };

    let mut sprite = Sprite::from_image(image_handle.clone());
    sprite.custom_size = Some(Vec2::new(layout.pixel_width, layout.pixel_height));
    commands.spawn((
        TerminalInlineObjectSprite,
        sprite,
        Transform::from_translation(Vec3::new(layout.center_x, layout.center_y, 5.0)),
        match ctx.mode {
            TerminalPresentationMode::Flat2d => Visibility::Visible,
            TerminalPresentationMode::Plane3d | TerminalPresentationMode::Mobius3d => {
                Visibility::Hidden
            }
        },
    ));

    let plane_layout = inline_kitty_plane_layout(layout);
    let (mesh_handle, material_handle) = ensure_kitty_plane_assets(
        object,
        &plane_layout,
        &image_handle,
        ctx.warp_amount,
        ctx.elapsed_secs,
        ctx.materials,
        ctx.meshes,
    );
    ctx.plane_children.push(
        commands
            .spawn((
                TerminalInlineObjectPlane,
                plane_layout,
                Mesh3d(mesh_handle),
                MeshMaterial3d(material_handle),
                Transform::default(),
            ))
            .id(),
    );
}

fn inline_kitty_plane_layout(layout: &InlineLayout) -> InlineKittyPlaneLayout {
    InlineKittyPlaneLayout {
        local_x: layout.local_x,
        local_y: layout.local_y,
        local_width: layout.local_width,
        local_height: layout.local_height,
        x_segments: layout.columns.clamp(2, 24),
        y_segments: layout.rows.clamp(2, 24),
    }
}

fn ensure_kitty_plane_assets(
    object: &mut crate::inline::KittyInlineObject,
    layout: &InlineKittyPlaneLayout,
    image_handle: &Handle<Image>,
    warp_amount: f32,
    elapsed_secs: f32,
    materials: &mut Assets<StandardMaterial>,
    meshes: &mut Assets<Mesh>,
) -> (Handle<Mesh>, Handle<StandardMaterial>) {
    let needs_rebuild = object.plane.as_ref().is_none_or(|cache| {
        cache.x_segments != layout.x_segments || cache.y_segments != layout.y_segments
    });
    if needs_rebuild {
        if let Some(cache) = object.plane.take() {
            meshes.remove(&cache.mesh);
            materials.remove(&cache.material);
        }
        let mesh = build_kitty_plane_mesh(layout, warp_amount, elapsed_secs);
        let mesh_handle = meshes.add(mesh);
        let material_handle = materials.add(StandardMaterial {
            base_color: Color::WHITE,
            base_color_texture: Some(image_handle.clone()),
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            ..default()
        });
        object.plane = Some(crate::inline::KittyPlaneCache {
            x_segments: layout.x_segments,
            y_segments: layout.y_segments,
            mesh: mesh_handle.clone(),
            material: material_handle.clone(),
        });
        return (mesh_handle, material_handle);
    }

    let cache = object.plane.as_mut().expect("plane cache should exist");
    if let Some(mut mesh) = meshes.get_mut(&cache.mesh) {
        write_kitty_plane_positions(&mut mesh, layout, warp_amount, elapsed_secs);
    }
    if let Some(mut material) = materials.get_mut(&cache.material) {
        material.base_color_texture = Some(image_handle.clone());
    }
    (cache.mesh.clone(), cache.material.clone())
}

fn build_kitty_plane_mesh(
    layout: &InlineKittyPlaneLayout,
    warp_amount: f32,
    elapsed_secs: f32,
) -> Mesh {
    let vertex_count = ((layout.x_segments + 1) * (layout.y_segments + 1)) as usize;
    let mut positions = Vec::with_capacity(vertex_count);
    let normals = vec![[0.0, 0.0, 1.0]; vertex_count];
    let mut uvs = Vec::with_capacity(vertex_count);
    let mut indices = Vec::with_capacity((layout.x_segments * layout.y_segments * 6) as usize);

    for y in 0..=layout.y_segments {
        let v = y as f32 / layout.y_segments as f32;
        let py = layout.local_y + (0.5 - v) * layout.local_height;
        for x in 0..=layout.x_segments {
            let u = x as f32 / layout.x_segments as f32;
            let px = layout.local_x + (u - 0.5) * layout.local_width;
            positions.push([
                px,
                py,
                plane_surface_z(px, py, warp_amount, elapsed_secs) + 1.5,
            ]);
            uvs.push([u, v]);
        }
    }

    for y in 0..layout.y_segments {
        for x in 0..layout.x_segments {
            let row = y * (layout.x_segments + 1);
            let next_row = (y + 1) * (layout.x_segments + 1);
            let i0 = row + x;
            let i1 = i0 + 1;
            let i2 = next_row + x;
            let i3 = i2 + 1;
            indices.extend_from_slice(&[i0, i2, i1, i1, i2, i3]);
        }
    }

    Mesh::new(
        PrimitiveTopology::TriangleList,
        bevy::asset::RenderAssetUsages::default(),
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
    .with_inserted_indices(Indices::U32(indices))
}

fn write_kitty_plane_positions(
    mesh: &mut Mesh,
    layout: &InlineKittyPlaneLayout,
    warp_amount: f32,
    elapsed_secs: f32,
) {
    let Some(VertexAttributeValues::Float32x3(positions)) =
        mesh.attribute_mut(Mesh::ATTRIBUTE_POSITION)
    else {
        return;
    };

    let mut index = 0;
    for y in 0..=layout.y_segments {
        let v = y as f32 / layout.y_segments as f32;
        let py = layout.local_y + (0.5 - v) * layout.local_height;
        for x in 0..=layout.x_segments {
            let u = x as f32 / layout.x_segments as f32;
            let px = layout.local_x + (u - 0.5) * layout.local_width;
            if index < positions.len() {
                positions[index] = [
                    px,
                    py,
                    plane_surface_z(px, py, warp_amount, elapsed_secs) + 1.5,
                ];
            }
            index += 1;
        }
    }
}

/// Animates Kitty image planes attached to the warped terminal surface.
///
/// This runs after [`sync_inline_objects`] and updates cached plane mesh positions in place when
/// warp is active, instead of rebuilding inline entities every frame.
pub(crate) fn animate_inline_kitty_planes(
    presentation: Res<TerminalPresentation>,
    warp: Res<TerminalPlaneWarp>,
    time: Res<Time>,
    query: Query<(&InlineKittyPlaneLayout, &Mesh3d), With<TerminalInlineObjectPlane>>,
    mut meshes: ResMut<Assets<Mesh>>,
) {
    if !matches!(
        presentation.mode,
        TerminalPresentationMode::Plane3d | TerminalPresentationMode::Mobius3d
    ) || warp.amount <= 0.0
    {
        return;
    }

    let elapsed_secs = time.elapsed_secs();
    for (layout, mesh3d) in query.iter() {
        let Some(mut mesh) = meshes.get_mut(&mesh3d.0) else {
            continue;
        };
        write_kitty_plane_positions(&mut mesh, layout, warp.amount, elapsed_secs);
    }
}

fn spawn_rgp_object(
    commands: &mut Commands,
    object_id: u32,
    object: &mut crate::inline::RgpInlineObject,
    style: crate::inline::InlineStyle,
    materials: &mut Assets<StandardMaterial>,
    meshes: &mut Assets<Mesh>,
    asset_server: &AssetServer,
) {
    match object {
        crate::inline::RgpInlineObject::Obj {
            meshes: source_meshes,
            handles,
        } => {
            let depth_key = (style.depth.max(0.0) * 100.0).round() as u32;
            let mesh_handles = if let Some((existing_key, existing_handles)) = handles.as_ref() {
                if *existing_key == depth_key {
                    existing_handles.clone()
                } else {
                    for handle in existing_handles {
                        meshes.remove(handle);
                    }
                    let mesh_handles = source_meshes
                        .iter()
                        .cloned()
                        .map(|mesh| meshes.add(extrude_mesh(mesh, style.depth)))
                        .collect::<Vec<_>>();
                    *handles = Some((depth_key, mesh_handles.clone()));
                    mesh_handles
                }
            } else {
                let mesh_handles = source_meshes
                    .iter()
                    .cloned()
                    .map(|mesh| meshes.add(extrude_mesh(mesh, style.depth)))
                    .collect::<Vec<_>>();
                *handles = Some((depth_key, mesh_handles.clone()));
                mesh_handles
            };
            let material = materials.add(rgp_object_material(style.color));
            let root = commands
                .spawn((
                    TerminalRgpObject { object_id },
                    RgpAnimationState::default(),
                    Transform::default(),
                    Visibility::Visible,
                ))
                .id();
            let children = mesh_handles
                .into_iter()
                .map(|handle| {
                    commands
                        .spawn((
                            Mesh3d(handle),
                            MeshMaterial3d(material.clone()),
                            Transform::default(),
                        ))
                        .id()
                })
                .collect::<Vec<_>>();
            commands.entity(root).add_children(&children);
        }
        crate::inline::RgpInlineObject::Gltf { asset_path, handle } => {
            let handle = if let Some(handle) = handle.as_ref() {
                handle.clone()
            } else {
                let scene =
                    asset_server.load(GltfAssetLabel::Scene(0).from_asset(asset_path.clone()));
                *handle = Some(scene.clone());
                scene
            };
            commands.spawn((
                TerminalRgpObject { object_id },
                RgpAnimationState::default(),
                Transform::default(),
                Visibility::Visible,
                WorldAssetRoot(handle),
            ));
        }
        crate::inline::RgpInlineObject::Stl { mesh, handle } => {
            let depth_key = (style.depth.max(0.0) * 100.0).round() as u32;
            let mesh_handle = match handle.as_ref() {
                Some((existing_key, existing_handle)) if *existing_key == depth_key => {
                    existing_handle.clone()
                }
                Some((_, existing_handle)) => {
                    meshes.remove(existing_handle);
                    let mesh_handle = meshes.add(extrude_mesh(mesh.clone(), style.depth));
                    *handle = Some((depth_key, mesh_handle.clone()));
                    mesh_handle
                }
                None => {
                    let mesh_handle = meshes.add(extrude_mesh(mesh.clone(), style.depth));
                    *handle = Some((depth_key, mesh_handle.clone()));
                    mesh_handle
                }
            };
            let material = materials.add(rgp_object_material(style.color));
            let root = commands
                .spawn((
                    TerminalRgpObject { object_id },
                    RgpAnimationState::default(),
                    Transform::default(),
                    Visibility::Visible,
                ))
                .id();

            let child = commands
                .spawn((
                    Mesh3d(mesh_handle),
                    MeshMaterial3d(material.clone()),
                    Transform::default(),
                ))
                .id();
            commands.entity(root).add_child(child);
        }
    }
}

/// Builds the material RGP mesh objects spawn with. `color` is the raw style
/// color; the brightness multiplier is applied afterwards, either by
/// [`apply_instance_brightness`] or by [`apply_rgp_restyle`].
fn rgp_object_material(color: Option<[u8; 3]>) -> StandardMaterial {
    let use_lighting = true;
    let [r, g, b] = match color {
        Some([r, g, b]) => [r, g, b],
        None => [255, 255, 255],
    };
    StandardMaterial {
        base_color: Color::srgb_u8(r, g, b),
        emissive: if use_lighting {
            LinearRgba::rgb(0.02, 0.02, 0.02)
        } else {
            LinearRgba::rgb(0.0, 0.0, 0.0)
        },
        metallic: 0.0,
        perceptual_roughness: if use_lighting { 0.88 } else { 1.0 },
        reflectance: if use_lighting { 0.18 } else { 0.0 },
        cull_mode: None,
        unlit: !use_lighting,
        ..default()
    }
}

/// Bakes a brightness multiplier into a material's base color and emissive.
fn apply_brightness(material: &mut StandardMaterial, brightness: f32) {
    let linear = material.base_color.to_linear();
    material.base_color = Color::linear_rgba(
        linear.red * brightness,
        linear.green * brightness,
        linear.blue * brightness,
        linear.alpha,
    );
    material.emissive = LinearRgba::new(
        material.emissive.red * brightness,
        material.emissive.green * brightness,
        material.emissive.blue * brightness,
        material.emissive.alpha,
    );
}

/// Synchronizes RGP inline objects.
#[derive(SystemParam)]
pub(crate) struct RgpSyncParams<'w, 's> {
    app_config: Res<'w, AppConfig>,
    terminal: Res<'w, TerminalSurface>,
    viewport: Res<'w, TerminalViewport>,
    presentation: Res<'w, TerminalPresentation>,
    mobius_transition: Res<'w, MobiusTransition>,
    plane_warp: Res<'w, TerminalPlaneWarp>,
    time: Res<'w, Time>,
    plane_query: PlaneTransformQuery<'w, 's>,
    inline_objects: Res<'w, TerminalInlineObjects>,
    query: Query<
        'w,
        's,
        (
            &'static TerminalRgpObject,
            &'static mut Transform,
            &'static mut Visibility,
            &'static mut RgpAnimationState,
        ),
    >,
}

/// Synchronizes RGP object entities.
///
/// This runs after [`sync_inline_objects`]. It does not create registrations itself; instead, it
/// positions existing [`TerminalRgpObject`] roots from [`TerminalInlineObjects`] anchor data.
///
/// In [`TerminalPresentationMode::Flat2d`] objects are placed in screen space above the terminal
/// surface. In the 3D modes they are projected onto the active terminal surface using the current
/// [`TerminalPlane`] transform.
pub(crate) fn sync_rgp_objects(mut params: RgpSyncParams) {
    let RgpSyncParams {
        app_config,
        terminal,
        viewport,
        presentation,
        mobius_transition,
        plane_warp,
        time,
        plane_query,
        inline_objects,
        query,
    } = &mut params;
    let cell_width = viewport.size.x / terminal.cols.max(1) as f32;
    let cell_height = viewport.size.y / terminal.rows.max(1) as f32;
    let elapsed_secs = time.elapsed_secs();
    let delta_secs = time.delta_secs();
    let mobius_progress = active_mobius_progress(presentation.mode, mobius_transition);

    for (object, mut transform, mut visibility, mut animation_state) in query.iter_mut() {
        let Some(anchor) = inline_objects.anchors.get(&object.object_id) else {
            *visibility = Visibility::Hidden;
            continue;
        };
        let layout = inline_layout(anchor, terminal, viewport, cell_width, cell_height);
        let base_scale = layout.pixel_width.max(layout.pixel_height).max(1.0) * 0.9;
        let scale = base_scale * anchor.style.scale.max(0.001);
        let scale3 = Vec3::new(
            anchor.style.scale3.x.max(0.001),
            anchor.style.scale3.y.max(0.001),
            anchor.style.scale3.z.max(0.001),
        );
        let base_oblique = if anchor.style.depth > 0.0 {
            Quat::from_rotation_y(0.75) * Quat::from_rotation_x(0.35)
        } else {
            Quat::IDENTITY
        };
        let explicit_rotation = Quat::from_euler(
            EulerRot::XYZ,
            anchor.style.rotation.x.to_radians(),
            anchor.style.rotation.y.to_radians(),
            anchor.style.rotation.z.to_radians(),
        );
        let (spin, tilt, bob) = rgp_object_animation(
            &anchor.style,
            &mut animation_state,
            &app_config.cursor.animation,
            elapsed_secs,
            delta_secs,
            cell_height,
        );
        let animated_rotation = Quat::from_rotation_y(spin) * Quat::from_rotation_x(tilt);
        let object_rotation = base_oblique * explicit_rotation * animated_rotation;
        let object_scale = Vec3::splat(scale) * scale3;

        match presentation.mode {
            TerminalPresentationMode::Flat2d => {
                transform.translation = Vec3::new(
                    layout.center_x
                        + anchor.style.offset.x * (terminal.pixmap_dimensions().x as f32),
                    layout.center_y
                        + bob
                        + anchor.style.offset.y * (terminal.pixmap_dimensions().y as f32),
                    CURSOR_DEPTH + anchor.style.depth * 4.0 + anchor.style.offset.z,
                );
                transform.rotation = object_rotation;
                transform.scale = object_scale;
                *visibility = Visibility::Visible;
            }
            TerminalPresentationMode::Plane3d | TerminalPresentationMode::Mobius3d => {
                let Ok(plane_transform) = plane_query.single() else {
                    *visibility = Visibility::Hidden;
                    continue;
                };
                let local_position = plane_surface_point(
                    presentation.mode,
                    layout.local_x,
                    layout.local_y,
                    plane_warp.amount,
                    elapsed_secs,
                    8.0 + anchor.style.depth * 1.5,
                    mobius_progress,
                ) + anchor.style.offset;
                transform.translation = plane_transform.transform_point(local_position);
                transform.rotation = plane_transform.rotation * object_rotation;
                transform.scale = object_scale;
                *visibility = Visibility::Visible;
            }
        }
    }
}

/// Computes the built-in spin/tilt/bob animation for one RGP object and
/// advances its integrated state.
///
/// Objects without per-object animation fields evaluate the v1 absolute-time
/// expressions verbatim, so their motion is bit-identical to v1; the
/// accumulators are refreshed in lockstep so switching to per-object rates
/// later is continuous. Objects with per-object fields integrate
/// `state += delta * rate`, which makes mid-flight rate changes smooth:
/// `spin=0` holds the current angle, and `phase` offsets both channels
/// (desynchronizing otherwise-lockstep objects). A respawn resets the state,
/// which is already a visual discontinuity.
fn rgp_object_animation(
    style: &InlineStyle,
    state: &mut RgpAnimationState,
    animation: &CursorAnimationConfig,
    elapsed_secs: f32,
    delta_secs: f32,
    cell_height: f32,
) -> (f32, f32, f32) {
    if !style.animate {
        return (0.0, 0.0, 0.0);
    }

    let has_custom = style.spin.is_some()
        || style.bob.is_some()
        || style.bob_amplitude.is_some()
        || style.phase != 0.0;
    if !has_custom {
        state.spin_angle = elapsed_secs * animation.spin_speed;
        state.bob_phase = elapsed_secs * animation.bob_speed;
        return (
            elapsed_secs * animation.spin_speed,
            elapsed_secs * animation.spin_speed * 0.7,
            (elapsed_secs * animation.bob_speed).sin() * cell_height * animation.bob_amplitude,
        );
    }

    state.spin_angle += delta_secs * style.spin.unwrap_or(animation.spin_speed);
    state.bob_phase += delta_secs * style.bob.unwrap_or(animation.bob_speed);
    let spin = state.spin_angle + style.phase;
    let tilt = spin * 0.7;
    let bob = (state.bob_phase + style.phase).sin()
        * cell_height
        * style.bob_amplitude.unwrap_or(animation.bob_amplitude);
    (spin, tilt, bob)
}

#[cfg(test)]
mod rgp_animation_tests {
    use super::*;

    fn config() -> CursorAnimationConfig {
        CursorAnimationConfig::default()
    }

    fn v1_style() -> InlineStyle {
        InlineStyle {
            animate: true,
            ..Default::default()
        }
    }

    #[test]
    fn v1_path_matches_the_absolute_time_expressions_bit_exactly() {
        let animation = config();
        let style = v1_style();
        let mut state = RgpAnimationState::default();
        for elapsed in [0.0_f32, 0.25, 1.0, 7.5, 3600.0] {
            let (spin, tilt, bob) =
                rgp_object_animation(&style, &mut state, &animation, elapsed, 0.016, 20.0);
            assert_eq!(spin, elapsed * animation.spin_speed);
            assert_eq!(tilt, elapsed * animation.spin_speed * 0.7);
            assert_eq!(
                bob,
                (elapsed * animation.bob_speed).sin() * 20.0 * animation.bob_amplitude
            );
        }
    }

    #[test]
    fn switching_to_per_object_rates_is_continuous() {
        let animation = config();
        let mut state = RgpAnimationState::default();
        let mut style = v1_style();

        // Run the v1 path for a while; the accumulator tracks it.
        let elapsed = 5.0_f32;
        let (v1_spin, ..) =
            rgp_object_animation(&style, &mut state, &animation, elapsed, 0.016, 20.0);

        // A `u;spin=` arrives: the very next frame advances from the v1
        // angle by exactly delta * new_rate. No snap.
        style.spin = Some(0.4);
        let delta = 0.016_f32;
        let (spin, ..) =
            rgp_object_animation(&style, &mut state, &animation, elapsed + delta, delta, 20.0);
        assert!((spin - (v1_spin + delta * 0.4)).abs() < 1e-5);
    }

    #[test]
    fn animate_off_is_a_rest_pose_and_freezes_state() {
        let animation = config();
        let style = InlineStyle::default();
        let mut state = RgpAnimationState {
            spin_angle: 3.0,
            bob_phase: 1.0,
        };
        let result = rgp_object_animation(&style, &mut state, &animation, 9.0, 0.016, 20.0);
        assert_eq!(result, (0.0, 0.0, 0.0));
        assert_eq!(state.spin_angle, 3.0);
        assert_eq!(state.bob_phase, 1.0);
    }

    #[test]
    fn zero_spin_rate_holds_the_current_angle() {
        let animation = config();
        let mut style = v1_style();
        style.spin = Some(0.0);
        let mut state = RgpAnimationState {
            spin_angle: 2.5,
            bob_phase: 0.0,
        };
        for _ in 0..10 {
            let (spin, ..) = rgp_object_animation(&style, &mut state, &animation, 1.0, 0.016, 20.0);
            assert_eq!(spin, 2.5);
        }
    }

    #[test]
    fn phase_offsets_both_channels() {
        let animation = config();
        let mut style = v1_style();
        style.spin = Some(0.0);
        style.bob = Some(0.0);
        style.phase = 1.0;
        let mut state = RgpAnimationState::default();
        let (spin, tilt, bob) =
            rgp_object_animation(&style, &mut state, &animation, 0.0, 0.0, 20.0);
        assert_eq!(spin, 1.0);
        assert_eq!(tilt, 0.7);
        assert_eq!(bob, 1.0_f32.sin() * 20.0 * animation.bob_amplitude);
    }
}

/// Restyle application parameters.
#[derive(SystemParam)]
pub(crate) struct RgpRestyleParams<'w, 's> {
    inline_objects: ResMut<'w, TerminalInlineObjects>,
    rgp_roots: Query<'w, 's, (Entity, &'static TerminalRgpObject)>,
    parent_query: Query<'w, 's, &'static ChildOf>,
    material_query: Query<
        'w,
        's,
        (
            Entity,
            &'static MeshMaterial3d<StandardMaterial>,
            &'static ChildOf,
        ),
    >,
    materials: ResMut<'w, Assets<StandardMaterial>>,
    commands: Commands<'w, 's>,
}

/// Rewrites materials in place for objects whose `color`/`brightness`
/// changed, instead of despawning and respawning anything.
///
/// Only mesh-backed (OBJ/STL) objects are routed here: their materials are
/// derived entirely from [`crate::inline::InlineStyle`], so the full
/// brightened material can be recomputed from the anchor style and written
/// through [`Assets::get_mut`]. The rewritten entities are marked
/// [`BrightnessAdjusted`] so [`apply_instance_brightness`] — which runs
/// afterwards — never multiplies brightness on top of the already-adjusted
/// result. Transforms, children, and [`RgpAnimationState`] accumulators are
/// untouched, so the pose stays continuous.
pub(crate) fn apply_rgp_restyle(mut params: RgpRestyleParams) {
    let RgpRestyleParams {
        inline_objects,
        rgp_roots,
        parent_query,
        material_query,
        materials,
        commands,
    } = &mut params;
    let restyle = inline_objects.take_restyle_objects();
    if restyle.is_empty() {
        return;
    }

    let targets = rgp_roots
        .iter()
        .filter(|(_, object)| restyle.contains(&object.object_id))
        .map(|(entity, object)| (entity, object.object_id))
        .collect::<HashMap<_, _>>();
    if targets.is_empty() {
        return;
    }

    for (entity, material_handle, child_of) in material_query.iter() {
        let mut current = child_of.parent();
        let object_id = loop {
            if let Some(object_id) = targets.get(&current) {
                break Some(*object_id);
            }
            let Ok(next) = parent_query.get(current) else {
                break None;
            };
            current = next.parent();
        };
        let Some(object_id) = object_id else {
            continue;
        };
        let Some(style) = inline_objects
            .anchors
            .get(&object_id)
            .map(|anchor| anchor.style)
        else {
            continue;
        };
        let Some(mut material) = materials.get_mut(&material_handle.0) else {
            continue;
        };
        *material = rgp_object_material(style.color);
        apply_brightness(&mut material, style.brightness);
        commands.entity(entity).insert(BrightnessAdjusted);
    }
}

/// Brightness application parameters.
#[derive(SystemParam)]
pub(crate) struct BrightnessParams<'w, 's> {
    cursor_settings: Res<'w, CursorSettings>,
    inline_objects: Res<'w, TerminalInlineObjects>,
    rgp_roots: Query<'w, 's, (Entity, &'static TerminalRgpObject)>,
    cursor_roots: Query<'w, 's, Entity, With<CursorModel>>,
    parent_query: Query<'w, 's, &'static ChildOf>,
    material_query: Query<
        'w,
        's,
        (
            Entity,
            &'static mut MeshMaterial3d<StandardMaterial>,
            &'static ChildOf,
        ),
        Without<BrightnessAdjusted>,
    >,
    materials: ResMut<'w, Assets<StandardMaterial>>,
    commands: Commands<'w, 's>,
}

/// Applies per-instance brightness to spawned materials.
///
/// This runs after [`sync_rgp_objects`] so newly spawned object descendants already exist. It walks
/// up each material-bearing entity through [`ChildOf`] relationships, finds either an
/// [`TerminalRgpObject`] root or a [`CursorModel`] root and clones the referenced material with
/// the effective brightness applied.
///
/// Adjusted entities receive [`BrightnessAdjusted`] so the same material branch is not processed
/// again every frame.
pub(crate) fn apply_instance_brightness(mut params: BrightnessParams) {
    let BrightnessParams {
        cursor_settings,
        inline_objects,
        rgp_roots,
        cursor_roots,
        parent_query,
        material_query,
        materials,
        commands,
    } = &mut params;
    if material_query.is_empty() {
        return;
    }

    let rgp_brightness = rgp_roots
        .iter()
        .filter_map(|(entity, object)| {
            let brightness = inline_objects
                .anchors
                .get(&object.object_id)
                .map(|anchor| anchor.style.brightness)?;
            Some((entity, brightness))
        })
        .collect::<HashMap<_, _>>();
    let cursor_roots = cursor_roots.iter().collect::<Vec<_>>();

    for (entity, mut material_handle, parent) in material_query.iter_mut() {
        let mut current = parent.parent();
        let mut brightness = None;

        loop {
            if let Some(value) = rgp_brightness.get(&current) {
                brightness = Some(*value);
                break;
            }
            if cursor_roots.contains(&current) {
                brightness = Some(cursor_settings.brightness);
                break;
            }
            let Ok(next) = parent_query.get(current) else {
                break;
            };
            current = next.parent();
        }

        let Some(brightness) = brightness else {
            continue;
        };

        let Some(source_material) = materials.get(&material_handle.0).cloned() else {
            continue;
        };
        let mut adjusted = source_material;
        apply_brightness(&mut adjusted, brightness);
        material_handle.0 = materials.add(adjusted);
        commands.entity(entity).insert(BrightnessAdjusted);
    }
}

// ── Viz renderers ──
//
// The `viz.*` family renders as ordinary Bevy entities, never into the
// Vello terminal surface: one root entity per visualization id with one
// small keyed mesh child per snapshot item. Mutations arrive exclusively
// through the registry's granular rebuild/removal sets — the inline
// scene-wide dirty flag is never involved, so a snapshot refresh can never
// respawn a transmission's scene, and `sync_inline_objects`' full sync
// never touches viz entities (its despawn queries match the inline
// markers, not [`VizObjectRoot`]).

/// Fraction of its grid cell a viz child occupies horizontally.
const VIZ_BAR_WIDTH_FRACTION: f32 = 0.8;

/// Minimum bar height as a fraction of the cell, so zero-magnitude items
/// remain visible (an idle process is still a process).
const VIZ_BAR_MIN_HEIGHT_FRACTION: f32 = 0.08;

/// Bar depth in the root's normalized space.
const VIZ_BAR_DEPTH: f32 = 0.8;

/// Grid dimensions (columns, rows) for `count` keyed children: near-square,
/// never taller than wide, always at least one cell.
pub(crate) fn viz_grid_dims(count: usize) -> (usize, usize) {
    if count == 0 {
        return (1, 1);
    }
    let cols = (count as f32).sqrt().ceil().max(1.0) as usize;
    (cols, count.div_ceil(cols))
}

/// Root-local rest pose for the child at `index` of `count`: a unit cube
/// translated and scaled into its grid cell within the root's normalized
/// `[-0.5, 0.5]` footprint. Bars rise from the cell bottom with height
/// proportional to the clamped magnitude, so the layout is resolution
/// independent — the root's per-frame scale is the anchored footprint in
/// pixels.
pub(crate) fn viz_child_pose(index: usize, count: usize, magnitude: f32) -> (Vec3, Vec3) {
    let (grid_cols, grid_rows) = viz_grid_dims(count);
    let cell_width = 1.0 / grid_cols as f32;
    let cell_height = 1.0 / grid_rows as f32;
    let col = index % grid_cols;
    let row = index / grid_cols;
    let height = cell_height
        * (VIZ_BAR_MIN_HEIGHT_FRACTION
            + (1.0 - 2.0 * VIZ_BAR_MIN_HEIGHT_FRACTION) * magnitude.clamp(0.0, 1.0));
    let x = -0.5 + (col as f32 + 0.5) * cell_width;
    let cell_bottom = 0.5 - (row as f32 + 1.0) * cell_height;
    let y = cell_bottom + cell_height * VIZ_BAR_MIN_HEIGHT_FRACTION * 0.5 + height * 0.5;
    (
        Vec3::new(x, y, 0.0),
        Vec3::new(cell_width * VIZ_BAR_WIDTH_FRACTION, height, VIZ_BAR_DEPTH),
    )
}

/// Linear-space blend from `base` toward `flash` by `factor` (clamped).
fn mix_colors(base: Color, flash: Color, factor: f32) -> Color {
    let base = base.to_linear();
    let flash = flash.to_linear();
    let factor = factor.clamp(0.0, 1.0);
    Color::linear_rgba(
        base.red + (flash.red - base.red) * factor,
        base.green + (flash.green - base.green) * factor,
        base.blue + (flash.blue - base.blue) * factor,
        base.alpha + (flash.alpha - base.alpha) * factor,
    )
}

/// Smooth one-pulse envelope over normalized time: zero at both ends so an
/// expiring flash restores the base color seamlessly.
fn flash_envelope(t: f32) -> f32 {
    (t.clamp(0.0, 1.0) * std::f32::consts::PI).sin()
}

/// The animated pose (translation, scale) of a keyed child at normalized
/// effect time `t` in `0.0..=1.0`, computed from the ledger's base pose —
/// never incrementally — so animations cannot drift and always end exactly
/// on the base.
pub(crate) fn viz_effect_pose(
    effect: VizEffectKind,
    base_translation: Vec3,
    base_scale: Vec3,
    t: f32,
) -> (Vec3, Vec3) {
    let t = t.clamp(0.0, 1.0);
    match effect {
        // Shrink to nothing; the child is despawned at expiry.
        VizEffectKind::Died => (base_translation, base_scale * (1.0 - t)),
        // A decaying horizontal shake: four oscillations whose amplitude
        // reaches zero exactly at expiry.
        VizEffectKind::Survived => {
            let amplitude = base_scale.x * 0.35 * (1.0 - t);
            let offset = (t * std::f32::consts::TAU * 4.0).sin() * amplitude;
            (base_translation + Vec3::X * offset, base_scale)
        }
        // One smooth swell back to rest.
        VizEffectKind::Highlight => (
            base_translation,
            base_scale * (1.0 + 0.25 * flash_envelope(t)),
        ),
        // Color-only flashes.
        VizEffectKind::Denied | VizEffectKind::Missing | VizEffectKind::Timeout => {
            (base_translation, base_scale)
        }
    }
}

/// The animated material color of a keyed child at normalized effect time
/// `t`, blending the palette `base` toward the effect's flash color. Every
/// non-`died` envelope is zero at `t = 1.0` so expiry restores the base
/// color exactly.
pub(crate) fn viz_effect_color(effect: VizEffectKind, base: Color, t: f32) -> Color {
    let (flash, strength) = match effect {
        // Darken in step with the shrink; the child never comes back.
        VizEffectKind::Died => (Color::srgb(0.05, 0.05, 0.06), t.clamp(0.0, 1.0)),
        VizEffectKind::Survived => (Color::srgb(1.0, 1.0, 0.95), 0.35 * flash_envelope(t)),
        VizEffectKind::Denied => (Color::srgb(0.85, 0.20, 0.15), flash_envelope(t)),
        VizEffectKind::Missing => (Color::srgb(0.55, 0.55, 0.58), flash_envelope(t)),
        VizEffectKind::Timeout => (Color::srgb(0.85, 0.60, 0.20), flash_envelope(t)),
        VizEffectKind::Highlight => (Color::srgb(1.0, 1.0, 0.95), 0.6 * flash_envelope(t)),
    };
    mix_colors(base, flash, strength)
}

/// Builds the bespoke material a viz child spawns with. Deliberately NOT
/// routed through [`apply_instance_brightness`]: children spawn with
/// [`BrightnessAdjusted`] already set, both because effect animations
/// rewrite `base_color` in place (a cloned-and-swapped handle would orphan
/// the ledger's copy) and so the every-frame brightness scan skips them.
fn viz_child_material(palette: VizPaletteSlot) -> StandardMaterial {
    StandardMaterial {
        base_color: palette.color(),
        emissive: LinearRgba::rgb(0.02, 0.02, 0.02),
        metallic: 0.0,
        perceptual_roughness: 0.88,
        reflectance: 0.18,
        cull_mode: None,
        ..default()
    }
}

/// Mutable access to keyed children during the rebuild diff.
type VizChildPoseQuery<'w, 's> =
    Query<'w, 's, (&'static mut Transform, Option<&'static mut VizEffectAnim>), With<VizKeyedItem>>;

/// Rebuild parameters for the viz renderer.
#[derive(SystemParam)]
pub(crate) struct VizRebuildParams<'w, 's> {
    commands: Commands<'w, 's>,
    registry: ResMut<'w, VizRegistry>,
    meshes: ResMut<'w, Assets<Mesh>>,
    materials: ResMut<'w, Assets<StandardMaterial>>,
    /// The one shared unit-cube mesh every viz child renders with, created
    /// lazily and cached for the app's lifetime (rebuilds never allocate
    /// meshes; per-child variety is all transform and material).
    unit_mesh: Local<'s, Option<Handle<Mesh>>>,
    roots: Query<'w, 's, (Entity, &'static mut VizObjectRoot)>,
    children: VizChildPoseQuery<'w, 's>,
}

/// Applies queued viz registry work to the entity scene: despawns removed
/// visualizations, spawns or key-diffs rebuilt ones, and lowers queued
/// keyed effects onto child animations.
///
/// Runs after [`crate::viz::apply_viz_commands`] (and after
/// `answer_queries`, see [`crate::viz::VizPlugin`]) behind a has-work
/// `run_if`. Everything here is granular: only the ids in the drained sets
/// are touched, and within a diffed tree only changed children mutate —
/// an unchanged key keeps its entity, its material, and any running
/// animation.
pub(crate) fn rebuild_viz_objects(mut params: VizRebuildParams) {
    let VizRebuildParams {
        commands,
        registry,
        meshes,
        materials,
        unit_mesh,
        roots,
        children,
    } = &mut params;

    let removals = registry.take_removals();
    let rebuilds = registry.take_rebuilds();

    let mut root_index: HashMap<u32, Entity> = HashMap::new();
    for (entity, root) in roots.iter() {
        if removals.contains(&root.viz_id) {
            // Despawning the root despawns its keyed children with it.
            commands.entity(entity).despawn();
        } else {
            root_index.insert(root.viz_id, entity);
        }
    }

    let unit_mesh = unit_mesh
        .get_or_insert_with(|| meshes.add(Mesh::from(Cuboid::new(1.0, 1.0, 1.0))))
        .clone();

    // Ledgers of trees spawned this pass: their `VizObjectRoot` insert is
    // deferred until the next command flush, so same-frame effect
    // resolution reads these instead of the root query.
    let mut fresh: HashMap<u32, HashMap<String, VizChildRecord>> = HashMap::new();
    // Children whose `died` animation a same-pass snapshot re-assert just
    // cancelled (F5). Consulted in the effect loop below so an effect
    // landing in the same pass sees the child as alive again (data wins),
    // while the `VizEffectAnim` removal is still a deferred command.
    let mut restored_from_died: HashSet<Entity> = HashSet::new();

    for id in rebuilds {
        let Some(entry) = registry.get(id) else {
            // Unreachable while `remove`/`clear_all` move ids to the
            // removal set; drop any stale tree rather than render a dead
            // id.
            if let Some(entity) = root_index.remove(&id) {
                commands.entity(entity).despawn();
            }
            continue;
        };
        let specs = viz_child_specs(&entry.payload);
        if let Some(&root_entity) = root_index.get(&id) {
            if let Ok((_, mut root)) = roots.get_mut(root_entity) {
                let restored = diff_viz_children(
                    &mut root,
                    root_entity,
                    &specs,
                    commands,
                    children,
                    materials,
                    &unit_mesh,
                );
                restored_from_died.extend(restored);
            }
        } else {
            let (root_entity, ledger) = spawn_viz_tree(commands, id, &specs, materials, &unit_mesh);
            root_index.insert(id, root_entity);
            fresh.insert(id, ledger);
        }
    }

    // Lower queued effects onto per-child animations. A key the ledger
    // does not carry renders nothing, by design: a kill racing a snapshot
    // refresh is not an error.
    let effect_ids: Vec<u32> = registry
        .iter()
        .filter(|(_, entry)| !entry.pending_effects.is_empty())
        .map(|(id, _)| id)
        .collect();
    for id in effect_ids {
        for QueuedVizEffect { key, effect } in registry.take_pending_effects(id) {
            let target = if let Some(ledger) = fresh.get(&id) {
                ledger.get(&key)
            } else if let Some(&root_entity) = root_index.get(&id) {
                roots
                    .get(root_entity)
                    .ok()
                    .and_then(|(_, root)| root.children.get(&key))
            } else {
                None
            };
            let Some(record) = target else {
                continue;
            };
            // A running `died` animation is terminal for presentation: the
            // child is confirmed gone and is shrinking away. A later effect
            // must NOT replace that animation and resurrect the bar (F4) —
            // only a snapshot (data) may undo a death, and when it does so
            // this same pass it records the child in `restored_from_died`,
            // so an effect landing alongside that snapshot still applies.
            let running_died = !restored_from_died.contains(&record.entity)
                && children
                    .get(record.entity)
                    .ok()
                    .and_then(|(_, anim)| anim)
                    .is_some_and(|anim| anim.effect == VizEffectKind::Died);
            if running_died {
                continue;
            }
            // Insert-or-replace: a newer effect on the same child restarts
            // the animation from the ledger's base pose, never from a
            // mid-animation pose, so repeated effects cannot drift.
            commands.entity(record.entity).insert(VizEffectAnim {
                effect,
                elapsed: 0.0,
                base_translation: record.base_translation,
                base_scale: record.base_scale,
            });
        }
    }
}

/// Spawns a fresh root and keyed children for `viz_id`, returning the root
/// entity and its child ledger. The root spawns hidden; [`sync_viz_objects`]
/// places and shows it later the same frame (or leaves it hidden while
/// unanchored).
fn spawn_viz_tree(
    commands: &mut Commands,
    viz_id: u32,
    specs: &[VizChildSpec],
    materials: &mut Assets<StandardMaterial>,
    unit_mesh: &Handle<Mesh>,
) -> (Entity, HashMap<String, VizChildRecord>) {
    let root_entity = commands
        .spawn((
            VizObjectRoot {
                viz_id,
                children: HashMap::new(),
            },
            Transform::default(),
            Visibility::Hidden,
        ))
        .id();
    let count = specs.len();
    let mut ledger = HashMap::with_capacity(count);
    for (index, spec) in specs.iter().enumerate() {
        // A hostile snapshot may repeat a key; the first occurrence wins
        // so the ledger and the entity tree cannot diverge.
        if ledger.contains_key(&spec.key) {
            continue;
        }
        let (translation, scale) = viz_child_pose(index, count, spec.magnitude);
        let record = spawn_viz_child(
            commands,
            root_entity,
            spec,
            translation,
            scale,
            materials,
            unit_mesh,
        );
        ledger.insert(spec.key.clone(), record);
    }
    commands.entity(root_entity).insert(VizObjectRoot {
        viz_id,
        children: ledger.clone(),
    });
    (root_entity, ledger)
}

/// Spawns one keyed child mesh under `root_entity` and returns its ledger
/// record.
fn spawn_viz_child(
    commands: &mut Commands,
    root_entity: Entity,
    spec: &VizChildSpec,
    translation: Vec3,
    scale: Vec3,
    materials: &mut Assets<StandardMaterial>,
    unit_mesh: &Handle<Mesh>,
) -> VizChildRecord {
    let material = materials.add(viz_child_material(spec.palette));
    let entity = commands
        .spawn((
            Mesh3d(unit_mesh.clone()),
            MeshMaterial3d(material.clone()),
            Transform {
                translation,
                scale,
                ..default()
            },
            VizKeyedItem {
                key: spec.key.clone(),
            },
            // Bespoke material, deliberately outside the brightness pass —
            // see `viz_child_material`.
            BrightnessAdjusted,
        ))
        .id();
    commands.entity(root_entity).add_child(entity);
    VizChildRecord {
        entity,
        material,
        palette: spec.palette,
        base_translation: translation,
        base_scale: scale,
    }
}

/// Diffs a live tree against a fresh snapshot by semantic key: unchanged
/// children keep their entities, palette changes recolor the material in
/// place, layout changes move the ledger's base pose (and any running
/// animation's restore target), added keys spawn, and dropped keys
/// despawn. The despawned children's materials are freed when their last
/// handle (the ledger record) drops.
fn diff_viz_children(
    root: &mut VizObjectRoot,
    root_entity: Entity,
    specs: &[VizChildSpec],
    commands: &mut Commands,
    children: &mut VizChildPoseQuery<'_, '_>,
    materials: &mut Assets<StandardMaterial>,
    unit_mesh: &Handle<Mesh>,
) -> Vec<Entity> {
    let count = specs.len();
    let mut retained: HashMap<String, VizChildRecord> = HashMap::with_capacity(count);
    // Children whose running `died` animation this re-assert just cancelled
    // (F5); returned so the caller's effect loop treats them as alive.
    let mut restored_from_died: Vec<Entity> = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        if retained.contains_key(&spec.key) {
            // Hostile duplicate key: first occurrence wins.
            continue;
        }
        let (translation, scale) = viz_child_pose(index, count, spec.magnitude);
        if let Some(mut record) = root.children.remove(&spec.key) {
            if record.palette != spec.palette {
                record.palette = spec.palette;
                if let Some(mut material) = materials.get_mut(&record.material) {
                    material.base_color = spec.palette.color();
                }
            }
            record.base_translation = translation;
            record.base_scale = scale;
            if let Ok((mut transform, anim)) = children.get_mut(record.entity) {
                match anim {
                    // Data wins over presentation: a snapshot that
                    // re-asserts a key mid-`died` cancels the death
                    // animation and restores the child at its fresh rest
                    // pose and palette color. Without this the child stays
                    // gone until the *next* snapshot even though the data
                    // already brought it back (F5). Pairs with the effect
                    // loop treating a running `died` as terminal for later
                    // effects (F4): the snapshot is the one thing that may
                    // undo a death, and it undoes it now.
                    Some(anim) if anim.effect == VizEffectKind::Died => {
                        transform.translation = translation;
                        transform.scale = scale;
                        if let Some(mut material) = materials.get_mut(&record.material) {
                            material.base_color = spec.palette.color();
                        }
                        commands.entity(record.entity).remove::<VizEffectAnim>();
                        restored_from_died.push(record.entity);
                    }
                    // A non-terminal animation in flight: move its restore
                    // target so expiry lands on the fresh pose.
                    Some(mut anim) => {
                        anim.base_translation = translation;
                        anim.base_scale = scale;
                    }
                    None => {
                        if transform.translation != translation || transform.scale != scale {
                            transform.translation = translation;
                            transform.scale = scale;
                        }
                    }
                }
            }
            retained.insert(spec.key.clone(), record);
        } else {
            let record = spawn_viz_child(
                commands,
                root_entity,
                spec,
                translation,
                scale,
                materials,
                unit_mesh,
            );
            retained.insert(spec.key.clone(), record);
        }
    }
    // Keys the new snapshot no longer carries.
    for record in root.children.values() {
        commands.entity(record.entity).despawn();
    }
    root.children = retained;
    restored_from_died
}

/// The [`InlineAnchor`] a viz root lays out with, or `None` when the
/// visualization must be hidden (no live entry, or anchored nowhere —
/// unplaced or scrolled fully off the top).
fn viz_root_anchor(registry: &VizRegistry, viz_id: u32) -> Option<InlineAnchor> {
    let anchor = registry.get(viz_id)?.anchor?;
    Some(InlineAnchor {
        row: anchor.row,
        col: anchor.col,
        columns: u32::from(anchor.cols),
        rows: u32::from(anchor.rows),
        style: InlineStyle::default(),
    })
}

/// The terminal plane transform, disjoint from viz roots.
type VizPlaneQuery<'w, 's> =
    Query<'w, 's, &'static Transform, (With<TerminalPlane>, Without<VizObjectRoot>)>;

/// One running keyed-effect animation with everything the animator needs.
type VizAnimatedQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static ChildOf,
        &'static VizKeyedItem,
        &'static mut Transform,
        &'static mut VizEffectAnim,
        &'static MeshMaterial3d<StandardMaterial>,
    ),
>;

/// Per-frame positioning parameters for viz roots.
#[derive(SystemParam)]
pub(crate) struct VizSyncParams<'w, 's> {
    registry: Res<'w, VizRegistry>,
    terminal: Res<'w, TerminalSurface>,
    viewport: Res<'w, TerminalViewport>,
    presentation: Res<'w, TerminalPresentation>,
    mobius_transition: Res<'w, MobiusTransition>,
    plane_warp: Res<'w, TerminalPlaneWarp>,
    time: Res<'w, Time>,
    plane_query: VizPlaneQuery<'w, 's>,
    roots: Query<
        'w,
        's,
        (
            &'static VizObjectRoot,
            &'static mut Transform,
            &'static mut Visibility,
        ),
    >,
}

/// Positions every viz root from its registry anchor, per frame, the
/// [`sync_rgp_objects`] way: anchor → [`inline_layout`] → screen space in
/// [`TerminalPresentationMode::Flat2d`], or projected onto the active
/// terminal surface in the 3D modes. A visualization with no anchor (never
/// placed, or scrolled fully off the top) is hidden, payload intact, until
/// a later placing `viz.set` re-anchors it.
///
/// The root's scale is the anchored footprint in pixels; children lay out
/// in the root's normalized space ([`viz_child_pose`]), so a resize or
/// warp needs no child updates.
pub(crate) fn sync_viz_objects(mut params: VizSyncParams) {
    let VizSyncParams {
        registry,
        terminal,
        viewport,
        presentation,
        mobius_transition,
        plane_warp,
        time,
        plane_query,
        roots,
    } = &mut params;
    let cell_width = viewport.size.x / terminal.cols.max(1) as f32;
    let cell_height = viewport.size.y / terminal.rows.max(1) as f32;
    let elapsed_secs = time.elapsed_secs();
    let mobius_progress = active_mobius_progress(presentation.mode, mobius_transition);

    for (root, mut transform, mut visibility) in roots.iter_mut() {
        let Some(anchor) = viz_root_anchor(registry, root.viz_id) else {
            *visibility = Visibility::Hidden;
            continue;
        };
        let layout = inline_layout(&anchor, terminal, viewport, cell_width, cell_height);
        let scale = Vec3::new(
            layout.pixel_width.max(1.0),
            layout.pixel_height.max(1.0),
            cell_height.max(1.0),
        );
        match presentation.mode {
            TerminalPresentationMode::Flat2d => {
                transform.translation = Vec3::new(layout.center_x, layout.center_y, CURSOR_DEPTH);
                transform.rotation = Quat::IDENTITY;
                transform.scale = scale;
                *visibility = Visibility::Visible;
            }
            TerminalPresentationMode::Plane3d | TerminalPresentationMode::Mobius3d => {
                let Ok(plane_transform) = plane_query.single() else {
                    *visibility = Visibility::Hidden;
                    continue;
                };
                let local_position = plane_surface_point(
                    presentation.mode,
                    layout.local_x,
                    layout.local_y,
                    plane_warp.amount,
                    elapsed_secs,
                    8.0,
                    mobius_progress,
                );
                transform.translation = plane_transform.transform_point(local_position);
                transform.rotation = plane_transform.rotation;
                transform.scale = scale;
                *visibility = Visibility::Visible;
            }
        }
    }
}

/// Advances every running keyed-effect animation and expires it after
/// [`VIZ_EFFECT_SECONDS`]: `died` despawns the child and drops its ledger
/// entry (until a later snapshot re-adds the key); every other effect
/// restores the ledger's base pose and palette color exactly. Poses and
/// colors are computed from the animation's stored base each frame, never
/// incrementally, so a hostile effect stream cannot accumulate drift.
pub(crate) fn animate_viz_effects(
    time: Res<Time>,
    mut commands: Commands,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut roots: Query<&mut VizObjectRoot>,
    mut animated: VizAnimatedQuery,
) {
    let delta = time.delta_secs();
    for (entity, child_of, keyed, mut transform, mut anim, material) in animated.iter_mut() {
        anim.elapsed += delta;
        let palette = roots
            .get(child_of.parent())
            .ok()
            .and_then(|root| root.children.get(&keyed.key))
            .map(|record| record.palette)
            .unwrap_or(VizPaletteSlot::Neutral);
        if anim.elapsed >= VIZ_EFFECT_SECONDS {
            if anim.effect == VizEffectKind::Died {
                // Confirmed gone: drop the child until a later snapshot
                // re-adds the key.
                if let Ok(mut root) = roots.get_mut(child_of.parent()) {
                    root.children.remove(&keyed.key);
                }
                commands.entity(entity).despawn();
            } else {
                transform.translation = anim.base_translation;
                transform.scale = anim.base_scale;
                if let Some(mut material) = materials.get_mut(&material.0) {
                    material.base_color = palette.color();
                }
                commands.entity(entity).remove::<VizEffectAnim>();
            }
            continue;
        }
        let t = anim.elapsed / VIZ_EFFECT_SECONDS;
        let (translation, scale) =
            viz_effect_pose(anim.effect, anim.base_translation, anim.base_scale, t);
        transform.translation = translation;
        transform.scale = scale;
        if let Some(mut material) = materials.get_mut(&material.0) {
            material.base_color = viz_effect_color(anim.effect, palette.color(), t);
        }
    }
}

fn extrude_mesh(mesh: Mesh, depth: f32) -> Mesh {
    if depth <= 0.0 {
        return mesh;
    }

    let Some(VertexAttributeValues::Float32x3(source_positions)) =
        mesh.attribute(Mesh::ATTRIBUTE_POSITION)
    else {
        return mesh;
    };
    // `depth` is meant to give thickness to flat artwork. Applying the same extrusion to meshes
    // that already have volume creates overlapping surfaces and unstable depth ordering.
    let mut min_z = f32::INFINITY;
    let mut max_z = f32::NEG_INFINITY;
    for &[_, _, z] in source_positions {
        min_z = min_z.min(z);
        max_z = max_z.max(z);
    }
    if (max_z - min_z).abs() > 1e-4 {
        return mesh;
    }
    let Some(indices) = mesh.indices() else {
        return mesh;
    };

    let indices = match indices {
        Indices::U16(values) => values.iter().map(|&value| value as u32).collect::<Vec<_>>(),
        Indices::U32(values) => values.clone(),
    };
    if indices.len() < 3 {
        return mesh;
    }

    let thickness = depth * 0.03;
    let half = thickness * 0.5;
    let source_len = source_positions.len() as u32;

    let mut positions = Vec::<[f32; 3]>::with_capacity(source_positions.len() * 2);
    let mut normals = Vec::<[f32; 3]>::with_capacity(source_positions.len() * 2);

    for &[x, y, z] in source_positions {
        positions.push([x, y, z + half]);
        normals.push([0.0, 0.0, 1.0]);
    }
    for &[x, y, z] in source_positions {
        positions.push([x, y, z - half]);
        normals.push([0.0, 0.0, -1.0]);
    }

    let mut out_indices = Vec::<u32>::with_capacity(indices.len() * 4);
    for triangle in indices.chunks_exact(3) {
        out_indices.extend_from_slice(triangle);
        out_indices.extend_from_slice(&[
            triangle[2] + source_len,
            triangle[1] + source_len,
            triangle[0] + source_len,
        ]);
    }

    let mut edge_counts = HashMap::<(u32, u32), u32>::new();
    for triangle in indices.chunks_exact(3) {
        for edge in [
            (triangle[0], triangle[1]),
            (triangle[1], triangle[2]),
            (triangle[2], triangle[0]),
        ] {
            let key = if edge.0 < edge.1 {
                edge
            } else {
                (edge.1, edge.0)
            };
            *edge_counts.entry(key).or_insert(0) += 1;
        }
    }

    for ((a, b), count) in edge_counts {
        if count != 1 {
            continue;
        }

        let front_a = source_positions[a as usize];
        let front_b = source_positions[b as usize];
        let edge = Vec3::new(
            front_b[0] - front_a[0],
            front_b[1] - front_a[1],
            front_b[2] - front_a[2],
        );
        let side_normal = Vec3::new(edge.y, -edge.x, 0.0).normalize_or_zero();

        let base = positions.len() as u32;
        positions.extend_from_slice(&[
            [front_a[0], front_a[1], front_a[2] + half],
            [front_b[0], front_b[1], front_b[2] + half],
            [front_b[0], front_b[1], front_b[2] - half],
            [front_a[0], front_a[1], front_a[2] - half],
        ]);
        for _ in 0..4 {
            normals.push([side_normal.x, side_normal.y, side_normal.z]);
        }
        out_indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    Mesh::new(PrimitiveTopology::TriangleList, Default::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_indices(Indices::U32(out_indices))
}

/// Animates the terminal plane warp.
///
/// This updates the front and back meshes stored in [`TerminalPlaneMeshes`]. It is independent of
/// the redraw path and only mutates mesh vertex positions, so plane presentation can keep moving
/// even when the terminal contents are otherwise static.
pub fn animate_terminal_plane_warp(
    time: Res<Time>,
    presentation: Res<TerminalPresentation>,
    mobius_transition: Res<MobiusTransition>,
    warp: Res<TerminalPlaneWarp>,
    plane_meshes: Res<TerminalPlaneMeshes>,
    mut meshes: ResMut<Assets<Mesh>>,
) {
    if presentation.mode == TerminalPresentationMode::Flat2d {
        return;
    }

    let needs_update = match presentation.mode {
        TerminalPresentationMode::Flat2d => false,
        TerminalPresentationMode::Plane3d => {
            presentation.is_changed() || warp.is_changed() || warp.amount > 0.0
        }
        // Reapply the strip every frame so mode switches and time-based motion are visible.
        TerminalPresentationMode::Mobius3d => true,
    };
    if !needs_update {
        return;
    }

    let pulse = warp.amount * (0.96 + 0.04 * (time.elapsed_secs() * 2.2).sin());
    let mobius_progress = active_mobius_progress(presentation.mode, &mobius_transition);
    apply_plane_warp(
        meshes.get_mut(&plane_meshes.front),
        presentation.mode,
        pulse,
        time.elapsed_secs(),
        -1.0,
        mobius_progress,
    );
    apply_plane_warp(
        meshes.get_mut(&plane_meshes.back),
        presentation.mode,
        pulse,
        time.elapsed_secs(),
        1.0,
        mobius_progress,
    );
}

/// Advances the Mobius transition and restores normal 3D interaction when it completes.
pub fn animate_mobius_transition(
    time: Res<Time>,
    mut presentation: ResMut<TerminalPresentation>,
    mut mobius_transition: ResMut<MobiusTransition>,
    mut plane_view: ResMut<TerminalPlaneView>,
    mut redraw: ResMut<TerminalRedrawState>,
) {
    if presentation.mode != TerminalPresentationMode::Mobius3d {
        mobius_transition.stop();
        return;
    }

    if !mobius_transition.active {
        return;
    }

    mobius_transition.elapsed_secs += time.delta_secs();
    redraw.request();

    if mobius_transition.finished() {
        plane_view.zoom = mobius_transition.end_zoom.max(0.1);
        if mobius_transition.direction == crate::scene::MobiusTransitionDirection::Exiting {
            plane_view.yaw = mobius_transition.source_yaw;
            plane_view.pitch = mobius_transition.source_pitch;
            plane_view.camera_offset = mobius_transition.source_camera_offset;
            presentation.mode = mobius_transition.source_mode;
        }
        mobius_transition.stop();
        redraw.request();
    }
}

/// Applies stage updates queued from RGP `c` sequences to the presentation
/// resources, mirroring the keyboard and web-control semantics. Mode changes
/// dispatch instantly (the Möbius transition owns its own clock); the other
/// fields apply instantly or start a [`StageTween`] when `dur` is set.
pub fn apply_rgp_stage(
    mut inline_objects: ResMut<TerminalInlineObjects>,
    mut presentation: ResMut<TerminalPresentation>,
    mut plane_warp: ResMut<TerminalPlaneWarp>,
    mut plane_view: ResMut<TerminalPlaneView>,
    mut mobius_transition: ResMut<MobiusTransition>,
    mut stage_tween: ResMut<StageTween>,
    mut redraw: ResMut<TerminalRedrawState>,
) {
    for update in inline_objects.take_stage_updates() {
        let mut applied = false;

        if let Some(mode) = update.mode {
            let target = match mode {
                RgpStageMode::Flat2d => TerminalPresentationMode::Flat2d,
                RgpStageMode::Plane3d => TerminalPresentationMode::Plane3d,
                RgpStageMode::Mobius3d => TerminalPresentationMode::Mobius3d,
            };
            if apply_stage_mode_change(
                target,
                &mut presentation,
                &plane_view,
                &mut mobius_transition,
            ) {
                // A mode change is a scene cut: it cancels any camera tween.
                stage_tween.stop();
                applied = true;
            }
        }

        // While the Möbius transition owns the camera, view fields are
        // dropped (mirroring the mouse gate); warp is never gated.
        let camera_gated = mobius_transition.active;
        let warp = update.warp.map(|value| value.clamp(0.0, 1.0));
        let yaw = if camera_gated { None } else { update.yaw };
        let pitch = if camera_gated { None } else { update.pitch };
        let zoom = if camera_gated {
            None
        } else {
            update.zoom.map(|value| value.clamp(0.1, 4.0))
        };

        if warp.is_some() || yaw.is_some() || pitch.is_some() || zoom.is_some() {
            let duration = update.dur.unwrap_or(0.0);
            if duration > 0.0 {
                // A new tween replaces the previous one wholesale and
                // retargets from the current live values.
                *stage_tween = StageTween {
                    active: true,
                    elapsed_secs: 0.0,
                    duration_secs: duration,
                    ease: update.ease.unwrap_or_default(),
                    warp: warp.map(|end| StageChannel {
                        start: plane_warp.amount,
                        end,
                    }),
                    yaw: yaw.map(|end| StageChannel {
                        start: plane_view.yaw,
                        end,
                    }),
                    pitch: pitch.map(|end| StageChannel {
                        start: plane_view.pitch,
                        end,
                    }),
                    zoom: zoom.map(|end| StageChannel {
                        start: plane_view.zoom,
                        end,
                    }),
                };
            } else {
                if stage_tween.active {
                    stage_tween.stop();
                }
                if let Some(value) = warp {
                    plane_warp.amount = value;
                }
                if let Some(value) = yaw {
                    plane_view.yaw = value;
                }
                if let Some(value) = pitch {
                    plane_view.pitch = value;
                }
                if let Some(value) = zoom {
                    plane_view.zoom = value;
                }
            }
            applied = true;
        }

        if applied {
            redraw.request();
        }
    }
}

/// Advances the stage tween, feeding the warp and camera resources every
/// frame so the change-driven presentation systems keep firing.
pub fn animate_stage_tween(
    time: Res<Time>,
    mut stage_tween: ResMut<StageTween>,
    mut plane_warp: ResMut<TerminalPlaneWarp>,
    mut plane_view: ResMut<TerminalPlaneView>,
    mut redraw: ResMut<TerminalRedrawState>,
) {
    if !stage_tween.active {
        return;
    }

    stage_tween.elapsed_secs += time.delta_secs();
    let progress = (stage_tween.elapsed_secs / stage_tween.duration_secs).clamp(0.0, 1.0);
    let eased = stage_tween.ease.apply(progress);

    if let Some(channel) = stage_tween.warp {
        plane_warp.amount = channel.sample(eased).clamp(0.0, 1.0);
    }
    if let Some(channel) = stage_tween.yaw {
        plane_view.yaw = channel.sample(eased);
    }
    if let Some(channel) = stage_tween.pitch {
        plane_view.pitch = channel.sample(eased);
    }
    if let Some(channel) = stage_tween.zoom {
        plane_view.zoom = channel.sample(eased).clamp(0.1, 4.0);
    }
    redraw.request();

    if progress >= 1.0 {
        // Eased progress is exactly 1.0 at the end for every curve, so the
        // final write above landed the exact target values.
        stage_tween.stop();
    }
}

fn active_mobius_progress(
    mode: TerminalPresentationMode,
    mobius_transition: &MobiusTransition,
) -> f32 {
    if mode != TerminalPresentationMode::Mobius3d {
        return 0.0;
    }

    if mobius_transition.active {
        mobius_transition.morph_progress()
    } else {
        1.0
    }
}

fn apply_plane_warp(
    mesh: Option<AssetMut<'_, Mesh>>,
    mode: TerminalPresentationMode,
    pulse: f32,
    elapsed_secs: f32,
    direction: f32,
    mobius_progress: f32,
) {
    let Some(mut mesh) = mesh else {
        return;
    };
    let Some(VertexAttributeValues::Float32x2(uvs)) = mesh.attribute(Mesh::ATTRIBUTE_UV_0) else {
        return;
    };
    let uvs = uvs.clone();
    let Some(VertexAttributeValues::Float32x3(positions)) =
        mesh.attribute_mut(Mesh::ATTRIBUTE_POSITION)
    else {
        return;
    };

    for (position, uv) in positions.iter_mut().zip(uvs.iter()) {
        let x = uv[0] - 0.5;
        let y = 0.5 - uv[1];
        let point = plane_surface_point(mode, x, y, pulse, elapsed_secs, 0.0, mobius_progress);
        position[0] = point.x;
        position[1] = point.y;
        position[2] = match mode {
            TerminalPresentationMode::Plane3d => point.z * direction,
            TerminalPresentationMode::Flat2d | TerminalPresentationMode::Mobius3d => point.z,
        };
    }
}

/// Cursor synchronization parameters.
#[derive(SystemParam)]
pub(crate) struct CursorSyncParams<'w, 's> {
    app_config: Res<'w, AppConfig>,
    cursor_settings: Res<'w, CursorSettings>,
    runtime: Res<'w, TerminalRuntime>,
    terminal: Res<'w, TerminalSurface>,
    viewport: Res<'w, TerminalViewport>,
    presentation: Res<'w, TerminalPresentation>,
    mobius_transition: Res<'w, MobiusTransition>,
    plane_warp: Res<'w, TerminalPlaneWarp>,
    time: Res<'w, Time>,
    plane_query: Query<'w, 's, &'static Transform, (With<TerminalPlane>, Without<CursorModel>)>,
    query: CursorTransformQuery<'w, 's>,
}

/// Synchronizes the 3D cursor model with the terminal cursor.
///
/// This runs after [`render_terminal_widget`], once the cursor model has been spawned and the latest
/// terminal cursor position is available from [`TerminalRuntime`]. It updates the [`CursorModel`]
/// transform and visibility for both 2D and 3D presentation modes.
///
/// In 3D mode the cursor model is positioned relative to the current [`TerminalPlane`] transform
/// and warp amount.
pub(crate) fn sync_asset_to_terminal_cursor(mut params: CursorSyncParams) {
    let CursorSyncParams {
        app_config,
        cursor_settings,
        runtime,
        terminal,
        viewport,
        presentation,
        mobius_transition,
        plane_warp,
        time,
        plane_query,
        query,
    } = &mut params;
    if query.is_empty() {
        return;
    }

    let pose_ctx = CursorPoseContext {
        runtime,
        terminal,
        viewport,
        mode: presentation.mode,
        plane_warp_amount: plane_warp.amount,
        mobius_progress: active_mobius_progress(presentation.mode, mobius_transition),
        elapsed_secs: time.elapsed_secs(),
        plane_query,
    };
    let (translation, rotation, scale, cursor_visibility) =
        cursor_pose(app_config, cursor_settings, &pose_ctx);
    for (mut transform, mut visibility) in query.iter_mut() {
        transform.translation = translation;
        transform.rotation = rotation;
        transform.scale = Vec3::splat(scale.max(0.001));
        *visibility = cursor_visibility;
    }
}

fn cursor_pose(
    app_config: &AppConfig,
    settings: &CursorSettings,
    ctx: &CursorPoseContext<'_, '_, '_>,
) -> (Vec3, Quat, f32, Visibility) {
    let cols = ctx.terminal.cols.max(1) as f32;
    let rows = ctx.terminal.rows.max(1) as f32;
    let cell_width = ctx.viewport.size.x / cols;
    let cell_height = ctx.viewport.size.y / rows;
    let scale = cell_width.min(cell_height) * app_config.cursor.model.scale_factor;

    let screen = ctx.runtime.parser.screen();
    let (cursor_row, cursor_col) = screen.cursor_position();
    let cursor_col = cursor_col.min(ctx.terminal.cols.saturating_sub(1)) as f32;
    let cursor_row = cursor_row.min(ctx.terminal.rows.saturating_sub(1)) as f32;

    let cursor_x = cursor_col + 0.5 + app_config.cursor.model.x_offset;
    let local_x = ctx.viewport.center.x - ctx.viewport.size.x * 0.5 + cursor_x * cell_width;
    let local_y =
        ctx.viewport.center.y + ctx.viewport.size.y * 0.5 - (cursor_row + 0.5) * cell_height;
    let spin = ctx.elapsed_secs * settings.spin_speed;
    let bob = (ctx.elapsed_secs * settings.bob_speed).sin() * cell_height * settings.bob_amplitude;
    let plane_bob = if ctx.viewport.size.y > 0.0 {
        bob / ctx.viewport.size.y
    } else {
        0.0
    };

    let (translation, rotation, visibility) = match ctx.mode {
        TerminalPresentationMode::Flat2d => (
            Vec3::new(local_x, local_y + bob, CURSOR_DEPTH),
            Quat::from_rotation_y(spin) * Quat::from_rotation_x(-0.25),
            if !settings.visible || screen.hide_cursor() {
                Visibility::Hidden
            } else {
                Visibility::Visible
            },
        ),
        TerminalPresentationMode::Plane3d | TerminalPresentationMode::Mobius3d => {
            let Ok(plane_transform) = ctx.plane_query.single() else {
                return (Vec3::ZERO, Quat::IDENTITY, scale, Visibility::Hidden);
            };
            let plane_local_x = cursor_x / cols - 0.5;
            let plane_local_y = 0.5 - (cursor_row + 0.5) / rows + plane_bob;
            let local_position = plane_surface_point(
                ctx.mode,
                plane_local_x,
                plane_local_y,
                ctx.plane_warp_amount,
                ctx.elapsed_secs,
                app_config.cursor.model.plane_offset,
                ctx.mobius_progress,
            );
            (
                plane_transform.transform_point(local_position),
                plane_transform.rotation
                    * (Quat::from_rotation_y(spin) * Quat::from_rotation_x(-0.25)),
                if settings.visible {
                    Visibility::Visible
                } else {
                    Visibility::Hidden
                },
            )
        }
    };

    (translation, rotation, scale, visibility)
}

fn plane_surface_z(local_x: f32, local_y: f32, warp_amount: f32, elapsed_secs: f32) -> f32 {
    if warp_amount <= 0.0 {
        return 0.0;
    }

    let pulse = warp_amount * (0.96 + 0.04 * (elapsed_secs * 2.2).sin());
    let radius = (local_x * local_x + local_y * local_y).sqrt();
    let core = (-radius * 9.0).exp();
    let ring = (-(radius - 0.22).powi(2) * 18.0).exp();
    -(core * 360.0 + ring * 72.0) * pulse
}

fn plane_surface_point(
    mode: TerminalPresentationMode,
    local_x: f32,
    local_y: f32,
    warp_amount: f32,
    elapsed_secs: f32,
    depth_offset: f32,
    mobius_progress: f32,
) -> Vec3 {
    match mode {
        TerminalPresentationMode::Flat2d => Vec3::new(local_x, local_y, depth_offset),
        TerminalPresentationMode::Plane3d => Vec3::new(
            local_x,
            local_y,
            plane_surface_z(local_x, local_y, warp_amount, elapsed_secs) + depth_offset,
        ),
        TerminalPresentationMode::Mobius3d => {
            let source_point = Vec3::new(local_x, local_y, depth_offset);
            let target_point =
                mobius_surface_point(local_x, local_y, warp_amount, elapsed_secs, depth_offset);
            source_point.lerp(target_point, mobius_progress)
        }
    }
}

fn mobius_surface_point(
    local_x: f32,
    local_y: f32,
    warp_amount: f32,
    elapsed_secs: f32,
    depth_offset: f32,
) -> Vec3 {
    let twist = 1.0 + warp_amount * 0.06 * (elapsed_secs * 0.7).sin();
    let angle = (local_x + 0.5) * std::f32::consts::TAU;
    let radius = 0.24 + warp_amount * 0.015;
    let width = local_y * (0.42 + warp_amount * 0.04);
    let half_angle = angle * 0.5 * twist;
    let cos_half = half_angle.cos();
    let sin_half = half_angle.sin();
    let ring = radius + width * cos_half;

    Vec3::new(
        ring * angle.cos(),
        ring * angle.sin(),
        width * sin_half * 320.0 + depth_offset,
    )
}

#[cfg(test)]
mod viz_render_tests {
    use super::*;
    use crate::viz::{PsItem, PsV1, VizAnchor, VizCapture, VizPayload};
    use std::time::Duration;

    const ID: u32 = 0x8000_0001;

    fn ps_payload(items: &[(u32, f32, &str)]) -> VizPayload {
        VizPayload::Ps(PsV1 {
            capture: VizCapture {
                source: "test/synthetic".to_string(),
                ts: "2026-07-22T00:00:00Z".to_string(),
            },
            items: items
                .iter()
                .map(|(pid, cpu, state)| PsItem {
                    pid: *pid,
                    name: format!("proc{pid}"),
                    cpu: *cpu,
                    mem: 0,
                    state: (*state).to_string(),
                })
                .collect(),
        })
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.init_resource::<VizRegistry>();
        app.init_resource::<Assets<Mesh>>();
        app.init_resource::<Assets<StandardMaterial>>();
        app.init_resource::<Time>();
        // The runtime order: rebuild (spawn/diff/effect lowering), then the
        // animation pass, with a command flush between them.
        app.add_systems(Update, (rebuild_viz_objects, animate_viz_effects).chain());
        app
    }

    fn registry_mut(app: &mut App) -> Mut<'_, VizRegistry> {
        app.world_mut().resource_mut::<VizRegistry>()
    }

    fn keyed_children(app: &mut App) -> HashMap<String, Entity> {
        let world = app.world_mut();
        let mut query = world.query::<(Entity, &VizKeyedItem)>();
        query
            .iter(world)
            .map(|(entity, keyed)| (keyed.key.clone(), entity))
            .collect()
    }

    fn root_entity(app: &mut App, id: u32) -> Option<Entity> {
        let world = app.world_mut();
        let mut query = world.query::<(Entity, &VizObjectRoot)>();
        query
            .iter(world)
            .find(|(_, root)| root.viz_id == id)
            .map(|(entity, _)| entity)
    }

    fn ledger(app: &mut App, id: u32) -> HashMap<String, VizChildRecord> {
        let world = app.world_mut();
        let mut query = world.query::<&VizObjectRoot>();
        query
            .iter(world)
            .find(|root| root.viz_id == id)
            .map(|root| root.children.clone())
            .unwrap_or_default()
    }

    fn material_count(app: &App) -> usize {
        app.world()
            .resource::<Assets<StandardMaterial>>()
            .iter()
            .count()
    }

    fn mesh_count(app: &App) -> usize {
        app.world().resource::<Assets<Mesh>>().iter().count()
    }

    /// Advances the clock by `seconds` and runs one frame.
    fn advance(app: &mut App, seconds: f32) {
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(Duration::from_secs_f32(seconds));
        app.update();
    }

    // ── Diff logic ──

    #[test]
    fn diff_keeps_unchanged_children_and_touches_only_changes() {
        let mut app = test_app();
        registry_mut(&mut app).upsert(
            ID,
            ps_payload(&[(1, 50.0, "running"), (2, 10.0, "sleeping")]),
            None,
        );
        app.update();
        let first = keyed_children(&mut app);
        assert_eq!(first.len(), 2);
        assert_eq!(mesh_count(&app), 1, "one shared unit mesh for everything");
        assert_eq!(material_count(&app), 2, "one bespoke material per child");

        // A watcher refresh: pid 2 gone, pid 3 new, pid 1 unchanged.
        registry_mut(&mut app).upsert(
            ID,
            ps_payload(&[(1, 50.0, "running"), (3, 5.0, "sleeping")]),
            None,
        );
        app.update();
        let second = keyed_children(&mut app);
        assert_eq!(second.len(), 2);
        assert_eq!(
            second.get("1"),
            first.get("1"),
            "an unchanged key keeps its entity"
        );
        assert!(second.contains_key("3"));
        assert!(
            app.world().get_entity(first["2"]).is_err(),
            "the dropped key's child despawned"
        );
        assert_eq!(mesh_count(&app), 1, "rebuilds never allocate meshes");
        assert_eq!(
            material_count(&app),
            3,
            "only the added child allocated a material"
        );

        // An identical refresh is a no-op on the tree.
        registry_mut(&mut app).upsert(
            ID,
            ps_payload(&[(1, 50.0, "running"), (3, 5.0, "sleeping")]),
            None,
        );
        app.update();
        assert_eq!(keyed_children(&mut app), second);
        assert_eq!(material_count(&app), 3);
    }

    #[test]
    fn palette_change_recolors_the_material_in_place() {
        let mut app = test_app();
        registry_mut(&mut app).upsert(ID, ps_payload(&[(1, 50.0, "running")]), None);
        app.update();
        let before = ledger(&mut app, ID)["1"].clone();
        assert_eq!(before.palette, VizPaletteSlot::Active);

        registry_mut(&mut app).upsert(ID, ps_payload(&[(1, 50.0, "zombie")]), None);
        app.update();
        let after = ledger(&mut app, ID)["1"].clone();
        assert_eq!(after.entity, before.entity, "same entity");
        assert_eq!(after.material, before.material, "same material handle");
        assert_eq!(after.palette, VizPaletteSlot::Alert);
        let materials = app.world().resource::<Assets<StandardMaterial>>();
        let material = materials.get(&after.material).expect("material lives");
        assert_eq!(material.base_color, VizPaletteSlot::Alert.color());
        assert_eq!(material_count(&app), 1, "recolored, not reallocated");
    }

    /// F6: a hostile snapshot may repeat a domain key. The renderer is
    /// first-occurrence-wins, so the tree and ledger hold exactly one child
    /// per distinct key, and repeated refreshes stay stable — while the
    /// registry payload's `item_count` still reports the raw count (the
    /// read-back never hides that the wire carried duplicates).
    #[test]
    fn duplicate_keys_render_first_wins_with_a_stable_ledger() {
        let mut app = test_app();
        registry_mut(&mut app).upsert(
            ID,
            ps_payload(&[
                (1, 50.0, "running"),
                (1, 10.0, "sleeping"),
                (2, 5.0, "sleeping"),
            ]),
            None,
        );
        app.update();
        let children = keyed_children(&mut app);
        assert_eq!(children.len(), 2, "one child per distinct key");
        assert!(children.contains_key("1") && children.contains_key("2"));
        assert_eq!(
            ledger(&mut app, ID).len(),
            2,
            "the ledger matches the tree, no divergence"
        );
        assert_eq!(
            registry_mut(&mut app)
                .get(ID)
                .expect("live")
                .payload
                .item_count(),
            3,
            "item_count is the raw payload count, duplicates included"
        );

        // A refresh keeping the duplicate is a stable no-op on the tree:
        // the same first-occurrence keys map to the same entities.
        registry_mut(&mut app).upsert(
            ID,
            ps_payload(&[
                (1, 50.0, "running"),
                (1, 99.0, "zombie"),
                (2, 5.0, "sleeping"),
            ]),
            None,
        );
        app.update();
        assert_eq!(
            keyed_children(&mut app),
            children,
            "keys stay stable across a duplicate-bearing refresh"
        );
    }

    #[test]
    fn removal_and_reset_despawn_whole_trees() {
        let mut app = test_app();
        registry_mut(&mut app).upsert(ID, ps_payload(&[(1, 50.0, "running")]), None);
        registry_mut(&mut app).upsert(ID + 1, ps_payload(&[(2, 10.0, "sleeping")]), None);
        app.update();
        assert!(root_entity(&mut app, ID).is_some());
        assert!(root_entity(&mut app, ID + 1).is_some());

        assert!(registry_mut(&mut app).remove(ID));
        app.update();
        assert!(
            root_entity(&mut app, ID).is_none(),
            "removed tree despawned"
        );
        assert!(
            root_entity(&mut app, ID + 1).is_some(),
            "the other tree is untouched"
        );
        assert_eq!(keyed_children(&mut app).len(), 1);

        registry_mut(&mut app).clear_all();
        app.update();
        assert!(
            root_entity(&mut app, ID + 1).is_none(),
            "reset despawns everything"
        );
        assert!(keyed_children(&mut app).is_empty());
    }

    // ── Effect expiry ──

    #[test]
    fn died_effect_shrinks_then_removes_the_child_until_the_next_snapshot() {
        let mut app = test_app();
        registry_mut(&mut app).upsert(ID, ps_payload(&[(1, 50.0, "running")]), None);
        app.update();
        let child = keyed_children(&mut app)["1"];
        let base_scale = ledger(&mut app, ID)["1"].base_scale;

        assert!(registry_mut(&mut app).queue_effect(ID, "1".to_string(), VizEffectKind::Died));
        app.update();
        assert!(
            app.world().get::<VizEffectAnim>(child).is_some(),
            "the effect lowered onto the child"
        );

        advance(&mut app, VIZ_EFFECT_SECONDS * 0.5);
        let transform = app
            .world()
            .get::<Transform>(child)
            .expect("alive mid-animation");
        assert!(
            (transform.scale - base_scale * 0.5).length() < 1e-4,
            "halfway through the shrink"
        );

        advance(&mut app, VIZ_EFFECT_SECONDS);
        assert!(
            app.world().get_entity(child).is_err(),
            "the died child despawned at expiry"
        );
        assert!(
            !ledger(&mut app, ID).contains_key("1"),
            "its ledger entry dropped with it"
        );
        assert!(root_entity(&mut app, ID).is_some(), "the root survives");

        // A later snapshot honestly re-adds the key.
        registry_mut(&mut app).upsert(ID, ps_payload(&[(1, 50.0, "running")]), None);
        app.update();
        assert!(keyed_children(&mut app).contains_key("1"));
    }

    #[test]
    fn non_died_effects_expire_restoring_base_pose_and_color() {
        let mut app = test_app();
        registry_mut(&mut app).upsert(ID, ps_payload(&[(1, 50.0, "running")]), None);
        app.update();
        let child = keyed_children(&mut app)["1"];
        let record = ledger(&mut app, ID)["1"].clone();

        assert!(registry_mut(&mut app).queue_effect(ID, "1".to_string(), VizEffectKind::Highlight));
        app.update();
        advance(&mut app, VIZ_EFFECT_SECONDS * 0.5);
        let transform = app.world().get::<Transform>(child).expect("alive");
        assert!(
            transform.scale != record.base_scale,
            "mid-pulse the child is swollen"
        );
        let materials = app.world().resource::<Assets<StandardMaterial>>();
        let mid_color = materials.get(&record.material).expect("lives").base_color;
        assert_ne!(
            mid_color,
            VizPaletteSlot::Active.color(),
            "mid-flash the color moved"
        );

        advance(&mut app, VIZ_EFFECT_SECONDS);
        assert!(
            app.world().get::<VizEffectAnim>(child).is_none(),
            "the animation expired"
        );
        let transform = app.world().get::<Transform>(child).expect("alive");
        assert_eq!(transform.translation, record.base_translation);
        assert_eq!(transform.scale, record.base_scale);
        let materials = app.world().resource::<Assets<StandardMaterial>>();
        let color = materials.get(&record.material).expect("lives").base_color;
        assert_eq!(
            color,
            VizPaletteSlot::Active.color(),
            "the palette color is restored exactly"
        );
    }

    #[test]
    fn effects_on_absent_keys_drain_and_render_nothing() {
        let mut app = test_app();
        registry_mut(&mut app).upsert(ID, ps_payload(&[(1, 50.0, "running")]), None);
        app.update();
        assert!(registry_mut(&mut app).queue_effect(ID, "999".to_string(), VizEffectKind::Died));
        app.update();
        {
            let world = app.world_mut();
            let mut query = world.query::<&VizEffectAnim>();
            assert_eq!(query.iter(world).count(), 0, "nothing animates");
        }
        assert!(
            registry_mut(&mut app)
                .get(ID)
                .expect("live")
                .pending_effects
                .is_empty(),
            "the queue still drained"
        );
    }

    #[test]
    fn a_same_frame_set_and_effect_lands_on_the_fresh_tree() {
        let mut app = test_app();
        {
            let mut registry = registry_mut(&mut app);
            registry.upsert(ID, ps_payload(&[(1, 50.0, "running")]), None);
            assert!(registry.queue_effect(ID, "1".to_string(), VizEffectKind::Highlight));
        }
        app.update();
        let child = keyed_children(&mut app)["1"];
        assert!(
            app.world().get::<VizEffectAnim>(child).is_some(),
            "the effect resolved against the just-spawned ledger"
        );
    }

    /// F4: a running `died` animation is terminal for presentation — a
    /// later effect on the same key must not replace it and resurrect the
    /// confirmed-dead bar. The death runs to completion.
    #[test]
    fn a_later_effect_cannot_resurrect_a_dying_child() {
        let mut app = test_app();
        registry_mut(&mut app).upsert(ID, ps_payload(&[(1, 50.0, "running")]), None);
        app.update();
        let child = keyed_children(&mut app)["1"];

        // The kill watcher confirms the death.
        assert!(registry_mut(&mut app).queue_effect(ID, "1".to_string(), VizEffectKind::Died));
        app.update();
        let anim = app
            .world()
            .get::<VizEffectAnim>(child)
            .expect("the death is animating");
        assert_eq!(anim.effect, VizEffectKind::Died);

        // A later effect on the same key must be dropped, not applied.
        assert!(registry_mut(&mut app).queue_effect(ID, "1".to_string(), VizEffectKind::Highlight));
        app.update();
        let anim = app
            .world()
            .get::<VizEffectAnim>(child)
            .expect("the child is still dying");
        assert_eq!(
            anim.effect,
            VizEffectKind::Died,
            "a later effect never cancels a confirmed death"
        );
        assert!(
            registry_mut(&mut app)
                .get(ID)
                .expect("live")
                .pending_effects
                .is_empty(),
            "the dropped effect drained from the queue"
        );

        // The confirmed death still lands at its own expiry.
        advance(&mut app, VIZ_EFFECT_SECONDS);
        assert!(
            app.world().get_entity(child).is_err(),
            "the death completes despite the intervening effect"
        );
    }

    /// F5: data wins over presentation — a snapshot that re-asserts a key
    /// mid-`died` cancels the death animation and restores the child now,
    /// rather than waiting for the following snapshot. Coherent with F4:
    /// only a snapshot may undo a death.
    #[test]
    fn a_snapshot_reassert_cancels_a_running_died_animation() {
        let mut app = test_app();
        registry_mut(&mut app).upsert(ID, ps_payload(&[(1, 50.0, "running")]), None);
        app.update();
        let child = keyed_children(&mut app)["1"];
        let base = ledger(&mut app, ID)["1"].clone();

        assert!(registry_mut(&mut app).queue_effect(ID, "1".to_string(), VizEffectKind::Died));
        app.update();
        advance(&mut app, VIZ_EFFECT_SECONDS * 0.5);
        assert!(
            app.world().get::<VizEffectAnim>(child).is_some(),
            "the child is mid-death"
        );

        // A fresh snapshot re-asserts the key. Advance by zero so the death
        // cannot expire on its own: only the re-assert may end it.
        registry_mut(&mut app).upsert(ID, ps_payload(&[(1, 50.0, "running")]), None);
        advance(&mut app, 0.0);
        assert!(
            app.world().get::<VizEffectAnim>(child).is_none(),
            "the re-assert cancelled the death animation"
        );
        assert_eq!(
            keyed_children(&mut app).get("1"),
            Some(&child),
            "the same child is kept, not respawned"
        );
        let transform = app
            .world()
            .get::<Transform>(child)
            .expect("restored, alive");
        assert_eq!(
            transform.scale, base.base_scale,
            "the rest scale is restored"
        );
        let materials = app.world().resource::<Assets<StandardMaterial>>();
        let color = materials.get(&base.material).expect("lives").base_color;
        assert_eq!(
            color,
            VizPaletteSlot::Active.color(),
            "the palette color is restored — the death darkening is undone"
        );

        // It does not despawn at the original death's would-be expiry.
        advance(&mut app, VIZ_EFFECT_SECONDS);
        assert!(
            keyed_children(&mut app).contains_key("1"),
            "the restored child survives"
        );
    }

    // ── Anchor visibility ──

    #[test]
    fn viz_root_anchor_hides_unplaced_and_dead_visualizations() {
        let mut registry = VizRegistry::default();
        assert!(
            viz_root_anchor(&registry, ID).is_none(),
            "no entry means hidden"
        );
        registry.upsert(ID, ps_payload(&[(1, 50.0, "running")]), None);
        assert!(
            viz_root_anchor(&registry, ID).is_none(),
            "unplaced stays hidden"
        );
        registry.upsert(
            ID,
            ps_payload(&[(1, 50.0, "running")]),
            Some(VizAnchor {
                row: 5,
                col: 3,
                cols: 10,
                rows: 4,
            }),
        );
        let anchor = viz_root_anchor(&registry, ID).expect("anchored");
        assert_eq!(
            (anchor.row, anchor.col, anchor.columns, anchor.rows),
            (5, 3, 10, 4)
        );
        // Scrolled fully off the top: anchor dropped, payload kept, hidden.
        registry.apply_scroll(20);
        assert!(registry.get(ID).is_some(), "the payload survives");
        assert!(viz_root_anchor(&registry, ID).is_none());
    }

    // ── Pure vocabulary ──

    #[test]
    fn grid_poses_stay_inside_the_footprint_and_scale_with_magnitude() {
        for count in [1usize, 2, 5, 9, 17, 256] {
            let (cols, rows) = viz_grid_dims(count);
            assert!(cols * rows >= count, "{count} items fit");
            assert!(cols >= rows, "wider than tall");
            for index in 0..count {
                let (translation, scale) = viz_child_pose(index, count, 0.7);
                assert!(translation.x - scale.x * 0.5 >= -0.5 - 1e-4);
                assert!(translation.x + scale.x * 0.5 <= 0.5 + 1e-4);
                assert!(translation.y - scale.y * 0.5 >= -0.5 - 1e-4);
                assert!(translation.y + scale.y * 0.5 <= 0.5 + 1e-4);
            }
        }
        let (_, low) = viz_child_pose(0, 4, 0.0);
        let (_, high) = viz_child_pose(0, 4, 1.0);
        assert!(high.y > low.y, "magnitude drives bar height");
        assert!(low.y > 0.0, "zero magnitude keeps a visible sliver");
    }

    #[test]
    fn effect_poses_and_colors_end_exactly_on_the_base() {
        fn linear_distance(a: Color, b: Color) -> f32 {
            let a = a.to_linear();
            let b = b.to_linear();
            (a.red - b.red)
                .abs()
                .max((a.green - b.green).abs())
                .max((a.blue - b.blue).abs())
        }
        let base_translation = Vec3::new(0.1, -0.2, 0.0);
        let base_scale = Vec3::new(0.4, 0.6, 0.8);
        let base_color = VizPaletteSlot::Active.color();
        for effect in [
            VizEffectKind::Survived,
            VizEffectKind::Denied,
            VizEffectKind::Missing,
            VizEffectKind::Timeout,
            VizEffectKind::Highlight,
        ] {
            let (translation, scale) = viz_effect_pose(effect, base_translation, base_scale, 1.0);
            assert!(
                (translation - base_translation).length() < 1e-4,
                "{} ends on the base translation",
                effect.name()
            );
            assert!(
                (scale - base_scale).length() < 1e-3,
                "{} ends on the base scale",
                effect.name()
            );
            assert!(
                linear_distance(viz_effect_color(effect, base_color, 1.0), base_color) < 1e-4,
                "{} ends on the base color",
                effect.name()
            );
            assert!(
                linear_distance(viz_effect_color(effect, base_color, 0.0), base_color) < 1e-4,
                "{} starts on the base color",
                effect.name()
            );
        }
        let (_, died_scale) =
            viz_effect_pose(VizEffectKind::Died, base_translation, base_scale, 1.0);
        assert_eq!(died_scale, Vec3::ZERO, "died shrinks to nothing");
    }
}
