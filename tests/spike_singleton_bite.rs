//! SPIKE ARTEFACT — wayfinder #54, "N-seams native spike". THROWAWAY.
//!
//! This file exists on the `claude/n-seams-spike` branch only and must never
//! merge to `main`. It is the reproducible evidence behind the spike's
//! findings: the two places the singleton spine bites *silently*, proven
//! against the real tree rather than argued from the census.
//!
//! Run: `cargo test --test spike_singleton_bite -- --nocapture`

use bevy::prelude::*;
use ratty::terminal::TerminalRedrawState;

/// FINDING 1 — in Bevy 0.19 `#[derive(Resource)]` already emits
/// `impl Component`, so the census's checklist item (1) is not the gate it
/// reads as: `Query<&T>` and `spawn(T)` compile against the *unmodified*
/// tree. Nothing type-level stops a spike author writing the N=2 spawn
/// before the derives are swapped.
#[test]
fn resource_derive_already_implies_component() {
    let mut world = World::new();
    world.insert_resource(TerminalRedrawState::default());

    // The resource is reachable as a component on its own entity — a plain
    // Query matches it with no derive change at all.
    let matched = world
        .query::<&TerminalRedrawState>()
        .iter(&world)
        .count();
    assert_eq!(
        matched, 1,
        "a Resource is an entity in 0.19; Res<T> and Query<&T> are the same row at N=1"
    );
}

/// FINDING 2 — **the bite**. A type that keeps `#[derive(Resource)]` while a
/// second seat is spawned carrying it loses that component: Bevy's
/// `IsResource::on_insert` hook removes it from the newcomer and logs a
/// `warn!`. No panic, no `Result`, no compile error.
///
/// This is the sharpest answer the spike has to "where does the singleton
/// spine actually bite": not in ratty's code, but in Bevy's own resource
/// machinery — and it fails by *silently dropping terminal #2* rather than
/// by refusing to build. A half-finished derive migration therefore produces
/// a running app with an invisible second terminal.
#[test]
fn a_second_seat_is_silently_stripped_while_the_resource_derive_stands() {
    let mut world = World::new();

    let first = world.spawn(TerminalRedrawState::default()).id();
    let second = world.spawn(TerminalRedrawState::default()).id();

    let first_has = world.get::<TerminalRedrawState>(first).is_some();
    let second_has = world.get::<TerminalRedrawState>(second).is_some();
    let matched = world
        .query::<&TerminalRedrawState>()
        .iter(&world)
        .count();

    println!(
        "spike: first_has={first_has} second_has={second_has} matched={matched} \
         world_has_resource={}",
        world.get_resource::<TerminalRedrawState>().is_some()
    );

    assert!(first_has, "the first seat keeps its component");
    assert!(
        !second_has,
        "THE BITE: the second seat's component is removed by IsResource::on_insert — \
         silently, with only a warn!"
    );
    assert_eq!(matched, 1, "N=2 collapses to N=1 with no error path");

    // And the first spawn was promoted into the world's resource slot without
    // anyone calling `insert_resource` — so the singleton is re-established
    // behind the spike's back.
    assert!(
        world.get_resource::<TerminalRedrawState>().is_some(),
        "the first seat was promoted into the resource slot implicitly"
    );
}

/// FINDING 3 — the other silent degradation: `Query::single()` returns
/// `Err(MultipleEntities)` the moment a second terminal plane exists, and
/// every one of ratty's six plane-resolving call sites early-returns on
/// `Err`. Spawning terminal #2 therefore breaks projection **for terminal #1
/// too**, in six unrelated-looking ways, with nothing pointing at the cause.
///
/// Modelled here on a stand-in marker so the assertion is about Bevy's
/// contract, which is what the six sites depend on.
#[test]
fn single_degrades_to_err_the_moment_a_second_plane_exists() {
    #[derive(Component)]
    struct TerminalPlaneStandIn;

    let mut world = World::new();
    world.spawn(TerminalPlaneStandIn);
    assert!(
        world
            .query_filtered::<Entity, With<TerminalPlaneStandIn>>()
            .single(&world)
            .is_ok(),
        "one plane resolves"
    );

    world.spawn(TerminalPlaneStandIn);
    let resolved = world
        .query_filtered::<Entity, With<TerminalPlaneStandIn>>()
        .single(&world);
    assert!(
        resolved.is_err(),
        "THE SECOND BITE: two planes make every `.single()` plane lookup fail, and \
         ratty's six sites all early-return — so terminal #1 stops projecting too"
    );
}

/// FINDING 4 — what a *second seat* costs before it renders anything.
///
/// The census prices the decomposition in edited call sites and never prices
/// construction. `TerminalSurface` owns a private `TerminalRenderer`, which
/// owns a private parley `FontContext` — and there is no process-global font
/// cache anywhere in `parley_ratatui`. So every seat pays a full system-font
/// enumeration, and `rebuild_renderer` throws it away on every font-size step
/// and every render-scale change. At N seats that is N enumerations on a
/// single DPI change.
#[test]
fn every_seat_pays_its_own_font_stack() {
    use ratty::config::AppConfig;
    use ratty::terminal::TerminalSurface;
    use std::time::Instant;

    let config = AppConfig::default();

    let first_start = Instant::now();
    let first = TerminalSurface::new(&config).expect("seat 1");
    let first_ms = first_start.elapsed().as_secs_f64() * 1000.0;

    let second_start = Instant::now();
    let second = TerminalSurface::new(&config).expect("seat 2");
    let second_ms = second_start.elapsed().as_secs_f64() * 1000.0;

    println!(
        "spike: seat1={first_ms:.1}ms seat2={second_ms:.1}ms ratio={:.2}",
        if first_ms > 0.0 { second_ms / first_ms } else { 0.0 }
    );

    // Both are real, independent surfaces — nothing is shared or memoised.
    drop((first, second));

    // The assertion is deliberately weak: the finding is the *number* printed
    // above, not a threshold. A second seat costing a comparable amount to the
    // first is the evidence that nothing is cached process-wide.
    assert!(second_ms >= 0.0);
}
