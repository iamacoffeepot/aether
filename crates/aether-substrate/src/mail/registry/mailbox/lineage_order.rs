//! Where an actor stands in the actor tree by creation order (ADR-0248 §5):
//! the value a reader sorts by, and the read that builds it from the birth
//! serials the route records carry.
//!
//! The tree is ordered by creation and by nothing else. A parent's children
//! stand in the order the registry applied their births, and so do the root
//! actors. The registry stamps that order on each record ([`BirthSerial`])
//! and says nothing about what it means: the renderer paints by it, and any
//! other reader may use it or ignore it.
//!
//! Nothing here takes a lock or an atomic. The serials are written by the
//! registry's one writer as part of the batch that inserts a record, and the
//! read is one load of the route view that writer already publishes, so a
//! reader sees a whole committed batch or none of it. An actor that owned
//! the order instead would have to be told of every birth and close by mail,
//! and would answer its readers one hop late.

use std::cmp::Ordering;

use aether_actor::ErasedActorRef;
use aether_data::MAX_SCOPE_PATH_DEPTH;

use crate::mail::registry::names::lineage_prefixes;

use super::Registry;
use super::route::BirthSerial;

/// Where one actor stands in the actor tree by creation order: one step per
/// segment of its canonical path, root first, each step the birth serial of
/// the actor that prefix names.
///
/// Two values compare step by step, and a value that is a prefix of another
/// sorts first. So a parent sorts before its children, a child sorts between
/// its parent and its parent's next sibling, and two siblings sort in the
/// order they were created. Sorting actors ascending walks the tree as a
/// document is read: each parent, then its children in creation order.
///
/// It is opaque. A reader gets one only from
/// [`NativeCtx::lineage_order`](crate::actor::native::ctx::NativeCtx::lineage_order),
/// compares it, and copies it; it names no mailbox, it cannot be built from
/// parts, and it is not a kind field, so it is never mailed. A value is
/// meaningful only against others read from the same engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineageOrder {
    /// The steps in use are `steps[..depth]`. The rest stay `None`, so the
    /// derived equality agrees with the ordering below.
    steps: [Option<BirthSerial>; MAX_SCOPE_PATH_DEPTH],
    depth: usize,
}

impl LineageOrder {
    const ROOT: Self = Self { steps: [None; MAX_SCOPE_PATH_DEPTH], depth: 0 };

    /// This order one level deeper, at the child born with `serial`, or
    /// `None` past the path depth cap.
    fn beneath(mut self, serial: BirthSerial) -> Option<Self> {
        *self.steps.get_mut(self.depth)? = Some(serial);
        self.depth += 1;

        Some(self)
    }

    fn steps(&self) -> &[Option<BirthSerial>] {
        &self.steps[..self.depth]
    }
}

impl Ord for LineageOrder {
    fn cmp(&self, other: &Self) -> Ordering {
        self.steps().cmp(other.steps())
    }
}

impl PartialOrd for LineageOrder {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Registry {
    /// Where the actor `actor` proves stands in the actor tree by creation
    /// order, from one load of the published route view (ADR-0248 §5).
    ///
    /// The actor's own record gives its canonical name and the last step.
    /// Each ancestor is found by folding that name one segment at a time, one
    /// probe of the same loaded view per ancestor, at most
    /// `MAX_SCOPE_PATH_DEPTH - 1`; a record found at a fold counts only when
    /// it carries that prefix as its canonical name, the check
    /// [`Self::live_route`] makes for a whole path. Every lifecycle answers,
    /// `Dropped` included, so an actor that has closed keeps its place.
    ///
    /// `None` when `actor` or one of its ancestors holds no record. The birth
    /// arms refuse a name whose parent holds none
    /// (`RegistryEffectError::ParentUnknown`), and a record leaves the table
    /// only where [`Self::actor_path`] says one does, so neither follows a
    /// mint that survives.
    ///
    /// The crate-private path behind
    /// [`NativeCtx::lineage_order`](crate::actor::native::ctx::NativeCtx::lineage_order).
    pub(crate) fn lineage_order(&self, actor: ErasedActorRef) -> Option<LineageOrder> {
        let routes = self.routes.load();
        let own = routes.entry_for(&actor.id())?;
        let name = own.canonical_name.as_str();

        lineage_prefixes(name).try_fold(LineageOrder::ROOT, |order, (prefix, id)| {
            let born = if prefix.len() == name.len() {
                own.born
            } else {
                routes.entry_for(&id).filter(|ancestor| ancestor.canonical_name.as_str() == prefix)?.born
            };

            order.beneath(born)
        })
    }
}
