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
