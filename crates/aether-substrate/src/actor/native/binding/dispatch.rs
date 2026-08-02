//! The ADR-0093 hold-until-resolve in-flight ledger — the `&self`
//! interior-mutability bridge between a handler's dispatch primitive and the
//! per-actor table that parks its hold.

use std::any::Any;
use std::sync::Arc;

use super::NativeBinding;
use crate::mail::{Mail, Source};
use crate::runtime::trace::SettlementHold;
use aether_data::Kind;

/// ADR-0093 hold-until-resolve dispatch: the `&self`-interior-mutability
/// bridge between [`super::ctx::NativeCtx`](crate::actor::native::ctx::NativeCtx)'s dispatch primitive and the
/// per-actor `InflightTable` (crate-internal). Each method
/// takes the table lock for one operation — mint+insert at dispatch,
/// fill-output from the worker, take at completion — matching the
/// `outbound` / `blob_producer` locking pattern (uncontended, single
/// logical writer).
impl NativeBinding {
    /// Insert a freshly-minted in-flight dispatch entry and return its
    /// [`DispatchId`](super::offload::blocking::DispatchId). Called on
    /// the actor thread at dispatch time, after the hold is acquired and
    /// before the worker spawns. `hold` is `None` when the dispatching
    /// context carried no chain to hold (ADR-0168 §2).
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_insert(
        &self,
        hold: Option<SettlementHold>,
        reply_to: Source,
        context: Box<dyn Any + Send>,
    ) -> super::offload::blocking::DispatchId {
        self.inflight
            .lock()
            .expect("in-flight ledger poisoned; fail-fast per ADR-0063")
            .dispatch_insert(hold, reply_to, context)
    }

    /// Arm a typed deferred completion in the ordinary ADR-0093 ledger.
    /// The returned move-only capability retains this binding only weakly.
    pub(crate) fn dispatch_arm<O, C>(
        self: &Arc<Self>,
        hold: Option<SettlementHold>,
        reply_to: Source,
        context: C,
    ) -> super::offload::blocking::DeferredCompletion<O>
    where
        C: Send + 'static,
    {
        let dispatch_id = self.dispatch_insert(hold, reply_to, Box::new(context));
        super::offload::blocking::DeferredCompletion::new(Arc::downgrade(self), dispatch_id)
    }

    /// Shared deferred-completion tail. Fill the named dispatch's output
    /// slot under the ledger mutex, drop the lock, then push exactly one
    /// [`TaskCompletionWake`](super::offload::blocking::TaskCompletionWake)
    /// for the winning fill.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_complete<O>(&self, id: super::offload::blocking::DispatchId, output: O)
    where
        O: Send + 'static,
    {
        if self
            .inflight
            .lock()
            .expect("in-flight ledger poisoned; fail-fast per ADR-0063")
            .dispatch_fill_output(id, Box::new(output))
            == super::offload::blocking::FillOutcome::Filled
        {
            self.mailer.push(Mail::new(
                self.self_mailbox(),
                super::offload::blocking::TaskCompletionWake::ID,
                super::offload::blocking::TaskCompletionWake { dispatch_id: id.0 }.encode_into_bytes(),
                1,
            ));
        }
    }

    /// Remove the named dispatch entry and rebuild its
    /// [`TaskDone`](super::offload::blocking::TaskDone). Called on the
    /// actor thread when the completion-wake mail lands.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_take<O: 'static, C: 'static>(
        &self,
        id: super::offload::blocking::DispatchId,
    ) -> Option<super::offload::blocking::TaskDone<O, C>> {
        self.inflight.lock().expect("in-flight ledger poisoned; fail-fast per ADR-0063").dispatch_take(id)
    }

    /// Remove the named dispatch entry and hand back its parked
    /// `(Option<SettlementHold>, Source)` without any downcast — the
    /// release path for a worker that never armed. The spawn-error branch of
    /// [`dispatch_blocking_resumed_with`](crate::actor::native::ctx::NativeCtx::dispatch_blocking_resumed_with)
    /// calls this and drops the returned hold so the chain settles.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_abandon(
        &self,
        id: super::offload::blocking::DispatchId,
    ) -> Option<(Option<SettlementHold>, Source)> {
        self.inflight.lock().expect("in-flight ledger poisoned; fail-fast per ADR-0063").dispatch_abandon(id)
    }

    /// Non-consuming peek-then-take of the named dispatch entry: probe its
    /// boxed output + context against `O` / `C` and only remove + rebuild
    /// the [`TaskDone`](super::offload::blocking::TaskDone) on a match,
    /// leaving the entry intact on a mismatch. The `#[handler(task)]`
    /// dispatch chain calls this to route a completion to the right
    /// output-typed handler without a wrong-type probe consuming the entry.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_try_take<O: 'static, C: 'static>(
        &self,
        id: super::offload::blocking::DispatchId,
    ) -> Option<super::offload::blocking::TaskDone<O, C>> {
        self.inflight.lock().expect("in-flight ledger poisoned; fail-fast per ADR-0063").dispatch_try_take(id)
    }
}
