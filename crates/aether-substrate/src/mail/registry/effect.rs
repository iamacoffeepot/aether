use std::error::Error;
use std::fmt;
use std::process::abort;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aether_actor::RegistryChanged;
use aether_data::Kind;
use aether_data::{KindDescriptor, MailboxDescriptor, SchemaType};

use super::mailbox::MailboxEntry;
use crate::mail::Mail;
use crate::mail::mailer::Mailer;
use crate::mail::view::View;
use crate::mail::{KindId, MailboxId};
use crate::scheduler::SeizeHandle;

/// Opaque identity for one accepted `Starting` reservation. Tokens are
/// registry-local and monotonically allocated; callers may compare and return
/// them but cannot manufacture one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivationToken(u64);

impl ActivationToken {
    pub(super) fn next(counter: &mut u64) -> Self {
        *counter = counter.checked_add(1).unwrap_or_else(|| {
            tracing::error!("activation token sequence exhausted; registry cannot remain coherent");
            abort();
        });
        Self(*counter)
    }
}

/// Route identity prepared away from the registry writer. It deliberately
/// names no storage representation.
pub struct PreparedRoute {
    pub id: MailboxId,
    pub canonical_name: String,
}

impl PreparedRoute {
    pub fn named(canonical_name: String) -> Self {
        Self { id: MailboxId::from_name(&canonical_name), canonical_name }
    }

    pub fn with_id(id: MailboxId, canonical_name: String) -> Self {
        Self { id, canonical_name }
    }
}

/// Opaque activation prepared beside a route. The temporary legacy adapter
/// carries only an endpoint; actor state and scheduler/storage coordinates do
/// not enter the owner effect contract.
pub struct PreparedActivation {
    legacy: LegacyEndpoint,
}

struct LegacyEndpoint(MailboxEntry);

impl PreparedActivation {
    pub(super) fn legacy(entry: MailboxEntry) -> Self {
        Self { legacy: LegacyEndpoint(entry) }
    }

    pub(super) fn into_legacy(self) -> MailboxEntry {
        self.legacy.0
    }
}

pub enum RegistryEffect {
    ReserveStarting { route: PreparedRoute },
    CancelStarting { id: MailboxId, token: ActivationToken },
    PublishLive { route: PreparedRoute, activation: PreparedActivation },
    DropMailbox(MailboxId),
    RemoveMailbox(MailboxId),
    InstallSeize { id: MailboxId, handle: SeizeHandle },
    RegisterKind { descriptor: KindDescriptor, reject_conflict: bool },
}

impl RegistryEffect {
    pub(super) fn reserve_named(canonical_name: String) -> Self {
        Self::ReserveStarting { route: PreparedRoute::named(canonical_name) }
    }

    pub(super) fn reserve_with_id(id: MailboxId, canonical_name: String) -> Self {
        Self::ReserveStarting { route: PreparedRoute::with_id(id, canonical_name) }
    }

    pub(super) fn publish_named(canonical_name: String, entry: MailboxEntry) -> Self {
        Self::PublishLive { route: PreparedRoute::named(canonical_name), activation: PreparedActivation::legacy(entry) }
    }

    pub(super) fn publish_with_id(id: MailboxId, canonical_name: String, entry: MailboxEntry) -> Self {
        Self::PublishLive {
            route: PreparedRoute::with_id(id, canonical_name),
            activation: PreparedActivation::legacy(entry),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartingCancellation {
    Cancelled(MailboxId),
    TokenMismatch(MailboxId),
    NotStarting(MailboxId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryApplied {
    Starting { id: MailboxId, token: ActivationToken },
    StartingCancellation(StartingCancellation),
    Mailbox(MailboxId),
    Dropped(String),
    Removed(bool),
    SeizeInstalled(bool),
    Kind(KindId),
}

#[derive(Debug)]
pub enum RegistryEffectError {
    Name(super::NameConflict),
    Drop(super::DropError),
    Kind(super::KindConflict),
    OwnerClosed,
}

impl fmt::Display for RegistryEffectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(error) => error.fmt(formatter),
            Self::Drop(error) => error.fmt(formatter),
            Self::Kind(error) => error.fmt(formatter),
            Self::OwnerClosed => formatter.write_str("registry owner closed before applying the effect batch"),
        }
    }
}

impl Error for RegistryEffectError {}

pub struct EffectBatch {
    pub(super) effects: Vec<RegistryEffect>,
}

impl EffectBatch {
    pub fn new(effects: Vec<RegistryEffect>) -> Self {
        Self { effects }
    }
}

pub struct RegistryCompletion<T> {
    receiver: crossbeam_channel::Receiver<Result<T, RegistryEffectError>>,
}

impl<T> RegistryCompletion<T> {
    pub(super) fn new(receiver: crossbeam_channel::Receiver<Result<T, RegistryEffectError>>) -> Self {
        Self { receiver }
    }

    pub fn wait_timeout(
        self,
        timeout: Duration,
    ) -> Result<Result<T, RegistryEffectError>, crossbeam_channel::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

#[derive(Clone)]
pub struct RegistryInventory {
    pub mailboxes: Vec<MailboxDescriptor>,
    pub kinds: Vec<KindDescriptor>,
    pub mailbox_generation: u64,
    pub kind_generation: u64,
}

pub(super) struct ChangeSubscriber {
    pending: AtomicBool,
    target: MailboxId,
    mailer: Arc<Mailer>,
}

impl ChangeSubscriber {
    fn notification(&self) -> Option<Mail> {
        (!self.pending.swap(true, Ordering::AcqRel))
            .then(|| Mail::new(self.target, RegistryChanged::ID, RegistryChanged.encode_into_bytes(), 1))
    }

    pub(super) fn notify(&self) {
        if let Some(mail) = self.notification() {
            self.mailer.push(mail);
        }
    }

    pub(super) fn notify_via_relay(&self) {
        if let Some(mail) = self.notification() {
            self.mailer.relay_mail(mail);
        }
    }
}

#[must_use = "dropping the subscription stops RegistryChanged delivery"]
pub struct RegistrySubscription {
    subscriber: Arc<ChangeSubscriber>,
    inventory: View<RegistryInventory>,
}

impl RegistrySubscription {
    pub(super) fn new(subscriber: Arc<ChangeSubscriber>, inventory: View<RegistryInventory>) -> Self {
        Self { subscriber, inventory }
    }

    /// Acknowledge the generation just consumed. Clearing `pending` before
    /// re-reading both views closes the publish-vs-clear race: a publication
    /// in the gap either sends its own wake or is observed and re-armed here.
    pub fn acknowledge(&self, mailbox_generation: u64, kind_generation: u64) {
        self.subscriber.pending.store(false, Ordering::Release);
        let inventory = self.inventory.load();
        if inventory.table().mailbox_generation != mailbox_generation
            || inventory.table().kind_generation != kind_generation
        {
            self.subscriber.notify();
        }
    }
}

pub(super) fn subscriber(
    target: MailboxId,
    mailer: Arc<Mailer>,
    inventory: View<RegistryInventory>,
) -> (Arc<ChangeSubscriber>, RegistrySubscription) {
    let subscriber = Arc::new(ChangeSubscriber { pending: AtomicBool::new(false), target, mailer });
    let subscription = RegistrySubscription::new(Arc::clone(&subscriber), inventory);
    (subscriber, subscription)
}

pub(super) fn bytes_kind(name: String) -> KindDescriptor {
    KindDescriptor { name, schema: SchemaType::Bytes }
}
