use aether_actor::{DEHYDRATE_HELD_UNSAVED, ReplyMode};
use wasmtime::Store;

use super::instantiate::Placement;
use super::{
    Component, ComponentCtx, CorrelationCursor, MAX_DELIVERABLE_MAIL_BYTES, PendingReplies, SMALL_REGION_BYTES,
    StateBundle,
};
use crate::actor::native::ctx::NativeCtx;
use crate::actor::wasm::host_fns::{GuestReply, guest_answer};
use crate::actor::wasm::reply_table::HeldChain;
use crate::mail::registry::{PreparedAliasRetirement, PreparedAliasRoute};

impl Component {
    /// Loudly log an init config rejected by [`Component::instantiate`] (ADR-0095)
    /// because it could not be delivered safely — either past the absolute
    /// ceiling, or to a guest with no allocator export. Mirrors the dispatch
    /// oversize log; the caller returns an `Err` that surfaces as
    /// `LoadResult::Err` rather than writing or trapping. Associated (no
    /// `&self`) because `instantiate` has no `Component` yet.
    pub(super) fn log_oversize_config(store: &Store<ComponentCtx>, config_bytes: usize, reason: &str) {
        tracing::error!(
            target: "aether_substrate::component",
            actor = %store.data().actor_name(),
            config_bytes,
            small_region_bytes = SMALL_REGION_BYTES,
            deliverable_cap_bytes = MAX_DELIVERABLE_MAIL_BYTES,
            reason,
            "rejecting init config; cannot deliver safely (see ADR-0095)",
        );
    }

    /// Issue 584 Phase 2b (ADR-0079 amended): pre-shutdown mail-allowed
    /// hook. It has two callers, both in the component trampoline: a
    /// republish's prepare, which runs it before `on_dehydrate` on the guest
    /// it may replace, and the trampoline's close, which runs it on the live
    /// guest before the `Component` value drops, whichever exit closed the
    /// trampoline (a `DropComponent`, an engine teardown, a birth cancelled
    /// after `wire`). Same trap containment as the other hooks: a guest
    /// trap is logged here and the caller goes on, so it never stalls a
    /// close.
    pub fn unwire(&mut self) {
        if let Some(f) = self.unwire.clone()
            && let Err(e) = f.call(&mut self.store, self.self_mailbox_id)
        {
            tracing::error!(target: "aether_substrate::component", error = %e, "unwire hook trapped");
        }
    }

    /// Invoke the guest's `on_dehydrate` hook if it exports one.
    /// Wasmtime traps (guest panics, unreachable) are caught and
    /// logged rather than propagated — per ADR-0015, a panicking
    /// hook must not stall teardown.
    ///
    /// ADR-0243 §6: a guest that left a live held reply unsaved returns
    /// `DEHYDRATE_HELD_UNSAVED`. That is recorded as a save error, so the
    /// trampoline's replace takes its existing save-error rollback through
    /// [`Self::take_save_error`] and reinstates this guest rather than
    /// stranding the requester. The refusing hook still saved its state, so
    /// [`Self::take_saved_state`] yields the bundle the reinstated guest gets
    /// back (issue 7125). A save error the hook already recorded is kept.
    pub fn on_dehydrate(&mut self) {
        let Some(f) = self.on_dehydrate.clone() else {
            return;
        };
        match f.call(&mut self.store, ()) {
            Ok(DEHYDRATE_HELD_UNSAVED) => {
                let error = &mut self.store.data_mut().save_state_error;
                if error.is_none() {
                    *error = Some("on_dehydrate refused: a held reply is live and was not saved".to_owned());
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!(target: "aether_substrate::component", error = %e, "on_dehydrate hook trapped");
            }
        }
    }

    /// The next send correlation and the next reply-lineage id this guest
    /// would mint, recorded by the component trampoline when the guest
    /// leaves its slot (unload or replace) so the slot's next occupant
    /// resumes both through [`Self::resume_correlations`] (ADR-0139 §3,
    /// #6422). Also read from a replacement that failed to rehydrate, so the
    /// reinstated guest resumes past it (#6134).
    #[must_use]
    pub fn correlation_cursor(&self) -> CorrelationCursor {
        self.store.data().correlation_cursor()
    }

    /// Continue this mailbox's send correlation and reply-lineage sequences
    /// from a guest that left the slot, so this guest never mints a request
    /// id or a reply `MailId` that one already used (ADR-0139 §3, #6422).
    /// Only ever raises either counter. The consumer is the component
    /// trampoline's replace: it calls this after [`Self::instantiate`] (which
    /// cannot send) and before `on_rehydrate` or the first delivery, and
    /// again on the reinstated guest when a replacement fails to rehydrate
    /// (#6134).
    pub fn resume_correlations(&mut self, cursor: CorrelationCursor) {
        self.store.data_mut().resume_correlations(cursor);
    }

    /// Answer every reply this guest still holds with the `unanswered`
    /// value it registered when it held (ADR-0243 §6), because it is
    /// unloading or its actor is closing, and no guest is left to answer.
    /// Each answer goes out on its requester's chain, and only then does
    /// that chain's settlement hold release, so `Sent` precedes `Release`
    /// as for a guest's own answer. Under engine teardown every requester
    /// is closing with the engine, so the chains release unanswered, as
    /// they do for a native actor. A slot reserved to a held answer is
    /// skipped; the trampoline discards its candidate's held outbox first,
    /// which restores it.
    ///
    /// The consumer is the component trampoline's close, which releases the
    /// guest.
    pub fn answer_held_at_close(&mut self) {
        let ctx = self.store.data_mut();
        let held = ctx.reply_table.drain_held();
        if ctx.binding.is_engine_teardown() {
            return;
        }

        for (entry, HeldChain { hold, root, parent, recipient, unanswered }) in held {
            // In the name of the address that held, as the guest's own answer
            // would be: an inline child's reply comes from the child.
            let reply = GuestReply { mail: unanswered, count: 1, from: recipient };
            match guest_answer(ctx, entry, reply) {
                Ok(answer) => ctx.answer(answer, Some((parent, root))),
                Err(status) => tracing::error!(
                    target: "aether_substrate::component",
                    actor = %ctx.actor_name(),
                    status,
                    "a held reply's unanswered value could not be sent; its requester's chain releases unanswered",
                ),
            }
            drop(hold);
        }
    }

    /// Move out the guest's reply table — its pending handles and the next
    /// handle it would issue — when the guest leaves its slot. The consumer
    /// is the component trampoline, which takes it after `unwire` and
    /// `on_dehydrate` (both may still answer handles) and hands it to the
    /// slot's next occupant through [`Self::resume_replies`] (#6409).
    #[must_use]
    pub fn take_pending_replies(&mut self) -> PendingReplies {
        self.store.data_mut().take_pending_replies()
    }

    /// Install the reply table a guest that left this slot carried, so a
    /// handle it issued still answers its own requester and this guest
    /// numbers new handles past it (#6409). The consumer is the component
    /// trampoline; call it after [`Self::instantiate`] succeeds and before
    /// the first delivery or `on_rehydrate`, while this instance's own
    /// table is still empty, or on a guest reinstated after its replacement
    /// failed to rehydrate, whose table was moved out (#6134).
    pub fn resume_replies(&mut self, replies: PendingReplies) {
        self.store.data_mut().resume_replies(replies);
    }

    /// Send every mail this candidate held (#7067) in the order it sent it,
    /// on the chain of the turn `ctx` is dispatching, and stop holding. A
    /// send is stamped with that turn's inbound as its parent and root, so
    /// the turn's chain settles only after the mail does; a detached send
    /// opens its own chain. A reply goes out on its requester's chain when
    /// its slot held one (ADR-0243 §6), after which its slot is freed and
    /// the requester's hold released. A no-op for an outbox never held.
    ///
    /// The consumer is a republish committing its candidate.
    pub fn flush_held_outbox<A, M: ReplyMode>(&mut self, ctx: &NativeCtx<'_, A, M>) {
        self.store.data_mut().flush_held(ctx.in_flight_mail_id(), ctx.in_flight_root());
    }

    /// Drop every mail this candidate held (#7067): its sends never recorded
    /// `Sent` and go nowhere, and each reply slot it reserved is put back
    /// exactly, chain included, so the old guest answers its requester once
    /// it takes the reply table back. A no-op for an outbox never held.
    ///
    /// The consumer is a republish aborting its candidate, before it moves
    /// the reply table back to the old guest.
    pub fn discard_held_outbox(&mut self) {
        self.store.data_mut().discard_held();
    }

    /// Extract the state bundle the guest deposited via `save_state`
    /// during `on_dehydrate`. Returns `None` if `save_state` was never
    /// called (component doesn't implement migration, or the hook is
    /// a no-op). Called by the control plane *after* `on_dehydrate`
    /// runs on the old instance — the bundle has to outlive the
    /// store.
    pub fn take_saved_state(&mut self) -> Option<StateBundle> {
        self.store.data_mut().saved_state.take()
    }

    /// Drain logical inline-child aliases staged during the just-returned
    /// guest call. The trampoline publishes them through the registry owner
    /// before its handler-end buffered mail is routed.
    pub fn drain_pending_aliases(&mut self) -> Vec<PreparedAliasRoute> {
        self.store.data_mut().take_pending_aliases()
    }

    /// Drain the inline-child aliases the just-returned guest call despawned
    /// (#4228). The trampoline retires each route through the registry owner
    /// and notifies its watchers, the teardown mirror of
    /// [`Self::drain_pending_aliases`].
    pub fn drain_pending_alias_retirements(&mut self) -> Vec<PreparedAliasRetirement> {
        self.store.data_mut().take_pending_alias_retirements()
    }

    /// Extract a failure recorded by `save_state` (size cap, OOB).
    /// `None` on clean saves and on components that didn't attempt a
    /// save. Checked by the control plane to decide whether to abort
    /// the replace (ADR-0016 §4).
    pub fn take_save_error(&mut self) -> Option<String> {
        self.store.data_mut().save_state_error.take()
    }

    /// Write the prior-state bytes into a delivery region (ADR-0095, via
    /// `place`) and invoke `on_rehydrate(version, ptr, len)`. The component
    /// trampoline calls it on a republish's candidate with the old guest's
    /// bundle, and on the old guest itself with that same bundle when the
    /// republish aborts, so the reinstated guest gets back what its
    /// `on_dehydrate` saved (ADR-0016 §4, issue 7125). Returns
    /// `Ok(())` if the instance doesn't export `on_rehydrate` (ADR-0016 §3: the
    /// bundle is silently discarded when no handler claims it).
    ///
    /// ADR-0016 §4 specifies that a trap here aborts the replace, so errors are
    /// propagated rather than contained (unlike `on_dehydrate` / `unwire`). A
    /// region that can't be allocated, or a bundle past the deliverable ceiling,
    /// propagates as an `Err` too.
    pub fn call_on_rehydrate(&mut self, bundle: &StateBundle) -> wasmtime::Result<()> {
        let Some(f) = self.on_rehydrate.clone() else {
            return Ok(());
        };
        let len = bundle.bytes.len();
        // Wasm32 ABI carries `u32` byte lengths; bundle bytes are
        // bounded by guest memory size (well below `u32::MAX`).
        #[allow(clippy::cast_possible_truncation)]
        let byte_len = len as u32;
        let ptr = match Self::place(
            &mut self.store,
            self.realloc.as_ref(),
            self.small_ptr,
            &mut self.large_ptr,
            &mut self.large_cap,
            len,
        )? {
            Placement::At(ptr) => ptr,
            Placement::Oversize => {
                return Err(wasmtime::Error::msg(format!(
                    "rehydrate state of {len} bytes exceeds the {MAX_DELIVERABLE_MAIL_BYTES}-byte deliverable bound"
                )));
            }
            Placement::NoAllocator => {
                return Err(wasmtime::Error::msg("cannot rehydrate state: guest exports no realloc_p32 allocator"));
            }
        };
        if !bundle.bytes.is_empty() {
            self.memory.write(&mut self.store, ptr as usize, &bundle.bytes)?;
        }
        f.call(&mut self.store, (bundle.version, ptr, byte_len))?;
        Ok(())
    }

    /// Read a `u32` from guest linear memory at `offset`. Test-only
    /// accessor: the production mail path writes into an allocator
    /// region and the guest interprets the bytes — nothing in non-test
    /// code reads guest memory directly.
    ///
    /// # Panics
    /// Panics if the memory read fails — fail-fast per ADR-0063:
    /// tests construct the offset/length pair directly, so an
    /// out-of-bounds read is a test bug.
    #[cfg(test)]
    pub fn read_u32(&mut self, offset: usize) -> u32 {
        let mut buf = [0u8; 4];
        self.memory.read(&mut self.store, offset, &mut buf).expect("test memory read");
        u32::from_le_bytes(buf)
    }

    /// Read `len` bytes from guest linear memory starting at `offset`.
    /// Test-only accessor for verifying that a rehydrate hook copied
    /// bytes to a known marker offset.
    ///
    /// # Panics
    /// Panics if the memory read fails — fail-fast per ADR-0063:
    /// tests construct the offset/length pair directly, so an
    /// out-of-bounds read is a test bug.
    #[cfg(test)]
    pub fn read_bytes(&mut self, offset: usize, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        self.memory.read(&mut self.store, offset, &mut buf).expect("test memory read");
        buf
    }
}
