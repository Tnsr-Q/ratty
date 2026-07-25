//! The avatar presentation layer (#23 §1/§3): a screen-fixed mascot at
//! the nine HUD anchors and a real-text speech bubble, composited over
//! every presentation mode.
//!
//! Two dedicated overlay cameras keep the avatar structurally immune to
//! RGP camera movement: the bubble draws through its own vello scene →
//! texture pair → fullscreen quad on a `Camera2d` (order 5), and the
//! mascot renders through a fixed-pose orthographic `Camera3d` (order 6)
//! on an isolated render layer — the RGP `c`-verb machinery only ever
//! writes `TerminalPlaneCamera`, so nothing here can warp, yaw, zoom, or
//! Möbius-twist by construction. The effects wash (order 10) still tints
//! the avatar like everything else on screen — the mood organ's
//! whole-surface contract.
//!
//! Camera order stack: 0 flat-2D terminal → 1 terminal 3D → 5 bubble →
//! 6 mascot → 10 effects. The bubble composites *under* the mascot so
//! the speaking glow (drawn in the overlay scene) sits behind it.
//!
//! Honest limitations: one bubble (the active utterance only — queued
//! speech is state, not pixels); body text taller than the window clips
//! behind a visible overflow marker while the full text stays queryable
//! via `state.executions`.

use bevy::camera::ClearColorConfig;
use bevy::camera::visibility::{NoFrustumCulling, RenderLayers};
use bevy::gltf::GltfAssetLabel;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use parley_ratatui::vello::kurbo::{Affine, BezPath, Circle, Point, Rect, RoundedRect, Stroke};
use parley_ratatui::vello::peniko::{Color as PenikoColor, Fill};

use super::text::{AvatarTextSystem, paint_layout, paint_layout_revealed};
use super::{ActiveUtterance, AvatarChoreography, AvatarState};
use crate::direct_render::{
    AvatarOverlayExchange, AvatarOverlayFrame, new_terminal_image, new_terminal_render_image,
    resize_terminal_image,
};
use crate::present::{TerminalPresentMaterial, fullscreen_quad};
use crate::terminal::render_scale_for_window;

/// Render layer for the bubble overlay quad (terminal flat view is 0,
/// effects wash is 1).
pub const AVATAR_BUBBLE_LAYER: usize = 2;
/// Render layer for the mascot and its light.
pub const AVATAR_MASCOT_LAYER: usize = 3;
/// Nominal mascot height at `scale = 1`, logical px.
pub const AVATAR_BASE_SIZE_PX: f32 = 96.0;
/// Margin between the mascot and the window edge, logical px.
pub const AVATAR_MARGIN_PX: f32 = 24.0;
/// Bubble body size at `scale = 1`, logical px.
pub const AVATAR_FONT_LOGICAL_PX: f32 = 15.0;
/// Fraction of the pinned utterance duration spent revealing; the rest
/// holds the full text (the glow fades over the tail).
const REVEAL_FRACTION: f32 = 0.7;
/// Uniform scale mapping SpinyMouse model units to logical pixels.
/// Placeholder pending the manual calibration gate (the model's native
/// extent is only observable on screen); one constant, one edit.
const MASCOT_UNITS_TO_PX: f32 = 48.0;

// Overlay palette: CRT-adjacent, readable over any scene.
const BUBBLE_SCRIM: PenikoColor = PenikoColor::from_rgba8(12, 14, 20, 235);
const BUBBLE_ACCENT: PenikoColor = PenikoColor::from_rgba8(89, 194, 255, 255);
const BUBBLE_TEXT: PenikoColor = PenikoColor::from_rgba8(232, 238, 244, 255);
const GLOW_COLOR: PenikoColor = PenikoColor::from_rgba8(89, 194, 255, 90);

/// The dedicated bubble overlay camera (`Camera2d`, order 5).
#[derive(Component)]
pub struct AvatarBubbleCamera;

/// The dedicated mascot camera (`Camera3d`, order 6, fixed pose).
#[derive(Component)]
pub struct AvatarMascotCamera;

/// Root entity of the spawned mascot glTF scene.
#[derive(Component)]
pub struct AvatarMascotRoot;

/// The fullscreen overlay quad presenting the bubble texture.
#[derive(Component)]
pub struct AvatarOverlayQuad;

/// Marks mascot descendants already moved onto the mascot layer.
#[derive(Component)]
pub struct AvatarLayerTagged;

/// The overlay's render/present texture pair (1×1 while idle).
#[derive(Resource)]
pub struct AvatarOverlayImages {
    /// Storage texture vello rasterizes into.
    pub render: Handle<Image>,
    /// Texture the overlay quad samples (sRGB view).
    pub present: Handle<Image>,
}

/// Spawns the overlay cameras, the mascot light, and the overlay quad.
pub(super) fn setup_avatar(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<TerminalPresentMaterial>>,
) {
    commands.spawn((
        AvatarBubbleCamera,
        Camera2d,
        Camera {
            order: 5,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        RenderLayers::layer(AVATAR_BUBBLE_LAYER),
        Msaa::Off,
    ));
    commands.spawn((
        AvatarMascotCamera,
        Camera3d::default(),
        Camera {
            order: 6,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        Projection::Orthographic(OrthographicProjection {
            near: -2000.0,
            far: 2000.0,
            ..OrthographicProjection::default_3d()
        }),
        Transform::from_xyz(0.0, 0.0, 800.0).looking_at(Vec3::ZERO, Vec3::Y),
        RenderLayers::layer(AVATAR_MASCOT_LAYER),
        Msaa::Off,
    ));
    // Scene lights live on layer 0; the mascot needs its own.
    commands.spawn((
        DirectionalLight {
            illuminance: 15_000.0,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::ZYX, 0.0, -0.9, -0.45)),
        RenderLayers::layer(AVATAR_MASCOT_LAYER),
    ));

    let render = images.add(new_terminal_render_image(1, 1, "ratty.avatar.render"));
    let present = images.add(new_terminal_image(1, 1, "ratty.avatar.present"));
    commands.spawn((
        AvatarOverlayQuad,
        Mesh2d(meshes.add(fullscreen_quad())),
        MeshMaterial2d(materials.add(TerminalPresentMaterial {
            texture: present.clone(),
        })),
        Transform::default(),
        Visibility::Hidden,
        NoFrustumCulling,
        RenderLayers::layer(AVATAR_BUBBLE_LAYER),
    ));
    commands.insert_resource(AvatarOverlayImages { render, present });
}

/// Spawns/despawns the mascot scene root on shown/`needs_respawn` edges.
pub(super) fn sync_avatar_mascot(
    mut commands: Commands,
    mut state: ResMut<AvatarState>,
    asset_server: Res<AssetServer>,
    roots: Query<Entity, With<AvatarMascotRoot>>,
) {
    let want_mascot = state.shown;
    let have_mascot = !roots.is_empty();
    if want_mascot && (!have_mascot || state.needs_respawn) {
        for root in &roots {
            commands.entity(root).despawn();
        }
        commands.spawn((
            AvatarMascotRoot,
            Transform::default(),
            Visibility::Visible,
            RenderLayers::layer(AVATAR_MASCOT_LAYER),
            WorldAssetRoot(
                asset_server
                    .load(GltfAssetLabel::Scene(0).from_asset(state.model.embedded_url)),
            ),
        ));
        state.needs_respawn = false;
    } else if !want_mascot && have_mascot {
        for root in &roots {
            commands.entity(root).despawn();
        }
        state.needs_respawn = false;
    }
}

/// Moves loaded glTF descendants onto the mascot layer: scenes spawn on
/// layer 0, invisible to the layer-3 camera (the
/// `apply_instance_brightness` upward-walk pattern).
/// Mascot descendants not yet moved onto the mascot layer.
type UntaggedMeshQuery<'w, 's> =
    Query<'w, 's, (Entity, &'static ChildOf), (With<Mesh3d>, Without<AvatarLayerTagged>)>;

pub(super) fn propagate_avatar_layer(
    mut commands: Commands,
    untagged: UntaggedMeshQuery,
    parents: Query<&ChildOf>,
    roots: Query<Entity, With<AvatarMascotRoot>>,
) {
    if untagged.is_empty() || roots.is_empty() {
        return;
    }
    let roots: Vec<Entity> = roots.iter().collect();
    for (entity, child_of) in &untagged {
        let mut current = child_of.parent();
        loop {
            if roots.contains(&current) {
                commands.entity(entity).insert((
                    RenderLayers::layer(AVATAR_MASCOT_LAYER),
                    AvatarLayerTagged,
                ));
                break;
            }
            let Ok(next) = parents.get(current) else {
                break;
            };
            current = next.parent();
        }
    }
}

/// The mascot's anchor position in window space (logical px, origin
/// top-left, y down), margin-inset toward the window center.
fn anchor_window_px(state: &AvatarState, window_size: Vec2) -> Vec2 {
    let normalized = state.anchor.normalized();
    let half = AVATAR_BASE_SIZE_PX * state.scale * 0.5;
    let inset = AVATAR_MARGIN_PX + half;
    let inset_toward_center = |n: f32, extent: f32| -> f32 {
        if n < 0.25 {
            n * extent + inset
        } else if n > 0.75 {
            n * extent - inset
        } else {
            n * extent
        }
    };
    Vec2::new(
        inset_toward_center(normalized.x, window_size.x) + state.offset.x,
        inset_toward_center(normalized.y, window_size.y) + state.offset.y,
    )
}

/// Window-space (y down, logical) → mascot-camera world space (y up,
/// origin center). The fixed orthographic camera's default scaling mode
/// tracks the window, so world units are logical pixels.
fn window_to_world(window_px: Vec2, window_size: Vec2) -> Vec3 {
    Vec3::new(
        window_px.x - window_size.x * 0.5,
        window_size.y * 0.5 - window_px.y,
        10.0,
    )
}

/// Anchor pose, idle bob, and one-shot choreography on the mascot root.
pub(super) fn animate_avatar_mascot(
    time: Res<Time>,
    mut state: ResMut<AvatarState>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut roots: Query<&mut Transform, With<AvatarMascotRoot>>,
) {
    let Ok(window) = windows.single() else {
        return;
    };
    let Ok(mut transform) = roots.single_mut() else {
        return;
    };
    let now = time.elapsed_secs();
    let window_size = window.resolution.size();
    let base = window_to_world(anchor_window_px(&state, window_size), window_size);

    // Presence furniture breathes: a persistent subtle idle bob.
    let mut translation = base;
    translation.y += (now * 1.6).sin() * 2.5 * state.scale;
    let mut rotation = Quat::IDENTITY;
    let mut scale_pulse = 1.0f32;

    // One-shot gesture envelopes on the root transform (#23 §1: named
    // choreographies, no skinning, no animation graph).
    if let Some(gesture) = state.gesture {
        let elapsed = (time.elapsed() - gesture.started).as_secs_f32();
        let duration = gesture.which.duration_secs();
        if elapsed >= duration {
            state.gesture = None;
        } else {
            let progress = elapsed / duration;
            let envelope = (progress * std::f32::consts::PI).sin();
            match gesture.which {
                AvatarChoreography::Bob => {
                    translation.y +=
                        (progress * std::f32::consts::TAU * 2.0).sin() * 10.0 * envelope;
                }
                AvatarChoreography::Tilt => {
                    rotation = Quat::from_rotation_z(
                        (progress * std::f32::consts::TAU).sin() * 0.18 * envelope,
                    );
                }
                AvatarChoreography::Lean => {
                    rotation = Quat::from_rotation_z(-0.22 * envelope);
                    translation.x -= 6.0 * envelope * state.scale;
                }
                AvatarChoreography::Nod => {
                    rotation = Quat::from_rotation_x(
                        (progress * std::f32::consts::TAU * 2.0).sin() * 0.20 * envelope,
                    );
                }
                AvatarChoreography::Spin => {
                    rotation = Quat::from_rotation_y(progress * std::f32::consts::TAU);
                    scale_pulse = 1.0 + 0.05 * envelope;
                }
            }
        }
    }

    transform.translation = translation;
    transform.rotation = rotation;
    transform.scale =
        Vec3::splat(MASCOT_UNITS_TO_PX * state.model.scale * state.scale * scale_pulse);
}

/// The speaking-glow alpha envelope over one utterance's lifetime.
fn glow_alpha(active: &ActiveUtterance, now_secs: f32, started_secs: f32) -> f32 {
    let elapsed = now_secs - started_secs;
    let duration = active.utterance.duration.as_secs_f32();
    let ramp_in = (elapsed / 0.25).clamp(0.0, 1.0);
    let fade_out = ((duration - elapsed) / 0.4).clamp(0.0, 1.0);
    let pulse = 0.85 + 0.15 * (elapsed * std::f32::consts::TAU * 1.2).sin();
    ramp_in * fade_out * pulse
}

/// Builds and publishes the overlay scene: glow, bubble chrome,
/// attribution chip, and the cluster-revealed body text.
#[allow(clippy::too_many_arguments)]
pub(super) fn sync_avatar_overlay(
    state: Res<AvatarState>,
    time: Res<Time>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut images: ResMut<Assets<Image>>,
    overlay: Option<ResMut<AvatarOverlayImages>>,
    mut text_system: Local<AvatarTextSystem>,
    exchange: Res<AvatarOverlayExchange>,
    mut quads: Query<&mut Visibility, With<AvatarOverlayQuad>>,
    mut was_live: Local<bool>,
) {
    let Some(overlay) = overlay else {
        return;
    };
    let Ok(window) = windows.single() else {
        return;
    };
    let Ok(mut visibility) = quads.single_mut() else {
        return;
    };

    let active = state.speech.active().filter(|_| state.shown);
    let Some(active) = active else {
        // Steady-state zero cost while idle: hide the quad and shrink the
        // texture pair to 1×1 once.
        if *was_live {
            *visibility = Visibility::Hidden;
            if let Some(mut image) = images.get_mut(&overlay.render) {
                resize_terminal_image(&mut image, 1, 1);
            }
            if let Some(mut image) = images.get_mut(&overlay.present) {
                resize_terminal_image(&mut image, 1, 1);
            }
            *was_live = false;
        }
        return;
    };
    *was_live = true;
    *visibility = Visibility::Visible;

    let physical = window.resolution.physical_size();
    let (width, height) = (physical.x.max(1), physical.y.max(1));
    if let Some(mut image) = images.get_mut(&overlay.render) {
        resize_terminal_image(&mut image, width, height);
    }
    if let Some(mut image) = images.get_mut(&overlay.present) {
        resize_terminal_image(&mut image, width, height);
    }

    let render_scale = render_scale_for_window(window);
    let window_size = window.resolution.size();
    let now = time.elapsed().as_secs_f32();
    let started = active.started.as_secs_f32();

    // ── Typewriter reveal: byte offset from the pinned duration ──
    let text = &active.utterance.text;
    let total_chars = text.chars().count();
    let reveal_secs = active.utterance.duration.as_secs_f32() * REVEAL_FRACTION;
    let revealed_chars = if reveal_secs <= f32::EPSILON {
        total_chars
    } else {
        (((now - started) / reveal_secs).clamp(0.0, 1.0) * total_chars as f32).floor() as usize
    };
    let revealed_bytes = text
        .char_indices()
        .nth(revealed_chars)
        .map_or(text.len(), |(offset, _)| offset);

    // ── Shape ──
    let font_px = AVATAR_FONT_LOGICAL_PX * state.scale * render_scale;
    let wrap_px = (0.42 * width as f32).clamp(240.0 * render_scale, 560.0 * render_scale);
    let speaker = active
        .utterance
        .from
        .clone()
        .unwrap_or_else(|| format!("agent {}", active.utterance.namespace));
    let bubble = text_system.bubble(text, &speaker, wrap_px, font_px);

    // ── Geometry (physical px, y down) ──
    let mascot_px = anchor_window_px(&state, window_size) * render_scale;
    let half_px = AVATAR_BASE_SIZE_PX * state.scale * 0.5 * render_scale;
    let pad = 12.0 * render_scale;
    let chip_h = bubble.attribution_size.y + pad;
    let max_body_h = height as f32 * 0.55;
    let body_h = bubble.body_size.y.min(max_body_h);
    let clipped = bubble.body_size.y > max_body_h;
    let bubble_w = bubble.body_size.x.max(bubble.attribution_size.x) + pad * 2.0;
    let bubble_h = body_h + chip_h + pad * 2.0;

    // Place the bubble adjacent to the mascot, toward the window center.
    let normalized = state.anchor.normalized();
    let gap = 14.0 * render_scale;
    let x = if normalized.x > 0.5 {
        mascot_px.x - half_px - gap - bubble_w
    } else if normalized.x < 0.5 {
        mascot_px.x + half_px + gap
    } else {
        mascot_px.x - bubble_w * 0.5
    };
    let y = if normalized.y >= 0.5 {
        mascot_px.y - half_px - gap - bubble_h
    } else {
        mascot_px.y + half_px + gap
    };
    let margin = 8.0 * render_scale;
    let x = x.clamp(margin, (width as f32 - bubble_w - margin).max(margin));
    let y = y.clamp(margin, (height as f32 - bubble_h - margin).max(margin));

    // ── Paint ──
    let mut scene = exchange.take_recycled_scene();

    // Speaking glow, behind the mascot's screen rect (the bubble camera
    // composites under the mascot camera).
    let alpha = glow_alpha(active, now, started);
    if alpha > 0.0 {
        let glow = GLOW_COLOR.with_alpha(GLOW_COLOR.components[3] * alpha);
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            glow,
            None,
            &Circle::new(
                Point::new(f64::from(mascot_px.x), f64::from(mascot_px.y)),
                f64::from(half_px * 1.35),
            ),
        );
    }

    let rect = RoundedRect::new(
        f64::from(x),
        f64::from(y),
        f64::from(x + bubble_w),
        f64::from(y + bubble_h),
        f64::from(8.0 * render_scale),
    );
    scene.fill(Fill::NonZero, Affine::IDENTITY, BUBBLE_SCRIM, None, &rect);
    scene.stroke(
        &Stroke::new(f64::from(1.5 * render_scale)),
        Affine::IDENTITY,
        BUBBLE_ACCENT,
        None,
        &rect,
    );

    // Tail toward the mascot center.
    let tail_base_x = if normalized.x > 0.5 {
        x + bubble_w
    } else if normalized.x < 0.5 {
        x
    } else {
        x + bubble_w * 0.5
    };
    let tail_base_y = (mascot_px.y.clamp(y + pad, y + bubble_h - pad)).min(y + bubble_h);
    let mut tail = BezPath::new();
    tail.move_to(Point::new(
        f64::from(tail_base_x),
        f64::from(tail_base_y - 6.0 * render_scale),
    ));
    tail.line_to(Point::new(
        f64::from(mascot_px.x),
        f64::from(mascot_px.y),
    ));
    tail.line_to(Point::new(
        f64::from(tail_base_x),
        f64::from(tail_base_y + 6.0 * render_scale),
    ));
    tail.close_path();
    scene.fill(Fill::NonZero, Affine::IDENTITY, BUBBLE_ACCENT, None, &tail);

    // Attribution chip: accent pill + bold speaker, visible from frame
    // one — the bubble always identifies the speaking agent (#23).
    let chip_origin = Vec2::new(x + pad, y + pad * 0.75);
    let pill = RoundedRect::new(
        f64::from(chip_origin.x - pad * 0.4),
        f64::from(chip_origin.y - 2.0 * render_scale),
        f64::from(chip_origin.x + bubble.attribution_size.x + pad * 0.4),
        f64::from(chip_origin.y + bubble.attribution_size.y + 2.0 * render_scale),
        f64::from(4.0 * render_scale),
    );
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        BUBBLE_ACCENT.with_alpha(0.18),
        None,
        &pill,
    );
    paint_layout(&mut scene, &bubble.attribution, chip_origin, BUBBLE_ACCENT);

    // Body: cluster-revealed, clipped to the bubble when over-tall, with
    // a visible overflow marker (the full text stays queryable).
    let body_origin = Vec2::new(x + pad, y + pad + chip_h);
    if clipped {
        scene.push_layer(
            Fill::NonZero,
            parley_ratatui::vello::peniko::Mix::Normal,
            1.0,
            Affine::IDENTITY,
            &Rect::new(
                f64::from(x),
                f64::from(y),
                f64::from(x + bubble_w),
                f64::from(y + pad + chip_h + body_h),
            ),
        );
    }
    paint_layout_revealed(&mut scene, &bubble.body, body_origin, BUBBLE_TEXT, revealed_bytes);
    if clipped {
        scene.pop_layer();
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            BUBBLE_ACCENT,
            None,
            &Circle::new(
                Point::new(
                    f64::from(x + bubble_w * 0.5),
                    f64::from(y + bubble_h - pad * 0.5),
                ),
                f64::from(2.0 * render_scale),
            ),
        );
    }

    exchange.publish_frame(AvatarOverlayFrame {
        render_image: overlay.render.clone(),
        present_image: overlay.present.clone(),
        width,
        height,
        scene,
    });
}
