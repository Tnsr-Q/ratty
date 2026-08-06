//! Focus: the screen-global authority over which terminal the user's
//! keystrokes reach (#56 cluster 3, ratifying #51's Shape R).
//!
//! The eight focus invariants (#56 decision 7), enforced here:
//!
//! 1. One authority, `Option<Entity>`-shaped, screen-global
//!    ([`FocusedTerminal`]); at most one focused terminal, zero legal.
//! 2. One writer: [`drain_focus_requests`] is the only mutation site; every
//!    origin is a [`FocusRequest`], never a direct write.
//! 3. User beats wire within a frame (the `src/web.rs` "JS controls are
//!    user input" precedent, extended).
//! 4. Focus is routing, never authority — it aims the user's keyboard and
//!    widens nothing a wire may do. Wire delivery ignores focus entirely.
//! 5. Focus transitions are loud: [`FocusGained`]/[`FocusLost`] fire for
//!    both affected terminals and both repaint.
//! 6. Translation modes travel with the parser; the keyboard `Local`
//!    carries only physical modifier state (see `src/keyboard.rs`).
//! 7. Capture beats hover beats focus for the mouse (M4.6's picking seam).
//! 8. No implicit focus: no fallback-to-terminal-#1 from nothing, no focus
//!    from wire spawn, no focus from a missing pick. Death succession among
//!    live terminals (decision 8's restatement) is NOT implicit focus —
//!    the drain's MRU fallback exists so a normal shell `exit` does not
//!    leave the user deaf.
//!
//! Lifecycle policy (#56 decision 8): boot focuses the boot seat via
//! [`FocusOrigin::SpawnPolicy`]; user-initiated spawns focus their child;
//! wire spawns never do (no grant exception); on the focused terminal's
//! death the drain applies most-recently-focused succession.

use std::cmp::Reverse;

use bevy::ecs::message::{Message, MessageReader, MessageWriter};
use bevy::prelude::*;

use crate::identity::{TerminalId, TerminalIdentity};
use crate::terminal::TerminalRedrawState;

/// Where a [`FocusRequest`] originated. The class split (user vs wire)
/// is the whole arbitration surface: within a frame any user-class
/// request beats any wire-class request, and within a class the last
/// request wins (the `src/web.rs` writer-order precedent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusOrigin {
    /// A pointer press that hit a terminal plane (M4.6's picking seam).
    /// User class.
    PointerPress,
    /// A granted `term.focus` wire verb (M4.5, `TerminalFocus`-gated).
    /// Wire class — the keystroke-capture primitive, arrived at politely.
    WireVerb,
    /// A page-level control on wasm ("JS controls are user input",
    /// `src/web.rs`). User class.
    JsControl,
    /// Lifecycle policy (#56 decision 8): boot focuses the boot seat; a
    /// user-initiated spawn focuses its child. Wire spawns never emit
    /// this. User class.
    SpawnPolicy,
    /// The focus-cycle chord (#56 decision 10's rider). User class.
    Keybinding,
    /// Most-recently-focused succession after the focused terminal dies.
    /// Synthesized by the drain itself — no external emitter.
    Fallback,
}

impl FocusOrigin {
    /// Whether this origin carries user intent (invariant 3's arbitration
    /// class).
    fn is_user_class(self) -> bool {
        match self {
            FocusOrigin::PointerPress
            | FocusOrigin::JsControl
            | FocusOrigin::SpawnPolicy
            | FocusOrigin::Keybinding => true,
            FocusOrigin::WireVerb | FocusOrigin::Fallback => false,
        }
    }
}

/// A request to move focus. Every writer — pointer, wire verb, JS
/// control, spawn policy — emits one of these; only
/// [`drain_focus_requests`] touches the authority (invariant 2).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusRequest {
    /// The terminal seat to focus, or `None` to focus nothing.
    pub target: Option<Entity>,
    /// The request's arbitration class and audit trail.
    pub origin: FocusOrigin,
}

/// Fired for the terminal that just became focused (invariant 5). Both
/// sides of a transition repaint — the drain requests the redraws
/// itself, so observers need not.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusGained(pub Entity);

/// Fired for the terminal that just lost focus (invariant 5). Fires only
/// for a live loser — a despawned terminal cannot repaint and its
/// corpse gets no events.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusLost(pub Entity);

/// The focus authority (Shape R): a screen-global `Option<Entity>` plus
/// the most-recently-focused history that drives death succession and
/// the focus-cycle keybinding.
///
/// Fields are private: the type enforces cardinality-at-most-one, and
/// the single-writer invariant (only [`drain_focus_requests`] mutates,
/// in this module) enforces the rest. A dangling entity can sit in
/// `current` for the slice of a frame between a despawn and the drain's
/// next run — generational ids make every lookup through it fail safely,
/// and the drain sweeps it into MRU succession.
#[derive(Resource, Default)]
pub struct FocusedTerminal {
    current: Option<Entity>,
    /// Most-recently-focused first. Pruned to live seats whenever the
    /// drain mutates (bounded by the live-terminal count); stale entries
    /// in between only ever lose lookups, never win them.
    mru: Vec<Entity>,
}

impl FocusedTerminal {
    /// The focused terminal seat, if any (zero is legal: before boot
    /// policy lands, and the frame slice after the focused terminal
    /// dies).
    pub fn get(&self) -> Option<Entity> {
        self.current
    }

    /// Whether `entity` is the focused terminal.
    pub fn is_focused(&self, entity: Entity) -> bool {
        self.current == Some(entity)
    }

    /// Test-only direct write, bypassing the drain — for worlds that
    /// assert a consumer's behavior rather than drain policy. Production
    /// writes go through [`drain_focus_requests`] alone (invariant 2).
    #[cfg(test)]
    pub(crate) fn set_for_test(&mut self, entity: Option<Entity>) {
        self.current = entity;
    }

    /// The focus-cycle target (#56 decision 10's rider): the
    /// least-recently-focused live seat, so repeated cycling visits
    /// every terminal round-robin off pure MRU data. Never-focused seats
    /// count as least recent of all (tie-broken by mint order, ascending
    /// [`TerminalId`]); the current seat is never a target. `None` means
    /// the chord is a no-op — nothing live, or only the focused seat
    /// remains.
    pub fn cycle_target(&self, live: &[(Entity, TerminalId)]) -> Option<Entity> {
        live.iter()
            .filter(|(entity, _)| Some(*entity) != self.current)
            .max_by_key(|(entity, id)| {
                let recency = match self.mru.iter().position(|m| m == entity) {
                    // In the history: larger index = focused longer ago.
                    Some(index) => index,
                    // Never focused: least recent of all.
                    None => usize::MAX,
                };
                (recency, Reverse(*id))
            })
            .map(|(entity, _)| *entity)
    }
}

/// The single writer (invariant 2): validates targets against live
/// seats, arbitrates user-beats-wire within the frame, applies
/// most-recently-focused succession when the focused terminal has died,
/// and makes every transition loud (invariant 5) — both sides get their
/// redraw requested here, not by observers.
pub(crate) fn drain_focus_requests(
    mut requests: MessageReader<FocusRequest>,
    mut focus: ResMut<FocusedTerminal>,
    seats: Query<(Entity, &TerminalIdentity)>,
    mut redraw: Query<&mut TerminalRedrawState>,
    mut gained: MessageWriter<FocusGained>,
    mut lost: MessageWriter<FocusLost>,
) {
    let alive = |entity: Entity| seats.contains(entity);

    // Arbitrate the frame's requests. A request naming a corpse is
    // dropped at the door — the authority must never point at a dead
    // entity on purpose (invariant 1's safe half).
    let mut winner: Option<FocusRequest> = None;
    for request in requests.read() {
        if let Some(target) = request.target
            && !alive(target)
        {
            debug!("drain_focus_requests: dropping request for dead entity {target:?}");
            continue;
        }
        winner = match winner {
            // User class beats wire class within the frame; within a
            // class, the last request wins (invariant 3).
            Some(current) if current.origin.is_user_class() && !request.origin.is_user_class() => {
                Some(current)
            }
            _ => Some(*request),
        };
    }

    // Death succession (#56 decision 8): the focused terminal died and no
    // valid request replaced it — the most-recently-focused survivor
    // takes over; with no history, mint order. Never `None` while live
    // terminals exist: under `None` every normal shell `exit` would drop
    // the user deaf until they click something.
    let stale_current = focus.current.is_some_and(|current| !alive(current));
    if winner.is_none() && stale_current {
        let survivor = focus
            .mru
            .iter()
            .copied()
            .find(|&entity| alive(entity))
            .or_else(|| {
                seats
                    .iter()
                    .min_by_key(|(_, identity)| identity.id())
                    .map(|(entity, _)| entity)
            });
        winner = Some(FocusRequest {
            target: survivor,
            origin: FocusOrigin::Fallback,
        });
    }

    let Some(request) = winner else {
        return;
    };
    // A no-op request must not touch the resource: consumers react to
    // change ticks, and a stale `current` equal to the target is
    // impossible (corpse targets were dropped above).
    if request.target == focus.current {
        return;
    }

    let previous = focus.current;
    focus.current = request.target;
    if let Some(target) = request.target {
        focus.mru.retain(|&entity| entity != target);
        focus.mru.insert(0, target);
    }
    // Prune corpses while already mutating: MRU stays bounded by the
    // live-seat count without spending a change tick on quiet frames.
    focus.mru.retain(|&entity| alive(entity));

    // Loud transitions (invariant 5): events for both sides, redraws for
    // both live sides — the cursor style flips on each.
    if let Some(previous) = previous
        && alive(previous)
    {
        lost.write(FocusLost(previous));
        if let Ok(mut redraw) = redraw.get_mut(previous) {
            redraw.request();
        }
    }
    if let Some(target) = request.target {
        gained.write(FocusGained(target));
        if let Ok(mut redraw) = redraw.get_mut(target) {
            redraw.request();
        }
    }
}

/// Boot half of the lifecycle policy (#56 decision 8): startup focuses
/// the boot terminal through the same request bus as every other writer.
/// Runs at Startup after `setup_scene`; like `setup_scene`, a world
/// without exactly one boot seat is a broken world.
pub(crate) fn focus_boot_terminal(
    seats: Query<Entity, With<TerminalIdentity>>,
    mut requests: MessageWriter<FocusRequest>,
) {
    let seat = seats.single().expect("exactly one terminal seat at boot");
    requests.write(FocusRequest {
        target: Some(seat),
        origin: FocusOrigin::SpawnPolicy,
    });
}

#[cfg(test)]
mod tests {
    use bevy::ecs::message::Messages;
    use bevy::ecs::system::RunSystemOnce;

    use super::*;
    use crate::identity::TerminalRegistry;

    /// A world with the focus authority, the three focus messages, and a
    /// registry to mint real identities from.
    fn focus_world() -> World {
        let mut world = World::new();
        world.init_resource::<FocusedTerminal>();
        world.init_resource::<Messages<FocusRequest>>();
        world.init_resource::<Messages<FocusGained>>();
        world.init_resource::<Messages<FocusLost>>();
        world.insert_resource(TerminalRegistry::default());
        world
    }

    /// Spawns a terminal seat with a freshly minted identity and a
    /// redraw flag, returning the seat entity.
    fn spawn_seat(world: &mut World) -> Entity {
        let identity = world
            .resource_mut::<TerminalRegistry>()
            .allocate()
            .expect("the test pool is nowhere near 128 seats");
        world.spawn((identity, TerminalRedrawState::default())).id()
    }

    fn request(world: &mut World, target: Option<Entity>, origin: FocusOrigin) {
        world
            .resource_mut::<Messages<FocusRequest>>()
            .write(FocusRequest { target, origin });
    }

    fn run_drain(world: &mut World) {
        world
            .run_system_once(drain_focus_requests)
            .expect("drain runs");
    }

    fn focused(world: &mut World) -> Option<Entity> {
        world.resource::<FocusedTerminal>().get()
    }

    /// Clears both seats' redraw flags so a test can assert exactly who
    /// repainted.
    fn clear_redraws(world: &mut World) {
        let mut query = world.query::<&mut TerminalRedrawState>();
        for mut redraw in query.iter_mut(world) {
            redraw.take();
        }
    }

    /// Reads-and-clears the seat's redraw flag (the flag has no
    /// non-consuming read accessor by design).
    fn needs_redraw(world: &mut World, seat: Entity) -> bool {
        world
            .get_mut::<TerminalRedrawState>(seat)
            .expect("seat has a redraw flag")
            .take()
    }

    #[test]
    fn a_request_focuses_a_live_seat_and_no_request_means_no_focus() {
        let mut world = focus_world();
        let seat = spawn_seat(&mut world);
        assert_eq!(
            world.query::<&TerminalIdentity>().iter(&world).count(),
            1,
            "seat count asserted (#58 rider)"
        );

        // Invariant 8: live seats alone never focus anything.
        run_drain(&mut world);
        assert_eq!(focused(&mut world), None, "no implicit focus from nothing");

        request(&mut world, Some(seat), FocusOrigin::SpawnPolicy);
        run_drain(&mut world);
        assert_eq!(focused(&mut world), Some(seat));
    }

    #[test]
    fn a_user_request_beats_a_wire_request_in_the_same_frame() {
        let mut world = focus_world();
        let seat_a = spawn_seat(&mut world);
        let seat_b = spawn_seat(&mut world);
        assert_eq!(
            world.query::<&TerminalIdentity>().iter(&world).count(),
            2,
            "seat count asserted (#58 rider)"
        );

        // User first, wire last: emission order must not matter across
        // classes (invariant 3), only within one.
        request(&mut world, Some(seat_a), FocusOrigin::PointerPress);
        request(&mut world, Some(seat_b), FocusOrigin::WireVerb);
        run_drain(&mut world);
        assert_eq!(
            focused(&mut world),
            Some(seat_a),
            "the user-class request wins over the later wire request"
        );

        // Within a class the last request wins (the web.rs precedent).
        request(&mut world, Some(seat_a), FocusOrigin::PointerPress);
        request(&mut world, Some(seat_b), FocusOrigin::JsControl);
        run_drain(&mut world);
        assert_eq!(focused(&mut world), Some(seat_b));
    }

    #[test]
    fn a_request_naming_a_corpse_is_dropped() {
        let mut world = focus_world();
        let seat_a = spawn_seat(&mut world);
        let seat_b = spawn_seat(&mut world);
        request(&mut world, Some(seat_a), FocusOrigin::SpawnPolicy);
        run_drain(&mut world);

        world.despawn(seat_b);
        assert_eq!(
            world.query::<&TerminalIdentity>().iter(&world).count(),
            1,
            "seat count asserted (#58 rider)"
        );
        request(&mut world, Some(seat_b), FocusOrigin::WireVerb);
        run_drain(&mut world);
        assert_eq!(
            focused(&mut world),
            Some(seat_a),
            "the authority never points at a dead entity on purpose"
        );
    }

    #[test]
    fn focus_transitions_are_loud_and_both_sides_repaint() {
        let mut world = focus_world();
        let seat_a = spawn_seat(&mut world);
        let seat_b = spawn_seat(&mut world);
        request(&mut world, Some(seat_a), FocusOrigin::SpawnPolicy);
        run_drain(&mut world);
        clear_redraws(&mut world);
        // A bare World never swaps message buffers; drop the setup
        // transition's events so the assertion sees only the move.
        world.resource_mut::<Messages<FocusGained>>().clear();
        world.resource_mut::<Messages<FocusLost>>().clear();

        request(&mut world, Some(seat_b), FocusOrigin::PointerPress);
        run_drain(&mut world);

        let gained: Vec<_> = world
            .resource_mut::<Messages<FocusGained>>()
            .drain()
            .collect();
        let lost: Vec<_> = world
            .resource_mut::<Messages<FocusLost>>()
            .drain()
            .collect();
        assert_eq!(gained, vec![FocusGained(seat_b)]);
        assert_eq!(lost, vec![FocusLost(seat_a)]);
        assert!(
            needs_redraw(&mut world, seat_a) && needs_redraw(&mut world, seat_b),
            "both terminals repaint (invariant 5): the cursor style flips on each"
        );
    }

    #[test]
    fn a_noop_request_stays_quiet() {
        let mut world = focus_world();
        let seat = spawn_seat(&mut world);
        request(&mut world, Some(seat), FocusOrigin::SpawnPolicy);
        run_drain(&mut world);
        clear_redraws(&mut world);
        world.resource_mut::<Messages<FocusGained>>().clear();

        request(&mut world, Some(seat), FocusOrigin::PointerPress);
        run_drain(&mut world);
        assert_eq!(
            world
                .resource_mut::<Messages<FocusGained>>()
                .drain()
                .count(),
            0,
            "re-focusing the focused terminal fires nothing"
        );
        assert!(!needs_redraw(&mut world, seat));
    }

    #[test]
    fn the_focused_terminals_death_falls_back_to_the_mru_survivor() {
        let mut world = focus_world();
        let seat_a = spawn_seat(&mut world);
        let seat_b = spawn_seat(&mut world);
        let seat_c = spawn_seat(&mut world);
        assert_eq!(
            world.query::<&TerminalIdentity>().iter(&world).count(),
            3,
            "seat count asserted (#58 rider)"
        );

        // History: A focused, then C — MRU is [C, A]; B never focused.
        request(&mut world, Some(seat_a), FocusOrigin::SpawnPolicy);
        run_drain(&mut world);
        request(&mut world, Some(seat_c), FocusOrigin::PointerPress);
        run_drain(&mut world);

        // C dies mid-focus: succession lands on A, the most-recently-
        // focused survivor — not on B, and not on `None` (decision 8).
        world.despawn(seat_c);
        run_drain(&mut world);
        assert_eq!(focused(&mut world), Some(seat_a));
        let lost: Vec<_> = world
            .resource_mut::<Messages<FocusLost>>()
            .drain()
            .collect();
        assert!(
            !lost.contains(&FocusLost(seat_c)),
            "a corpse gets no events — it cannot repaint"
        );

        // A dies with no focused history left: mint order breaks the tie
        // rather than leaving the user deaf.
        world.despawn(seat_a);
        run_drain(&mut world);
        assert_eq!(focused(&mut world), Some(seat_b));

        // Bounded-resource rider: the history holds live seats only.
        let mru = &world.resource::<FocusedTerminal>().mru;
        assert_eq!(mru.as_slice(), &[seat_b], "MRU pruned to live seats");
    }

    #[test]
    fn cycle_targets_the_least_recently_focused_live_seat() {
        let mut world = focus_world();
        let seat_a = spawn_seat(&mut world);
        let seat_b = spawn_seat(&mut world);
        let seat_c = spawn_seat(&mut world);
        let live: Vec<(Entity, TerminalId)> = world
            .query::<(Entity, &TerminalIdentity)>()
            .iter(&world)
            .map(|(entity, identity)| (entity, identity.id()))
            .collect();

        request(&mut world, Some(seat_a), FocusOrigin::SpawnPolicy);
        run_drain(&mut world);

        // Never-focused seats are least recent; mint order breaks the
        // tie: A → B → C → back to A, every seat reachable (decision
        // 10's stated purpose for the rider).
        let focus = world.resource::<FocusedTerminal>();
        assert_eq!(focus.cycle_target(&live), Some(seat_b));

        request(&mut world, Some(seat_b), FocusOrigin::SpawnPolicy);
        run_drain(&mut world);
        let focus = world.resource::<FocusedTerminal>();
        assert_eq!(focus.cycle_target(&live), Some(seat_c));

        request(&mut world, Some(seat_c), FocusOrigin::SpawnPolicy);
        run_drain(&mut world);
        let focus = world.resource::<FocusedTerminal>();
        assert_eq!(focus.cycle_target(&live), Some(seat_a));
    }

    #[test]
    fn cycling_alone_is_a_noop() {
        let mut world = focus_world();
        let seat = spawn_seat(&mut world);
        let live = vec![(
            seat,
            world
                .get::<TerminalIdentity>(seat)
                .expect("seat identity")
                .id(),
        )];
        request(&mut world, Some(seat), FocusOrigin::SpawnPolicy);
        run_drain(&mut world);
        let focus = world.resource::<FocusedTerminal>();
        assert_eq!(
            focus.cycle_target(&live),
            None,
            "N=1: the chord is a no-op, never a self-focus churn"
        );
    }
}
