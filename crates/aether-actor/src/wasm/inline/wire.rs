//! Running an inline child's `wire` (ADR-0249 §4, §6): the one helper
//! every path that wires a rebuilt child goes through, and the walk a guest's
//! `wire` export runs over its children after the entry actor's hook.

use aether_data::MailboxId;

use super::{Child, Registry, Reinserted};
use crate::model::Anyone;
use crate::model::ctx::Erased;
use crate::model::ctx::reply_mode::Unchecked;
use crate::wasm::ActorInitError;
use crate::wasm::ctx::{NO_INBOUND_SOURCE, WasmCtx};
use crate::wasm::inline::unwire_child;

/// Run `child`'s `wire` if it owes one, through a ctx addressed to `id`.
///
/// A [`Child::Wired`] passes through untouched, so a second spawn of the
/// same name wires nothing. An [`Child::Unwired`] runs `erased_wire`; on
/// success it comes back [`Child::Wired`], and on failure it runs
/// `erased_unwire`, as `install_inline_child` does, and comes back
/// [`Child::Unwired`] beside the error so the caller can re-seat it.
fn wire_child(registry: &Registry, id: MailboxId, child: Child) -> Result<Child, (Child, ActorInitError)> {
    let mut actor = match child {
        Child::Wired(actor) => return Ok(Child::Wired(actor)),
        Child::Unwired(actor) => actor,
    };
    let mut ctx: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(id.0, registry, NO_INBOUND_SOURCE);

    match actor.erased_wire(&mut ctx) {
        Ok(()) => Ok(Child::Wired(actor)),
        Err(error) => {
            actor.erased_unwire(&mut ctx);
            Err((Child::Unwired(actor), error))
        }
    }
}

/// Wire the child seated at `id` if it owes a `wire`, and seat it again
/// (ADR-0249 §6). A child that is not seated is passed over: it is out on
/// the stack of a caller that holds it. A child that despawned itself inside
/// its own `wire` has no seat to return to: it runs its `unwire` and drops.
/// A child whose `wire` refused is seated again unwired.
pub fn wire_seated(registry: &Registry, id: MailboxId) -> Result<(), ActorInitError> {
    let Some(child) = registry.take(id) else {
        return Ok(());
    };

    match wire_child(registry, id, child) {
        Ok(wired) => {
            match registry.reinsert(id, wired) {
                Reinserted::Seated => {}
                Reinserted::Departed(departed) => drop(unwire_child(registry, id, departed)),
            }
            Ok(())
        }
        Err((unwired, error)) => {
            drop(registry.reinsert(id, unwired));
            Err(error)
        }
    }
}

/// Run `wire` on every inline child of `registry` that has not wired yet,
/// in registry walk order, which is rebuild order, parents first
/// (ADR-0249 §6). The `export!` `wire` shims call it after the entry
/// actor's own hook returns `Ok`.
///
/// Each child goes through `wire_seated`. The first failure ends the
/// walk; the refused child is left unwired and resident.
pub fn wire_rebuilt_children(registry: &Registry) -> Result<(), ActorInitError> {
    registry.child_metas().into_iter().try_for_each(|meta| wire_seated(registry, meta.id))
}

#[cfg(test)]
mod tests {
    use super::{wire_child, wire_rebuilt_children};
    use crate::mail::{Mail, PriorState};
    use crate::wasm::inline::{Child, ChildRecord, Registry};
    use crate::wasm::{ActorInitError, ErasedWasmActor, WasmCtx, WasmDropCtx};
    use crate::{Anyone, Erased, Unchecked};
    use aether_data::MailboxId;
    use alloc::boxed::Box;
    use alloc::rc::Rc;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    /// The order `wire` ran in, by each child's label.
    type WireLog = Rc<RefCell<Vec<&'static str>>>;

    /// A child that writes its label to the shared log when its `wire`
    /// runs, and refuses when told to. These tests never dispatch it.
    struct LoggingChild {
        label: &'static str,
        log: WireLog,
        refuses: bool,
        unwired: WireLog,
    }

    impl ErasedWasmActor for LoggingChild {
        fn erased_namespace(&self) -> &'static str {
            "test.inline.logging_child"
        }
        fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>, _mail: Mail<'_>) -> u32 {
            unreachable!("the wire tests never dispatch this child")
        }
        fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>) -> Result<(), ActorInitError> {
            self.log.borrow_mut().push(self.label);
            if self.refuses {
                return Err(ActorInitError::new("the logging child's wire refused"));
            }
            Ok(())
        }
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>) {
            self.unwired.borrow_mut().push(self.label);
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

    fn logging(label: &'static str, log: &WireLog, unwired: &WireLog, refuses: bool) -> Box<dyn ErasedWasmActor> {
        Box::new(LoggingChild { label, log: Rc::clone(log), refuses, unwired: Rc::clone(unwired) })
    }

    fn record(label: &str, parent: u64) -> ChildRecord {
        ChildRecord { full_subname: String::from(label), parent, ..ChildRecord::default() }
    }

    /// Unwired children wire in walk order. Catches a successor going live
    /// with setup its `wire` never ran.
    #[test]
    fn unwired_children_wire_in_walk_order() {
        let registry = Registry::new();
        let root = 0x7300_u64;
        registry.set_self_id(root);
        let log = WireLog::default();
        let unwired = WireLog::default();
        registry.insert_child(MailboxId(0x7301), record("first", root), logging("first", &log, &unwired, false));
        registry.insert_child(MailboxId(0x7302), record("second", root), logging("second", &log, &unwired, false));

        wire_rebuilt_children(&registry).expect("both children wire");

        assert_eq!(log.borrow().as_slice(), ["first", "second"]);
        assert!(unwired.borrow().is_empty());
    }

    /// Wired children are skipped. Catches a second `wire` on one instance.
    #[test]
    fn wired_children_are_skipped() {
        let registry = Registry::new();
        let root = 0x7400_u64;
        registry.set_self_id(root);
        let log = WireLog::default();
        let unwired = WireLog::default();
        registry.seat_wired(MailboxId(0x7401), record("wired", root), logging("wired", &log, &unwired, false));
        registry.insert_child(MailboxId(0x7402), record("unwired", root), logging("unwired", &log, &unwired, false));

        wire_rebuilt_children(&registry).expect("the unwired child wires");

        assert_eq!(log.borrow().as_slice(), ["unwired"]);
    }

    /// A refusal fails the walk, runs that child's `unwire`, and leaves the
    /// refused child unwired. Catches a refused `wire` installed as live.
    #[test]
    fn a_refusal_fails_the_walk_and_leaves_the_child_unwired() {
        let registry = Registry::new();
        let root = 0x7500_u64;
        registry.set_self_id(root);
        let log = WireLog::default();
        let unwired = WireLog::default();
        registry.insert_child(MailboxId(0x7501), record("first", root), logging("first", &log, &unwired, false));
        registry.insert_child(MailboxId(0x7502), record("refuser", root), logging("refuser", &log, &unwired, true));
        registry.insert_child(MailboxId(0x7503), record("later", root), logging("later", &log, &unwired, false));

        let result = wire_rebuilt_children(&registry);

        assert!(result.is_err(), "the refusing child fails the walk");
        assert_eq!(log.borrow().as_slice(), ["first", "refuser"]);
        assert_eq!(unwired.borrow().as_slice(), ["refuser"]);
        let child = registry.take(MailboxId(0x7502)).expect("the refused child stays resident");
        assert!(matches!(child, Child::Unwired(_)), "the refused child is left unwired");
        let _ = registry.reinsert(MailboxId(0x7502), child);
    }

    /// A `Wired` child passes through `wire_child` untouched.
    #[test]
    fn a_wired_child_passes_through_wire_child() {
        let registry = Registry::new();
        let root = 0x7600_u64;
        registry.set_self_id(root);
        let log = WireLog::default();
        let unwired = WireLog::default();
        let child = Child::Wired(logging("wired", &log, &unwired, false));

        let Ok(wired) = wire_child(&registry, MailboxId(0x7601), child) else {
            panic!("a wired child wires");
        };
        assert!(matches!(wired, Child::Wired(_)), "a wired child stays wired");
        assert!(log.borrow().is_empty(), "a wired child runs no wire");
    }
}
