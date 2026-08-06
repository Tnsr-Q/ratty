//! The terminals organ (#49): terminals on the wire, and the roster that
//! makes them addressable.
//!
//! This module owns the wire-facing half of terminal lifecycle. The
//! identity half — the monotonic [`TerminalId`] and the recyclable
//! 128-slot namespace lease — lives in [`crate::identity`] and is not
//! duplicated here; the roster stores only what the wire needs: a handle,
//! who created the terminal, and what state it is in.
//!
//! ## Handles
//!
//! A handle is `<session-nonce-hex>-<seq>`, minted through the app-global
//! [`crate::query_channel::QuerySession::mint_execution_id`] — the same
//! counter macro playback and avatar utterances draw from, so no two live
//! handles of any family collide and every byte rides a wire payload and
//! JSON unescaped. Handles are references, never authority: knowing one
//! grants nothing, which is why `state.terminals` may publish every row's
//! id scene-scoped. **A handle is a name, not a secret.**
//!
//! Every seat gets a row, not just wire-spawned ones — minted at
//! [`crate::scene::dress_terminal_seat`], the single site the boot, chord
//! and wire paths already share. `state.terminals` that enumerated only
//! wire-born terminals would lie by omission, and the boot terminal needs
//! a handle for "wire-unkillable" to be a testable property rather than a
//! comment.
//!
//! ## The stamp rule
//!
//! [`TerminalRow::creator`] is a [`TerminalId`], never a namespace (#56
//! decision 17). The namespace recycles: a creator that dies frees slot 3,
//! the next terminal leases slot 3, and a creator field keyed on the
//! namespace would silently re-parent every orphan to a stranger. The
//! wire-facing `creator` in a `state.terminals` row is resolved to a live
//! namespace ordinal at read time, from the id.
//!
//! ## Honest limitations
//!
//! - `spawning` is a one-frame state — the seat entity is a deferred
//!   `Commands` insert — not an admission queue.
//! - The wire cannot place a terminal in space, and cannot choose a grid
//!   at spawn. See the `term.place` documentation for why, and
//!   `protocols/terminals.md` for the full contract.

use std::collections::{BTreeMap, HashMap};

use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::prelude::*;

use crate::identity::{
    TERMINAL_FOCUS_BURST, TERMINAL_FOCUS_PER_SEC, TERMINAL_SPAWN_BURST, TERMINAL_SPAWNS_PER_SEC,
    TerminalId,
};

/// Where a terminal is in its wire-visible lifecycle. Callers poll this
/// through `state.terminals` rather than through `state.executions`: a
/// terminal is not transport-epoch metadata, and #56 decision 19 makes
/// this field the readiness signal precisely so `term.spawn` never has to
/// ack `code=started` (which, under `protocols/query.md`'s
/// absence-means-finished rule, would tell a conforming caller the spawn
/// had completed while it was still spawning).
///
/// Two of the three are DERIVED, never stored — see
/// [`TerminalRow::wire_state`]. A stored readiness flag would need a
/// system to flip it, and when that system ran relative to the query
/// channel would decide what a caller observed: a schedule tiebreak
/// masquerading as a lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalWireState {
    /// Admitted, but the seat entity has not flushed into the world yet.
    Spawning,
    /// Live and addressable.
    Ready,
    /// A close has committed; the despawn runs after this frame's replies
    /// flush.
    Closing,
}

impl TerminalWireState {
    /// The wire spelling. Frozen contract — clients match on these.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spawning => "spawning",
            Self::Ready => "ready",
            Self::Closing => "closing",
        }
    }
}

/// One live terminal's wire-facing row.
#[derive(Debug, Clone)]
pub struct TerminalRow {
    /// The handle wire callers address this terminal by.
    pub handle: String,
    /// The terminal whose ingress asked for this one, if any. `None`
    /// means no wire caller may address it destructively: the boot
    /// terminal, every chord-spawned seat, and every orphan whose creator
    /// has died.
    pub creator: Option<TerminalId>,
    /// Whether the wire created this terminal. A mint-time fact that never
    /// changes — deliberately distinct from [`Self::creator`], which is a
    /// live relationship and IS cleared when the creator dies. Conflating
    /// them would make orphans read as chord-born.
    pub wire_born: bool,
    /// Whether a close has committed for this terminal. The one piece of
    /// lifecycle that must be stored: it is a decision the applier made,
    /// not a fact about the world.
    pub closing: bool,
}

impl TerminalRow {
    /// This terminal's wire-visible state, given whether its seat entity
    /// is live in the world right now.
    ///
    /// `spawning` is literally "the row exists and the seat does not" —
    /// `spawn_terminal` inserts the row synchronously but spawns the seat
    /// through deferred `Commands`, so the gap is real, and deriving the
    /// answer from the world means no observer can ever disagree with it.
    pub fn wire_state(&self, seat_live: bool) -> TerminalWireState {
        if self.closing {
            TerminalWireState::Closing
        } else if seat_live {
            TerminalWireState::Ready
        } else {
            TerminalWireState::Spawning
        }
    }
}

/// The wire's view of every live terminal: handle, creator, state.
///
/// A resource rather than a component on the seat, deliberately. A seat's
/// components are only visible after the `Commands` flush, so a
/// component-borne handle would not be addressable in the very batch that
/// minted it — a `term.close` naming a handle from the same PTY chunk
/// would answer `unknown-id`, and `state.terminals` could never show a
/// `spawning` row at all. The cost is exactly one line in the despawn
/// sweep.
///
/// Iteration is [`TerminalId`] order (mint order) because `TerminalId`
/// derives `Ord`: `state.terminals` pagination needs a stable sequence.
#[derive(Resource, Default)]
pub struct TerminalRoster {
    rows: BTreeMap<TerminalId, TerminalRow>,
    /// Per-arrival-terminal spawn and focus budgets. Bounded by
    /// construction — at most one entry per live terminal — and swept with
    /// their terminal.
    spawn_buckets: HashMap<TerminalId, TokenBucket>,
    focus_buckets: HashMap<TerminalId, TokenBucket>,
}

/// A token bucket over a per-terminal wire budget (the sound organ's
/// `PlayBucket` shape, generalized over its two rates).
#[derive(Debug, Clone, Copy)]
struct TokenBucket {
    tokens: f64,
    last: f64,
    burst: u32,
    per_sec: u32,
}

impl TokenBucket {
    fn new(now: f64, burst: u32, per_sec: u32) -> Self {
        Self {
            tokens: f64::from(burst),
            last: now,
            burst,
            per_sec,
        }
    }

    /// Refills by elapsed time, then takes one token if available.
    fn try_take(&mut self, now: f64) -> bool {
        let elapsed = (now - self.last).max(0.0);
        self.tokens = (self.tokens + elapsed * f64::from(self.per_sec)).min(f64::from(self.burst));
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

impl TerminalRoster {
    /// Records a freshly dressed seat. Called once per terminal, from the
    /// one dressing site.
    pub(crate) fn insert(
        &mut self,
        id: TerminalId,
        handle: String,
        creator: Option<TerminalId>,
        wire_born: bool,
    ) {
        self.rows.insert(
            id,
            TerminalRow {
                handle,
                creator,
                wire_born,
                closing: false,
            },
        );
    }

    /// The row for a live terminal.
    pub fn row(&self, id: TerminalId) -> Option<&TerminalRow> {
        self.rows.get(&id)
    }

    /// The mutable row for a live terminal. The only field a caller may
    /// change after the mint is `closing` — everything else is either a
    /// mint-time fact or the sweep's to clear.
    pub(crate) fn row_mut(&mut self, id: TerminalId) -> Option<&mut TerminalRow> {
        self.rows.get_mut(&id)
    }

    /// Resolves a wire handle to its terminal.
    ///
    /// A closed terminal's handle, a handle minted by a previous process,
    /// and a caller's invention all resolve `None` alike — the wire cannot
    /// distinguish them, and `unknown-id` is the honest answer to all
    /// three (the `owns_execution_id` staleness template). Nothing aliases:
    /// handles are never reused.
    pub fn by_handle(&self, handle: &str) -> Option<TerminalId> {
        self.rows
            .iter()
            .find(|(_, row)| row.handle == handle)
            .map(|(id, _)| *id)
    }

    /// Live terminals in the roster.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the roster holds no terminals (only true before boot).
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Every row, in mint order.
    pub fn iter(&self) -> impl Iterator<Item = (TerminalId, &TerminalRow)> {
        self.rows.iter().map(|(id, row)| (*id, row))
    }

    /// Takes a spawn token for `arrival`, refilling by elapsed time.
    pub(crate) fn take_spawn_token(&mut self, arrival: TerminalId, now: f64) -> bool {
        self.spawn_buckets
            .entry(arrival)
            .or_insert_with(|| TokenBucket::new(now, TERMINAL_SPAWN_BURST, TERMINAL_SPAWNS_PER_SEC))
            .try_take(now)
    }

    /// Takes a focus token for `arrival`, refilling by elapsed time.
    pub(crate) fn take_focus_token(&mut self, arrival: TerminalId, now: f64) -> bool {
        self.focus_buckets
            .entry(arrival)
            .or_insert_with(|| TokenBucket::new(now, TERMINAL_FOCUS_BURST, TERMINAL_FOCUS_PER_SEC))
            .try_take(now)
    }

    /// Despawn sweep: drops a dead terminal's row and budgets, and orphans
    /// every terminal it created.
    ///
    /// Orphaning rather than cascade-closing is the locked rule (#49 §2):
    /// a creator's children are principals in their own right, possibly
    /// with a user inside. Clearing `creator` makes "orphans become
    /// creator-less like terminal #1 — wire-unaddressable" literally true
    /// in the data, so the close authority needs no separate orphan clause.
    pub(crate) fn sweep_terminal(&mut self, id: TerminalId) {
        self.rows.remove(&id);
        self.spawn_buckets.remove(&id);
        self.focus_buckets.remove(&id);
        for row in self.rows.values_mut() {
            if row.creator == Some(id) {
                row.creator = None;
            }
        }
    }
}

/// One `state.terminals` row, resolved against the live world.
///
/// Built in `answer_queries` because the roster alone cannot answer it:
/// the namespace and the grid live on the seat entity and its transport,
/// and `creator_ns` is the creator's *current* namespace — resolved from
/// the creator's `TerminalId` at read time, never stored (the stamp rule).
#[derive(Debug, Clone)]
pub struct TerminalRowSnapshot {
    /// The terminal this row describes.
    pub id: TerminalId,
    /// Its wire handle.
    pub handle: String,
    /// Its lifecycle state.
    pub state: TerminalWireState,
    /// Its leased namespace.
    pub ns: u8,
    /// Its creator, if it still has one.
    pub creator: Option<TerminalId>,
    /// The creator's live namespace ordinal — the wire-facing rendering
    /// of `creator`, resolved now rather than stored.
    pub creator_ns: Option<u8>,
    /// Live grid columns, or `None` while the seat entity has not flushed.
    pub cols: Option<u16>,
    /// Live grid rows, or `None` while the seat entity has not flushed.
    pub rows: Option<u16>,
}

/// `state.terminals`: the roster as tier-1 scene-global public state.
///
/// The quads are visibly on screen, so enumerating them observes nothing
/// a viewer cannot already see, and a handle grants nothing — #18's
/// "visibility grants observation, not control" does the work.
///
/// `creator` is the one own-scoped field (#56 decision 15): it is grafted
/// on only when the querier IS the creator. Absent-when-foreign, never a
/// distinguishable "exists but hidden" marker — and the value is the
/// creator's namespace ordinal, which is why it can never appear under any
/// other key: a namespace is a stable, enumerable, wire-visible address,
/// so leaking it under a second name would defeat the scoping entirely.
pub fn terminals_state_items(
    rows: &[TerminalRowSnapshot],
    source: crate::runtime::IngressSource,
) -> Vec<(u64, serde_json::Value)> {
    use serde_json::json;

    let mut sorted: Vec<&TerminalRowSnapshot> = rows.iter().collect();
    sorted.sort_by_key(|row| row.id);
    sorted
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let mut value = json!({
                "id": row.handle,
                "state": row.state.as_str(),
                // `ns` is the leased namespace, which is also the address
                // wire object ids carry — scene-global public state, like
                // the quad itself.
                "ns": row.ns,
                "cols": row.cols,
                "rows": row.rows,
                // Live truth, not a placeholder: nothing in this build
                // renders a per-terminal position or scale. The focused
                // terminal draws 1:1 and centered, and `sync_terminal_layout`
                // rewrites every viewport centre to the origin on each
                // layout pass. `term.place` refuses these fields for the
                // same reason.
                "x": 0.0,
                "y": 0.0,
                "scale": 1.0,
            });
            if row.creator == Some(source.terminal()) {
                value["creator"] = json!(row.creator_ns);
            }
            // The pagination key is positional: `TerminalId` has no public
            // raw accessor and the wire id is a string handle, so the
            // mint-ordered index is the only honest stable key.
            (index as u64, value)
        })
        .collect()
}

/// Applies the `term.*` family.
///
/// Every arm opens the same way, in this order and for reasons that do not
/// commute:
///
/// 1. **Wire origin.** A replayed `term.spawn` forks a process and a
///    replayed `term.close` kills a session someone is typing into, so
///    macro, bookmark and rule origins are refused before anything else is
///    even read. Belt-and-suspenders beside the structural closure (the
///    family is control-plane, so nothing recordable can carry it).
/// 2. **Capability.** Both gates default DENY, derived purely from
///    `(IngressSource, AppConfig)` — the wire can never grant itself one.
/// 3. **Rate.** The live cap bounds how many terminals exist, not how fast
///    they are made; one PTY chunk can carry arbitrarily many commands.
/// 4. **Target resolution**, then ownership, then validation, then commit.
#[allow(clippy::too_many_arguments)]
pub fn apply_terminal_commands(
    mut commands: MessageReader<crate::ai::AiCommand>,
    mut spawn: crate::scene::TerminalSpawnParams,
    time: Res<Time>,
    mut seats: Query<(
        &mut crate::terminal::TerminalSurface,
        &mut crate::runtime::TerminalRuntime,
        &mut crate::scene::TerminalViewport,
        &mut crate::terminal::TerminalRedrawState,
    )>,
    mut plane_query: crate::scene::TerminalPlaneLayoutQuery,
    mut plane_back_query: crate::scene::TerminalPlaneBackLayoutQuery,
    mut pending_closes: ResMut<PendingTerminalCloses>,
    mut focus_requests: MessageWriter<crate::focus::FocusRequest>,
    mut acks: MessageWriter<crate::query_channel::AckOutcome>,
    mut diagnostics: crate::query_channel::DiagnosticsSink,
) {
    use crate::ai::{AiCommand, CommandOrigin};
    use crate::capability::SceneCapability;
    use crate::osc::RattyAiCommand;
    use crate::query::codes;
    use crate::query_channel::ack_commit;

    let now = time.elapsed_secs_f64();

    for AiCommand {
        source,
        ack_token,
        origin,
        command,
    } in commands.read()
    {
        macro_rules! reject {
            ($action:literal, $code:expr, $($message:tt)+) => {{
                let message = format!($($message)+);
                warn!("ratty-term: {} rejected: {message}", $action);
                crate::query_channel::reject(
                    &mut diagnostics,
                    &mut acks,
                    *source,
                    ack_token,
                    $action,
                    $code,
                    message,
                );
            }};
        }
        macro_rules! guard_wire_origin {
            ($action:literal) => {
                if *origin != CommandOrigin::Wire {
                    reject!(
                        $action,
                        codes::NOT_PERMITTED,
                        "terminal lifecycle must originate from live ingress (a replayed \
                         spawn would fork a process; a replayed close would kill a live \
                         session)"
                    );
                    continue;
                }
            };
        }
        macro_rules! require_capability {
            ($action:literal, $capability:expr, $($why:tt)+) => {
                if !$capability.granted_to(*source, &spawn.app_config) {
                    reject!($action, codes::NOT_PERMITTED, $($why)+);
                    continue;
                }
            };
        }

        match command {
            RattyAiCommand::TermSpawn {
                x,
                y,
                scale,
                cols,
                rows,
            } => {
                guard_wire_origin!("term.spawn");
                require_capability!(
                    "term.spawn",
                    SceneCapability::TerminalLifecycle,
                    "terminal lifecycle requires the terminal-lifecycle capability \
                     ([trust.local] terminal_lifecycle)"
                );
                // The five payload fields are the frozen wire shape, and
                // this build can honor none of them: `dress_terminal_seat`
                // sizes every new seat from the window unconditionally, and
                // nothing renders a per-terminal position. Refusing is
                // reversible — accepting and discarding would be a lie with
                // an `ok=1` on it. `caps.terminals.spawn_fields` is `[]`
                // so a caller never has to learn this from an ack.
                if x.is_some() || y.is_some() || scale.is_some() {
                    reject!(
                        "term.spawn",
                        codes::UNSUPPORTED,
                        "terminal placement geometry is not rendered in this build: every \
                         terminal is centered and drawn 1:1 when focused"
                    );
                    continue;
                }
                if cols.is_some() || rows.is_some() {
                    reject!(
                        "term.spawn",
                        codes::UNSUPPORTED,
                        "term.spawn takes no grid: a new seat is sized from the window. \
                         Spawn, wait for state=ready, then term.place;cols=&rows="
                    );
                    continue;
                }
                if !spawn.roster.take_spawn_token(source.terminal(), now) {
                    reject!(
                        "term.spawn",
                        codes::RATE_LIMITED,
                        "spawn rate budget exhausted ({} per second, burst {}); the live \
                         cap bounds how many terminals exist, this bounds how fast",
                        crate::identity::TERMINAL_SPAWNS_PER_SEC,
                        crate::identity::TERMINAL_SPAWN_BURST
                    );
                    continue;
                }
                spawn_from_wire(&mut spawn, *source, ack_token, &mut acks, &mut diagnostics);
            }
            RattyAiCommand::TermPlace {
                id,
                x,
                y,
                scale,
                cols,
                rows,
            } => {
                guard_wire_origin!("term.place");
                require_capability!(
                    "term.place",
                    SceneCapability::TerminalLifecycle,
                    "terminal placement requires the terminal-lifecycle capability \
                     ([trust.local] terminal_lifecycle)"
                );
                let target = match resolve_target(&spawn.roster, *source, id.as_deref()) {
                    Ok(target) => target,
                    Err(message) => {
                        reject!("term.place", codes::UNKNOWN_ID, "{message}");
                        continue;
                    }
                };
                // The arrival terminal is always placeable by its own
                // ingress (arrival is the address); any other handle
                // needs creator scope.
                if target != source.terminal()
                    && let Err(message) = require_creator(&spawn.roster, *source, target)
                {
                    reject!("term.place", codes::NOT_OWNER, "{message}");
                    continue;
                }
                // Geometry is refused WHOLE, before any field commits —
                // the `avatar.set` atomicity rule. Nothing in this build
                // renders a per-terminal position: the focused seat draws
                // 1:1 and centered, `sync_terminal_layout` rewrites every
                // viewport centre to the origin on each pass, and the flat
                // present path is a fullscreen triangle with no placement
                // uniform. An `ok=1` here would be a lie.
                if x.is_some() || y.is_some() || scale.is_some() {
                    reject!(
                        "term.place",
                        codes::UNSUPPORTED,
                        "terminal placement geometry is not rendered in this build: every \
                         terminal is centered and drawn 1:1 when focused; cols/rows are \
                         live (see caps.terminals.place_fields)"
                    );
                    continue;
                }
                let Some(seat) = spawn.terminals.entity_of(target) else {
                    reject!(
                        "term.place",
                        codes::UNKNOWN_ID,
                        "that terminal is still spawning; poll state.terminals for state=ready"
                    );
                    continue;
                };
                let Ok((mut surface, mut runtime, mut viewport, mut redraw)) = seats.get_mut(seat)
                else {
                    reject!(
                        "term.place",
                        codes::UNKNOWN_ID,
                        "that terminal is still spawning; poll state.terminals for state=ready"
                    );
                    continue;
                };
                // Absent keys keep their current value — naming only
                // `cols` must not silently reset `rows`.
                let next_cols = cols.unwrap_or_else(|| surface.cols);
                let next_rows = rows.unwrap_or_else(|| surface.rows);
                if !crate::identity::grid_is_admissible(next_cols, next_rows) {
                    reject!(
                        "term.place",
                        codes::BAD_COMMAND,
                        "grid {next_cols}x{next_rows} is outside {}..={} per axis or over \
                         {} cells; vt100 underflows below two and a wire-chosen grid \
                         becomes a CPU-side image",
                        crate::identity::MIN_TERMINAL_AXIS,
                        crate::identity::MAX_TERMINAL_AXIS,
                        crate::identity::MAX_TERMINAL_CELLS
                    );
                    continue;
                }
                if cols.is_some() || rows.is_some() {
                    // The window-resize path, narrowed to one seat. Every
                    // step matters: skip `sync_terminal_layout` and the 3D
                    // planes render the new grid at the old logical size
                    // while `position_to_cell` divides a stale viewport by
                    // the new grid, so every mouse cell resolves wrong
                    // until the next window resize.
                    let layout = surface.resize_grid(next_cols, next_rows);
                    let pty = layout.pty_pixels();
                    runtime.resize(layout.cols, layout.rows, pty.x as u16, pty.y as u16);
                    crate::scene::sync_terminal_layout(
                        seat,
                        layout,
                        &mut viewport,
                        &mut plane_query,
                        &mut plane_back_query,
                    );
                    redraw.request();
                }
                // An all-absent term.place is a vacuous commit, acked ok
                // (the `avatar.set` convention).
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::TermFocus { id } => {
                guard_wire_origin!("term.focus");
                require_capability!(
                    "term.focus",
                    SceneCapability::TerminalFocus,
                    "terminal focus requires the terminal-focus capability \
                     ([trust.local] terminal_focus); focus is the keystroke-capture \
                     primitive, granted separately from lifecycle"
                );
                let target = match resolve_target(&spawn.roster, *source, id.as_deref()) {
                    Ok(target) => target,
                    Err(message) => {
                        reject!("term.focus", codes::UNKNOWN_ID, "{message}");
                        continue;
                    }
                };
                // Creator scope on the handle-carrying form (#49 §2/§8):
                // the bare form targets self and needs only the grant.
                if id.is_some()
                    && let Err(message) = require_creator(&spawn.roster, *source, target)
                {
                    reject!("term.focus", codes::NOT_OWNER, "{message}");
                    continue;
                }
                if !spawn.roster.take_focus_token(source.terminal(), now) {
                    reject!(
                        "term.focus",
                        codes::RATE_LIMITED,
                        "focus rate budget exhausted ({} per second, burst {}); without \
                         one a granted caller wins every frame the user does not act",
                        crate::identity::TERMINAL_FOCUS_PER_SEC,
                        crate::identity::TERMINAL_FOCUS_BURST
                    );
                    continue;
                }
                let Some(seat) = spawn.terminals.entity_of(target) else {
                    reject!(
                        "term.focus",
                        codes::UNKNOWN_ID,
                        "that terminal is still spawning; poll state.terminals for state=ready"
                    );
                    continue;
                };
                // Wire class: any user-class request in the same frame
                // beats it (focus invariant 3), and focus routes the
                // keyboard without widening anything a wire may do
                // (invariant 4).
                focus_requests.write(crate::focus::FocusRequest {
                    target: Some(seat),
                    origin: crate::focus::FocusOrigin::WireVerb,
                });
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::TermClose { id } => {
                guard_wire_origin!("term.close");
                require_capability!(
                    "term.close",
                    SceneCapability::TerminalLifecycle,
                    "terminal lifecycle requires the terminal-lifecycle capability \
                     ([trust.local] terminal_lifecycle)"
                );
                let target = match resolve_target(&spawn.roster, *source, id.as_deref()) {
                    Ok(target) => target,
                    Err(message) => {
                        reject!("term.close", codes::UNKNOWN_ID, "{message}");
                        continue;
                    }
                };
                // Close authority, in this order. Note that a creator-less
                // row refuses even the BARE self-form: terminal #1, every
                // chord-spawned seat and every orphan are wire-unkillable
                // by construction, and that must hold from their own
                // ingress too or "wire-unkillable" is only half true.
                let Some(row) = spawn.roster.row(target) else {
                    reject!(
                        "term.close",
                        codes::UNKNOWN_ID,
                        "that terminal is no longer live"
                    );
                    continue;
                };
                match row.creator {
                    None => {
                        reject!(
                            "term.close",
                            codes::NOT_OWNER,
                            "that terminal has no wire creator and cannot be closed from the \
                             wire (the boot terminal, user-spawned terminals, and orphans)"
                        );
                        continue;
                    }
                    // The creator closes its creation; a wire-born
                    // terminal closes itself (this is what the reply-flush
                    // deferral below exists for).
                    Some(creator)
                        if creator == source.terminal() || target == source.terminal() => {}
                    Some(_) => {
                        reject!(
                            "term.close",
                            codes::NOT_OWNER,
                            "that terminal was created by another agent"
                        );
                        continue;
                    }
                }
                // Closing a still-`spawning` row is legal and cancels the
                // spawn — one verb covers mid-flight cancellation, and
                // there is no `term.cancel`.
                spawn
                    .roster
                    .row_mut(target)
                    .expect("the row was just read")
                    .closing = true;
                pending_closes.push(target);
                ack_commit(&mut acks, *source, ack_token);
            }
            _ => {}
        }
    }
}

/// Terminals whose close has committed, awaiting the despawn.
///
/// The despawn must not happen in the applier. `answer_queries` writes a
/// reply into the ARRIVAL terminal's own transport and drops it with a
/// warn when that terminal is gone, and Bevy inserts a command flush at
/// every ordering edge — so a `Commands::despawn` in an applier ordered
/// before the query channel flushes *before* the reply is written, and a
/// self-close would silently eat its own ack, every time.
#[derive(Resource, Default)]
pub struct PendingTerminalCloses(Vec<TerminalId>);

impl PendingTerminalCloses {
    /// Queues a close. Idempotent by construction: a second close on an
    /// already-`closing` row must not push again, or a close loop against
    /// one row would grow this vector without bound — the only unbounded
    /// resource in the design.
    fn push(&mut self, id: TerminalId) {
        if !self.0.contains(&id) {
            self.0.push(id);
        }
    }

    /// Queues a close from the user's chord. Same buffer, same drain —
    /// one despawn site, so the wire and the human cannot diverge on
    /// teardown ordering.
    pub(crate) fn push_from_chord(&mut self, id: TerminalId) {
        self.push(id);
    }

    /// Despawn sweep: a terminal that died by another path between the
    /// applier and the drain must not be despawned again.
    pub(crate) fn sweep_terminal(&mut self, id: TerminalId) {
        self.0.retain(|pending| *pending != id);
    }

    /// Test-only: the queued closes.
    #[cfg(test)]
    pub(crate) fn test_pending(&self) -> &[TerminalId] {
        &self.0
    }
}

/// Despawns terminals whose close committed, after this frame's replies
/// have been written.
///
/// Registered `.after(answer_queries)` — that edge is the whole point of
/// the deferral. The `Remove<TerminalIdentity>` observer then does every
/// teardown: the scene lock, pending jumps, the roster row and its orphan
/// pass, the namespace-keyed globals, the owned planes, and the lease as
/// its last act.
pub(crate) fn despawn_closed_terminals(
    mut pending: ResMut<PendingTerminalCloses>,
    registry: Res<crate::identity::TerminalRegistry>,
    mut commands: Commands,
) {
    for id in pending.0.drain(..) {
        // The honest resolver: an id whose terminal already died by
        // another path resolves `None` and nothing aliases, because ids
        // never recycle.
        if let Some(seat) = registry.entity_of(id) {
            commands.entity(seat).despawn();
        }
    }
}

/// Resolves a `term.*` target: absent `id=` is the arrival terminal
/// (arrival is the address), a present handle resolves through the roster.
///
/// A present-but-empty `id=` is a handle nobody minted, never the bare
/// form — the wire distinguishes "no key" from "empty value", and
/// collapsing them would let a typo silently retarget a caller's own seat.
fn resolve_target(
    roster: &TerminalRoster,
    source: crate::runtime::IngressSource,
    id: Option<&str>,
) -> Result<TerminalId, String> {
    match id {
        None => Ok(source.terminal()),
        Some(handle) => roster.by_handle(handle).ok_or_else(|| {
            "no live terminal carries that handle (closed, or minted by a previous session)"
                .to_string()
        }),
    }
}

/// The creator-scope check (#49 §2): the caller must have created the
/// terminal it names. A terminal with no creator — the boot seat, every
/// chord-spawned seat, every orphan — is addressable by nobody.
fn require_creator(
    roster: &TerminalRoster,
    source: crate::runtime::IngressSource,
    target: TerminalId,
) -> Result<(), String> {
    let Some(row) = roster.row(target) else {
        return Err("that terminal is no longer live".to_string());
    };
    match row.creator {
        None => Err(
            "that terminal has no wire creator and cannot be addressed by handle from the \
             wire (the boot terminal, user-spawned terminals, and orphans)"
                .to_string(),
        ),
        Some(creator) if creator == source.terminal() => Ok(()),
        Some(_) => Err("that terminal was created by another agent".to_string()),
    }
}

/// The wire's half of the spawner. Native only: on wasm the page API owns
/// terminal lifecycle (#53/#86), so the verb refuses rather than pretends.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_from_wire(
    spawn: &mut crate::scene::TerminalSpawnParams,
    source: crate::runtime::IngressSource,
    ack_token: &Option<String>,
    acks: &mut MessageWriter<crate::query_channel::AckOutcome>,
    diagnostics: &mut crate::query_channel::DiagnosticsSink,
) {
    use crate::query::codes;
    use crate::query_channel::ack_commit_with_payload;
    use crate::scene::SpawnTerminalError;

    let config = spawn.app_config.as_ref().clone();
    // The creator is the ARRIVAL terminal's id, never its namespace: the
    // namespace recycles and would re-parent this row to a stranger.
    let creator = source.terminal();
    let result = crate::scene::spawn_terminal(spawn, Some(creator), true, |ingress| {
        // #12 is structural here: no command, no cwd, no env. A wire-born
        // terminal runs the config-default shell in ratty's own working
        // directory, exactly as the spawn chord does, because nothing in
        // the command could say otherwise.
        let runtime = crate::runtime::TerminalRuntime::spawn(
            &config,
            &crate::runtime::RuntimeOptions {
                command: None,
                working_dir: std::env::current_dir().ok(),
            },
            ingress,
        )?;
        let surface = crate::terminal::TerminalSurface::new(&config)?;
        Ok::<_, anyhow::Error>((surface, runtime))
    });
    match result {
        Ok((_seat, identity)) => {
            let handle = spawn
                .roster
                .row(identity.id())
                .expect("the spawner inserted this row synchronously")
                .handle
                .clone();
            info!(
                "ratty-term: term.spawn created {:?} on namespace {} for {:?}",
                identity.id(),
                identity.namespace(),
                creator
            );
            // Immediate commit (#56 decision 19): ok=1, NO status code,
            // handle in `data`. Readiness is the `state` field on the
            // state.terminals row — never `code=started`, which under
            // protocols/query.md's absence-means-finished rule would tell
            // a conforming caller the spawn had already completed.
            ack_commit_with_payload(acks, source, ack_token, serde_json::json!({ "id": handle }));
        }
        Err(error) => {
            let (code, message) = match &error {
                SpawnTerminalError::LiveCap { live, cap } => (
                    codes::TERMINAL_CAP,
                    format!("{live} of {cap} terminals are live; close one first"),
                ),
                SpawnTerminalError::Alloc(alloc) => (codes::TERMINAL_CAP, format!("{alloc}")),
                SpawnTerminalError::Build(build) => (codes::INTERNAL, format!("{build:#}")),
            };
            warn!("ratty-term: term.spawn rejected: {message}");
            crate::query_channel::reject(
                diagnostics,
                acks,
                source,
                ack_token,
                "term.spawn",
                code,
                message,
            );
        }
    }
}

/// The wasm refusal: terminal lifecycle on the web belongs to the page API
/// (#53's canvas, #86's fork), not to a byte stream, and
/// `TerminalRuntime::spawn` does not exist on this target at all.
#[cfg(target_arch = "wasm32")]
fn spawn_from_wire(
    _spawn: &mut crate::scene::TerminalSpawnParams,
    source: crate::runtime::IngressSource,
    ack_token: &Option<String>,
    acks: &mut MessageWriter<crate::query_channel::AckOutcome>,
    diagnostics: &mut crate::query_channel::DiagnosticsSink,
) {
    crate::query_channel::reject(
        diagnostics,
        acks,
        source,
        ack_token,
        "term.spawn",
        crate::query::codes::UNSUPPORTED,
        "terminal spawn is not available in the web build: the page API owns lifecycle there"
            .to_string(),
    );
}

/// Registers the terminals organ.
pub struct TerminalsPlugin;

impl Plugin for TerminalsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TerminalRoster>()
            .init_resource::<PendingTerminalCloses>()
            .add_systems(
                Update,
                apply_terminal_commands
                    .after(crate::systems::pump_pty_output)
                    // The user's chord beats the wire for the shared
                    // spawner bundle — honest arbitration of a real
                    // conflict, not a tiebreak.
                    .after(crate::scene::spawn_requested_terminals)
                    // The edge auto-inserts a command flush, so a
                    // same-chunk spawn-then-focus finds a LIVE seat rather
                    // than being dropped at the drain's liveness door.
                    .before(crate::focus::drain_focus_requests),
            )
            // THE deferral: replies are written into the arrival
            // terminal's own transport by `answer_queries`, so a
            // self-close's seat must outlive that system or the ack it
            // just committed is dropped with a warn.
            .add_systems(
                Update,
                despawn_closed_terminals
                    .after(crate::query_channel::answer_queries)
                    .run_if(|pending: Res<PendingTerminalCloses>| !pending.0.is_empty()),
            );
    }
}

#[cfg(test)]
mod tests {
    use bevy::ecs::message::Messages;
    use bevy::ecs::system::RunSystemOnce;

    use super::*;
    use crate::ai::{AiCommand, CommandOrigin};
    use crate::config::AppConfig;
    use crate::identity::{TerminalIdentity, TerminalRegistry};
    use crate::osc::RattyAiCommand;
    use crate::query::codes;
    use crate::query_channel::{AckOutcome, QuerySession};
    use crate::runtime::IngressSource;

    fn id(raw: u64) -> TerminalId {
        TerminalId::from_raw(raw)
    }

    /// A world with the spawner bundle, the roster, and one boot seat —
    /// the shape `apply_terminal_commands` runs against. Both gates start
    /// DENIED, exactly as a shipped config does.
    fn organ_world() -> (World, TerminalIdentity) {
        let mut world = World::new();
        world.insert_resource(AppConfig::default());
        world.init_resource::<Assets<Mesh>>();
        world.init_resource::<Assets<StandardMaterial>>();
        world.init_resource::<Assets<Image>>();
        world.init_resource::<QuerySession>();
        world.init_resource::<TerminalRoster>();
        world.init_resource::<PendingTerminalCloses>();
        world.init_resource::<Time>();
        world.init_resource::<crate::focus::FocusedTerminal>();
        world.init_resource::<Messages<AiCommand>>();
        world.init_resource::<Messages<AckOutcome>>();
        world.init_resource::<Messages<crate::focus::FocusRequest>>();
        world.init_resource::<Messages<crate::focus::FocusGained>>();
        world.init_resource::<Messages<crate::focus::FocusLost>>();
        world.spawn((Window::default(), bevy::window::PrimaryWindow));

        let mut registry = TerminalRegistry::default();
        let identity = registry.allocate().expect("a fresh registry has slots");
        let (runtime, _host) = crate::runtime::TerminalRuntime::virtual_channel(
            &AppConfig::default(),
            identity.ingress(),
        );
        let seat = world
            .spawn((
                identity,
                runtime,
                crate::terminal::TerminalSurface::new(&AppConfig::default())
                    .expect("surface construction is CPU-only"),
                crate::scene::TerminalViewport {
                    size: Vec2::new(800.0, 600.0),
                    center: Vec2::ZERO,
                },
                crate::terminal::TerminalRedrawState::default(),
                crate::identity::terminal_session_state(),
            ))
            .id();
        registry
            .bind(identity.id(), seat)
            .expect("the lease is live");
        world.insert_resource(registry);
        let handle = world.resource_mut::<QuerySession>().mint_execution_id();
        world
            .resource_mut::<TerminalRoster>()
            .insert(identity.id(), handle, None, false);
        (world, identity)
    }

    fn grant(world: &mut World, lifecycle: bool, focus: bool) {
        let mut config = world.resource_mut::<AppConfig>();
        config.trust.local.terminal_lifecycle = lifecycle;
        config.trust.local.terminal_focus = focus;
    }

    fn send(
        world: &mut World,
        source: IngressSource,
        ack: Option<&str>,
        origin: CommandOrigin,
        command: RattyAiCommand,
    ) -> Vec<AckOutcome> {
        world
            .resource_mut::<Messages<AiCommand>>()
            .write(AiCommand {
                source,
                ack_token: ack.map(str::to_string),
                origin,
                command,
            });
        world
            .run_system_once(apply_terminal_commands)
            .expect("the organ runs");
        world.flush();
        world.resource_mut::<Messages<AiCommand>>().clear();
        world
            .resource_mut::<Messages<AckOutcome>>()
            .drain()
            .collect()
    }

    fn wire(world: &mut World, source: IngressSource, command: RattyAiCommand) -> Vec<AckOutcome> {
        send(world, source, Some("t"), CommandOrigin::Wire, command)
    }

    fn spawn_command() -> RattyAiCommand {
        RattyAiCommand::TermSpawn {
            x: None,
            y: None,
            scale: None,
            cols: None,
            rows: None,
        }
    }

    fn seat_count(world: &mut World) -> usize {
        world.query::<&TerminalIdentity>().iter(world).count()
    }

    fn only_ack(acks: &[AckOutcome]) -> &AckOutcome {
        assert_eq!(acks.len(), 1, "exactly one ack per tok= command");
        &acks[0]
    }

    #[test]
    fn both_gates_default_to_denied_and_mutate_nothing() {
        let (mut world, boot) = organ_world();
        let acks = wire(&mut world, boot.ingress(), spawn_command());
        let ack = only_ack(&acks);
        assert!(!ack.ok);
        assert_eq!(ack.code, Some(codes::NOT_PERMITTED));
        assert_eq!(seat_count(&mut world), 1, "seat count asserted (#58 rider)");

        let acks = wire(
            &mut world,
            boot.ingress(),
            RattyAiCommand::TermFocus { id: None },
        );
        assert_eq!(only_ack(&acks).code, Some(codes::NOT_PERMITTED));

        // And the gates are independent: focus granted alone still refuses
        // a spawn, which is #56 decision 18's whole point.
        grant(&mut world, false, true);
        let acks = wire(&mut world, boot.ingress(), spawn_command());
        assert_eq!(only_ack(&acks).code, Some(codes::NOT_PERMITTED));
        assert_eq!(seat_count(&mut world), 1, "seat count asserted (#58 rider)");
    }

    #[test]
    fn a_replayed_spawn_is_refused_before_anything_else_is_read() {
        let (mut world, boot) = organ_world();
        // Granted, so only the origin can be doing the refusing.
        grant(&mut world, true, true);
        for origin in [
            CommandOrigin::Macro,
            CommandOrigin::Bookmark,
            CommandOrigin::Rule,
        ] {
            let acks = send(
                &mut world,
                boot.ingress(),
                Some("t"),
                origin,
                spawn_command(),
            );
            assert_eq!(
                only_ack(&acks).code,
                Some(codes::NOT_PERMITTED),
                "{origin:?}"
            );
            assert_eq!(seat_count(&mut world), 1, "seat count asserted (#58 rider)");
        }
    }

    #[test]
    fn a_handle_carrying_focus_needs_creator_scope_but_the_bare_form_does_not() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, true);

        // A second seat nobody created — the chord/boot shape.
        let stranger = world
            .resource_mut::<TerminalRegistry>()
            .allocate()
            .expect("slots");
        let (runtime, _host) = crate::runtime::TerminalRuntime::virtual_channel(
            &AppConfig::default(),
            stranger.ingress(),
        );
        let seat = world
            .spawn((stranger, runtime, crate::identity::terminal_session_state()))
            .id();
        world
            .resource_mut::<TerminalRegistry>()
            .bind(stranger.id(), seat)
            .expect("live");
        let stranger_handle = world.resource_mut::<QuerySession>().mint_execution_id();
        world.resource_mut::<TerminalRoster>().insert(
            stranger.id(),
            stranger_handle.clone(),
            None,
            false,
        );

        // The bare form is self-focus: the grant is the whole authority.
        let acks = wire(
            &mut world,
            boot.ingress(),
            RattyAiCommand::TermFocus { id: None },
        );
        assert!(only_ack(&acks).ok, "self-focus needs only the grant");

        // Naming a terminal this caller did not create is refused, even
        // holding the grant — creator scope is an ownership fact, not a
        // capability one.
        let acks = wire(
            &mut world,
            boot.ingress(),
            RattyAiCommand::TermFocus {
                id: Some(stranger_handle),
            },
        );
        let ack = only_ack(&acks);
        assert!(!ack.ok);
        assert_eq!(ack.code, Some(codes::NOT_OWNER));
    }

    #[test]
    fn an_unknown_or_empty_handle_answers_unknown_id() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, true);
        for handle in ["not-a-handle", ""] {
            let acks = wire(
                &mut world,
                boot.ingress(),
                RattyAiCommand::TermFocus {
                    id: Some(handle.to_string()),
                },
            );
            let ack = only_ack(&acks);
            assert!(!ack.ok, "`{handle}`");
            // An empty `id=` is a handle nobody minted, NOT the bare
            // self-form: collapsing them would let a typo silently
            // retarget the caller's own seat.
            assert_eq!(ack.code, Some(codes::UNKNOWN_ID), "`{handle}`");
        }
    }

    #[test]
    fn the_focus_budget_bounds_a_granted_caller() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, true);
        // Time never advances in this world, so the burst is all there is
        // — which is exactly the frame-rate attack the budget exists for.
        for attempt in 0..TERMINAL_FOCUS_BURST {
            let acks = wire(
                &mut world,
                boot.ingress(),
                RattyAiCommand::TermFocus { id: None },
            );
            assert!(only_ack(&acks).ok, "attempt {attempt} is within the burst");
        }
        let acks = wire(
            &mut world,
            boot.ingress(),
            RattyAiCommand::TermFocus { id: None },
        );
        let ack = only_ack(&acks);
        assert!(!ack.ok);
        assert_eq!(ack.code, Some(codes::RATE_LIMITED));
    }

    /// The spawn ack is immediate-commit (#56 decision 19): `ok=1`, NO
    /// status code, the handle in `data`. `code=started` here would tell a
    /// conforming caller — one following `protocols/query.md`'s
    /// absence-from-`state.executions`-means-finished rule — that the
    /// spawn had already completed.
    ///
    /// Native only: `TerminalRuntime::spawn` forks a real PTY, and the
    /// wasm build refuses the verb outright.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_granted_spawn_acks_ok_with_a_handle_and_never_started() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, false);

        let acks = wire(&mut world, boot.ingress(), spawn_command());
        let ack = only_ack(&acks);
        assert!(ack.ok, "the spawn committed");
        assert_eq!(
            ack.code, None,
            "immediate commit carries no status qualifier — never codes::STARTED"
        );
        let handle = ack.payload.as_ref().expect("the handle rides data=")["id"]
            .as_str()
            .expect("a string handle")
            .to_string();
        assert!(!handle.is_empty());
        assert_eq!(seat_count(&mut world), 2, "seat count asserted (#58 rider)");

        // The handle is addressable IMMEDIATELY — the roster row is
        // inserted synchronously even though the seat entity was deferred.
        let roster = world.resource::<TerminalRoster>();
        let child = roster.by_handle(&handle).expect("addressable at once");
        let row = roster.row(child).expect("row");
        assert_eq!(
            row.creator,
            Some(boot.id()),
            "the creator is the ARRIVAL terminal's id, never its namespace"
        );
        assert!(row.wire_born);

        // Decision 8: a wire spawn never focuses its child. Only the
        // grant's own verb may move focus, and it has a separate gate.
        assert!(
            world
                .resource_mut::<Messages<crate::focus::FocusRequest>>()
                .drain()
                .next()
                .is_none(),
            "no focus request from a wire spawn"
        );
    }

    /// `term.spawn` refuses every payload field rather than discarding it:
    /// `dress_terminal_seat` sizes each new seat from the window, so an
    /// accepted `cols=` would be an `ok=1` on a grid that never happened.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn spawn_refuses_every_field_it_cannot_honor() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, false);
        for command in [
            RattyAiCommand::TermSpawn {
                x: Some(5.0),
                y: None,
                scale: None,
                cols: None,
                rows: None,
            },
            RattyAiCommand::TermSpawn {
                x: None,
                y: None,
                scale: None,
                cols: Some(80),
                rows: Some(24),
            },
        ] {
            let acks = wire(&mut world, boot.ingress(), command);
            let ack = only_ack(&acks);
            assert!(!ack.ok);
            assert_eq!(ack.code, Some(codes::UNSUPPORTED));
            assert_eq!(seat_count(&mut world), 1, "seat count asserted (#58 rider)");
        }
    }

    /// The live cap refuses through the wire with its own code, and the
    /// refusal creates nothing.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_live_cap_answers_terminal_cap_on_the_wire() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, false);
        world.resource_mut::<AppConfig>().terminal.max_live = 2;

        let acks = wire(&mut world, boot.ingress(), spawn_command());
        assert!(only_ack(&acks).ok, "the second seat fits");
        assert_eq!(seat_count(&mut world), 2, "seat count asserted (#58 rider)");

        let acks = wire(&mut world, boot.ingress(), spawn_command());
        let ack = only_ack(&acks);
        assert!(!ack.ok);
        assert_eq!(ack.code, Some(codes::TERMINAL_CAP));
        assert_eq!(seat_count(&mut world), 2, "seat count asserted (#58 rider)");
    }

    /// The spawn budget bounds rate, which the cap cannot: closes are
    /// deferred and one PTY chunk can carry arbitrarily many commands.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_spawn_budget_bounds_a_burst_within_one_chunk() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, false);
        world.resource_mut::<AppConfig>().terminal.max_live = 128;

        for attempt in 0..TERMINAL_SPAWN_BURST {
            let acks = wire(&mut world, boot.ingress(), spawn_command());
            assert!(only_ack(&acks).ok, "attempt {attempt} is within the burst");
        }
        let acks = wire(&mut world, boot.ingress(), spawn_command());
        let ack = only_ack(&acks);
        assert!(!ack.ok);
        assert_eq!(ack.code, Some(codes::RATE_LIMITED));
        assert_eq!(
            seat_count(&mut world),
            1 + TERMINAL_SPAWN_BURST as usize,
            "seat count asserted (#58 rider): the refused spawn forked nothing"
        );
    }

    /// A token-less spawn still works; the creator discovers its handle
    /// from `state.terminals`, whose rows carry `creator`.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_token_less_spawn_commits_silently() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, false);
        let acks = send(
            &mut world,
            boot.ingress(),
            None,
            CommandOrigin::Wire,
            spawn_command(),
        );
        assert!(acks.is_empty(), "no tok=, no ack — fire and forget");
        assert_eq!(seat_count(&mut world), 2, "seat count asserted (#58 rider)");
        assert_eq!(world.resource::<TerminalRoster>().len(), 2);
    }

    /// `term.place` is half real and half refused. `cols`/`rows` drive the
    /// genuine per-seat resize — grid, PTY, viewport and owned planes —
    /// while `x`/`y`/`scale` have no lowering target at all.
    #[test]
    fn place_resizes_the_grid_for_real_and_refuses_the_geometry() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, false);
        let seat = world
            .resource::<TerminalRegistry>()
            .entity_of(boot.id())
            .expect("the boot seat is bound");
        let before = world
            .get::<crate::scene::TerminalViewport>(seat)
            .expect("viewport")
            .size;

        let acks = wire(
            &mut world,
            boot.ingress(),
            RattyAiCommand::TermPlace {
                id: None,
                x: None,
                y: None,
                scale: None,
                cols: Some(80),
                rows: Some(24),
            },
        );
        assert!(only_ack(&acks).ok, "the grid is real");
        let surface = world
            .get::<crate::terminal::TerminalSurface>(seat)
            .expect("surface");
        assert_eq!((surface.cols, surface.rows), (80, 24));
        // The viewport must move with the grid: skip that and the planes
        // render the new grid at the old logical size, and every mouse
        // cell resolves wrong until the next window resize.
        let after = world
            .get::<crate::scene::TerminalViewport>(seat)
            .expect("viewport")
            .size;
        assert_ne!(before, after, "sync_terminal_layout ran");

        // Geometry is refused whole, and refusing it changes nothing.
        let acks = wire(
            &mut world,
            boot.ingress(),
            RattyAiCommand::TermPlace {
                id: None,
                x: Some(5.0),
                y: None,
                scale: None,
                cols: Some(100),
                rows: None,
            },
        );
        let ack = only_ack(&acks);
        assert!(!ack.ok);
        assert_eq!(ack.code, Some(codes::UNSUPPORTED));
        let surface = world
            .get::<crate::terminal::TerminalSurface>(seat)
            .expect("surface");
        assert_eq!(
            (surface.cols, surface.rows),
            (80, 24),
            "a refused command commits none of its fields"
        );
    }

    #[test]
    fn place_bounds_the_grid_and_leaves_absent_axes_alone() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, false);
        let seat = world
            .resource::<TerminalRegistry>()
            .entity_of(boot.id())
            .expect("bound");

        let place = |world: &mut World, cols, rows| {
            wire(
                world,
                boot.ingress(),
                RattyAiCommand::TermPlace {
                    id: None,
                    x: None,
                    y: None,
                    scale: None,
                    cols,
                    rows,
                },
            )
        };

        place(&mut world, Some(80), Some(24));
        // vt100 underflows below two; a wire-chosen grid becomes a
        // CPU-side image above the ceiling. `u16` is not a bound.
        for (cols, rows) in [
            (Some(1), None),
            (None, Some(1)),
            (Some(513), None),
            (Some(512), Some(512)),
            (Some(u16::MAX), Some(u16::MAX)),
        ] {
            let acks = place(&mut world, cols, rows);
            let ack = only_ack(&acks);
            assert!(!ack.ok, "{cols:?}x{rows:?}");
            assert_eq!(ack.code, Some(codes::BAD_COMMAND), "{cols:?}x{rows:?}");
        }
        let surface = world
            .get::<crate::terminal::TerminalSurface>(seat)
            .expect("surface");
        assert_eq!((surface.cols, surface.rows), (80, 24), "nothing committed");

        // Naming one axis leaves the other where it was.
        place(&mut world, Some(100), None);
        let surface = world
            .get::<crate::terminal::TerminalSurface>(seat)
            .expect("surface");
        assert_eq!((surface.cols, surface.rows), (100, 24));

        // An all-absent place is a vacuous commit, acked ok.
        let acks = place(&mut world, None, None);
        assert!(only_ack(&acks).ok);
        let surface = world
            .get::<crate::terminal::TerminalSurface>(seat)
            .expect("surface");
        assert_eq!((surface.cols, surface.rows), (100, 24));
    }

    /// Terminal #1 is wire-unkillable by construction, and so is every
    /// other creator-less seat — from a third party AND from its own
    /// ingress. The bare self-form is the case the locked doc left open,
    /// and leaving it open would make "wire-unkillable" only half true.
    #[test]
    fn a_creator_less_terminal_refuses_every_close_including_its_own() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, false);
        let boot_handle = world
            .resource::<TerminalRoster>()
            .row(boot.id())
            .expect("row")
            .handle
            .clone();

        for id in [None, Some(boot_handle)] {
            let acks = wire(
                &mut world,
                boot.ingress(),
                RattyAiCommand::TermClose { id: id.clone() },
            );
            let ack = only_ack(&acks);
            assert!(!ack.ok, "{id:?}");
            assert_eq!(ack.code, Some(codes::NOT_OWNER), "{id:?}");
            assert_eq!(seat_count(&mut world), 1, "seat count asserted (#58 rider)");
            assert!(
                world
                    .resource::<PendingTerminalCloses>()
                    .test_pending()
                    .is_empty(),
                "a refused close queues nothing"
            );
        }
    }

    /// The creator closes its creation; a third party cannot. Close is an
    /// ownership fact, so it answers `not-owner` even when the caller
    /// holds the capability.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn only_the_creator_or_the_terminal_itself_may_close_it() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, false);
        let acks = wire(&mut world, boot.ingress(), spawn_command());
        let child_handle = only_ack(&acks).payload.as_ref().expect("payload")["id"]
            .as_str()
            .expect("handle")
            .to_string();
        let child = world
            .resource::<TerminalRoster>()
            .by_handle(&child_handle)
            .expect("live");
        let child_identity = *world
            .get::<TerminalIdentity>(
                world
                    .resource::<TerminalRegistry>()
                    .entity_of(child)
                    .expect("bound"),
            )
            .expect("identity");

        // A third party — here the child naming its own SIBLING would be
        // the same shape; the child naming ITSELF is permitted below.
        let acks = wire(&mut world, boot.ingress(), spawn_command());
        let sibling_handle = only_ack(&acks).payload.as_ref().expect("payload")["id"]
            .as_str()
            .expect("handle")
            .to_string();
        let acks = wire(
            &mut world,
            child_identity.ingress(),
            RattyAiCommand::TermClose {
                id: Some(sibling_handle),
            },
        );
        let ack = only_ack(&acks);
        assert!(!ack.ok);
        assert_eq!(
            ack.code,
            Some(codes::NOT_OWNER),
            "a sibling is not a creator"
        );
        assert_eq!(seat_count(&mut world), 3, "seat count asserted (#58 rider)");

        // The creator may.
        let acks = wire(
            &mut world,
            boot.ingress(),
            RattyAiCommand::TermClose {
                id: Some(child_handle),
            },
        );
        assert!(only_ack(&acks).ok);
        assert_eq!(
            world
                .resource::<TerminalRoster>()
                .row(child)
                .expect("still listed while closing")
                .wire_state(true),
            TerminalWireState::Closing,
            "the row reads `closing` before the despawn runs"
        );
        // The applier despawned nothing — that is the whole deferral.
        assert_eq!(seat_count(&mut world), 3, "seat count asserted (#58 rider)");
        assert_eq!(
            world.resource::<PendingTerminalCloses>().test_pending(),
            &[child]
        );

        // A second close is idempotent and queues nothing new: this
        // buffer is the design's only unbounded resource.
        let still_listed = world
            .resource::<TerminalRoster>()
            .row(child)
            .expect("row")
            .handle
            .clone();
        let acks = wire(
            &mut world,
            boot.ingress(),
            RattyAiCommand::TermClose {
                id: Some(still_listed),
            },
        );
        assert!(only_ack(&acks).ok, "closing an already-closing row acks ok");
        assert_eq!(
            world
                .resource::<PendingTerminalCloses>()
                .test_pending()
                .len(),
            1,
            "and pushes nothing"
        );

        // The drain despawns it.
        world
            .run_system_once(despawn_closed_terminals)
            .expect("the drain runs");
        world.flush();
        assert_eq!(seat_count(&mut world), 2, "seat count asserted (#58 rider)");
        // The row goes with the seat through the despawn observer, which
        // this world deliberately does not register — the orphan test
        // below installs it and proves that half.
    }

    /// A wire-born terminal may close itself — the case the reply-flush
    /// deferral exists for.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_wire_born_terminal_can_close_itself_and_the_despawn_is_deferred() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, false);
        let acks = wire(&mut world, boot.ingress(), spawn_command());
        let handle = only_ack(&acks).payload.as_ref().expect("payload")["id"]
            .as_str()
            .expect("handle")
            .to_string();
        let child = world
            .resource::<TerminalRoster>()
            .by_handle(&handle)
            .expect("live");
        let child_identity = *world
            .get::<TerminalIdentity>(
                world
                    .resource::<TerminalRegistry>()
                    .entity_of(child)
                    .expect("bound"),
            )
            .expect("identity");

        let acks = wire(
            &mut world,
            child_identity.ingress(),
            RattyAiCommand::TermClose { id: None },
        );
        assert!(only_ack(&acks).ok, "a wire-born terminal closes itself");
        assert_eq!(
            seat_count(&mut world),
            2,
            "seat count asserted (#58 rider): the seat outlives the applier so its \
             own ack can still be written to its own transport"
        );
        world
            .run_system_once(despawn_closed_terminals)
            .expect("the drain runs");
        world.flush();
        assert_eq!(seat_count(&mut world), 1, "seat count asserted (#58 rider)");
    }

    /// A creator's death orphans its children rather than cascading, and
    /// an orphan is wire-unaddressable — the same clause that protects
    /// terminal #1, with no second rule.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn an_orphan_becomes_unaddressable_rather_than_being_cascade_closed() {
        let (mut world, boot) = organ_world();
        world.add_observer(crate::scene::sweep_despawned_terminal);
        world.init_resource::<crate::macros::MacroRegistry>();
        world.init_resource::<crate::presence::PresenceRegistry>();
        world.init_resource::<crate::viz::VizRegistry>();
        world.init_resource::<crate::avatar::AvatarState>();
        world.init_resource::<crate::sound::SoundState>();
        world.init_resource::<crate::bookmarks::BookmarkRegistry>();
        world.init_resource::<crate::bookmarks::PendingBookmarkJumps>();
        world.init_resource::<crate::ai::AiObjectRegistry>();
        grant(&mut world, true, false);

        // boot -> X, then X -> Y.
        let acks = wire(&mut world, boot.ingress(), spawn_command());
        let x_handle = only_ack(&acks).payload.as_ref().expect("payload")["id"]
            .as_str()
            .expect("handle")
            .to_string();
        let x = world
            .resource::<TerminalRoster>()
            .by_handle(&x_handle)
            .expect("live");
        let x_identity = *world
            .get::<TerminalIdentity>(
                world
                    .resource::<TerminalRegistry>()
                    .entity_of(x)
                    .expect("bound"),
            )
            .expect("identity");
        let acks = wire(&mut world, x_identity.ingress(), spawn_command());
        let y_handle = only_ack(&acks).payload.as_ref().expect("payload")["id"]
            .as_str()
            .expect("handle")
            .to_string();
        let y = world
            .resource::<TerminalRoster>()
            .by_handle(&y_handle)
            .expect("live");
        assert_eq!(seat_count(&mut world), 3, "seat count asserted (#58 rider)");

        // boot closes X. Y survives — it is a principal, possibly with a
        // user inside — but loses its creator.
        wire(
            &mut world,
            boot.ingress(),
            RattyAiCommand::TermClose { id: Some(x_handle) },
        );
        world
            .run_system_once(despawn_closed_terminals)
            .expect("the drain runs");
        world.flush();
        assert_eq!(
            seat_count(&mut world),
            2,
            "seat count asserted (#58 rider): a creator's death does not cascade"
        );
        assert_eq!(
            world
                .resource::<TerminalRoster>()
                .row(y)
                .expect("Y survives")
                .creator,
            None,
            "the orphan is creator-less in the DATA, so no second rule is needed"
        );

        // And now nobody can close it — not its grandparent, not itself.
        let y_identity = *world
            .get::<TerminalIdentity>(
                world
                    .resource::<TerminalRegistry>()
                    .entity_of(y)
                    .expect("bound"),
            )
            .expect("identity");
        for (source, id) in [
            (boot.ingress(), Some(y_handle.clone())),
            (y_identity.ingress(), None),
        ] {
            let acks = wire(&mut world, source, RattyAiCommand::TermClose { id });
            assert_eq!(only_ack(&acks).code, Some(codes::NOT_OWNER));
        }
        assert_eq!(seat_count(&mut world), 2, "seat count asserted (#58 rider)");
    }

    #[test]
    fn a_granted_focus_writes_a_wire_class_request_the_user_can_beat() {
        let (mut world, boot) = organ_world();
        grant(&mut world, true, true);
        wire(
            &mut world,
            boot.ingress(),
            RattyAiCommand::TermFocus { id: None },
        );

        let requests: Vec<crate::focus::FocusRequest> = world
            .resource_mut::<Messages<crate::focus::FocusRequest>>()
            .drain()
            .collect();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].origin,
            crate::focus::FocusOrigin::WireVerb,
            "wire class: a same-frame user request beats it (focus invariant 3)"
        );
    }

    #[test]
    fn handles_resolve_while_live_and_never_after() {
        let mut roster = TerminalRoster::default();
        roster.insert(id(1), "sess-1".to_string(), None, false);
        roster.insert(id(2), "sess-2".to_string(), Some(id(1)), true);
        assert_eq!(roster.len(), 2);
        assert_eq!(roster.by_handle("sess-1"), Some(id(1)));
        assert_eq!(roster.by_handle("sess-2"), Some(id(2)));

        // A handle nobody minted, an empty string, and a closed
        // terminal's handle all answer the same: the wire cannot tell
        // them apart and `unknown-id` is honest for all three.
        assert_eq!(roster.by_handle("sess-9"), None);
        assert_eq!(roster.by_handle(""), None);
        roster.sweep_terminal(id(2));
        assert_eq!(roster.by_handle("sess-2"), None);
        assert_eq!(roster.len(), 1);
    }

    #[test]
    fn the_sweep_orphans_children_rather_than_cascading() {
        let mut roster = TerminalRoster::default();
        roster.insert(id(1), "boot".to_string(), None, false);
        roster.insert(id(2), "worker".to_string(), Some(id(1)), true);
        roster.insert(id(3), "grandchild".to_string(), Some(id(2)), true);

        roster.sweep_terminal(id(2));
        assert!(roster.row(id(2)).is_none(), "the dead terminal's row goes");
        let orphan = roster.row(id(3)).expect("a child outlives its creator");
        assert_eq!(
            orphan.creator, None,
            "orphans become creator-less — wire-unaddressable, never cascade-closed"
        );
        assert!(
            orphan.wire_born,
            "wire_born is a mint-time fact and survives orphaning"
        );
        assert_eq!(roster.row(id(1)).expect("boot lives").creator, None);
    }

    #[test]
    fn rows_iterate_in_mint_order() {
        let mut roster = TerminalRoster::default();
        for raw in [3, 1, 2] {
            roster.insert(id(raw), format!("h{raw}"), None, false);
        }
        let order: Vec<TerminalId> = roster.iter().map(|(id, _)| id).collect();
        assert_eq!(
            order,
            vec![id(1), id(2), id(3)],
            "state.terminals pagination needs a stable sequence"
        );
    }

    #[test]
    fn the_wire_state_spellings_are_frozen() {
        assert_eq!(TerminalWireState::Spawning.as_str(), "spawning");
        assert_eq!(TerminalWireState::Ready.as_str(), "ready");
        assert_eq!(TerminalWireState::Closing.as_str(), "closing");
    }

    #[test]
    fn readiness_is_derived_from_the_seat_and_closing_overrides_it() {
        let mut roster = TerminalRoster::default();
        roster.insert(id(1), "h1".to_string(), None, false);
        let row = roster.row(id(1)).expect("row");
        // The row exists before the seat entity flushes; that gap IS
        // `spawning`, and no system has to notice it.
        assert_eq!(row.wire_state(false), TerminalWireState::Spawning);
        assert_eq!(row.wire_state(true), TerminalWireState::Ready);

        let closing = TerminalRow {
            closing: true,
            ..row.clone()
        };
        assert_eq!(
            closing.wire_state(true),
            TerminalWireState::Closing,
            "a committed close outranks liveness — the seat is still there, briefly"
        );
        assert_eq!(closing.wire_state(false), TerminalWireState::Closing);
    }
}
