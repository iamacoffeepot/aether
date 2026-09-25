use aether_actor::DISPATCH_HANDLED_RELEASE;
use aether_actor::wasm::NO_INBOUND_SOURCE;

use crate::actor::native::envelope::Envelope;
use crate::actor::wasm::reply_table::{NO_REPLY_HANDLE, ReplyEntry};
use crate::mail::SourceAddr;

use super::instantiate::Placement;
use super::{Component, MAX_DELIVERABLE_MAIL_BYTES, SMALL_REGION_BYTES};

/// Sentinel the ADR-0033 `#[actor]` dispatcher returns from
/// `receive_p32` when mail arrives with a kind id the component has
/// no typed handler for and no fallback. Substrate-side, the
/// scheduler turns this into a `tracing::warn!` so the unhandled
/// kind surfaces in `engine_logs` without aborting the run. Strict-
/// receiver enforcement at the substrate (pre-delivery rejection)
/// is deferred to a later ADR; Phase 2 is warnings only.
pub const DISPATCH_UNKNOWN_KIND: u32 = 1;

/// Sentinel [`Component::deliver`] returns when it refused to deliver an
/// inbound — its payload exceeded the deliverable ceiling, or the guest exports
/// no allocator (ADR-0095). The mail was dropped (logged) without touching
/// guest memory or invoking `receive`; the caller treats it as a non-error so
/// the native dispatcher still discharges settlement.
pub const DISPATCH_DROPPED_OVERSIZE: u32 = 2;

impl Component {
    pub fn wire(&mut self) -> wasmtime::Result<()> {
        let Some(wire_fn) = self.wire.take() else {
            return Ok(());
        };
        let mailbox_id = self.self_mailbox_id;
        let rc = wire_fn.call(&mut self.store, mailbox_id)?;
        if rc != 0 {
            return Err(wasmtime::Error::msg(format!("guest wire returned non-zero rc {rc}")));
        }
        Ok(())
    }

    /// ADR-0163 §3: close the asset load window on this component's store
    /// ctx. The trampoline calls this once the guest's `wire` has returned,
    /// so post-window `asset_fetch_p32` traps while the catalog metadata
    /// stays queryable through `asset_catalog_p32` for the instance's life.
    pub fn close_load_window(&mut self) {
        self.store.data_mut().close_load_window();
    }

    /// Resolve the inbound mail's source `MailboxId` for the trailing
    /// `receive_p32` frame slot (issue 2001). A peer-component origin
    /// (`SourceAddr::Component`) yields that mailbox's raw id; every other
    /// origin — session, remote engine, or no reply target — yields
    /// `NO_INBOUND_SOURCE` (0). Mirrors what `source_of_p32` resolved from
    /// the reply table, but reads the inbound's `SourceAddr` directly (the
    /// same value the reply entry is built from) without a table lookup.
    fn resolve_inbound_source(addr: &SourceAddr) -> u64 {
        match addr {
            SourceAddr::Component(m) => m.0,
            _ => NO_INBOUND_SOURCE,
        }
    }

    /// Deliver a routed envelope into the component's linear memory and
    /// invoke `receive`. Returns the guest's return value (contract is
    /// currently informational; host-visible errors propagate as
    /// `wasmtime::Error`). `deliver` borrows the envelope and never
    /// discharges it: its ADR-0094 settlement stays with the caller's
    /// dispatcher.
    ///
    /// ADR-0013 + ADR-0017: once the payload has a place in guest memory,
    /// a fresh sender handle is allocated from the per-instance
    /// `ReplyTable` for every inbound that has a meaningful reply target —
    /// a Claude session (non-NIL `SessionToken`), a remote engine mailbox,
    /// or a peer component (`sender.addr = SourceAddr::Component(_)`
    /// populated by `ComponentCtx::send` / `NativeBinding::send_mail`).
    /// Broadcast-origin and system-generated mail pass `NO_REPLY_HANDLE`
    /// so the guest's `mail.reply_handle()` accessor returns `None`. A
    /// dropped delivery allocates nothing.
    ///
    /// The handle is freed when the guest answers it, or right after
    /// `receive` returns when the guest reports a single-class arm
    /// (`DISPATCH_HANDLED_RELEASE`, ADR-0112) or an unhandled kind
    /// (`DISPATCH_UNKNOWN_KIND`) — neither can answer later (#6412). A
    /// manual handler or `#[fallback]` returns `DISPATCH_HANDLED` and its
    /// handle stays held until answered. A table holding every
    /// addressable handle fails the delivery, which the trampoline turns
    /// into an ADR-0063 fail-fast.
    pub fn deliver(&mut self, env: &Envelope) -> wasmtime::Result<u32> {
        // ADR-0095: choose where in guest memory the payload lands via the
        // guest allocator — a fitting payload into the cached small region, a
        // larger one into the grown large region, anything past the ceiling or
        // to a no-allocator guest dropped loudly. A drop returns `Ok` without
        // invoking `receive`, so the trampoline's `forward_to_wasm` returns
        // normally and the native dispatcher discharges the inbound's
        // settlement bracket — no corruption, no trap, no hung caller.
        let payload_len = env.payload.len();
        // Wasm32 ABI carries `u32` byte lengths; only used in branches where
        // `payload_len <= MAX_DELIVERABLE_MAIL_BYTES`, so the cast can't lose data.
        #[allow(clippy::cast_possible_truncation)]
        let byte_len = payload_len as u32;
        let mail_ptr = match Self::place(
            &mut self.store,
            self.realloc.as_ref(),
            self.small_ptr,
            &mut self.large_ptr,
            &mut self.large_cap,
            payload_len,
        )? {
            Placement::At(ptr) => ptr,
            Placement::Oversize => {
                self.log_dropped_oversize(env, payload_len, "exceeds the absolute mail-size bound");
                return Ok(DISPATCH_DROPPED_OVERSIZE);
            }
            Placement::NoAllocator => {
                self.log_dropped_oversize(env, payload_len, "guest exports no realloc_p32 allocator (raw-FFI guest)");
                return Ok(DISPATCH_DROPPED_OVERSIZE);
            }
        };
        let handle = self.allocate_reply_handle(env)?;

        self.memory.write(&mut self.store, mail_ptr as usize, env.payload.bytes())?;
        // ADR-0080 §5 (issue iamacoffeepot/aether#722): publish the
        // inbound's lineage on `ComponentCtx` so any guest-triggered
        // `send_mail_p32` / `reply_mail_p32` host fn — both routed
        // through `ComponentCtx::send` — can stamp the outgoing mail
        // with `parent_mail = Some(inbound.mail_id)` and inherit the
        // chain `root`. Cleared after the call so a future cap-side
        // call site that bypasses `deliver` (today: only test
        // fixtures) doesn't accidentally pick up stale lineage.
        self.store.data().set_in_flight(env.mail_id, env.root);
        self.store.data().set_reply_correlation(env.sender);
        // ADR-0114 decision #1: thread the routed recipient through to
        // the guest as a `receive_p32` frame slot so a guest handler (and
        // the inline-child membrane) can read which address the mail was
        // sent to. For a normally-addressed actor this equals the actor's
        // own mailbox id.
        //
        // Issue 2001: thread the resolved inbound source as the trailing
        // slot too, so the guest's `WasmCtx::sender` is a single
        // ctx-field read on both the in-place and top-level paths and the
        // `source_of_p32` host round-trip can be retired. Resolved exactly
        // as `source_of_p32` did — a peer-component origin yields its
        // `MailboxId`, every other origin yields `NO_INBOUND_SOURCE` (0).
        let source = Self::resolve_inbound_source(&env.sender.addr);
        let result = self
            .receive
            .call(&mut self.store, (env.kind.0, mail_ptr, byte_len, env.count, handle, env.recipient.0, source));
        self.store.data().clear_in_flight();
        if let Ok(rc) = result
            && (rc == DISPATCH_HANDLED_RELEASE || rc == DISPATCH_UNKNOWN_KIND)
        {
            self.store.data_mut().reply_table.take(handle);
        }
        result
    }

    /// Allocate the reply handle for an inbound about to reach `receive`, or
    /// `NO_REPLY_HANDLE` when it has no reply target. Warns, naming the
    /// actor, each time the table's live count passes a new high-water mark,
    /// and errors when every addressable handle is held.
    fn allocate_reply_handle(&mut self, env: &Envelope) -> wasmtime::Result<u32> {
        // ADR-0042: carry the incoming correlation through to the
        // ReplyEntry so a subsequent `reply_mail` echoes it on the
        // outgoing reply. Session / engine mail that didn't originate
        // a correlation carries 0 — fine, echo of 0 is a no-op.
        let correlation = env.sender.correlation_id;
        let entry = match &env.sender.addr {
            SourceAddr::Session(token) => ReplyEntry::new(SourceAddr::Session(*token), correlation),
            SourceAddr::EngineMailbox { engine_id, mailbox_id } => ReplyEntry::new(
                SourceAddr::EngineMailbox { engine_id: *engine_id, mailbox_id: *mailbox_id },
                correlation,
            ),
            SourceAddr::Component(m) => ReplyEntry::new(SourceAddr::Component(*m), correlation),
            SourceAddr::None => return Ok(NO_REPLY_HANDLE),
        };

        let ctx = self.store.data_mut();
        let Some(handle) = ctx.reply_table.allocate(entry) else {
            return Err(wasmtime::Error::msg(format!(
                "component {} holds 1,048,576 unanswered reply handles, the most its reply table addresses",
                ctx.actor_name(),
            )));
        };
        if let Some(live) = ctx.reply_table.high_water() {
            tracing::warn!(
                target: "aether_substrate::component",
                actor = %ctx.actor_name(),
                live,
                "reply table grew past its preallocated slots; a request this component keeps and never answers holds its slot",
            );
        }
        Ok(handle)
    }

    /// Loudly log an inbound mail dropped by `deliver` because its payload
    /// could not be delivered safely (iamacoffeepot/aether#1337). The mail is
    /// dropped, not written; the caller settles via the native dispatcher.
    fn log_dropped_oversize(&self, env: &Envelope, payload_len: usize, reason: &str) {
        let kind_name = self.store.data().registry.kind_name(env.kind).unwrap_or_default();
        tracing::error!(
            target: "aether_substrate::component",
            kind = %kind_name,
            kind_id = env.kind.0,
            payload_bytes = payload_len,
            small_region_bytes = SMALL_REGION_BYTES,
            deliverable_cap_bytes = MAX_DELIVERABLE_MAIL_BYTES,
            reason,
            "dropping inbound mail; cannot deliver safely (see ADR-0095)",
        );
    }
}
