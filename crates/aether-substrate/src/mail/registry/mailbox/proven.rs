//! The registry's liveness reads, [`Registry::is_live`] over a reference and
//! the crate-private position form beside it, and the five mints beside
//! them.
//!
//! The callers of the gated mint outside the SDK itself. Three mint with no
//! read: the `Registry::declared_dependency` caller proved the dependency
//! `Live` at the dependent's birth, the `Registry::structural_erased` caller
//! mints a host-supplied position — the stamped dispatch source — and every
//! `Registry::activated` caller has just published the actor's own `Live`
//! route, whether the birth was a staged child, an embedder spawn, or a
//! chassis-composed capability. The fourth, `Registry::resolve_live`, is the
//! only one that answers the liveness question itself, because the position
//! it is handed arrived in a payload and nothing upstream proved it. The
//! fifth, `Registry::loaded`, types a reference the caller already holds.

use core::fmt;
use std::error::Error;

use aether_actor::{__mint_actor_ref, __mint_erased_actor_ref, ActorRef, ErasedActorRef};
use aether_data::MailboxCategory;

use crate::mail::registry::names::categorise_mailbox_name;
use crate::mail::{KindId, MailboxId};

use super::resolve::{ResolvedRoute, resolve_route};
use super::{CapturedDisposition, Registry};

/// Why an embedder could not type a load reply's sender as the loaded actor
/// (`PassiveChassis::adopt_load`). Carries no position: the embedder already
/// holds the erased reference it asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptRefused {
    /// The sender's route is not `Live` now: the component was dropped
    /// before the embedder adopted its reply.
    NotLive,
    /// The sender is live but is not a loaded component's trampoline, so the
    /// reply did not come from a load.
    NotComponent,
}

impl fmt::Display for AdoptRefused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLive => formatter.write_str("the load reply's sender is no longer live"),
            Self::NotComponent => formatter.write_str("the load reply's sender is not a loaded component"),
        }
    }
}

impl Error for AdoptRefused {}

/// Why a position that arrived in a payload could not be proven
/// (ADR-0230 section 3).
///
/// The two arms are the distinction a refusing capability reports: a
/// `Dropped` route names an actor that existed and has since retired, while
/// an `Unknown` one names an id nothing ever registered — a fold the caller
/// computed, or a typo. A cap that collapsed them would tell a component
/// that unloaded cleanly the same thing it tells a caller who guessed an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveLiveError {
    /// A route was published under this id and has since been dropped.
    Dropped(MailboxId),
    /// No live route stands under this id: it was never registered, or its
    /// birth is still `Starting` and so not yet provable.
    Unknown(MailboxId),
}

impl fmt::Display for ResolveLiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dropped(id) => write!(formatter, "mailbox {id:?} already dropped"),
            Self::Unknown(id) => write!(formatter, "unknown mailbox id {id:?}"),
        }
    }
}

impl Registry {
    /// Whether the actor `target` proves is still `Live` in the published
    /// route view. A reference proves only that its actor reached `Live`
    /// (ADR-0230), which stays true after it departs; this answers the other
    /// question, "is it `Live` now".
    ///
    /// The http server's request reader is the consumer: it holds route
    /// members as references and skips one whose actor has departed but
    /// whose `MonitorNotice` has not yet purged it.
    pub fn is_live(&self, target: ErasedActorRef) -> bool {
        self.is_live_at(target.id())
    }

    /// Whether the published route view holds a `Live` endpoint at
    /// `candidate` — `Starting`, `Dropped`, and `Unknown` alike mean
    /// "not live".
    ///
    /// Reads the lock-free published snapshot through the hot-path
    /// `route_lookup`, exactly what the mailer's route step reads — no
    /// mail, no allocation, no lock the send path does not take.
    pub(crate) fn is_live_at(&self, candidate: MailboxId) -> bool {
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
    /// dependency still reached `Live`, and [`Self::is_live_at`] would answer
    /// `false` for it.
    pub(crate) fn declared_dependency<R>(position: MailboxId) -> ActorRef<R> {
        __mint_actor_ref(position)
    }

    /// Mint a reference for an actor whose `Live` route the caller has just
    /// published at `position`, with no registry read (ADR-0230 section 3's
    /// spawned-or-loaded door).
    ///
    /// The one claim every caller discharges is that it has itself just made
    /// this actor `Live`, so a read would repeat what the caller just decided —
    /// exactly as for [`Self::declared_dependency`]. The callers:
    ///
    /// - the native spawn finalizer's `promote`, which runs inside the catch-up
    ///   suffix the registry owner calls only after it has published a staged
    ///   child's `Live` route;
    /// - the eager [`SpawnBuilder::finish`](crate::SpawnBuilder::finish) terminals,
    ///   whose commit returns only once the route is `Live` — written directly
    ///   before the ADR-0165 seal, promoted by the owner after it;
    /// - the chassis boot of a composed capability, which records the reference
    ///   once the boot claim published the route and `init` and `wire` succeeded;
    /// - both pumped-actor boots, after the route was published `Live`.
    ///
    /// A refused birth never reaches it: it completes with its
    /// [`SpawnError`](crate::actor::native::SpawnError) or [`BootError`](crate::BootError)
    /// and mints nothing.
    pub(crate) fn activated<A>(position: MailboxId) -> ActorRef<A> {
        __mint_actor_ref(position)
    }

    /// Mint an erased reference for the host-stamped dispatch `position`,
    /// with no registry read (ADR-0230).
    ///
    /// Its first caller,
    /// [`NativeCtx::sender`](crate::actor::native::NativeCtx::sender),
    /// discharges the obligation the stamped dispatch source already
    /// answers: the host stamped this position at dispatch, so the answer is
    /// already known.
    ///
    /// Its second is the `host_turn` self-mail test in
    /// `crate::actor::native::slot::pumped`, which names the position it
    /// booted the probe at: a host turn has no sender, so
    /// `NativeCtx::sender` cannot serve.
    ///
    /// Its third is the session arm of `Mailer::send_reply` (and the wasm
    /// guest's `reply_mail_p32` session arm), which stamps the replying
    /// actor's own position on the egressed reply event: the reply was
    /// minted in that actor's id space, so the answer is already known.
    pub(crate) fn structural_erased(position: MailboxId) -> ErasedActorRef {
        __mint_erased_actor_ref(position)
    }

    /// Type the stamped sender of a load reply as the loaded actor `R`
    /// (ADR-0230 section 3's spawned-or-loaded door, for an embedder).
    ///
    /// A successful load reply is sent by the loaded actor itself, so the
    /// embedder already holds its erased reference from the reply event's
    /// stamped sender; this narrows it after checking the claim the typing
    /// adds: the sender's route is `Live` and is a component trampoline, the
    /// only actor the component host hands a load to. `R` is the export the
    /// embedder named in its load, which the host instantiated.
    ///
    /// Its one caller is
    /// [`PassiveChassis::adopt_load`](crate::chassis::builder::PassiveChassis::adopt_load),
    /// through the spawner that holds the chassis registry.
    pub(crate) fn loaded<R>(&self, sender: ErasedActorRef) -> Result<ActorRef<R>, AdoptRefused> {
        let position = sender.id();
        if !self.is_live_at(position) {
            return Err(AdoptRefused::NotLive);
        }
        let category = self.mailbox_name(position).as_deref().and_then(categorise_mailbox_name);
        if category != Some(MailboxCategory::Trampoline) {
            return Err(AdoptRefused::NotComponent);
        }
        Ok(__mint_actor_ref(position))
    }

    /// Prove a `position` that arrived in a payload (ADR-0230 section 3's
    /// payload-borne-id door), or say why it cannot be proven.
    ///
    /// The module's first mint that performs a read. The other three
    /// discharge an obligation something upstream already answered — a
    /// refused birth, a host-stamped dispatch source, a published child
    /// route — whereas a position carried in a kind
    /// field is a position and nothing more, so the only authority on whether
    /// it is occupied is this view. The read is the same published-route walk
    /// [`Self::entry_at`] takes, so a proof and a dispatch agree by construction
    /// rather than by two lookups kept in step.
    ///
    /// `Starting` reads as [`ResolveLiveError::Unknown`]: ADR-0230 section 1
    /// keeps `Starting` internal to the registry, so it is never a state a
    /// reference may be issued against — a reservation whose `init` fails is
    /// removed, and a reference minted against it would have outlived its
    /// claim.
    ///
    /// An inline-child alias mints, because `resolve_route`'s alias arm
    /// resolves to the host's `Live` endpoint: the alias names a real actor
    /// that reached `Live`, which is exactly what the reference claims. That
    /// is ADR-0230's closing consequence — the proof is about the actor the
    /// position reaches, not about the shape of the route record.
    ///
    /// Its one caller is
    /// [`NativeCtx::resolve_live`](crate::actor::native::NativeCtx::resolve_live),
    /// the single public spelling a capability uses.
    pub(crate) fn resolve_live(&self, position: MailboxId) -> Result<ErasedActorRef, ResolveLiveError> {
        let routes = self.routes.load();
        match resolve_route(position, |candidate| routes.entry_for(&candidate)) {
            ResolvedRoute::Live { .. } => Ok(__mint_erased_actor_ref(position)),
            ResolvedRoute::Dropped => Err(ResolveLiveError::Dropped(position)),
            ResolvedRoute::Starting { .. } | ResolvedRoute::Unknown => Err(ResolveLiveError::Unknown(position)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::mail::registry::{MailDispatch, noop_handler};
    use crate::testing::boot_authority;

    use super::*;

    #[test]
    fn is_live_is_true_only_for_live_routes() {
        let registry = Registry::new();
        let authority = boot_authority();
        let live = registry.register_inbox(&authority, "test.proven.live", noop_handler());
        let dropped = registry.register_inbox(&authority, "test.proven.dropped", noop_handler());
        assert!(registry.drop_mailbox(&authority, dropped).is_ok());

        assert!(registry.is_live_at(live));
        assert!(!registry.is_live_at(dropped), "a Dropped route is not live");
        assert!(!registry.is_live_at(MailboxId(0xdead_beef)), "an unknown id is not live");
    }

    // Tripwire: the accept set `resolve_live` mints over includes inline
    // routes, and its refusal splits dropped from unknown. The window cap's
    // subscriber accept set is inline and inline-alias routes as much as it
    // is inboxes, so a `resolve_live` rewritten over `entry() == Inbox { .. }`
    // or over the actor-registry slot check would silently stop every inline
    // subscriber from subscribing; and both refusal texts every cap reports
    // are read off the two arms this pins apart.
    #[test]
    fn resolve_live_proves_an_inline_route_and_splits_its_two_refusals() {
        let registry = Registry::new();
        let authority = boot_authority();
        let inline = registry.register_inline(&authority, "test.proven.inline", Arc::new(|_: MailDispatch<'_>| {}));

        assert!(registry.resolve_live(inline).is_ok(), "an inline-handler mailbox is provable");

        assert!(registry.drop_mailbox(&authority, inline).is_ok());
        assert_eq!(registry.resolve_live(inline), Err(ResolveLiveError::Dropped(inline)));
        assert_eq!(
            registry.resolve_live(MailboxId(0xdead_beef)),
            Err(ResolveLiveError::Unknown(MailboxId(0xdead_beef))),
        );
    }

    // A typed adoption over an arbitrary live actor would hand an embedder
    // an `ActorRef<R>` for something that never loaded as `R`; the refusal
    // is what keeps `adopt_load` a load door rather than a generic mint.
    #[test]
    fn loaded_refuses_a_live_actor_that_is_not_a_component() {
        let registry = Registry::new();
        let authority = boot_authority();
        let cap = registry.register_inbox(&authority, "aether.test.not-a-component", noop_handler());
        // A trampoline's id is its lineage fold; any id serves this test, which
        // reads only the canonical name the route carries.
        let trampoline = registry
            .try_register_inbox_with_id(
                &authority,
                MailboxId(0x7A11_0001),
                "aether.component/aether.embedded:probe",
                noop_handler(),
            )
            .expect("register a trampoline-named route");

        assert_eq!(registry.loaded::<()>(Registry::structural_erased(cap)), Err(AdoptRefused::NotComponent));
        assert!(registry.loaded::<()>(Registry::structural_erased(trampoline)).is_ok());
    }
}
