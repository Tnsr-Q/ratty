//! Scene setup and presentation resources.

mod mobius;
mod stage;

pub use mobius::{MobiusTransition, MobiusTransitionDirection};
pub use stage::{StageChannel, StageTween};

use bevy::asset::RenderAssetUsages;
use bevy::camera::ClearColorConfig;
use bevy::ecs::query::With;
use bevy::ecs::system::SystemParam;
use bevy::image::ImageSampler;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, Face, TextureDimension, TextureFormat};
use bevy::window::PrimaryWindow;

use bevy::camera::visibility::NoFrustumCulling;

use crate::config::AppConfig;
use crate::direct_render::{
    DirectTerminalSceneExchange, new_terminal_image, new_terminal_render_image,
};
use crate::identity::{TerminalIdentity, TerminalRegistry};
use crate::inline::TerminalInlineObjects;
use crate::present::{TerminalPresentMaterial, fullscreen_quad};
use crate::runtime::{IngressSource, TerminalRuntime};
use crate::systems::TerminalFrameDirty;
use crate::terminal::{
    TerminalLayout, TerminalRedrawState, TerminalSurface, render_scale_for_window,
};

/// Marker for the 2D terminal sprite.
#[derive(Component)]
pub struct TerminalSprite;

/// Marker for the front 3D terminal plane.
#[derive(Component)]
pub struct TerminalPlane;

/// Marker for the back 3D terminal plane.
#[derive(Component)]
pub struct TerminalPlaneBack;

/// Marker for the 3D presentation camera.
#[derive(Component)]
pub struct TerminalPlaneCamera;

/// Handles for terminal plane meshes.
#[derive(Component)]
pub struct TerminalPlaneMeshes {
    /// Front plane mesh.
    pub front: Handle<Mesh>,
    /// Back plane mesh.
    pub back: Handle<Mesh>,
}

/// Plane warp state.
#[derive(Component, Default)]
pub struct TerminalPlaneWarp {
    /// Warp amount.
    pub amount: f32,
}

impl TerminalPlaneWarp {
    /// Adjusts the warp amount.
    pub fn adjust(&mut self, delta: f32) {
        self.amount = (self.amount + delta).clamp(0.0, 1.0);
    }
}

/// Terminal viewport geometry.
#[derive(Component, Clone, Copy)]
pub struct TerminalViewport {
    /// Viewport size in logical pixels.
    pub size: Vec2,
    /// Viewport center in world space.
    pub center: Vec2,
}

/// Terminal presentation mode.
#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug)]
pub enum TerminalPresentationMode {
    /// Flat 2D presentation.
    Flat2d,
    /// Warped 3D presentation.
    Plane3d,
    /// Mobius-strip 3D presentation.
    Mobius3d,
}

impl TerminalPresentationMode {
    /// Returns whether the mode uses the 3D presentation camera and terminal plane.
    pub const fn is_3d(self) -> bool {
        !matches!(self, Self::Flat2d)
    }

    /// Returns whether the mode uses the Mobius-strip terminal surface.
    pub const fn is_mobius(self) -> bool {
        matches!(self, Self::Mobius3d)
    }
}

/// Active terminal presentation.
#[derive(Resource)]
pub struct TerminalPresentation {
    /// Current presentation mode.
    pub mode: TerminalPresentationMode,
}

impl TerminalPresentation {
    /// Toggles between the flat and warped 3D terminal views.
    pub fn toggle_plane_mode(&mut self) {
        self.mode = match self.mode {
            TerminalPresentationMode::Flat2d => TerminalPresentationMode::Plane3d,
            TerminalPresentationMode::Plane3d | TerminalPresentationMode::Mobius3d => {
                TerminalPresentationMode::Flat2d
            }
        };
    }

    /// Toggles the Mobius-strip terminal view.
    pub fn toggle_mobius_mode(&mut self) {
        self.mode = match self.mode {
            TerminalPresentationMode::Mobius3d => TerminalPresentationMode::Flat2d,
            TerminalPresentationMode::Flat2d | TerminalPresentationMode::Plane3d => {
                TerminalPresentationMode::Mobius3d
            }
        };
    }
}

/// Applies an absolute presentation-mode change, routing Möbius entry and
/// exit through the camera transition exactly like the keyboard and web
/// controls. Unlike the keyboard toggles, the change is absolute: leaving
/// the Möbius view lands on the requested mode, not the mode it was entered
/// from. Returns `true` when the mode actually changed.
pub(crate) fn apply_stage_mode_change(
    target: TerminalPresentationMode,
    presentation: &mut TerminalPresentation,
    plane_view: &TerminalPlaneView,
    mobius: &mut MobiusTransition,
) -> bool {
    if target == presentation.mode {
        return false;
    }
    let current = presentation.mode;
    if current == TerminalPresentationMode::Mobius3d {
        let current_zoom = if mobius.active {
            mobius.current_zoom()
        } else {
            plane_view.zoom
        };
        mobius.source_mode = target;
        mobius.begin_exit(plane_view, current_zoom);
    } else if target == TerminalPresentationMode::Mobius3d {
        presentation.mode = TerminalPresentationMode::Mobius3d;
        mobius.begin_enter(current, plane_view);
    } else {
        presentation.mode = target;
        mobius.stop();
    }
    true
}

/// Camera state for 3D presentation.
#[derive(Resource)]
pub struct TerminalPlaneView {
    /// Camera yaw.
    pub yaw: f32,
    /// Camera pitch.
    pub pitch: f32,
    /// Camera zoom factor.
    pub zoom: f32,
    /// Camera pan offset.
    pub camera_offset: Vec2,
    /// Indicates drag rotation.
    pub rotating: bool,
    /// Indicates drag panning.
    pub panning: bool,
    /// Last rotation cursor position.
    pub last_rotate_cursor: Option<Vec2>,
    /// Last pan cursor position.
    pub last_pan_cursor: Option<Vec2>,
}

impl Default for TerminalPlaneView {
    fn default() -> Self {
        Self {
            yaw: 0.18,
            pitch: 0.08,
            zoom: 1.0,
            camera_offset: Vec2::ZERO,
            rotating: false,
            panning: false,
            last_rotate_cursor: None,
            last_pan_cursor: None,
        }
    }
}

/// Model loading state.
#[derive(Resource)]
pub struct ModelLoadState {
    /// Indicates the scene has loaded models.
    pub loaded: bool,
    /// Indicates the first terminal frame was uploaded.
    pub first_frame_uploaded: bool,
}

type SpriteVisibilityQuery<'w, 's> = Query<'w, 's, &'static mut Visibility, With<TerminalSprite>>;
type PlaneVisibilityQuery<'w, 's> = Query<'w, 's, &'static mut Visibility, With<TerminalPlane>>;
type PlaneBackVisibilityQuery<'w, 's> =
    Query<'w, 's, &'static mut Visibility, With<TerminalPlaneBack>>;
type PlaneMaterialQuery<'w, 's> =
    Query<'w, 's, &'static MeshMaterial3d<StandardMaterial>, With<TerminalPlane>>;
type PlaneTransformQuery<'w, 's> = Query<'w, 's, &'static mut Transform, With<TerminalPlane>>;
type PlaneBackTransformQuery<'w, 's> =
    Query<'w, 's, &'static mut Transform, With<TerminalPlaneBack>>;
type PlaneCameraQuery<'w, 's> =
    Query<'w, 's, (&'static mut Projection, &'static mut Transform), With<TerminalPlaneCamera>>;
pub(crate) type TerminalPlaneLayoutQuery<'w, 's> =
    Query<'w, 's, &'static mut Transform, (With<TerminalPlane>, Without<TerminalSprite>)>;
pub(crate) type TerminalPlaneBackLayoutQuery<'w, 's> = Query<
    'w,
    's,
    &'static mut Transform,
    (
        With<TerminalPlaneBack>,
        Without<TerminalPlane>,
        Without<TerminalSprite>,
    ),
>;

#[derive(SystemParam)]
pub(crate) struct PresentationParams<'w, 's> {
    visibility_queries: ParamSet<
        'w,
        's,
        (
            SpriteVisibilityQuery<'w, 's>,
            PlaneVisibilityQuery<'w, 's>,
            PlaneBackVisibilityQuery<'w, 's>,
        ),
    >,
    plane_materials: PlaneMaterialQuery<'w, 's>,
    materials: ResMut<'w, Assets<StandardMaterial>>,
    plane_transforms: ParamSet<
        'w,
        's,
        (
            PlaneTransformQuery<'w, 's>,
            PlaneBackTransformQuery<'w, 's>,
            PlaneCameraQuery<'w, 's>,
        ),
    >,
    #[allow(clippy::type_complexity)]
    camera_2d: Query<
        'w,
        's,
        &'static mut Camera,
        (
            With<Camera2d>,
            Without<TerminalPlaneCamera>,
            Without<crate::effects::AiEffectCamera>,
            Without<crate::avatar::AvatarBubbleCamera>,
        ),
    >,
    camera_3d: Query<'w, 's, &'static mut Camera, (With<TerminalPlaneCamera>, Without<Camera2d>)>,
}

/// The seat entity owning a per-terminal scene entity (the front and
/// back planes). Pure data at M4.3: its only consumer is the despawn
/// sweep; M4.4's organ routing joins on it. Deliberately NOT `ChildOf`
/// parenting — the seat has no `Transform`, and
/// [`apply_terminal_presentation`] writes plane transforms in world
/// space, which parenting would silently make local-space.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalOwner(pub Entity);

/// Everything [`dress_terminal_seat`] needs, bundled so any system (the
/// Startup constructor today, M4.5's wire loop tomorrow) can embed it.
/// Systems register at `Plugin::build`, so per-terminal construction is a
/// FUNCTION over this bundle, never N system instances (#58 rider).
#[derive(SystemParam)]
pub struct TerminalSpawnParams<'w, 's> {
    pub(crate) commands: Commands<'w, 's>,
    terminals: ResMut<'w, TerminalRegistry>,
    app_config: Res<'w, AppConfig>,
    pub(crate) meshes: ResMut<'w, Assets<Mesh>>,
    materials: ResMut<'w, Assets<StandardMaterial>>,
    images: ResMut<'w, Assets<Image>>,
    primary_window: Query<'w, 's, &'static Window, With<PrimaryWindow>>,
}

#[derive(SystemParam)]
pub(crate) struct SetupSceneParams<'w, 's> {
    spawn: TerminalSpawnParams<'w, 's>,
    present_materials: ResMut<'w, Assets<TerminalPresentMaterial>>,
    runtime: Query<'w, 's, &'static mut TerminalRuntime>,
    terminal: Query<
        'w,
        's,
        (
            Entity,
            &'static TerminalIdentity,
            &'static mut TerminalSurface,
        ),
    >,
}

/// Sets up the terminal presentation scene: the scene-global stage once,
/// then the boot terminal seat through the same [`dress_terminal_seat`]
/// every spawned terminal goes through.
pub(crate) fn setup_scene(mut params: SetupSceneParams) {
    let SetupSceneParams {
        spawn,
        present_materials,
        runtime,
        terminal,
    } = &mut params;
    // Fatal, like the primary-window expect below: setup_scene is a
    // constructor, and a world without exactly one terminal seat (spawned by
    // main()/start() with the surface, runtime and identity aboard) is a
    // broken world.
    let (host, identity, mut terminal) = terminal.single_mut().expect("exactly one terminal seat");
    let identity = *identity;
    let mut runtime = runtime.single_mut().expect("exactly one terminal seat");

    setup_scene_stage(&mut spawn.commands);
    let present_handle = dress_terminal_seat(spawn, host, identity, &mut terminal, &mut runtime);

    // Present the terminal texture 1:1 with physical pixels via a fullscreen quad
    // whose shader fetches each texel by pixel coordinate (no resampling), rather
    // than a world-positioned sprite whose interpolated UVs resample it. The
    // `TerminalSprite` marker is kept so the flat/3D visibility toggle applies.
    //
    // Exactly one quad exists (decision 12's focused-1:1 scene view), so it
    // spawns in neither split half — bound explicitly to the BOOT seat's
    // present texture by the handle dress returned, never "first seat a
    // query finds". Focused rebinding is M4.4's work
    // (`sync_terminal_materials` already rebinds the handle every dirty
    // frame).
    let quad = spawn.meshes.add(fullscreen_quad());
    spawn.commands.spawn((
        TerminalSprite,
        Mesh2d(quad),
        MeshMaterial2d(present_materials.add(TerminalPresentMaterial {
            texture: present_handle,
        })),
        Transform::default(),
        Visibility::Visible,
        NoFrustumCulling,
    ));
}

/// The scene-global half of the old `setup_scene` (#56 decision 12's
/// screen side): the two cameras, the lights and the scene-view
/// resources. Runs exactly once; nothing in it references any terminal.
fn setup_scene_stage(commands: &mut Commands) {
    commands.spawn((
        Camera2d,
        Camera {
            order: 0,
            ..default()
        },
        Msaa::Off,
    ));
    commands.spawn((
        TerminalPlaneCamera,
        Camera3d::default(),
        Camera {
            order: 1,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        Projection::Orthographic(OrthographicProjection {
            near: -2000.0,
            far: 2000.0,
            ..OrthographicProjection::default_3d()
        }),
        Transform::from_xyz(0.0, 0.0, 800.0).looking_at(Vec3::ZERO, Vec3::Y),
        Msaa::Off,
    ));

    commands.spawn((
        PointLight {
            intensity: 190_000.0,
            range: 2200.0,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_xyz(220.0, 320.0, 1000.0),
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 15_000.0,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::ZYX, 0.0, -0.9, -0.45)),
    ));
    commands.spawn((
        PointLight {
            intensity: 45_000.0,
            range: 1800.0,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_xyz(-280.0, -120.0, 700.0),
    ));

    commands.insert_resource(TerminalPresentation {
        mode: TerminalPresentationMode::Flat2d,
    });
    commands.insert_resource(TerminalPlaneView::default());
    commands.insert_resource(MobiusTransition::default());
    commands.insert_resource(ModelLoadState {
        loaded: false,
        first_frame_uploaded: false,
    });
}

/// The per-terminal half of the old `setup_scene`, callable N times: binds
/// the seat in the registry, sizes the surface and transport to the
/// window, creates the seat's render/present/back textures, inserts the
/// world-derived per-terminal components, and spawns the seat's front and
/// back plane entities stamped with [`TerminalOwner`].
///
/// Returns the seat's present-texture handle so callers bind scene-view
/// materials (the 2D present quad at boot) explicitly by handle, never by
/// "first seat the query yields" — query iteration order is unstable the
/// moment an N=2 world exists.
pub(crate) fn dress_terminal_seat(
    params: &mut TerminalSpawnParams,
    seat: Entity,
    identity: TerminalIdentity,
    surface: &mut TerminalSurface,
    runtime: &mut TerminalRuntime,
) -> Handle<Image> {
    let TerminalSpawnParams {
        commands,
        terminals,
        app_config,
        meshes,
        materials,
        images,
        primary_window,
    } = params;
    terminals
        .bind(identity.id(), seat)
        .expect("the dressed seat's identity was minted by this registry");

    let terminal_opacity = app_config.window.opacity.clamp(0.0, 1.0);
    let window = primary_window.single().expect("primary window");
    let window_size = window.resolution.size().max(Vec2::ONE);
    let render_scale = render_scale_for_window(window);
    let layout = surface.resize_to_fit(window_size, render_scale);
    let pty_pixels = layout.pty_pixels();
    runtime.resize(
        layout.cols,
        layout.rows,
        pty_pixels.x as u16,
        pty_pixels.y as u16,
    );

    let terminal_alpha = (terminal_opacity * 255.0).round() as u8;
    let render_image_handle = images.add(new_terminal_render_image(
        layout.texture_size.x,
        layout.texture_size.y,
        crate::config::TERMINAL_RENDER_TEXTURE_LABEL,
    ));
    surface.render_image_handle = Some(render_image_handle);

    let image_handle = images.add(new_terminal_image(
        layout.texture_size.x,
        layout.texture_size.y,
        crate::config::TERMINAL_TEXTURE_LABEL,
    ));
    surface.image_handle = Some(image_handle.clone());

    let [r, g, b] = app_config.theme.background;
    let back_image = create_terminal_image(
        layout.texture_size.x,
        layout.texture_size.y,
        [
            r.saturating_sub(13),
            g.saturating_sub(11),
            b.saturating_sub(3),
            terminal_alpha,
        ],
    );
    let back_image_handle = images.add(back_image);
    surface.back_image_handle = Some(back_image_handle.clone());

    let viewport_center = Vec2::ZERO;

    let front_mesh = meshes.add(terminal_plane_mesh(32, 20));
    let back_mesh = meshes.add(terminal_plane_mesh(32, 20));
    // The terminal seat: the caller spawns the entity with the
    // externally-constructed surface, runtime and identity aboard, and
    // this dresses THAT entity with the world-derived per-terminal
    // components. The seat-count tests assert the count explicitly (#54:
    // nothing strips a second seat once the types stop being Resources).
    commands.entity(seat).insert((
        TerminalPlaneMeshes {
            front: front_mesh.clone(),
            back: back_mesh.clone(),
        },
        TerminalFrameDirty::default(),
        TerminalRedrawState::default(),
        TerminalViewport {
            size: layout.logical_size,
            center: viewport_center,
        },
        TerminalPlaneWarp::default(),
        TerminalInlineObjects::default(),
        // A FRESH mailbox per seat, born with the dress — never a clone
        // of another seat's exchange (an Arc clone shares the one slot:
        // #54's false PASS, asserted against by the seat tests).
        DirectTerminalSceneExchange::default(),
    ));

    commands.spawn((
        TerminalPlane,
        TerminalOwner(seat),
        Mesh3d(front_mesh),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgba(1.0, 1.0, 1.0, terminal_opacity),
            base_color_texture: surface.image_handle.clone(),
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            ..default()
        })),
        Transform::from_scale(layout.logical_size.extend(1.0)),
        Visibility::Hidden,
    ));

    commands.spawn((
        TerminalPlaneBack,
        TerminalOwner(seat),
        Mesh3d(back_mesh),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgba(1.0, 1.0, 1.0, terminal_opacity),
            base_color_texture: surface.back_image_handle.clone(),
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            ..default()
        })),
        Transform {
            translation: Vec3::new(0.0, 0.0, -2.0),
            rotation: Quat::from_rotation_y(std::f32::consts::PI),
            scale: layout.logical_size.extend(1.0),
        },
        Visibility::Hidden,
    ));

    image_handle
}

/// Failure modes of [`spawn_terminal`].
#[derive(Debug)]
pub enum SpawnTerminalError<E> {
    /// The 128-slot namespace pool is exhausted — reported explicitly,
    /// never masked by minting past the wire's `& 0x7F` width.
    Alloc(crate::identity::TerminalAllocError),
    /// Transport construction failed. The allocated lease was released
    /// before this was returned, so a failed spawn can never leak a
    /// namespace slot.
    Build(E),
}

/// Spawns and dresses a fresh terminal seat: allocates its identity,
/// builds its transport through `build`, spawns the seat with the
/// identity aboard, and runs the same [`dress_terminal_seat`] the boot
/// seat goes through. Returns the seat and its minted identity.
///
/// `build` receives ONLY the ingress stamp — #12's no-runtime-arguments
/// is structural: `RuntimeOptions` never enters this signature, and the
/// wire path uses config defaults. On build failure the lease is
/// released before the error propagates, so a leaked namespace on
/// transport failure is impossible by construction.
///
/// This is the dynamic entry (M4.5's wire loop, and the N=2 tests). It
/// is a function, not a system: systems register at `Plugin::build`, so
/// dynamic multiplicity must live in data reached from one system
/// (#58 rider).
///
/// # Errors
///
/// [`SpawnTerminalError::Alloc`] on pool exhaustion;
/// [`SpawnTerminalError::Build`] when `build` fails (lease restored).
pub fn spawn_terminal<E>(
    params: &mut TerminalSpawnParams,
    build: impl FnOnce(IngressSource) -> Result<(TerminalSurface, TerminalRuntime), E>,
) -> Result<(Entity, TerminalIdentity), SpawnTerminalError<E>> {
    let identity = params
        .terminals
        .allocate()
        .map_err(SpawnTerminalError::Alloc)?;
    let (mut surface, mut runtime) = match build(identity.ingress()) {
        Ok(parts) => parts,
        Err(error) => {
            params
                .terminals
                .release(identity.id())
                .expect("the just-allocated lease is live");
            return Err(SpawnTerminalError::Build(error));
        }
    };
    // The seat is born carrying its identity, then dressed in the same
    // command batch — the publisher can never observe a seat without its
    // mailbox (commands apply atomically).
    let seat = params.commands.spawn(identity).id();
    dress_terminal_seat(params, seat, identity, &mut surface, &mut runtime);
    params.commands.entity(seat).insert((surface, runtime));
    Ok((seat, identity))
}

/// Synchronizes Bevy presentation entities to the terminal texture layout.
pub(crate) fn sync_terminal_layout(
    layout: TerminalLayout,
    viewport: &mut TerminalViewport,
    plane_query: &mut TerminalPlaneLayoutQuery,
    plane_back_query: &mut TerminalPlaneBackLayoutQuery,
) {
    viewport.size = layout.logical_size;
    viewport.center = Vec2::ZERO;

    for mut transform in plane_query.iter_mut() {
        transform.scale = layout.logical_size.extend(1.0);
    }

    for mut transform in plane_back_query.iter_mut() {
        transform.scale = layout.logical_size.extend(1.0);
    }
}

fn create_terminal_image(width: u32, height: u32, fill: [u8; 4]) -> Image {
    let mut image = Image::new_fill(
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &fill,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    );
    image.sampler = ImageSampler::nearest();
    image
}

/// Applies the active terminal presentation mode.
pub(crate) fn apply_terminal_presentation(
    presentation: Res<TerminalPresentation>,
    plane_view: Res<TerminalPlaneView>,
    mobius_transition: Res<MobiusTransition>,
    mut params: PresentationParams,
) {
    let PresentationParams {
        visibility_queries,
        plane_materials,
        materials,
        plane_transforms,
        camera_2d,
        camera_3d,
    } = &mut params;
    let is_3d = presentation.mode.is_3d();
    let is_mobius = presentation.mode.is_mobius();
    let yaw = if is_mobius && mobius_transition.active {
        mobius_transition.current_yaw()
    } else {
        plane_view.yaw
    };
    let pitch = if is_mobius && mobius_transition.active {
        mobius_transition.current_pitch()
    } else {
        plane_view.pitch
    };
    let camera_offset = if is_mobius && mobius_transition.active {
        mobius_transition.current_camera_offset()
    } else {
        plane_view.camera_offset
    };
    let sprite_visibility = if is_3d {
        Visibility::Hidden
    } else {
        Visibility::Visible
    };
    let plane_visibility = if is_3d {
        Visibility::Visible
    } else {
        Visibility::Hidden
    };

    for mut visibility in &mut visibility_queries.p0() {
        *visibility = sprite_visibility;
    }

    for mut visibility in &mut visibility_queries.p1() {
        *visibility = plane_visibility;
    }

    for mut visibility in &mut visibility_queries.p2() {
        // A Mobius strip is one continuous ribbon, so the separate back sheet model does not map
        // cleanly. Render the front material double-sided instead.
        *visibility = if is_3d && !is_mobius {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
    }

    let cull_mode = if is_mobius { None } else { Some(Face::Back) };
    for front_material in plane_materials.iter() {
        // `get_mut` marks the material modified and re-prepares it on the
        // GPU; only take it when the cull mode actually changes.
        let needs_update = materials
            .get(&front_material.0)
            .is_some_and(|material| material.cull_mode != cull_mode);
        if needs_update && let Some(mut material) = materials.get_mut(&front_material.0) {
            material.cull_mode = cull_mode;
        }
    }

    // The 2D camera only contributes in flat mode; the 3D camera stays active
    // everywhere because the cursor model and RGP objects render through it
    // even in 2D mode. Whichever camera renders first owns the screen clear.
    for mut camera in camera_2d.iter_mut() {
        let active = !is_3d;
        if camera.is_active != active {
            camera.is_active = active;
        }
    }
    // This system only runs when presentation state changes, so assigning
    // unconditionally does not churn change detection every frame.
    for mut camera in camera_3d.iter_mut() {
        camera.clear_color = if is_3d {
            ClearColorConfig::Default
        } else {
            ClearColorConfig::None
        };
    }

    for mut transform in &mut plane_transforms.p0() {
        transform.rotation = if is_3d {
            Quat::from_euler(EulerRot::XYZ, pitch, yaw, 0.0)
        } else {
            Quat::IDENTITY
        };
    }

    // Presentation owns the back sheet's rotation and its depth behind the
    // front plane; X/Y are placement and stay untouched.
    for mut transform in &mut plane_transforms.p1() {
        if is_3d {
            transform.rotation =
                Quat::from_euler(EulerRot::XYZ, pitch, yaw + std::f32::consts::PI, 0.0);
            transform.translation.z = if is_mobius { 0.0 } else { -2.0 };
        } else {
            transform.rotation = Quat::IDENTITY;
            transform.translation.z = -2.0;
        }
    }

    for (mut projection, mut transform) in &mut plane_transforms.p2() {
        if let Projection::Orthographic(ortho) = projection.as_mut() {
            let zoom = if is_mobius && mobius_transition.active {
                mobius_transition.current_zoom()
            } else {
                plane_view.zoom
            };
            ortho.scale = if is_3d { zoom } else { 1.0 };
        }

        let offset = if is_3d {
            camera_offset.extend(0.0)
        } else {
            Vec3::ZERO
        };
        transform.translation = Vec3::new(0.0, 0.0, 800.0) + offset;
        transform.look_at(offset, Vec3::Y);
    }
}

fn terminal_plane_mesh(x_segments: u32, y_segments: u32) -> Mesh {
    let x_segments = x_segments.max(2);
    let y_segments = y_segments.max(2);
    let vertex_count = ((x_segments + 1) * (y_segments + 1)) as usize;

    let mut positions = Vec::with_capacity(vertex_count);
    let mut normals = Vec::with_capacity(vertex_count);
    let mut uvs = Vec::with_capacity(vertex_count);
    let mut indices = Vec::with_capacity((x_segments * y_segments * 6) as usize);

    for y in 0..=y_segments {
        let v = y as f32 / y_segments as f32;
        let py = 0.5 - v;
        for x in 0..=x_segments {
            let u = x as f32 / x_segments as f32;
            let px = u - 0.5;
            positions.push([px, py, 0.0]);
            normals.push([0.0, 0.0, 1.0]);
            uvs.push([u, v]);
        }
    }

    for y in 0..y_segments {
        for x in 0..x_segments {
            let row = y * (x_segments + 1);
            let next_row = (y + 1) * (x_segments + 1);
            let i0 = row + x;
            let i1 = i0 + 1;
            let i2 = next_row + x;
            let i3 = i2 + 1;
            indices.extend_from_slice(&[i0, i2, i1, i1, i2, i3]);
        }
    }

    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
    .with_inserted_indices(Indices::U32(indices))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The M4.2 seat test: runs the REAL spawner (`setup_scene`) in a scaffold
    /// world and asserts exactly one entity carries each swapped component —
    /// the #54 rider (once a type stops being a Resource, nothing strips a
    /// second seat; the count is asserted, never assumed). Each swap commit
    /// extends the tuple query below with its type.
    #[test]
    fn setup_scene_spawns_exactly_one_terminal_seat() {
        use bevy::ecs::system::RunSystemOnce;

        let mut world = World::new();
        world.insert_resource(AppConfig::default());
        world.init_resource::<Assets<Mesh>>();
        world.init_resource::<Assets<StandardMaterial>>();
        world.init_resource::<Assets<Image>>();
        world.init_resource::<Assets<TerminalPresentMaterial>>();
        // Mirror main()/start(): the boot identity is the registry's FIRST
        // allocation, and the seat is born pre-run with the surface,
        // runtime and identity aboard; setup_scene must dress THIS entity,
        // not spawn another.
        let mut registry = TerminalRegistry::default();
        let identity = registry.allocate().expect("a fresh registry has slots");
        let (runtime, _host) =
            TerminalRuntime::virtual_channel(&AppConfig::default(), identity.ingress());
        let seat = world
            .spawn((
                TerminalSurface::new(&AppConfig::default())
                    .expect("surface construction is CPU-only"),
                runtime,
                identity,
            ))
            .id();
        world.insert_resource(registry);
        world.spawn((Window::default(), bevy::window::PrimaryWindow));

        world
            .run_system_once(setup_scene)
            .expect("setup_scene should run");

        assert_eq!(
            world.query::<&TerminalSurface>().iter(&world).count(),
            1,
            "exactly one seat carries the surface"
        );
        assert_eq!(
            world.query::<&TerminalPlaneMeshes>().iter(&world).count(),
            1,
            "setup_scene dresses exactly one terminal seat"
        );
        assert_eq!(
            world
                .query::<(Entity, &TerminalPlaneMeshes)>()
                .single(&world)
                .expect("exactly one dressed seat")
                .0,
            seat,
            "setup_scene dressed the pre-spawned seat, not a new entity"
        );
        assert_eq!(
            world.query::<&TerminalFrameDirty>().iter(&world).count(),
            1,
            "exactly one seat carries the frame-dirty flag"
        );
        assert_eq!(
            world.query::<&TerminalViewport>().iter(&world).count(),
            1,
            "exactly one seat carries the viewport"
        );
        assert_eq!(
            world.query::<&TerminalPlaneWarp>().iter(&world).count(),
            1,
            "exactly one seat carries the plane warp"
        );
        assert_eq!(
            world.query::<&TerminalInlineObjects>().iter(&world).count(),
            1,
            "exactly one seat carries the inline objects"
        );
        assert_eq!(
            world.query::<&TerminalRuntime>().iter(&world).count(),
            1,
            "exactly one seat carries the runtime"
        );
        assert_eq!(
            world.query::<&TerminalRedrawState>().iter(&world).count(),
            1,
            "exactly one seat carries the redraw flag"
        );
        assert_eq!(
            world.query::<&TerminalIdentity>().iter(&world).count(),
            1,
            "exactly one seat carries the identity"
        );
        assert_eq!(
            world
                .query::<&DirectTerminalSceneExchange>()
                .iter(&world)
                .count(),
            1,
            "exactly one seat carries its frame exchange (the mailbox is \
             born with the dress)"
        );
        let identity = *world
            .query::<&TerminalIdentity>()
            .single(&world)
            .expect("exactly one identity");
        assert_eq!(
            identity.id(),
            crate::identity::TerminalId::from_raw(1),
            "the boot terminal is the registry's FIRST allocation — any \
             earlier pre-world allocation shifts this and changes wire bytes"
        );
        assert_eq!(
            identity.namespace(),
            0,
            "boot leases namespace 0, keeping every wire byte identical to \
             the single-terminal era"
        );
        assert_eq!(
            world
                .resource::<TerminalRegistry>()
                .entity_of(identity.id()),
            Some(seat),
            "dress binds the boot identity to the seat entity"
        );
        let owners: Vec<_> = world
            .query::<&TerminalOwner>()
            .iter(&world)
            .copied()
            .collect();
        assert_eq!(
            owners.len(),
            2,
            "exactly the seat's front and back planes carry TerminalOwner"
        );
        assert!(
            owners.iter().all(|owner| owner.0 == seat),
            "both planes are owned by the one boot seat"
        );
        assert_eq!(
            world
                .query::<(
                    &TerminalSurface,
                    &TerminalPlaneMeshes,
                    &TerminalFrameDirty,
                    &TerminalViewport,
                    &TerminalPlaneWarp,
                    &TerminalInlineObjects,
                    &TerminalRuntime,
                    &TerminalRedrawState,
                    &TerminalIdentity,
                    &DirectTerminalSceneExchange,
                )>()
                .iter(&world)
                .count(),
            1,
            "the swapped components co-locate on the one seat"
        );
    }

    /// Scaffold world for the spawner-path tests. Deliberately NOT
    /// `TerminalPlugin`: the M4.1-era `.single()` systems
    /// (`pump_pty_output`, `sync_terminal_materials`) stay single-seat
    /// until M4.4's loop conversion and would latch "exactly one terminal
    /// seat" warns in an N=2 world.
    fn spawner_world() -> World {
        let mut world = World::new();
        world.insert_resource(AppConfig::default());
        world.init_resource::<Assets<Mesh>>();
        world.init_resource::<Assets<StandardMaterial>>();
        world.init_resource::<Assets<Image>>();
        world.insert_resource(TerminalRegistry::default());
        world.spawn((Window::default(), bevy::window::PrimaryWindow));
        world
    }

    /// One real spawner call over the virtual transport, as a system so
    /// the `TerminalSpawnParams` bundle is exercised exactly as M4.5's
    /// wire loop will exercise it.
    fn spawn_virtual_terminal(mut params: TerminalSpawnParams) -> (Entity, TerminalIdentity) {
        spawn_terminal(&mut params, |source| {
            let (runtime, _host) = TerminalRuntime::virtual_channel(&AppConfig::default(), source);
            Ok::<_, std::convert::Infallible>((
                TerminalSurface::new(&AppConfig::default())
                    .expect("surface construction is CPU-only"),
                runtime,
            ))
        })
        .expect("the scaffold spawn succeeds")
    }

    /// The M4.3 exit-criteria test: two DirectTerminalSceneExchange
    /// instances coexist — two seats through the REAL spawner, seat count
    /// asserted explicitly (#58 rider), distinct slots (the Arc::ptr_eq
    /// assertion alone kills the clone-shaped false PASS — do not drop it
    /// as redundant), per-seat publish/take with no cross-talk, and each
    /// parser stamping its own identity.
    #[test]
    fn two_seats_two_exchanges_no_cross_talk() {
        use bevy::ecs::system::RunSystemOnce;

        let mut world = spawner_world();
        let (seat_a, id_a) = world
            .run_system_once(spawn_virtual_terminal)
            .expect("spawner system runs");
        let (seat_b, id_b) = world
            .run_system_once(spawn_virtual_terminal)
            .expect("spawner system runs");

        // (1) Two seats, count asserted explicitly — never assumed.
        assert_eq!(
            world
                .query::<(
                    &TerminalIdentity,
                    &TerminalSurface,
                    &DirectTerminalSceneExchange
                )>()
                .iter(&world)
                .count(),
            2,
            "exactly two dressed seats coexist"
        );

        // (2) Identities: monotonic ids, never reused; lowest-free slots.
        assert_ne!(id_a.id(), id_b.id(), "TerminalIds never repeat");
        assert_eq!(
            (id_a.id(), id_a.namespace(), id_b.id(), id_b.namespace()),
            (
                crate::identity::TerminalId::from_raw(1),
                0,
                crate::identity::TerminalId::from_raw(2),
                1
            ),
            "the two allocations are (1, ns 0) and (2, ns 1)"
        );

        // (3) Two mailboxes, two slots: the clone-shaped false PASS —
        // inserting a cloned exchange compiles and passes every count
        // test while sharing ONE inner slot (#54) — dies here.
        let exchange_a = world
            .entity(seat_a)
            .get::<DirectTerminalSceneExchange>()
            .expect("seat A carries its mailbox")
            .clone();
        let exchange_b = world
            .entity(seat_b)
            .get::<DirectTerminalSceneExchange>()
            .expect("seat B carries its mailbox")
            .clone();
        assert!(
            !DirectTerminalSceneExchange::same_slot(&exchange_a, &exchange_b),
            "each seat's exchange is a fresh slot, never a clone of another's"
        );

        // (4) Publish distinguishable frames; each take yields its OWN.
        exchange_a.publish_test_frame(101);
        exchange_b.publish_test_frame(202);
        assert_eq!(
            exchange_a.take_pending_width(),
            Some(101),
            "A's mailbox yields A's frame"
        );
        assert_eq!(
            exchange_b.take_pending_width(),
            Some(202),
            "B's mailbox yields B's frame"
        );

        // (5) Single-slot semantics preserved per terminal.
        assert_eq!(exchange_a.take_pending_width(), None);
        assert_eq!(exchange_b.take_pending_width(), None);

        // (6) Each parser stamps its own identity: real OSC 777 bytes in,
        // stamped commands out.
        for (seat, identity) in [(seat_a, id_a), (seat_b, id_b)] {
            let mut runtime = world
                .get_mut::<TerminalRuntime>(seat)
                .expect("the seat carries its runtime");
            runtime.parser.process(b"\x1b]777;ratty:mode;3d\x07");
            let commands = runtime.parser.callbacks_mut().take_ai_commands();
            assert_eq!(commands.len(), 1, "one command parsed");
            assert_eq!(
                commands[0].0,
                identity.ingress(),
                "the parser stamps the seat's OWN identity"
            );
        }

        // Each seat also owns its pair of plane entities.
        let owners: Vec<_> = world
            .query::<&TerminalOwner>()
            .iter(&world)
            .copied()
            .collect();
        assert_eq!(owners.len(), 4, "two planes per seat, four total");
        for seat in [seat_a, seat_b] {
            assert_eq!(
                owners.iter().filter(|owner| owner.0 == seat).count(),
                2,
                "front and back planes are stamped with their seat"
            );
        }
    }

    /// A failed transport build must release the lease (the pool is
    /// restored) while the consumed TerminalId is skipped forever —
    /// recycling is namespace-only.
    #[test]
    fn a_failed_transport_build_releases_the_namespace_lease() {
        use bevy::ecs::system::RunSystemOnce;

        let mut world = spawner_world();
        let result = world
            .run_system_once(|mut params: TerminalSpawnParams| {
                spawn_terminal(&mut params, |_source| {
                    Err::<(TerminalSurface, TerminalRuntime), &str>("transport refused")
                })
            })
            .expect("spawner system runs");
        assert!(
            matches!(result, Err(SpawnTerminalError::Build("transport refused"))),
            "the build error propagates untouched"
        );
        assert_eq!(
            world.query::<&TerminalIdentity>().iter(&world).count(),
            0,
            "no seat was born from the failed spawn"
        );

        let mut registry = world.resource_mut::<TerminalRegistry>();
        let next = registry.allocate().expect("the released slot is back");
        assert_eq!(
            next.namespace(),
            0,
            "the failed spawn's namespace lease came back to the pool"
        );
        assert_eq!(
            next.id(),
            crate::identity::TerminalId::from_raw(2),
            "the TerminalId consumed by the failed spawn is skipped, never reissued"
        );
    }

    fn fixtures() -> (TerminalPresentation, TerminalPlaneView, MobiusTransition) {
        (
            TerminalPresentation {
                mode: TerminalPresentationMode::Flat2d,
            },
            TerminalPlaneView::default(),
            MobiusTransition::default(),
        )
    }

    #[test]
    fn plain_mode_change_is_instant_and_stops_the_transition() {
        let (mut presentation, view, mut mobius) = fixtures();
        mobius.active = true;
        let changed = apply_stage_mode_change(
            TerminalPresentationMode::Plane3d,
            &mut presentation,
            &view,
            &mut mobius,
        );
        assert!(changed);
        assert_eq!(presentation.mode, TerminalPresentationMode::Plane3d);
        assert!(!mobius.active);
    }

    #[test]
    fn entering_mobius_starts_the_camera_transition() {
        let (mut presentation, view, mut mobius) = fixtures();
        let changed = apply_stage_mode_change(
            TerminalPresentationMode::Mobius3d,
            &mut presentation,
            &view,
            &mut mobius,
        );
        assert!(changed);
        assert_eq!(presentation.mode, TerminalPresentationMode::Mobius3d);
        assert!(mobius.active);
        assert_eq!(mobius.direction, MobiusTransitionDirection::Entering);
        assert_eq!(mobius.source_mode, TerminalPresentationMode::Flat2d);
    }

    #[test]
    fn leaving_mobius_exits_toward_the_requested_mode() {
        let (mut presentation, view, mut mobius) = fixtures();
        presentation.mode = TerminalPresentationMode::Mobius3d;
        mobius.source_mode = TerminalPresentationMode::Flat2d;
        let changed = apply_stage_mode_change(
            TerminalPresentationMode::Plane3d,
            &mut presentation,
            &view,
            &mut mobius,
        );
        assert!(changed);
        // The mode stays Mobius3d until the exit transition finishes, which
        // then restores `source_mode` - now the requested target.
        assert_eq!(presentation.mode, TerminalPresentationMode::Mobius3d);
        assert!(mobius.active);
        assert_eq!(mobius.direction, MobiusTransitionDirection::Exiting);
        assert_eq!(mobius.source_mode, TerminalPresentationMode::Plane3d);
        assert_eq!(mobius.start_zoom, view.zoom);
    }

    #[test]
    fn same_mode_is_a_no_op() {
        let (mut presentation, view, mut mobius) = fixtures();
        let changed = apply_stage_mode_change(
            TerminalPresentationMode::Flat2d,
            &mut presentation,
            &view,
            &mut mobius,
        );
        assert!(!changed);
        assert!(!mobius.active);
    }

    // ── App tier: the presentation applier over real entities ──

    #[test]
    fn cull_mode_toggle_reaches_every_plane() {
        use bevy::ecs::system::RunSystemOnce;

        let mut world = World::new();
        world.insert_resource(TerminalPresentation {
            mode: TerminalPresentationMode::Mobius3d,
        });
        world.insert_resource(TerminalPlaneView::default());
        world.insert_resource(MobiusTransition::default());
        world.init_resource::<Assets<StandardMaterial>>();
        let (first, second) = {
            let mut materials = world.resource_mut::<Assets<StandardMaterial>>();
            // The default cull mode is Some(Face::Back), the Plane3d value.
            (
                materials.add(StandardMaterial::default()),
                materials.add(StandardMaterial::default()),
            )
        };
        world.spawn((TerminalPlane, MeshMaterial3d(first.clone())));
        world.spawn((TerminalPlane, MeshMaterial3d(second.clone())));

        world
            .run_system_once(apply_terminal_presentation)
            .expect("the system should run");
        {
            let materials = world.resource::<Assets<StandardMaterial>>();
            for handle in [&first, &second] {
                assert_eq!(
                    materials
                        .get(handle)
                        .expect("the material exists")
                        .cull_mode,
                    None,
                    "Mobius renders every plane's front material double-sided"
                );
            }
        }

        world.resource_mut::<TerminalPresentation>().mode = TerminalPresentationMode::Plane3d;
        world
            .run_system_once(apply_terminal_presentation)
            .expect("the system should run");
        let materials = world.resource::<Assets<StandardMaterial>>();
        for handle in [&first, &second] {
            assert_eq!(
                materials
                    .get(handle)
                    .expect("the material exists")
                    .cull_mode,
                Some(Face::Back),
                "leaving Mobius restores back-face culling on every plane"
            );
        }
    }

    fn back_translation(app: &App, entity: Entity) -> Vec3 {
        app.world()
            .get::<Transform>(entity)
            .expect("the back sheet should still exist")
            .translation
    }

    #[test]
    fn back_plane_placement_survives_presentation_changes() {
        let mut app = App::new();
        app.insert_resource(TerminalPresentation {
            mode: TerminalPresentationMode::Plane3d,
        });
        app.insert_resource(TerminalPlaneView::default());
        app.insert_resource(MobiusTransition::default());
        app.init_resource::<Assets<StandardMaterial>>();
        app.add_systems(Update, apply_terminal_presentation);

        // A second terminal's back sheet would sit off-origin exactly like
        // this; presentation changes must not snap it back to (0, 0).
        let back = app
            .world_mut()
            .spawn((
                TerminalPlaneBack,
                Transform {
                    translation: Vec3::new(120.0, 40.0, -2.0),
                    rotation: Quat::from_rotation_y(std::f32::consts::PI),
                    scale: Vec3::ONE,
                },
                Visibility::Hidden,
            ))
            .id();

        app.update();
        assert_eq!(
            back_translation(&app, back),
            Vec3::new(120.0, 40.0, -2.0),
            "Plane3d keeps placement and applies the presentation depth"
        );

        app.world_mut().resource_mut::<TerminalPresentation>().mode =
            TerminalPresentationMode::Mobius3d;
        app.update();
        assert_eq!(
            back_translation(&app, back),
            Vec3::new(120.0, 40.0, 0.0),
            "Mobius zeroes only the presentation-owned depth"
        );

        app.world_mut().resource_mut::<TerminalPresentation>().mode =
            TerminalPresentationMode::Flat2d;
        app.update();
        assert_eq!(
            back_translation(&app, back),
            Vec3::new(120.0, 40.0, -2.0),
            "the flat-mode reset also keeps placement"
        );
    }
}
