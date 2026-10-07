//! Running an inline child's `unwire` (ADR-0249 §4, §6): the one helper
//! every path that closes a child goes through, and the cascade a guest's
//! `unwire` export runs over its children before the entry actor's hook.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cmp::Reverse;

use aether_data::MailboxId;

use super::{Child, InlineChildMeta, Registry, Reinserted};
use crate::model::Anyone;
use crate::model::ctx::Erased;
use crate::model::ctx::reply_mode::Unchecked;
use crate::wasm::ctx::{NO_INBOUND_SOURCE, WasmCtx};

/// Run `child`'s `unwire` if it owes one, through a ctx addressed to `id`,
/// and hand it back unwired.
///
/// This is what makes "`unwire` runs once, and only on what wired" hold:
/// a [`Child::Wired`] comes back [`Child::Unwired`], and an unwired child
/// passes through untouched, so a second close of the same child runs
/// nothing.
pub fn unwire_child(registry: &Registry, id: MailboxId, child: Child) -> Child {
    match child {
        Child::Wired(mut actor) => {
            let mut ctx: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(id.0, registry, NO_INBOUND_SOURCE);
            actor.erased_unwire(&mut ctx);
            Child::Unwired(actor)
        }
        unwired @ Child::Unwired(_) => unwired,
    }
}

/// Run `unwire` on every inline child of `registry` that owes one, children
/// before their parents, deepest first (ADR-0249 §6). The `export!` `unwire`
/// shims call it before the entry actor's own hook.
///
/// Each child is taken, unwired, and seated again as [`Child::Unwired`]. It
/// stays in the registry: a republish's prepare runs `on_dehydrate` after
/// `unwire`, and that walk saves the children it finds. Nothing is retired,
/// because a close tombstones the aliases itself.
///
/// A child that is not seated is passed over: an earlier child's `unwire`
/// despawned it, which already ran its `unwire`.
pub fn unwire_children(registry: &Registry) {
    for id in close_order(&registry.child_metas()) {
        let Some(child) = registry.take(id) else {
            continue;
        };
        // A child whose own `unwire` despawned it has no slot to go back to.
        // It is unwired by now, so it drops here.
        match registry.reinsert(id, unwire_child(registry, id, child)) {
            Reinserted::Seated => {}
            Reinserted::Departed(departed) => drop(departed),
        }
    }
}

/// The order a close unwires `metas` in: by depth, deepest first, and within
/// one depth the reverse of the order given, which is the registry's walk
/// order and the order a republish rebuilds children in.
fn close_order(metas: &[InlineChildMeta]) -> Vec<MailboxId> {
    let parents: BTreeMap<MailboxId, MailboxId> = metas.iter().map(|meta| (meta.id, meta.parent)).collect();
    let mut ordered: Vec<(usize, usize, MailboxId)> =
        metas.iter().enumerate().map(|(index, meta)| (depth_of(&parents, meta.id), index, meta.id)).collect();
    ordered.sort_by_key(|&(depth, index, _)| (Reverse(depth), Reverse(index)));

    ordered.into_iter().map(|(_, _, id)| id).collect()
}

/// The number of recorded parent links between `id` and the cluster root,
/// which has no slot and so no entry in `parents`. The walk is capped at the
/// child count, so a broken link that loops ends the count.
fn depth_of(parents: &BTreeMap<MailboxId, MailboxId>, id: MailboxId) -> usize {
    let mut depth = 0;
    let mut at = id;
    for _ in 0..parents.len() {
        let Some(parent) = parents.get(&at) else {
            break;
        };
        depth += 1;
        at = *parent;
    }
    depth
}

#[cfg(test)]
mod tests {
    use super::unwire_children;
    use crate::mail::{Mail, PriorState};
    use crate::reference::ErasedActorRef;
    use crate::wasm::ctx::NO_INBOUND_SOURCE;
    use crate::wasm::inline::{ChildRecord, Registry};
    use crate::wasm::{ActorInitError, ErasedWasmActor, WasmCtx, WasmDropCtx};
    use crate::{Anyone, Erased, Unchecked};
    use aether_data::MailboxId;
    use alloc::boxed::Box;
    use alloc::rc::Rc;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    /// The order `unwire` ran in, by each child's label.
    type UnwireLog = Rc<RefCell<Vec<&'static str>>>;

    /// A child that writes its label to the shared log when its `unwire`
    /// runs. These tests never dispatch it.
    struct LoggingChild {
        label: &'static str,
        log: UnwireLog,
    }

    impl ErasedWasmActor for LoggingChild {
        fn erased_namespace(&self) -> &'static str {
            "test.inline.logging_child"
        }
        fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>, _mail: Mail<'_>) -> u32 {
            unreachable!("the unwire tests never dispatch this child")
        }
        fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>) -> Result<(), ActorInitError> {
            Ok(())
        }
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>) {
            self.log.borrow_mut().push(self.label);
        }
        fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) -> Result<(), ActorInitError> {
            Ok(())
        }
        fn erased_on_rehydrate(
            &mut self,
            _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>,
            _prior: PriorState<'_>,
        ) -> Result<(), ActorInitError> {
            Ok(())
        }
    }

    fn logging(label: &'static str, log: &UnwireLog) -> Box<dyn ErasedWasmActor> {
        Box::new(LoggingChild { label, log: Rc::clone(log) })
    }

    fn record(label: &str, parent: u64) -> ChildRecord {
        ChildRecord { full_subname: String::from(label), parent, ..ChildRecord::default() }
    }

    /// A close unwires a leaf before the child it sits beneath, and two
    /// children at one depth in the reverse of the registry's walk order.
    /// The leaf's alias sorts first, so an order that only reversed the walk
    /// would unwire the child before its leaf. Catches a parent unwired
    /// before its child.
    #[test]
    fn a_close_unwires_the_deepest_child_first() {
        let registry = Registry::new();
        let root = 0x7000_u64;
        registry.set_self_id(root);
        let log = UnwireLog::default();
        let leaf = MailboxId(0x7010);
        let child = MailboxId(0x7020);
        let sibling = MailboxId(0x7030);
        registry.seat_wired(child, record("child", root), logging("child", &log));
        registry.seat_wired(sibling, record("sibling", root), logging("sibling", &log));
        registry.seat_wired(leaf, record("leaf", child.0), logging("leaf", &log));

        unwire_children(&registry);

        assert_eq!(log.borrow().as_slice(), ["leaf", "sibling", "child"]);
        assert_eq!(registry.child_metas().len(), 3, "the cascade leaves every child resident for the dehydrate walk");
    }

    /// A child a rebuild seated ran `init` and `on_rehydrate` and no `wire`,
    /// so a close owes it nothing. Catches `unwire` run on a child that
    /// never wired.
    #[test]
    fn a_close_runs_no_unwire_on_a_rebuilt_child() {
        let registry = Registry::new();
        let root = 0x7100_u64;
        registry.set_self_id(root);
        let log = UnwireLog::default();
        registry.insert_child(MailboxId(0x7101), record("rebuilt", root), logging("rebuilt", &log));

        unwire_children(&registry);

        assert!(log.borrow().is_empty(), "a child that did not wire ran unwire: {:?}", log.borrow());
    }

    /// A parent whose own `unwire` despawns its children runs after the
    /// cascade has unwired them. Catches the second `unwire` that despawn
    /// would run on a child the cascade already closed.
    #[test]
    fn a_child_the_close_unwired_is_not_unwired_again_by_a_despawn() {
        let registry = Registry::new();
        let root = 0x7200_u64;
        registry.set_self_id(root);
        let log = UnwireLog::default();
        let child = MailboxId(0x7201);
        registry.seat_wired(child, record("child", root), logging("child", &log));

        unwire_children(&registry);
        let ctx: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(root, &registry, NO_INBOUND_SOURCE);
        let removed = ctx.despawn_inline_child(ErasedActorRef::new(child));

        assert!(removed, "the unwired child was still resident for the despawn");
        assert_eq!(log.borrow().as_slice(), ["child"], "the child ran unwire once, at the close");
    }
}
