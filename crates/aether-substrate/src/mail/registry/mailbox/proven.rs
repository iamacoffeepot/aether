//! The registry's liveness read, [`Registry::is_live`], and the no-read
//! mints beside it.
//!
//! The callers of the gated mint outside the SDK itself. The
//! `Registry::declared_dependency` caller proved the dependency `Live` at the
//! dependent's birth, and mints with no read; the `Registry::structural_any`
//! caller mints a host-supplied position — the stamped dispatch source —
//! likewise with no read.

use aether_actor::{__mint_actor_ref, __mint_any_actor_ref, ActorRef, AnyActorRef};

use crate::mail::{KindId, MailboxId};

use super::{CapturedDisposition, Registry};

impl Registry {
    /// Whether the published route view holds a `Live` endpoint at
    /// `candidate` — `Starting`, `Dropped`, and `Unknown` alike mean
    /// "not live".
    ///
    /// Reads the lock-free published snapshot through the hot-path
    /// `route_lookup`, exactly what the mailer's route step reads — no
    /// mail, no allocation, no lock the send path does not take.
    pub fn is_live(&self, candidate: MailboxId) -> bool {
        // `route_lookup` ignores its kind on this path (the mailer's route
        // step passes the live kind); the zero kind carries that.
        matches!(self.route_lookup(KindId(0), candidate).into_captured(), CapturedDisposition::Live { .. })
    }

    /// Mint a reference for a declared dependency's `position`, with no
    /// registry read (ADR-0230).
    ///
    /// The one caller,
    /// [`NativeCtx::actor_ref`](crate::actor::native::NativeCtx::actor_ref),
    /// discharges the obligation `A: DependsOn<R>`: the dependent's birth was
    /// refused unless `R` was `Live`, checked before `init`, so the answer is
    /// already known. It performs no read because the claim an [`ActorRef`]
    /// carries is "reached `Live`", not "is `Live` now" — a `Dropped`
    /// dependency still reached `Live`, and [`Self::is_live`] would answer
    /// `false` for it.
    pub(crate) fn declared_dependency<R>(position: MailboxId) -> ActorRef<R> {
        __mint_actor_ref(position)
    }

    /// Mint an erased reference for the host-stamped dispatch `position`,
    /// with no registry read (ADR-0230).
    ///
    /// Its first caller,
    /// [`NativeCtx::sender`](crate::actor::native::NativeCtx::sender),
    /// discharges the obligation its `source_mailbox` already answers: the
    /// host stamped this position at dispatch, so the answer is already
    /// known.
    ///
    /// Its second is the `host_turn` self-mail test in
    /// `crate::actor::native::slot::pumped`, which names the position it
    /// booted the probe at: a host turn carries no inbound, so
    /// `source_mailbox` is `None` and `NativeCtx::sender` cannot serve.
    pub(crate) fn structural_any(position: MailboxId) -> AnyActorRef {
        __mint_any_actor_ref(position)
    }
}

#[cfg(test)]
mod tests {
    use crate::mail::registry::noop_handler;
    use crate::testing::boot_authority;

    use super::*;

    #[test]
    fn is_live_is_true_only_for_live_routes() {
        let registry = Registry::new();
        let authority = boot_authority();
        let live = registry.register_inbox(&authority, "test.proven.live", noop_handler());
        let dropped = registry.register_inbox(&authority, "test.proven.dropped", noop_handler());
        assert!(registry.drop_mailbox(&authority, dropped).is_ok());

        assert!(registry.is_live(live));
        assert!(!registry.is_live(dropped), "a Dropped route is not live");
        assert!(!registry.is_live(MailboxId(0xdead_beef)), "an unknown id is not live");
    }
}
