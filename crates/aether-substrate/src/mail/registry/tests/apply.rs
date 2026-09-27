//! Tests for [`super::super::mailbox::apply`] — the staged fold every
//! writer funnels through, including the direct pre-seal write path.

use std::any::Any;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aether_data::ReplyContract;

use crate::config::RegistryQueueCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::{EffectBatch, RegistryEffect, RegistryEffectError};
use crate::mail::registry::owner::RegistryOwnerLease;
use crate::mail::registry::{ContractBreak, MailboxEntry, Registry, RouteContract, noop_handler};
use crate::mail::{KindId, MailboxId};
use crate::scheduler::{BatchBudget, CycleResult, Drainable, SeizeHandle, SlotState, WakeSink};
use crate::testing::boot_authority as auth;

use super::support::contract;

const PING: KindId = KindId(1);
const QUERY: KindId = KindId(2);
const PROBE: KindId = KindId(3);
const REPORT: KindId = KindId(10);
const OTHER_REPORT: KindId = KindId(11);

#[test]
#[allow(clippy::disallowed_methods, reason = "the test deliberately races the two writer entry points")]
fn direct_and_owner_paths_share_the_transitional_writer() {
    use std::sync::Barrier;

    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let completion = registry
        .submit(EffectBatch::new(vec![RegistryEffect::publish_named(
            "shared-writer".to_owned(),
            MailboxEntry::Inbox { handler: noop_handler(), seize: Arc::default() },
        )]))
        .expect("owner accepts effect");
    let barrier = Arc::new(Barrier::new(2));
    let direct_registry = Arc::clone(&registry);
    let direct_barrier = Arc::clone(&barrier);
    let direct = thread::spawn(move || {
        direct_barrier.wait();
        direct_registry.try_register_inbox(&auth(), "shared-writer", noop_handler())
    });
    barrier.wait();
    owner.run_once();

    let owner_result = completion.wait_timeout(Duration::from_millis(100)).expect("owner completes");
    let direct_result = direct.join().expect("direct writer does not panic");
    assert_ne!(owner_result.is_ok(), direct_result.is_ok(), "exactly one serialized writer claims the route");
    assert_eq!(registry.list_mailbox_descriptors().iter().filter(|entry| entry.name == "shared-writer").count(), 1);
}

fn base() -> RouteContract {
    contract(&[(PING, ReplyContract::None), (QUERY, ReplyContract::One(REPORT))], false)
}

/// A live closure route publishing [`base`].
fn published_route(registry: &Registry, name: &str) -> MailboxId {
    let id = registry.try_register_inbox(&auth(), name, noop_handler()).expect("the route name is free");
    registry.publish_contract(&auth(), id, base()).expect("an empty contract takes any rows");
    id
}

fn refusal(result: Result<(), RegistryEffectError>) -> Option<ContractBreak> {
    match result {
        Err(RegistryEffectError::ContractBroken { contract_break, .. }) => Some(contract_break),
        Ok(()) => None,
        Err(error) => panic!("expected a contract break, got {error}"),
    }
}

/// ADR-0231 §5 at the registry: a republish that keeps every published row
/// replaces the contract. Catches a guard that compares contracts for
/// equality, which would refuse every added row and every `Manual` row a
/// replacement declares.
#[test]
fn republish_accepts_a_contract_that_keeps_the_published_rows() {
    let registry = Registry::new();
    let id = published_route(&registry, "test.contract.keeps");

    let extended = contract(
        &[(PING, ReplyContract::None), (QUERY, ReplyContract::One(REPORT)), (PROBE, ReplyContract::Manual)],
        true,
    );
    registry.publish_contract(&auth(), id, extended.clone()).expect("added rows and a fallback are kept");
    assert_eq!(registry.published_contract(id), Some(extended));

    let declared = contract(
        &[(PING, ReplyContract::None), (QUERY, ReplyContract::One(REPORT)), (PROBE, ReplyContract::One(REPORT))],
        true,
    );
    registry.publish_contract(&auth(), id, declared.clone()).expect("a manual row may declare its reply");
    assert_eq!(registry.published_contract(id), Some(declared));
}

/// ADR-0231 §5 at the registry: a republish that drops a row, changes a
/// reply, or drops the fallback is refused and the published contract stays.
/// Catches a guard that lets a republish shrink the published contract, or
/// one that compares rows and forgets the fallback.
#[test]
fn republish_refuses_a_contract_that_breaks_the_published_one() {
    let registry = Registry::new();
    let id = published_route(&registry, "test.contract.breaks");

    let dropped = contract(&[(PING, ReplyContract::None)], false);
    assert_eq!(refusal(registry.publish_contract(&auth(), id, dropped)), Some(ContractBreak::Row(QUERY)));
    let changed = contract(&[(PING, ReplyContract::None), (QUERY, ReplyContract::One(OTHER_REPORT))], false);
    assert_eq!(refusal(registry.publish_contract(&auth(), id, changed)), Some(ContractBreak::Row(QUERY)));
    assert_eq!(registry.published_contract(id), Some(base()), "a refused republish leaves the rows published");

    let with_fallback = contract(&[(PING, ReplyContract::None), (QUERY, ReplyContract::One(REPORT))], true);
    registry.publish_contract(&auth(), id, with_fallback.clone()).expect("an added fallback is kept");
    assert_eq!(refusal(registry.publish_contract(&auth(), id, base())), Some(ContractBreak::Fallback));
    assert_eq!(registry.published_contract(id), Some(with_fallback));
}

/// A republish needs a published contract to replace. Catches a republish
/// that resurrects a retired route's rows, or one that writes rows onto an
/// id with no route.
#[test]
fn republish_refuses_a_route_that_is_not_published() {
    let registry = Registry::new();
    let id = published_route(&registry, "test.contract.retired");
    registry.drop_mailbox(&auth(), id).expect("the live route retires");

    assert!(matches!(
        registry.publish_contract(&auth(), id, base()),
        Err(RegistryEffectError::ContractUnpublished(refused)) if refused == id
    ));
    let absent = MailboxId(0xC0_4747);
    assert!(matches!(
        registry.publish_contract(&auth(), absent, base()),
        Err(RegistryEffectError::ContractUnpublished(refused)) if refused == absent
    ));
    assert_eq!(registry.published_contract(id), None, "a retired route publishes no contract");
}

/// `InstallSeize` rebuilds the `Live` lifecycle around a populated seize
/// cell. Catches that rebuild resetting the route's published rows.
#[test]
fn installing_a_seize_handle_keeps_the_published_contract() {
    struct IdleSlot;

    impl Drainable for IdleSlot {
        fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
            CycleResult::Idle
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    let registry = Registry::new();
    let id = published_route(&registry, "test.contract.seized");
    let slot: Arc<dyn Drainable> = Arc::new(IdleSlot);

    assert!(registry.install_seize_handle(
        &auth(),
        id,
        SeizeHandle::new(Arc::new(SlotState::new()), Arc::downgrade(&slot))
    ));
    assert_eq!(registry.published_contract(id), Some(base()));
}
