//! The registry's liveness reads, [`Registry::is_live`] over a reference and
//! the crate-private position form beside it, and the six mints beside
//! them.
//!
//! The callers of the gated mint outside the SDK itself. Two mint with no
//! read, and both are typed: the `Registry::declared_dependency` caller
//! proved the dependency `Live` at the dependent's birth, and every
//! `Registry::activated` caller has just published the actor's own `Live`
//! route, whether the birth was a staged child, an embedder spawn, or a
//! chassis-composed capability. The third, `Registry::stamped_sender`, mints
//! a host-stamped position — a dispatch source, or the sender half of a
//! reply's mail id — only once one published-route read finds a record
//! standing there, so every erased reference names a route. The fourth,
//! `Registry::resolve_live`, is the only one that answers the liveness
//! question itself, because the position it is handed arrived in a payload
//! and nothing upstream proved it. The fifth, `Registry::loaded`, types a
//! reference the caller already holds, and the sixth, `Registry::live_child`,
//! answers the same liveness question for a child address folded beneath a
//! parent the caller already proved.

use core::fmt;
use std::error::Error;

use aether_actor::{__mint_actor_ref, __mint_erased_actor_ref, ActorRef, Addressable, ErasedActorRef, Resolve};
use aether_data::{Address, AddressForm, LoadName, MailboxCategory};

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

/// Why a child address beneath a proven parent could not be proven
/// (`PassiveChassis::child`). Names the child by its key and actor namespace,
/// never by a position: the embedder built the address from a reference and a
/// key, and those are what it can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRefused {
    /// The child's `NAMESPACE`.
    pub namespace: &'static str,
    /// The instance key the address carried, when it carried one.
    pub key: Option<LoadName>,
}

impl fmt::Display for ChildRefused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.key {
            Some(key) => {
                write!(formatter, "no live {} child keyed {:?} beneath the parent", self.namespace, key.as_str())
            }
            None => write!(formatter, "no live {} child beneath the parent", self.namespace),
        }
    }
}

impl Error for ChildRefused {}

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

    /// Mint an erased reference for the host-stamped `position` when a route
    /// record stands there, and `None` when none does (ADR-0230).
    ///
    /// The position is a stamp the host wrote — the dispatch source, or the
    /// sender half of the mail id a replier minted in its own id space — but
    /// `Mail`, `Source`, and the mailer's push and reply entries are public,
    /// so a stamp alone does not show the position was ever registered. The
    /// one read settles it: a reference minted here names a record
    /// [`Self::actor_path`] reads through the same view, and no path removes a
    /// record that has emitted mail, so its path answers for the session.
    ///
    /// Every lifecycle counts. A `Dropped` or retired-alias record is the
    /// departed actor a [`MonitorNotice`](aether_kinds::MonitorNotice) is
    /// stamped with, and a `Starting` record is a post-seal pumped actor whose
    /// `wire` mail leaves before its route is promoted. The chassis sentinel
    /// answers `None` with no special case, because no publish arm ever
    /// records a route at it.
    ///
    /// The cost is one lock-free load of the published route view and one
    /// hash probe — the read `route_lookup` already takes on every send.
    ///
    /// Its callers are
    /// [`NativeCtx::sender`](crate::actor::native::NativeCtx::sender), for
    /// both a component source and a reply's replier, and the session arms
    /// of `Mailer::send_reply` and the wasm guest's `reply_mail_p32`, which
    /// stamp the replying actor on an egressed reply event.
    pub(crate) fn stamped_sender(&self, position: MailboxId) -> Option<ErasedActorRef> {
        self.routes.load().entry_for(&position).map(|_| __mint_erased_actor_ref(position))
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
    /// The module's one mint that answers the liveness question. The typed
    /// mints discharge an obligation something upstream already answered — a
    /// refused birth, a published route — and [`Self::stamped_sender`] asks
    /// only whether any record stands at a host stamp, whereas a position
    /// carried in a kind field is a position and nothing more, so the only
    /// authority on whether it is occupied is this view. The read is the same published-route walk
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
    /// Its callers are
    /// [`NativeCtx::resolve_live`](crate::actor::native::NativeCtx::resolve_live),
    /// the single public spelling a capability uses, and the test-support
    /// fixtures `testing::registered_ref` and `testing::registered_binding`,
    /// which prove the inbox they just registered.
    pub(crate) fn resolve_live(&self, position: MailboxId) -> Result<ErasedActorRef, ResolveLiveError> {
        let routes = self.routes.load();
        match resolve_route(position, |candidate| routes.entry_for(&candidate)) {
            ResolvedRoute::Live { .. } => Ok(__mint_erased_actor_ref(position)),
            ResolvedRoute::Dropped => Err(ResolveLiveError::Dropped(position)),
            ResolvedRoute::Starting { .. } | ResolvedRoute::Unknown => Err(ResolveLiveError::Unknown(position)),
        }
    }

    /// Prove the child `address` names beneath a parent the caller already
    /// proved (ADR-0230 section 3's `Address<R>` door, for an embedder).
    ///
    /// The address is folded with `C`'s resolver beneath the parent's
    /// position, and the folded position is proven with the same
    /// published-route walk [`Self::resolve_live`] takes: only a `Live` route
    /// mints. `Starting`, `Dropped`, and never-registered positions refuse
    /// alike, and so does an address that is not a child address, since only
    /// a parent the caller holds a proof of anchors the fold.
    ///
    /// Its one caller is
    /// [`PassiveChassis::child`](crate::chassis::builder::PassiveChassis::child),
    /// through the spawner that holds the chassis registry; it builds the
    /// address with `aether_actor::child_address` from the parent's reference.
    pub(crate) fn live_child<C: Addressable>(&self, address: &Address<C>) -> Result<ActorRef<C>, ChildRefused> {
        let AddressForm::Beneath { parent, key } = address.form() else {
            return Err(ChildRefused { namespace: C::NAMESPACE, key: None });
        };
        let refused = || ChildRefused { namespace: C::NAMESPACE, key: key.clone() };
        let position = <C::Resolver as Resolve>::candidate(parent.0, C::NAMESPACE, key.as_ref().map(LoadName::as_str))
            .ok_or_else(refused)?;

        let routes = self.routes.load();
        match resolve_route(position, |candidate| routes.entry_for(&candidate)) {
            ResolvedRoute::Live { .. } => Ok(__mint_actor_ref(position)),
            ResolvedRoute::Dropped | ResolvedRoute::Starting { .. } | ResolvedRoute::Unknown => Err(refused()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use aether_actor::Many;

    use crate::config::RegistryQueueCapacities;
    use crate::mail::mailer::Mailer;
    use crate::mail::registry::effect::{EffectBatch, RegistryApplied, RegistryEffect};
    use crate::mail::registry::owner::RegistryOwnerLease;
    use crate::mail::registry::{MailDispatch, noop_handler};
    use crate::scheduler::WakeSink;
    use crate::testing::boot_authority;

    use super::*;

    struct ProbeChild;

    impl Addressable for ProbeChild {
        const NAMESPACE: &'static str = "test.proven.child";
        type Resolver = Many;
    }

    fn child_address(parent: MailboxId, key: &str) -> Address<ProbeChild> {
        Address::beneath(parent, Some(LoadName::new(key).expect("a valid key")))
    }

    // A child lookup that answered for any folded position would hand an
    // embedder a reference to a child that never reached `Live`, or to a
    // sibling under a different key. It proves only the `Live` route at the
    // exact fold beneath the parent, and refuses a `Starting` birth.
    #[test]
    fn live_child_proves_only_a_live_route_at_the_keyed_fold() {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let authority = boot_authority();
        let owner = RegistryOwnerLease::attach(
            boot_authority(),
            &registry,
            &mailer,
            WakeSink::detached(),
            RegistryQueueCapacities::default(),
        );
        let parent = registry.register_inbox(&authority, "test.proven.parent", noop_handler());
        let live = Many::resolve(parent.0, ProbeChild::NAMESPACE, "live");
        registry
            .try_register_inbox_with_id(&authority, live, "test.proven.parent/test.proven.child:live", noop_handler())
            .expect("register the live child");
        let starting = Many::resolve(parent.0, ProbeChild::NAMESPACE, "starting");
        let completion = registry
            .submit(EffectBatch::new(vec![RegistryEffect::reserve_with_id(
                starting,
                "test.proven.parent/test.proven.child:starting".to_owned(),
            )]))
            .expect("owner accepts the Starting reservation");
        owner.run_once();
        let reserved = completion.wait_timeout(Duration::from_millis(100)).expect("reservation completes");
        assert!(matches!(reserved.as_deref(), Ok([RegistryApplied::Starting { .. }])));

        assert!(registry.live_child(&child_address(parent, "live")).is_ok());
        let refused = registry.live_child(&child_address(parent, "starting")).expect_err("Starting is not provable");
        assert_eq!(refused.key.as_ref().map(LoadName::as_str), Some("starting"));
        assert_eq!(refused.namespace, ProbeChild::NAMESPACE);
        assert!(registry.live_child(&child_address(parent, "other")).is_err(), "a wrong key names no child");
        assert!(
            registry.live_child(&Address::<ProbeChild>::exact(live)).is_err(),
            "only an address beneath a parent is a child address",
        );
    }

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

        let cap = registry.resolve_live(cap).expect("the capability route is live");
        let trampoline = registry.resolve_live(trampoline).expect("the trampoline route is live");

        assert_eq!(registry.loaded::<ProbeChild>(cap), Err(AdoptRefused::NotComponent));
        assert!(registry.loaded::<ProbeChild>(trampoline).is_ok());
    }
}
