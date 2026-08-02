//! Fixtures shared across the registry test siblings: the activation
//! doubles a prepared birth stands on, the inventory subscriber, and the
//! traced envelopes the settlement assertions are written against.

use std::panic;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::chassis::settlement::SettlementRegistry;
use crate::mail::cost::CostCell;
use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::{
    ACTIVATION_BARRIER_KIND, ActivationReservation, ActivationToken, InstalledActivation, LiveActivation,
    PreparedCostCells, PreparedMail, PreparedRoute, PreparedSpawnActivation, PreparedSpawnCommit, PreparedSpawnFailure,
    RegistryApplied, RegistryEffect,
};
use crate::mail::registry::{MailboxEntry, OwnedDispatch, Registry};
use crate::mail::{KindId, Mail, MailId, MailboxId, Source, SourceAddr};
use crate::testing::boot_authority as auth;

pub(super) struct InventorySubscriber;

impl aether_actor::Addressable for InventorySubscriber {
    const NAMESPACE: &'static str = "test.inventory-subscriber";
    type Resolver = aether_actor::One;
}

impl aether_actor::HandlesKind<aether_actor::RegistryChanged> for InventorySubscriber {}

pub(super) fn inventory_subscription_fixture()
-> (Arc<Registry>, Arc<Mailer>, crossbeam_channel::Receiver<KindId>, MailboxId) {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let (sender, receiver) = crossbeam_channel::unbounded();
    let target = registry.register_inbox(
        &auth(),
        "inventory-subscriber",
        Arc::new(move |dispatch: OwnedDispatch| {
            sender.send(dispatch.kind).expect("inventory test receiver stays connected");
            dispatch.discharge();
        }),
    );
    (registry, mailer, receiver, target)
}

pub(super) fn starting_token(result: &[RegistryApplied]) -> ActivationToken {
    let [RegistryApplied::Starting { token, .. }] = result else {
        panic!("expected one Starting result, got {result:?}")
    };
    *token
}

struct FakePreparedActivation {
    deliveries: Arc<Mutex<Vec<u8>>>,
    scheduled: Arc<AtomicUsize>,
    registry: Arc<Registry>,
    expected_starting: Vec<MailboxId>,
    barrier_mail_id: MailId,
    cancelled: Arc<AtomicUsize>,
}

impl PreparedSpawnActivation for FakePreparedActivation {
    fn reserve(
        self: Box<Self>,
        _token: ActivationToken,
    ) -> Result<Arc<dyn ActivationReservation>, (Box<dyn PreparedSpawnActivation>, PreparedSpawnFailure)> {
        Ok(Arc::new(FakeActivationReservation {
            live: Mutex::new(Some(Box::new(FakeLiveActivation { deliveries: self.deliveries }))),
            scheduled: self.scheduled,
            registry: self.registry,
            expected_starting: self.expected_starting,
            barrier_mail_id: self.barrier_mail_id,
            cancelled: self.cancelled,
        }))
    }

    fn discard_at_home(self: Box<Self>, _failure: PreparedSpawnFailure) -> crossbeam_channel::Receiver<()> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        drop(self);
        let _ = tx.send(());
        rx
    }
}

struct FakeActivationReservation {
    live: Mutex<Option<Box<dyn LiveActivation>>>,
    scheduled: Arc<AtomicUsize>,
    registry: Arc<Registry>,
    expected_starting: Vec<MailboxId>,
    barrier_mail_id: MailId,
    cancelled: Arc<AtomicUsize>,
}

impl ActivationReservation for FakeActivationReservation {
    fn schedule(&self) {
        assert!(
            self.expected_starting.iter().all(|id| self.registry.route_lookup(KindId(0), *id).is_starting()),
            "every accepted Starting route is published before the first activation schedule"
        );
        self.scheduled.fetch_add(1, Ordering::SeqCst);
    }

    fn take_live(&self) -> Option<Box<dyn LiveActivation>> {
        self.live.lock().unwrap().take()
    }

    fn cancel(&self) {
        self.cancelled.fetch_add(1, Ordering::SeqCst);
    }

    fn join(&self) {}

    fn barrier_matches(&self, mail_id: MailId) -> bool {
        self.barrier_mail_id == mail_id
    }
}

struct FakeLiveActivation {
    deliveries: Arc<Mutex<Vec<u8>>>,
}

impl LiveActivation for FakeLiveActivation {
    fn install(self: Box<Self>, bootstrap: Vec<PreparedMail>, parked: Vec<PreparedMail>) -> InstalledActivation {
        for prepared in bootstrap.into_iter().chain(parked) {
            self.deliveries.lock().unwrap().push(prepared.mail.payload.bytes()[0]);
        }
        let deliveries = Arc::clone(&self.deliveries);
        InstalledActivation {
            entry: MailboxEntry::Inbox {
                handler: Arc::new(move |dispatch: OwnedDispatch| {
                    dispatch.discharge();
                    deliveries.lock().unwrap().push(dispatch.payload.bytes()[0]);
                }),
                seize: Arc::default(),
            },
            catch_up: Box::new(|| {}),
        }
    }

    fn cancel_at_home(self: Box<Self>) -> crossbeam_channel::Receiver<()> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = tx.send(());
        rx
    }
}

pub(super) fn prepared_test_spawn(
    registry: &Arc<Registry>,
    mailer: &Arc<Mailer>,
    name: &str,
    deliveries: Arc<Mutex<Vec<u8>>>,
    scheduled: Arc<AtomicUsize>,
    expected_starting: Vec<MailboxId>,
    bootstrap: u8,
) -> (MailboxId, Arc<CostCell>, Arc<AtomicUsize>, RegistryEffect) {
    let id = MailboxId::from_name(name);
    let cell = Arc::new(CostCell::new());
    let cancelled = Arc::new(AtomicUsize::new(0));
    let effect = RegistryEffect::PreparedSpawn(PreparedSpawnCommit::new(
        PreparedRoute::with_id(id, name.to_owned()),
        Box::new(FakePreparedActivation {
            deliveries,
            scheduled,
            registry: Arc::clone(registry),
            expected_starting,
            barrier_mail_id: MailId::new(id, 1),
            cancelled: Arc::clone(&cancelled),
        }),
        PreparedCostCells::new(Arc::clone(mailer.cost_table()), vec![(KindId(7), Arc::clone(&cell))]),
        vec![PreparedMail::bootstrap(Mail::new(id, KindId(7), vec![bootstrap], 1))],
    ));
    (id, cell, cancelled, effect)
}

pub(super) fn activation_barrier(id: MailboxId, token: ActivationToken, sequence: u64) -> Mail {
    let mail_id = MailId::new(id, sequence);
    Mail::new(id, ACTIVATION_BARRIER_KIND, token.value().to_le_bytes().to_vec(), 1)
        .with_reply_to(Source::with_correlation(SourceAddr::Component(id), sequence))
        .with_lineage(mail_id, mail_id, None)
}

pub(super) fn traced_unknown_mail(
    mailer: &Mailer,
    settlement: &SettlementRegistry,
    recipient: MailboxId,
    sequence: u64,
    payload: Vec<u8>,
) -> (Mail, crossbeam_channel::Receiver<()>) {
    let root = MailId::new(MailboxId(0x4111), sequence);
    let settled = settlement.subscribe_settlement(root);
    mailer.record_sent(root, root, None, root.sender, recipient, KindId(0x4111));
    (Mail::new(recipient, KindId(0x4111), payload, 1).with_lineage(root, root, None), settled)
}
