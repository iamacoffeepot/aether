//! The registry's answer to a resolution candidate: [`Registry::proven`]
//! and the liveness read it is built on, [`Registry::is_live`].
//!
//! The one caller of the gated mint outside the SDK itself. The caller
//! derived the candidate position through `R`'s `Resolve` strategy; this
//! module answers only whether the published route view holds a `Live`
//! endpoint there, and mints the reference on `Live` alone.

use aether_actor::{__mint_actor_ref, ActorRef};

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

    /// Mint a reference for `candidate` when the published route view holds
    /// a `Live` endpoint there, and `None` for `Starting`, `Dropped`, and
    /// `Unknown` alike: all three mean "no reference".
    ///
    /// [`Self::is_live`] plus the mint.
    pub fn proven<R>(&self, candidate: MailboxId) -> Option<ActorRef<R>> {
        self.is_live(candidate).then(|| __mint_actor_ref(candidate))
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
        assert!(registry.proven::<()>(live).is_some());
        assert!(registry.proven::<()>(dropped).is_none());
    }
}
