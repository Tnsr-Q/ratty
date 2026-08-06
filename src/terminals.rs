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

use std::collections::BTreeMap;

use bevy::prelude::*;

use crate::identity::TerminalId;

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

    /// Despawn sweep: drops a dead terminal's row, and orphans every
    /// terminal it created.
    ///
    /// Orphaning rather than cascade-closing is the locked rule (#49 §2):
    /// a creator's children are principals in their own right, possibly
    /// with a user inside. Clearing `creator` makes "orphans become
    /// creator-less like terminal #1 — wire-unaddressable" literally true
    /// in the data, so the close authority needs no separate orphan clause.
    pub(crate) fn sweep_terminal(&mut self, id: TerminalId) {
        self.rows.remove(&id);
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

/// Registers the terminals organ.
pub struct TerminalsPlugin;

impl Plugin for TerminalsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TerminalRoster>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: u64) -> TerminalId {
        TerminalId::from_raw(raw)
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
