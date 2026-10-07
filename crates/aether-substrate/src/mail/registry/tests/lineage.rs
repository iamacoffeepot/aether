//! Tests for [`super::super::mailbox::lineage`] — the birth serial a
//! route record carries, the order read from it, and the birth check that
//! keeps that read total (ADR-0248 §5).

use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use aether_data::ReplyContract;

use crate::config::RegistryQueueCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::{
    EffectBatch, PreparedActivation, PreparedAliasRoute, RegistryApplied, RegistryEffect, RegistryEffectError,
};
use crate::mail::registry::owner::RegistryOwnerLease;
use crate::mail::registry::{LineageOrder, MailboxEntry, Registry, RouteContract, noop_handler};
use crate::mail::{KindId, MailboxId};
use crate::scheduler::WakeSink;
use crate::testing::boot_authority as auth;
use crate::testing::canonical_id;

use super::resolve::contract;
use super::support::{activation_barrier, prepared_test_spawn, starting_token};

/// A registry whose owner the test steps by hand, so every effect is applied
/// by the owner's own fold in the order the test submits it.
struct Rig {
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    owner: RegistryOwnerLease,
}

impl Rig {
    fn new() -> Self {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let owner = RegistryOwnerLease::attach(
            auth(),
            &registry,
            &mailer,
            WakeSink::detached(),
            RegistryQueueCapacities::default(),
        );

        Self { registry, mailer, owner }
    }

    fn apply(&self, effect: RegistryEffect) -> Result<Vec<RegistryApplied>, RegistryEffectError> {
        let completion = self.registry.submit(EffectBatch::new(vec![effect])).expect("the owner accepts the batch");
        self.owner.run_once();

        completion.try_take().expect("one owner cycle applies the batch")
    }

    /// Reserve `name` as `Starting` at its lineage fold.
    fn reserve(&self, name: &str) -> Result<Vec<RegistryApplied>, RegistryEffectError> {
        self.apply(RegistryEffect::reserve_with_id(canonical_id(name), name.to_owned()))
    }

    /// Publish `name` as a `Live` inbox route at its lineage fold.
    fn publish(&self, name: &str) -> Result<Vec<RegistryApplied>, RegistryEffectError> {
        self.apply(RegistryEffect::publish_with_id(
            canonical_id(name),
            name.to_owned(),
            inbox(),
            RouteContract::empty(),
        ))
    }

    /// Publish `name` as an inline alias onto the live route `target`.
    fn alias(
        &self,
        name: &str,
        target: &str,
        contract: RouteContract,
    ) -> Result<Vec<RegistryApplied>, RegistryEffectError> {
        self.apply(RegistryEffect::PublishAlias(PreparedAliasRoute::new(
            canonical_id(name),
            name,
            canonical_id(target),
            contract,
        )))
    }

    /// Submit a prepared birth under `name`, as a handler's spawn stages one.
    fn spawn(&self, name: &str) -> Result<Vec<RegistryApplied>, RegistryEffectError> {
        let (id, _, _, birth) = prepared_test_spawn(
            &self.registry,
            &self.mailer,
            name,
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(AtomicUsize::new(0)),
            vec![canonical_id(name)],
            1,
        );
        assert_eq!(id, canonical_id(name));

        self.apply(birth)
    }

    /// The lineage order of the route standing at `name`, in any lifecycle.
    fn order(&self, name: &str) -> LineageOrder {
        let actor = self.registry.stamped_sender(canonical_id(name)).expect("a record stands at the name");

        self.registry.lineage_order(actor).expect("a record and its ancestors answer their order")
    }
}

fn inbox() -> MailboxEntry {
    MailboxEntry::Inbox { handler: noop_handler(), seize: Arc::default() }
}

fn promote(id: MailboxId, applied: &[RegistryApplied]) -> RegistryEffect {
    RegistryEffect::PromoteStarting {
        id,
        token: starting_token(applied),
        activation: PreparedActivation::legacy(inbox()),
        contract: RouteContract::empty(),
    }
}

fn parent_unknown(result: Result<Vec<RegistryApplied>, RegistryEffectError>, name: &str) -> bool {
    matches!(result, Err(RegistryEffectError::ParentUnknown { name: refused }) if refused == name)
}

/// Catches a stamp taken when the route goes `Live`, or a fresh serial drawn
/// in `PromoteStarting`: either would order siblings by how long each `init`
/// took, where the decision is the order the registry saw the requests.
#[test]
fn siblings_keep_reservation_order_when_promoted_in_the_other_order() {
    let rig = Rig::new();
    let (first, second) = ("test.order.parent/test.order.child:first", "test.order.parent/test.order.child:second");
    rig.publish("test.order.parent").expect("the parent publishes");
    let first_reserved = rig.reserve(first).expect("the first sibling reserves");
    let second_reserved = rig.reserve(second).expect("the second sibling reserves");

    rig.apply(promote(canonical_id(second), &second_reserved)).expect("the second sibling promotes first");
    rig.apply(promote(canonical_id(first), &first_reserved)).expect("the first sibling promotes second");

    assert!(rig.order(first) < rig.order(second));
}

/// Catches a comparison of leaf serials only, which would put a late child
/// after its parent's next sibling, and a prefix that sorts last, which would
/// put a parent over its own children.
#[test]
fn a_child_born_late_sorts_between_its_parent_and_the_next_sibling() {
    let rig = Rig::new();
    let (parent, sibling, child) = ("test.order.early", "test.order.late", "test.order.early/test.order.child:k");
    rig.publish(parent).expect("the parent publishes");
    rig.publish(sibling).expect("the parent's next sibling publishes");
    rig.publish(child).expect("the child publishes after both");

    assert!(rig.order(parent) < rig.order(child));
    assert!(rig.order(child) < rig.order(sibling));
}

/// Catches a rebuild site that draws a serial again: `promote_locked`, which
/// rebuilds a prepared birth's record as `Live`, and the same-alias branch of
/// `PublishAlias`, which rebuilds an alias with its grown contract. Either
/// would move an actor behind everything born while it was starting.
#[test]
fn a_record_rebuilt_in_place_keeps_its_order() {
    let rig = Rig::new();
    let (born, alias, later) = ("test.order.born", "test.order.born/test.order.inline:k", "test.order.later");
    let token = starting_token(&rig.spawn(born).expect("the prepared birth reserves"));
    rig.alias(alias, born, RouteContract::empty()).expect("the alias publishes onto its starting parent");
    let (born_before, alias_before) = (rig.order(born), rig.order(alias));
    rig.publish(later).expect("a later root publishes");

    rig.mailer.push(activation_barrier(canonical_id(born), token, 1));
    rig.owner.run_once();
    rig.alias(alias, born, contract(&[(KindId(7), ReplyContract::None)])).expect("the alias grows its contract");

    assert!(rig.registry.is_live_at(canonical_id(born)), "the barrier promoted the birth");
    assert_eq!(rig.order(born), born_before);
    assert_eq!(rig.order(alias), alias_before);
    assert!(rig.order(alias) < rig.order(later));
}

/// Catches a read that looks at `Live` routes only. The renderer reads the
/// order of every sender at the frame commit, so a draw from an actor that
/// closed before the commit would abort the engine.
#[test]
fn a_dropped_route_and_its_children_keep_their_order() {
    let rig = Rig::new();
    let (parent, child) = ("test.order.closing", "test.order.closing/test.order.child:k");
    rig.publish(parent).expect("the parent publishes");
    rig.publish(child).expect("the child publishes");
    let (parent_before, child_before) = (rig.order(parent), rig.order(child));

    rig.apply(RegistryEffect::DropMailbox(canonical_id(parent))).expect("the parent retires");

    assert!(!rig.registry.is_live_at(canonical_id(parent)));
    assert_eq!(rig.order(parent), parent_before);
    assert_eq!(rig.order(child), child_before);
}

/// Catches a birth arm that skips the parent check. A record beneath a
/// prefix with no record has no step to read for that prefix, and
/// `lineage_order` has no failure to report, so each of the four arms that
/// insert a record refuses the birth and accepts it once the parent stands.
#[test]
fn each_birth_arm_refuses_a_name_whose_parent_holds_no_record() {
    let rig = Rig::new();
    let (parent, host) = ("test.order.absent", "test.order.host");
    let name = |key: &str| format!("{parent}/test.order.child:{key}");
    let (reserved, published, aliased, spawned) =
        (name("reserved"), name("published"), name("aliased"), name("spawned"));
    rig.publish(host).expect("the alias target publishes");

    assert!(parent_unknown(rig.reserve(&reserved), &reserved));
    assert!(parent_unknown(rig.publish(&published), &published));
    assert!(parent_unknown(rig.alias(&aliased, host, RouteContract::empty()), &aliased));
    assert!(parent_unknown(rig.spawn(&spawned), &spawned));
    assert_eq!(rig.registry.len(), 1, "a refused birth leaves no record");

    rig.publish(parent).expect("the parent publishes");

    rig.reserve(&reserved).expect("the reservation is admitted beneath a standing parent");
    rig.publish(&published).expect("the publish is admitted beneath a standing parent");
    rig.alias(&aliased, host, RouteContract::empty()).expect("the alias is admitted beneath a standing parent");
    rig.spawn(&spawned).expect("the prepared birth is admitted beneath a standing parent");
    let born_in_order = [parent, &reserved, &published, &aliased, &spawned].map(|name| rig.order(name));
    assert!(born_in_order.is_sorted(), "each admitted birth drew the next serial");
}
