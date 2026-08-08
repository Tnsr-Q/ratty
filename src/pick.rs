//! Cell picking in 3D (M4.6): the mesh-UV raycast seam.
//!
//! The mechanism is #56 decision 10's, ratified: `plane_surface_z` and the
//! Möbius chart both take `elapsed_secs`, so the forward chart
//! (`plane_surface_point`) is time-animated and there is no inverse to
//! write. Instead, the warp system rebuilds the real mesh positions FROM
//! the UV attribute every animated frame, which makes the UV channel the
//! exact current-frame cell chart — for every mode, Möbius included.
//! Picking therefore raycasts the live front-plane meshes and reads the
//! hit's interpolated UV; `cell = (⌊u·cols⌋, ⌊v·rows⌋)` against the
//! OWNER's grid.
//!
//! Two locked policies live here:
//!
//! - **Front planes only.** The back sheet ([`TerminalPlaneBack`]) belongs
//!   to the same owner, but its debug texture is drawn column-mirrored and
//!   it is never a pick target. In Möbius the far half of the strip is the
//!   FRONT mesh's back-facing side, so excluding the back sheet loses
//!   nothing.
//! - **`RayCastBackfaces` is always on for terminal planes**, not
//!   mirroring the render `cull_mode` (#58's cross-check rider): Möbius
//!   needs it or the far half of the strip is unpickable. The render side
//!   flips `cull_mode` per mode in `apply_terminal_presentation`
//!   (src/scene/mod.rs) — the two policies deliberately DIFFER in Plane3d,
//!   where picking from strictly behind hits the front sheet's backface
//!   and yields a horizontally mirrored cell (accepted cost, flagged and
//!   not compensated). The pair is pinned by
//!   `raycast_backfaces_is_always_on_for_terminal_planes` in
//!   src/scene/mod.rs, beside the render-side cull test.

use bevy::ecs::system::SystemParam;
use bevy::picking::mesh_picking::ray_cast::{MeshRayCast, MeshRayCastSettings, RayCastVisibility};
use bevy::prelude::*;

use crate::scene::{TerminalOwner, TerminalPlane, TerminalPlaneCamera};

/// The 3D half of the `pick_cell` seam: the plane-view camera, the mesh
/// raycaster, and the staged front planes.
#[derive(SystemParam)]
pub(crate) struct PickParams<'w, 's> {
    ray_cast: MeshRayCast<'w, 's>,
    camera: Query<'w, 's, (&'static Camera, &'static GlobalTransform), With<TerminalPlaneCamera>>,
    planes: Query<
        'w,
        's,
        (
            Entity,
            &'static TerminalOwner,
            &'static Visibility,
            &'static GlobalTransform,
        ),
        With<TerminalPlane>,
    >,
}

impl PickParams<'_, '_> {
    /// The camera ray under a window cursor position. `None` until the
    /// camera has rendered once (its computed viewport state is empty
    /// before the first render frame).
    fn pointer_ray(&self, pointer: Vec2) -> Option<Ray3d> {
        let (camera, camera_transform) = self.camera.single().ok()?;
        camera.viewport_to_world(camera_transform, pointer).ok()
    }

    /// The hover/press pick: raycast the STAGED front planes, nearest hit
    /// wins, and the hit's interpolated UV is the cell chart. Returns the
    /// owning SEAT (routing joins on [`TerminalOwner`], so this is already
    /// correct when scene composition stages more than one plane).
    pub(crate) fn pick_3d(&mut self, pointer: Vec2) -> Option<(Entity, Vec2)> {
        let ray = self.pointer_ray(pointer)?;
        self.pick_ray(ray)
    }

    /// [`Self::pick_3d`] below the camera — the round-trip tests construct
    /// the orthographic ray analytically and enter here.
    pub(crate) fn pick_ray(&mut self, ray: Ray3d) -> Option<(Entity, Vec2)> {
        let Self {
            ray_cast, planes, ..
        } = self;
        // The filter reads the AUTHORED `Visibility` the presentation
        // applier writes (focused-1:1), not the render-computed
        // `InheritedVisibility`/`ViewVisibility` — those lag a PostUpdate
        // behind and default to hidden in bare test worlds. A hidden
        // plane's mesh is also stale (the warp rebuild is focus-gated), so
        // it must never eat a pick.
        let filter = |entity: Entity| {
            planes
                .get(entity)
                .is_ok_and(|(_, _, visibility, _)| *visibility != Visibility::Hidden)
        };
        let settings = MeshRayCastSettings::default()
            .with_visibility(RayCastVisibility::Any)
            .with_filter(&filter);
        let (plane, hit) = ray_cast.cast_ray(ray, &settings).first()?;
        let uv = hit.uv?;
        let (_, owner, _, _) = planes.get(*plane).ok()?;
        Some((owner.0, uv))
    }

    /// The capture chart (#56 decision 10): while a press is captured, the
    /// pointer must always produce a cell on the CAPTURED terminal. The
    /// ray is cast against that seat's front plane alone — visibility is
    /// deliberately ignored (capture beats hover beats focus: a mid-drag
    /// focus loss hides the plane but must not break the drag) — and a ray
    /// that misses the mesh intersects the plane's unbounded flat chart
    /// with UV clamped to [0, 1]. `None` only when the ray is parallel to
    /// the plane or the camera has not rendered yet; the caller freezes at
    /// the last cell.
    pub(crate) fn capture_uv(&mut self, pointer: Vec2, seat: Entity) -> Option<Vec2> {
        let ray = self.pointer_ray(pointer)?;
        self.capture_ray(ray, seat)
    }

    /// [`Self::capture_uv`] below the camera — the warped-capture tests
    /// construct the orthographic ray analytically and enter here.
    pub(crate) fn capture_ray(&mut self, ray: Ray3d, seat: Entity) -> Option<Vec2> {
        let Self {
            ray_cast, planes, ..
        } = self;
        let (plane, transform) = planes.iter().find_map(|(entity, owner, _, transform)| {
            (owner.0 == seat).then_some((entity, transform))
        })?;
        let filter = |entity: Entity| entity == plane;
        let settings = MeshRayCastSettings::default()
            .with_visibility(RayCastVisibility::Any)
            .with_filter(&filter);
        if let Some((_, hit)) = ray_cast.cast_ray(ray, &settings).first()
            && let Some(uv) = hit.uv
        {
            return Some(uv);
        }
        clamped_plane_uv(ray, transform)
    }
}

/// Intersects the plane entity's unbounded local z = 0 plane (the flat
/// chart) and clamps the resulting UV into [0, 1] — the drag-off-surface
/// rule. The full affine inverse is required: plane scale is non-uniform
/// (logical pixels in x/y, 1.0 in z). No `t >= 0` check — under the
/// orthographic stage camera the pointer names a world-space LINE, and the
/// intersection's UV is the honest clamp target on either side.
pub(crate) fn clamped_plane_uv(ray: Ray3d, plane: &GlobalTransform) -> Option<Vec2> {
    let inverse = plane.affine().inverse();
    let origin = inverse.transform_point3(ray.origin);
    let direction = inverse.transform_vector3(*ray.direction);
    if direction.z.abs() <= f32::EPSILON {
        return None;
    }
    let t = -origin.z / direction.z;
    let local = origin + direction * t;
    Some(Vec2::new(local.x + 0.5, 0.5 - local.y).clamp(Vec2::ZERO, Vec2::ONE))
}

/// The locked cell chart: `cell = (⌊u·cols⌋, ⌊v·rows⌋)` against the
/// owner's grid, clamped so the closed edge (u or v exactly 1.0) lands on
/// the last cell — same convention as `position_to_cell`'s clamp.
pub(crate) fn cell_from_uv(uv: Vec2, cols: u16, rows: u16) -> UVec2 {
    let cols = cols.max(1) as u32;
    let rows = rows.max(1) as u32;
    let u = uv.x.clamp(0.0, 1.0);
    let v = uv.y.clamp(0.0, 1.0);
    UVec2::new(
        ((u * cols as f32).floor() as u32).min(cols - 1),
        ((v * rows as f32).floor() as u32).min(rows - 1),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bevy::camera::primitives::{Aabb, MeshAabb};
    use bevy::ecs::system::RunSystemOnce;
    use bevy::picking::mesh_picking::ray_cast::RayCastBackfaces;

    use super::*;
    use crate::scene::{
        MobiusTransition, TerminalPlaneMeshes, TerminalPlaneWarp, TerminalPresentation,
        TerminalPresentationMode, terminal_plane_mesh,
    };
    use crate::systems::{animate_terminal_plane_warp, plane_surface_point};

    #[test]
    fn cell_from_uv_floors_and_clamps() {
        assert_eq!(cell_from_uv(Vec2::new(0.0, 0.0), 80, 24), UVec2::new(0, 0));
        assert_eq!(
            cell_from_uv(Vec2::new(1.0, 1.0), 80, 24),
            UVec2::new(79, 23),
            "the closed edge lands on the last cell"
        );
        assert_eq!(
            cell_from_uv(Vec2::new(0.5, 0.5), 80, 24),
            UVec2::new(40, 12)
        );
        assert_eq!(
            cell_from_uv(Vec2::new(-0.2, 1.7), 80, 24),
            UVec2::new(0, 23),
            "out-of-range UV (a clamped capture) stays on the grid"
        );
    }

    #[test]
    fn clamped_plane_uv_clamps_and_refuses_parallel_rays() {
        // A plane rotated like the 3D stage rotates it, scaled to a
        // terminal footprint.
        let transform = GlobalTransform::from(Transform {
            translation: Vec3::ZERO,
            rotation: Quat::from_euler(EulerRot::XYZ, 0.08, 0.18, 0.0),
            scale: Vec3::new(800.0, 600.0, 1.0),
        });

        // A ray straight down -Z far beyond the plane's right edge clamps
        // to u = 1.0.
        let ray = Ray3d {
            origin: Vec3::new(3000.0, 0.0, 1000.0),
            direction: Dir3::NEG_Z,
        };
        let uv = clamped_plane_uv(ray, &transform).expect("non-parallel ray intersects");
        assert_eq!(uv.x, 1.0, "off the right edge clamps to the last column");

        // Dead center round-trips to the center UV.
        let center = clamped_plane_uv(
            Ray3d {
                origin: Vec3::new(0.0, 0.0, 1000.0),
                direction: Dir3::NEG_Z,
            },
            &transform,
        )
        .expect("center ray intersects");
        assert!(
            (center - Vec2::splat(0.5)).length() < 1e-5,
            "the plane's origin is the center cell chart point, got {center:?}"
        );

        // A ray lying inside the plane is parallel: no honest cell exists.
        let in_plane = transform.affine().transform_vector3(Vec3::X);
        assert!(
            clamped_plane_uv(
                Ray3d {
                    origin: Vec3::new(0.0, 0.0, 5000.0),
                    direction: Dir3::new(in_plane).expect("nonzero"),
                },
                &transform,
            )
            .is_none(),
            "a parallel ray freezes the drag instead of inventing a cell"
        );
    }

    /// The shared scaffold: one seat, its front plane carrying the real
    /// `terminal_plane_mesh(32, 20)`, warped by the REAL warp system, with
    /// the Aabb recomputed from the warped positions the way the render
    /// app's `calculate_bounds` would.
    fn warped_world(
        mode: TerminalPresentationMode,
        warp_amount: f32,
        yaw: f32,
        pitch: f32,
        elapsed: Duration,
    ) -> (World, Entity, Entity, Transform) {
        bevy::tasks::ComputeTaskPool::get_or_init(bevy::tasks::TaskPool::default);
        let mut world = World::new();
        world.init_resource::<Assets<Mesh>>();
        world.insert_resource(TerminalPresentation { mode });
        world.insert_resource(MobiusTransition::default());
        world.init_resource::<crate::focus::FocusedTerminal>();
        let mut time = Time::<()>::default();
        time.advance_by(elapsed);
        world.insert_resource(time);

        let front = world
            .resource_mut::<Assets<Mesh>>()
            .add(terminal_plane_mesh(32, 20));
        let back = world
            .resource_mut::<Assets<Mesh>>()
            .add(terminal_plane_mesh(32, 20));

        let seat = world
            .spawn((
                TerminalPlaneWarp {
                    amount: warp_amount,
                },
                TerminalPlaneMeshes {
                    front: front.clone(),
                    back,
                },
            ))
            .id();
        world
            .resource_mut::<crate::focus::FocusedTerminal>()
            .set_for_test(Some(seat));

        let transform = Transform {
            translation: Vec3::ZERO,
            rotation: Quat::from_euler(EulerRot::XYZ, pitch, yaw, 0.0),
            scale: Vec3::new(800.0, 600.0, 1.0),
        };
        let plane = world
            .spawn((
                TerminalPlane,
                TerminalOwner(seat),
                Mesh3d(front.clone()),
                RayCastBackfaces,
                transform,
                GlobalTransform::from(transform),
                Visibility::Visible,
            ))
            .id();

        world
            .run_system_once(animate_terminal_plane_warp)
            .expect("warp system runs");

        let aabb: Aabb = world
            .resource::<Assets<Mesh>>()
            .get(&front)
            .expect("front mesh lives")
            .compute_aabb()
            .expect("plane mesh has an Aabb");
        world.entity_mut(plane).insert(aabb);

        (world, seat, plane, transform)
    }

    /// The mesh the warp system just wrote, reproduced through the pure
    /// forward chart exactly as `animate_terminal_plane_warp` feeds it:
    /// the PULSE (not the raw amount) is the chart's warp argument, and
    /// the Plane3d front mesh negates z.
    fn forward_chart_point(
        mode: TerminalPresentationMode,
        cell: UVec2,
        cols: u16,
        rows: u16,
        warp_amount: f32,
        elapsed_secs: f32,
        transform: &Transform,
    ) -> Vec3 {
        let local_x = (cell.x as f32 + 0.5) / cols as f32 - 0.5;
        let local_y = 0.5 - (cell.y as f32 + 0.5) / rows as f32;
        let pulse = warp_amount * (0.96 + 0.04 * (elapsed_secs * 2.2).sin());
        let mobius_progress = if mode == TerminalPresentationMode::Mobius3d {
            1.0
        } else {
            0.0
        };
        let mut point = plane_surface_point(
            mode,
            local_x,
            local_y,
            pulse,
            elapsed_secs,
            0.0,
            mobius_progress,
        );
        if mode == TerminalPresentationMode::Plane3d {
            point.z *= -1.0;
        }
        transform.transform_point(point)
    }

    fn pick_cell_by_ray(world: &mut World, ray: Ray3d, cols: u16, rows: u16) -> Option<UVec2> {
        world
            .run_system_once(move |mut pick: PickParams| {
                pick.pick_ray(ray)
                    .map(|(_, uv)| cell_from_uv(uv, cols, rows))
            })
            .expect("pick runs")
    }

    /// #58 exit criterion: pick-vs-forward-chart accuracy, Plane3d. Every
    /// sampled cell placed on the animated warped surface by the forward
    /// chart picks back as itself through the stage camera's ray.
    #[test]
    fn pick_round_trips_the_forward_chart_in_plane3d() {
        let (cols, rows) = (80u16, 24u16);
        let elapsed = Duration::from_millis(1234);
        let (mut world, _seat, _plane, transform) =
            warped_world(TerminalPresentationMode::Plane3d, 1.0, 0.35, -0.2, elapsed);
        assert_eq!(
            world.query::<&TerminalPlaneWarp>().iter(&world).count(),
            1,
            "seat count asserted (#58 rider)"
        );

        for col in (0..cols).step_by(7).chain([cols - 1]) {
            for row in (0..rows).step_by(5).chain([rows - 1]) {
                let cell = UVec2::new(col as u32, row as u32);
                let point = forward_chart_point(
                    TerminalPresentationMode::Plane3d,
                    cell,
                    cols,
                    rows,
                    1.0,
                    elapsed.as_secs_f32(),
                    &transform,
                );
                // The stage camera is orthographic looking straight down
                // -Z: the pointer over `point` is the ray through its
                // world x/y.
                let ray = Ray3d {
                    origin: point + Vec3::Z * 1000.0,
                    direction: Dir3::NEG_Z,
                };
                assert_eq!(
                    pick_cell_by_ray(&mut world, ray, cols, rows),
                    Some(cell),
                    "cell {cell:?} must round-trip through the warped mesh"
                );
            }
        }
    }

    /// #58 exit criterion: pick-vs-forward-chart accuracy, Mobius3d. The
    /// strip self-occludes, so a sample only asserts equality when its
    /// chart point is the ray's nearest strip crossing; occluded samples
    /// must resolve to a strictly NEARER surface point, never garbage —
    /// and the far half must contribute exact round trips (backface
    /// picking), or the sweep fails.
    #[test]
    fn pick_round_trips_the_forward_chart_in_mobius3d() {
        let (cols, rows) = (80u16, 24u16);
        let elapsed = Duration::from_millis(4321);
        let warp = 0.4;
        let (mut world, _seat, _plane, transform) = warped_world(
            TerminalPresentationMode::Mobius3d,
            warp,
            0.18,
            0.08,
            elapsed,
        );
        assert_eq!(
            world.query::<&TerminalPlaneWarp>().iter(&world).count(),
            1,
            "seat count asserted (#58 rider)"
        );

        let chart = |cell: UVec2| {
            forward_chart_point(
                TerminalPresentationMode::Mobius3d,
                cell,
                cols,
                rows,
                warp,
                elapsed.as_secs_f32(),
                &transform,
            )
        };

        let mut samples = 0usize;
        let mut exact = 0usize;
        let mut far_half_exact = 0usize;
        let mut grazing = 0usize;
        for col in (0..cols).step_by(3) {
            for row in (0..rows).step_by(6).chain([rows - 1]) {
                samples += 1;
                let cell = UVec2::new(col as u32, row as u32);
                let point = chart(cell);
                let ray = Ray3d {
                    origin: point + Vec3::Z * 2000.0,
                    direction: Dir3::NEG_Z,
                };
                let Some(picked) = pick_cell_by_ray(&mut world, ray, cols, rows) else {
                    // Where the twisted strip turns edge-on to the −Z
                    // ray (|sin(half)| near 1), the target is a sliver
                    // and Möller–Trumbore honestly rejects the graze.
                    grazing += 1;
                    continue;
                };
                if picked == cell {
                    exact += 1;
                    // u > 0.5 is the strip's far semicircle (angle past π).
                    if cell.x as f32 / cols as f32 > 0.5 {
                        far_half_exact += 1;
                    }
                } else {
                    // The mesh facets 32×20 segments; the analytic chart
                    // is smooth. On steep bands the chordal deviation
                    // legitimately shifts a pick one cell — the mesh IS
                    // the rendered truth. Anything further must be a
                    // genuine occluder nearer the camera.
                    let adjacent = picked.x.abs_diff(cell.x) <= 1 && picked.y.abs_diff(cell.y) <= 1;
                    let occluder = chart(picked);
                    assert!(
                        adjacent || occluder.z > point.z - 0.5,
                        "a mismatched pick must be facet-adjacent or a genuine occluder: \
                         cell {cell:?} (z {}) was beaten by {picked:?} (z {})",
                        point.z,
                        occluder.z
                    );
                }
            }
        }
        assert!(
            grazing * 4 < samples,
            "edge-on grazes must stay a sliver of the sweep: {grazing}/{samples}"
        );
        assert!(
            exact * 2 > samples,
            "the visible strip must round-trip broadly: {exact}/{samples} exact"
        );
        assert!(
            far_half_exact > 20,
            "the Möbius far half must be pickable (#58 exit criterion), got {far_half_exact}"
        );
    }

    /// The far half is pickable BECAUSE of `RayCastBackfaces`: a
    /// genuinely back-facing hit (found by computing the mesh triangle's
    /// facing, not assumed) disappears when the marker is removed — the
    /// always-on policy, not luck, is what the exit criterion rides on.
    #[test]
    fn the_mobius_far_half_needs_raycast_backfaces() {
        let (cols, rows) = (80u16, 24u16);
        let elapsed = Duration::from_millis(777);
        let warp = 0.4;
        let (mut world, _seat, plane, transform) = warped_world(
            TerminalPresentationMode::Mobius3d,
            warp,
            0.18,
            0.08,
            elapsed,
        );

        let chart = |cell: UVec2| {
            forward_chart_point(
                TerminalPresentationMode::Mobius3d,
                cell,
                cols,
                rows,
                warp,
                elapsed.as_secs_f32(),
                &transform,
            )
        };

        // Find a cell on the strip's away-facing half — STRONGLY
        // back-facing toward the camera, not edge-on (the twist decides
        // where along u that half lies): the mesh winds CCW over the
        // (u, v) grid, so the facet normal is (P(v+dv) − P) × (P(u+du)
        // − P); the ray travels −Z, and a backface has normal.z < 0.
        // Verify the pick lands before trusting the sample — grazes and
        // occlusions are real strip geometry, not policy.
        let mut verified = None;
        'search: for col in 0..cols {
            for row in 0..rows {
                let cell = UVec2::new(col as u32, row as u32);
                let p = chart(cell);
                let down_v = chart(UVec2::new(cell.x, cell.y + 1));
                let along_u = chart(UVec2::new(cell.x + 1, cell.y));
                let normal = (down_v - p).cross(along_u - p).normalize();
                if normal.z >= -0.5 {
                    continue;
                }
                // Sampled just in front of its own surface point, so no
                // other strip crossing can occlude the hit.
                let ray = Ray3d {
                    origin: p + Vec3::Z * 5.0,
                    direction: Dir3::NEG_Z,
                };
                if let Some(picked) = pick_cell_by_ray(&mut world, ray, cols, rows)
                    && picked.x.abs_diff(cell.x) <= 1
                    && picked.y.abs_diff(cell.y) <= 1
                {
                    verified = Some(ray);
                    break 'search;
                }
            }
        }
        let ray =
            verified.expect("some strongly back-facing far-half cell picks with backfaces on");

        world.entity_mut(plane).remove::<RayCastBackfaces>();
        assert_eq!(
            pick_cell_by_ray(&mut world, ray, cols, rows),
            None,
            "without RayCastBackfaces the far half is unpickable — the \
             always-on policy is load-bearing"
        );
    }

    /// The hover pick misses honestly off the mesh and refuses hidden
    /// planes; the capture chart clamps that same miss to an edge cell,
    /// off the plane's own transform — the drag-off-surface rule.
    #[test]
    fn hover_misses_where_the_capture_chart_clamps() {
        let elapsed = Duration::from_millis(50);
        let (mut world, _seat, plane, transform) =
            warped_world(TerminalPresentationMode::Plane3d, 0.0, 0.18, 0.08, elapsed);

        // A ray beyond the plane's right edge (local x = 2.0).
        let ray = Ray3d {
            origin: transform.transform_point(Vec3::new(2.0, 0.0, 0.0)) + Vec3::Z * 1000.0,
            direction: Dir3::NEG_Z,
        };
        assert_eq!(
            pick_cell_by_ray(&mut world, ray, 80, 24),
            None,
            "the hover pick honestly misses off the mesh"
        );
        let global = *world
            .entity(plane)
            .get::<GlobalTransform>()
            .expect("plane has a transform");
        let uv = clamped_plane_uv(ray, &global).expect("the capture chart never misses");
        assert_eq!(uv.x, 1.0, "off the right edge clamps to the last column");

        // A hidden plane (focused-1:1 keeps non-focused seats off the
        // stage, with STALE meshes) must never eat a hover pick. The
        // capture path deliberately ignores visibility instead — pinned
        // by `capture_reads_the_live_mesh_before_the_flat_chart` below
        // and the mid-drag hidden-plane assertion in the mouse tests.
        world.entity_mut(plane).insert(Visibility::Hidden);
        let center_ray = Ray3d {
            origin: Vec3::new(0.0, 0.0, 1000.0),
            direction: Dir3::NEG_Z,
        };
        assert_eq!(
            pick_cell_by_ray(&mut world, center_ray, 80, 24),
            None,
            "hover never picks a hidden plane"
        );
    }

    /// The capture chart is mesh-FIRST, flat-chart-fallback — in that
    /// order. Review-verified gap: with the mesh cast disabled entirely,
    /// every earlier test still passed (their planes were flat, so mesh
    /// and chart agreed numerically). Over a warped bulge they disagree,
    /// and a captured drag must read the live surface (#58's "drag clamp
    /// holds" in its 3D-warped half).
    #[test]
    fn capture_reads_the_live_mesh_before_the_flat_chart() {
        let (cols, rows) = (80u16, 24u16);
        let elapsed = Duration::from_millis(1234);
        let (mut world, seat, plane, transform) =
            warped_world(TerminalPresentationMode::Plane3d, 1.0, 0.35, -0.2, elapsed);

        // An off-center cell on the bulge: warp displaces it along +z
        // and, through the plane rotation, across screen x/y — the flat
        // chart names a DIFFERENT cell for the same ray.
        let cell = UVec2::new(30, 8);
        let point = forward_chart_point(
            TerminalPresentationMode::Plane3d,
            cell,
            cols,
            rows,
            1.0,
            elapsed.as_secs_f32(),
            &transform,
        );
        let ray = Ray3d {
            origin: point + Vec3::Z * 1000.0,
            direction: Dir3::NEG_Z,
        };
        let (mesh_uv, captured_uv) = world
            .run_system_once(move |mut pick: PickParams| {
                (
                    pick.pick_ray(ray).map(|(_, uv)| uv),
                    pick.capture_ray(ray, seat),
                )
            })
            .expect("pick runs");
        let mesh_uv = mesh_uv.expect("the ray hits the warped mesh");
        let captured_uv = captured_uv.expect("capture always yields");
        assert!(
            (captured_uv - mesh_uv).length() < 1e-6,
            "capture takes the mesh hit verbatim: {captured_uv:?} vs {mesh_uv:?}"
        );
        let global = *world
            .entity(plane)
            .get::<GlobalTransform>()
            .expect("plane has a transform");
        let flat_uv = clamped_plane_uv(ray, &global).expect("the flat chart intersects");
        assert!(
            (captured_uv - flat_uv).length() > 0.005,
            "over the bulge the flat chart must DISAGREE with the mesh \
             (mesh {captured_uv:?} vs flat {flat_uv:?}) — a capture that \
             clamps-always would return the wrong cell here"
        );

        // Off the mesh the fallback is still the clamp, through the same
        // entry point.
        let off_ray = Ray3d {
            origin: transform.transform_point(Vec3::new(2.0, 0.0, 0.0)) + Vec3::Z * 1000.0,
            direction: Dir3::NEG_Z,
        };
        let off_uv = world
            .run_system_once(move |mut pick: PickParams| pick.capture_ray(off_ray, seat))
            .expect("pick runs")
            .expect("the capture chart never misses");
        assert_eq!(
            off_uv.x, 1.0,
            "off the right edge clamps to the last column"
        );
    }
}
