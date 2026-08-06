//! Terminal identity: the monotonic [`TerminalId`], the recyclable wire
//! namespace lease, and the [`TerminalRegistry`] that mints both.
//!
//! Spine decision 2 (#56): `TerminalId` is monotonic, never reused, never
//! recycled. The namespace is a SEPARATE recyclable 128-slot lease (0..=127)
//! because the wire masks object ids with `& 0x7F` (see
//! [`crate::osc::ai_object_namespace`]) — seven bits, so 128 slots is a hard
//! ceiling and widening the pool would silently alias every allocation above
//! 127 (the u8-wider-than-wire landmine, #56 riders).
//!
//! Decision 17 (the stamp rule): every persisted stamp keys on `TerminalId`,
//! never the namespace — the namespace appears only in wire-facing addresses
//! and in structures that die with their terminal. A leaked `TerminalId` can
//! never equal any future terminal's id; a leaked namespace aliases the
//! recycled slot's next tenant.

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::resource::Resource;

/// The highest allocatable agent namespace: the wire masks object ids with
/// `& 0x7F`, so namespaces are seven bits wide even though they travel as
/// `u8`.
const MAX_NAMESPACE: u8 = 0x7F;

/// The hard ceiling on live terminals: one per namespace slot, and the
/// wire masks object ids with `& 0x7F`. `[terminal] max_live` is clamped
/// to this; no configuration can exceed it without aliasing every
/// allocation above 127.
pub const MAX_LIVE_TERMINALS: usize = MAX_NAMESPACE as usize + 1;

/// The smallest grid vt100 can drive. Below two columns its wide-character
/// wrap logic underflows (`cols - width` for a 2-cell glyph), and below two
/// rows its wrap/scroll bookkeeping does (`prev_row - scrolled`) — the same
/// floor [`crate::terminal::TerminalSurface::resize_to_fit`] clamps to.
pub const MIN_TERMINAL_AXIS: u16 = 2;

/// The largest grid a wire caller may ask for on either axis.
///
/// `TerminalSurface::resize` guards only zero, so `u16` is the only other
/// bound — and `u16` is not a bound: a grid is a `cols × rows` buffer of
/// ratatui cells whose texture becomes a CPU-side `w × h × 4` image, so
/// `cols=65535` is tens of gigabytes from one OSC sequence. A window can
/// never reach these sizes; the wire can, which is why the ceiling lives
/// here rather than in the resize path.
pub const MAX_TERMINAL_AXIS: u16 = 512;

/// The largest cell count a wire caller may ask for. The per-axis ceiling
/// alone still admits 512×512 — 262 144 cells, an order of magnitude past
/// any real terminal — so the area is bounded too.
pub const MAX_TERMINAL_CELLS: usize = 100_000;

/// Sustained wire-driven spawns per second, per arrival terminal (the
/// token-bucket refill rate). The live cap bounds concurrency, not rate:
/// closes are deferred a frame and one PTY chunk can carry arbitrarily
/// many commands, so a spawn/close cycle would otherwise fork processes at
/// frame rate while never exceeding the cap.
pub const TERMINAL_SPAWNS_PER_SEC: u32 = 1;

/// Wire-driven spawn burst per arrival terminal (the bucket capacity).
pub const TERMINAL_SPAWN_BURST: u32 = 4;

/// Sustained wire-driven focus moves per second, per arrival terminal.
/// Without a budget a granted caller wins every frame the user does not
/// act, which is keystroke capture by attrition rather than by grant.
pub const TERMINAL_FOCUS_PER_SEC: u32 = 4;

/// Wire-driven focus burst per arrival terminal.
pub const TERMINAL_FOCUS_BURST: u32 = 8;

/// The live-terminal cap this config asks for, clamped to what the wire
/// can address. A configured 0 would make the app unbootable and a
/// configured 9999 would alias namespaces, so both ends are clamped
/// rather than rejected.
pub fn max_live_terminals(config: &crate::config::TerminalConfig) -> usize {
    config.max_live.clamp(1, MAX_LIVE_TERMINALS)
}

/// Whether a wire-requested grid is admissible: both axes within
/// [`MIN_TERMINAL_AXIS`]`..=`[`MAX_TERMINAL_AXIS`], and the area within
/// [`MAX_TERMINAL_CELLS`].
pub fn grid_is_admissible(cols: u16, rows: u16) -> bool {
    (MIN_TERMINAL_AXIS..=MAX_TERMINAL_AXIS).contains(&cols)
        && (MIN_TERMINAL_AXIS..=MAX_TERMINAL_AXIS).contains(&rows)
        && usize::from(cols) * usize::from(rows) <= MAX_TERMINAL_CELLS
}

/// Monotonic terminal identity (spine decision 2): never reused, never
/// recycled. Starts at 1 so a zeroed value can never alias a live terminal.
///
/// `Ord` is derived so terminal-keyed maps iterate in mint order — render
/// submission and every future N measurement stay deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TerminalId(u64);

impl TerminalId {
    /// Test-only mint for fixtures. Production ids come only from
    /// [`TerminalRegistry::allocate`].
    #[cfg(test)]
    pub(crate) const fn from_raw(id: u64) -> Self {
        Self(id)
    }
}

/// The identity pair a terminal seat carries: the persistent [`TerminalId`]
/// and the leased wire namespace.
///
/// Fields are private and minted only by [`TerminalRegistry::allocate`] —
/// the pair is redundant data that could lie if constructed anywhere else,
/// and the stamp rule (decision 17) rests on that single-writer invariant.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalIdentity {
    id: TerminalId,
    namespace: u8,
}

impl TerminalIdentity {
    /// The persistent identity every stamp keys on.
    pub fn id(self) -> TerminalId {
        self.id
    }

    /// The leased wire namespace (0..=127); a wire-facing address, never a
    /// persisted key.
    pub fn namespace(self) -> u8 {
        self.namespace
    }

    /// The ingress context bytes arriving through this terminal's
    /// transport are stamped with.
    pub fn ingress(self) -> crate::runtime::IngressSource {
        crate::runtime::IngressSource::Local(self)
    }

    /// Test-only boot identity: what the first
    /// [`TerminalRegistry::allocate`] on a fresh registry mints
    /// (`TerminalId` 1, namespace 0).
    #[cfg(test)]
    pub(crate) const fn test_boot() -> Self {
        Self {
            id: TerminalId(1),
            namespace: 0,
        }
    }
}

/// The per-terminal session-half state (#56 decision 5) every seat is
/// born with, alongside its [`TerminalIdentity`]. Freshness for a
/// recycled namespace slot's next tenant is not a cleanup discipline —
/// it is `Default::default()` at spawn, unconditionally; the session
/// halves die with the seat entity and can never be inherited.
pub fn terminal_session_state() -> impl bevy::ecs::bundle::Bundle {
    (
        crate::query_channel::TerminalDiagnostics::default(),
        crate::macros::TerminalMacros::default(),
        crate::reactive::TerminalReactive::default(),
        crate::effects::AiEffects::default(),
    )
}

/// A live terminal's registry row: the leased namespace plus the seat
/// entity once [`TerminalRegistry::bind`] runs.
struct LiveTerminal {
    namespace: u8,
    entity: Option<Entity>,
}

/// Allocator and resolver for live terminals.
///
/// One allocation site ([`Self::allocate`]) and one release site (the
/// despawn sweep) exist so no interleaving lets a recycled namespace
/// coexist with the prior tenant's state.
#[derive(Resource)]
pub struct TerminalRegistry {
    /// Monotonic `TerminalId` counter; starts at 1, only increments — an id
    /// consumed by a failed spawn is skipped, never reissued.
    next_id: u64,
    /// Free namespace slots. `pop_first` makes allocation
    /// lowest-free-first, so the boot terminal deterministically leases
    /// namespace 0 and every wire byte matches the single-terminal era.
    free_namespaces: BTreeSet<u8>,
    live: HashMap<TerminalId, LiveTerminal>,
}

impl Default for TerminalRegistry {
    fn default() -> Self {
        Self {
            next_id: 1,
            free_namespaces: (0..=MAX_NAMESPACE).collect(),
            live: HashMap::new(),
        }
    }
}

/// Allocation failure: the pool is exhausted. Explicit by construction —
/// the allocator never mints above 127 and never aliases silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalAllocError {
    /// All 128 namespace slots are leased.
    NamespacesExhausted,
}

impl fmt::Display for TerminalAllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NamespacesExhausted => write!(
                f,
                "all 128 agent namespaces are live; the wire masks object ids \
                 with & 0x7F, so 0..=127 is a hard ceiling"
            ),
        }
    }
}

impl std::error::Error for TerminalAllocError {}

/// Release failure: double-free is loud, never swallowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalReleaseError {
    /// The id was never allocated, or was already released.
    UnknownTerminal(TerminalId),
    /// The id was live but its namespace slot was already free — a
    /// registry invariant violation, surfaced instead of masked.
    SlotAlreadyFree {
        /// The terminal whose release found the slot free.
        id: TerminalId,
        /// The namespace slot in question.
        namespace: u8,
    },
}

impl fmt::Display for TerminalReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTerminal(id) => {
                write!(f, "{id:?} is not a live terminal (double release?)")
            }
            Self::SlotAlreadyFree { id, namespace } => write!(
                f,
                "{id:?} was live but namespace {namespace} was already free; \
                 the registry invariant is broken"
            ),
        }
    }
}

impl std::error::Error for TerminalReleaseError {}

impl TerminalRegistry {
    /// Mints a fresh identity: the next monotonic [`TerminalId`] paired
    /// with the lowest free namespace slot.
    ///
    /// # Errors
    ///
    /// Errors explicitly when all 128 slots are leased; exhaustion is
    /// never masked by minting above 127 or aliasing a live slot.
    pub fn allocate(&mut self) -> Result<TerminalIdentity, TerminalAllocError> {
        let namespace = self
            .free_namespaces
            .pop_first()
            .ok_or(TerminalAllocError::NamespacesExhausted)?;
        // The only place namespaces are minted, so the only place the
        // u8-wider-than-wire landmine (#56 riders) needs a tripwire.
        debug_assert!(
            namespace <= MAX_NAMESPACE,
            "namespace {namespace} would alias under the wire's & 0x7F mask"
        );
        let id = TerminalId(self.next_id);
        self.next_id += 1;
        self.live.insert(
            id,
            LiveTerminal {
                namespace,
                entity: None,
            },
        );
        Ok(TerminalIdentity { id, namespace })
    }

    /// Returns a dead terminal's namespace to the pool.
    ///
    /// # Errors
    ///
    /// Errors on an unknown (or already released) id and on a
    /// slot-already-free registry invariant violation — double-free is
    /// loud, never swallowed.
    pub fn release(&mut self, id: TerminalId) -> Result<u8, TerminalReleaseError> {
        let live = self
            .live
            .remove(&id)
            .ok_or(TerminalReleaseError::UnknownTerminal(id))?;
        if !self.free_namespaces.insert(live.namespace) {
            return Err(TerminalReleaseError::SlotAlreadyFree {
                id,
                namespace: live.namespace,
            });
        }
        Ok(live.namespace)
    }

    /// Binds an allocated id to its seat entity.
    ///
    /// # Errors
    ///
    /// Errors when the id is not live — binding is only meaningful between
    /// [`Self::allocate`] and [`Self::release`].
    pub fn bind(&mut self, id: TerminalId, entity: Entity) -> Result<(), TerminalReleaseError> {
        let live = self
            .live
            .get_mut(&id)
            .ok_or(TerminalReleaseError::UnknownTerminal(id))?;
        live.entity = Some(entity);
        Ok(())
    }

    /// The honest resolver for deferred consumers: a dead [`TerminalId`]
    /// resolves `None`, and nothing aliases because ids never recycle.
    pub fn entity_of(&self, id: TerminalId) -> Option<Entity> {
        self.live.get(&id).and_then(|live| live.entity)
    }

    /// The live namespace leased to `id`, or `None` once it dies.
    pub fn namespace_of(&self, id: TerminalId) -> Option<u8> {
        self.live.get(&id).map(|live| live.namespace)
    }

    /// How many terminals hold a lease right now — the number the live
    /// cap is checked against.
    ///
    /// Deliberately the lease count and not a seat-entity query: leases
    /// mutate synchronously inside [`Self::allocate`] and [`Self::release`]
    /// while seat entities are deferred `Commands`, so a query-based count
    /// reads stale mid-batch and two spawns in one PTY chunk would both
    /// pass a cap they jointly exceed.
    pub fn live_count(&self) -> usize {
        self.live.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_mints_lowest_slot_first_and_errors_explicitly_on_exhaustion() {
        let mut registry = TerminalRegistry::default();
        for expected in 0..=MAX_NAMESPACE {
            let identity = registry.allocate().expect("128 slots exist");
            assert_eq!(
                identity.namespace(),
                expected,
                "allocation is lowest-free-first"
            );
            assert_eq!(
                identity.id(),
                TerminalId::from_raw(u64::from(expected) + 1),
                "ids are monotonic from 1"
            );
        }
        assert_eq!(
            registry.allocate(),
            Err(TerminalAllocError::NamespacesExhausted),
            "the 129th allocation errors explicitly; nothing above 127 is ever minted"
        );
    }

    #[test]
    fn recycle_reuses_the_lowest_freed_slot_while_ids_keep_climbing() {
        let mut registry = TerminalRegistry::default();
        let a = registry.allocate().expect("slot 0");
        let b = registry.allocate().expect("slot 1");
        assert_eq!(registry.release(a.id()), Ok(0), "release returns the slot");
        let c = registry.allocate().expect("the recycled slot");
        assert_eq!(c.namespace(), 0, "the freed slot is recycled lowest-first");
        assert_eq!(
            c.id(),
            TerminalId::from_raw(3),
            "the TerminalId counter never reuses — recycling is namespace-only"
        );
        assert_ne!(c.id(), a.id(), "a recycled slot's tenant is a new terminal");
        assert_eq!(b.namespace(), 1, "the untouched lease is unaffected");
    }

    #[test]
    fn double_release_and_unknown_ids_error_loudly() {
        let mut registry = TerminalRegistry::default();
        let a = registry.allocate().expect("slot 0");
        assert_eq!(registry.release(a.id()), Ok(0));
        assert_eq!(
            registry.release(a.id()),
            Err(TerminalReleaseError::UnknownTerminal(a.id())),
            "double-free is an explicit error, never swallowed"
        );
        assert_eq!(
            registry.release(TerminalId::from_raw(999)),
            Err(TerminalReleaseError::UnknownTerminal(TerminalId::from_raw(
                999
            ))),
            "an id the registry never minted errors"
        );
    }

    #[test]
    fn the_live_cap_clamps_to_what_the_wire_can_address() {
        let mut config = crate::config::TerminalConfig::default();
        assert_eq!(config.max_live, 4, "the shipped default (#56 decision 2)");
        assert_eq!(max_live_terminals(&config), 4);
        // A zero cap would make the app unbootable; a huge one would alias
        // namespaces above 127. Both ends clamp rather than reject.
        config.max_live = 0;
        assert_eq!(max_live_terminals(&config), 1);
        config.max_live = 9_999;
        assert_eq!(max_live_terminals(&config), MAX_LIVE_TERMINALS);
        assert_eq!(MAX_LIVE_TERMINALS, 128);

        // Additive for existing user configs: naming the new key leaves
        // every neighbor at its default, and omitting it is the default.
        let parsed = crate::config::AppConfig::from_toml_str("[terminal]\nmax_live = 1\n")
            .expect("the terminal section parses");
        assert_eq!(parsed.terminal.max_live, 1);
        assert_eq!(parsed.terminal.default_cols, 104);
        assert_eq!(parsed.terminal.scrollback, 2_000);
        let bare = crate::config::AppConfig::from_toml_str("[terminal]\ndefault_cols = 80\n")
            .expect("an existing config without the key still parses");
        assert_eq!(bare.terminal.max_live, 4);
    }

    #[test]
    fn the_grid_ceiling_bounds_both_axes_and_the_area() {
        assert!(grid_is_admissible(80, 24));
        assert!(grid_is_admissible(MIN_TERMINAL_AXIS, MIN_TERMINAL_AXIS));
        assert!(grid_is_admissible(MAX_TERMINAL_AXIS, 24));
        // vt100 underflows below two on either axis.
        assert!(!grid_is_admissible(1, 24));
        assert!(!grid_is_admissible(80, 1));
        assert!(!grid_is_admissible(0, 0));
        // A `u16` is not a bound: the grid becomes a CPU-side image.
        assert!(!grid_is_admissible(MAX_TERMINAL_AXIS + 1, 24));
        assert!(!grid_is_admissible(80, MAX_TERMINAL_AXIS + 1));
        assert!(!grid_is_admissible(u16::MAX, u16::MAX));
        // The per-axis ceiling alone still admits 262 144 cells, so the
        // area is bounded separately.
        assert!(!grid_is_admissible(MAX_TERMINAL_AXIS, MAX_TERMINAL_AXIS));
    }

    #[test]
    fn live_count_tracks_leases_not_entities() {
        let mut registry = TerminalRegistry::default();
        assert_eq!(registry.live_count(), 0);
        let a = registry.allocate().expect("slot 0");
        let _b = registry.allocate().expect("slot 1");
        // Counted at allocate, before any seat entity exists — the cap
        // must hold within a single command batch.
        assert_eq!(registry.live_count(), 2);
        registry.release(a.id()).expect("a is live");
        assert_eq!(registry.live_count(), 1);
    }

    #[test]
    fn bind_resolves_live_terminals_and_dead_ids_resolve_none() {
        let mut registry = TerminalRegistry::default();
        let a = registry.allocate().expect("slot 0");
        let mut world = bevy::ecs::world::World::new();
        let seat = world.spawn_empty().id();
        assert_eq!(registry.entity_of(a.id()), None, "unbound resolves None");
        registry.bind(a.id(), seat).expect("a is live");
        assert_eq!(registry.entity_of(a.id()), Some(seat));
        assert_eq!(registry.namespace_of(a.id()), Some(0));
        registry.release(a.id()).expect("a is live");
        assert_eq!(
            registry.entity_of(a.id()),
            None,
            "a dead TerminalId resolves None; nothing aliases"
        );
        assert_eq!(registry.namespace_of(a.id()), None);
        assert_eq!(
            registry.bind(a.id(), seat),
            Err(TerminalReleaseError::UnknownTerminal(a.id())),
            "binding a dead id errors"
        );
    }
}
