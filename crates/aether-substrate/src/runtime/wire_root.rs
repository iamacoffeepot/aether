//! ADR-0244: the held root every birth's `wire` runs under.
//!
//! A `wire` hook dispatches no inbound, so it has no in-flight root, and each
//! of its sends used to mint one of its own that nobody learned. A chassis
//! boot, an embedder's `spawn_actor`, and a handler-staged birth each open a
//! [`WireRoot`] instead: the one fresh root those sends inherit. A
//! handler-staged birth's causing chain stays beside it and keeps the holds
//! and birth-completing effects (ADR-0168), so the wire root gathers only the
//! actor's own startup sends. It is minted from the
//! chassis-root counter without a `Sent`, and it carries the settlement hold
//! that keeps it open while `wire` runs and its mail is still held: a root
//! with no sends settles when its hold is released, so a `wire` that sends
//! nothing still settles, and one that sends settles only once everything it
//! caused has been handled.

#[cfg(any(test, feature = "test-support"))]
use crossbeam_channel::Receiver;

use crate::mail::MailId;
use crate::mail::mailer::Mailer;
use crate::runtime::trace::SettlementHold;

/// A fresh root and the settlement hold on it. Dropping it releases the hold;
/// the root settles once that drop and every mail counted against it have
/// both happened.
#[must_use = "a WireRoot's hold is what keeps its root open; dropping it at once releases the root"]
pub struct WireRoot {
    hold: SettlementHold,
}

impl WireRoot {
    /// Mint a fresh root from `mailer`'s chassis-root counter and take the
    /// hold on it before anything can count against it.
    pub fn open(mailer: &Mailer) -> Self {
        Self { hold: mailer.acquire_settlement_hold(mailer.mint_wire_root()) }
    }

    /// The root the `wire` sends inherit.
    pub fn root(&self) -> MailId {
        self.hold.root()
    }

    /// Subscribe to this root's settlement. The subscription lands while
    /// this value still holds the root open, so the root cannot have settled
    /// and been evicted from the registry's settled set before it, and the
    /// receiver cannot miss the settle.
    ///
    /// Test-support only, like the waits that consume it: no production
    /// caller awaits a birth's `wire` (ADR-0244 §7).
    ///
    /// # Panics
    /// Panics if the chassis boot did not install its settlement registry on
    /// `mailer` — every built chassis does, before any birth runs.
    #[cfg(any(test, feature = "test-support"))]
    pub fn subscribe(&self, mailer: &Mailer) -> Receiver<()> {
        mailer
            .settlement_registry()
            .expect("the chassis boot installs its settlement registry on the mailer")
            .subscribe_settlement(self.root())
    }
}
