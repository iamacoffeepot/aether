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
use crate::mail::{CostCell, CostTable, KindId, MailId, MailboxId, SourceAddr};
use crate::scheduler::SeizeHandle;

/// Opaque identity for one accepted `Starting` reservation. Tokens are
/// registry-local and monotonically allocated; callers may compare and return
/// them but cannot manufacture one.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ActivationToken(u64);

impl ActivationToken {
    pub(super) fn next(counter: &mut u64) -> Self {
        *counter = counter.checked_add(1).unwrap_or_else(|| {
            tracing::error!("activation token sequence exhausted; registry cannot remain coherent");
            abort();
        });
        Self(*counter)
    }

    pub(crate) fn value(self) -> u64 {
        self.0
    }

    pub(crate) fn from_value(value: u64) -> Option<Self> {
        (value != 0).then_some(Self(value))
    }
}

/// Runtime-private control kind ordering activation-home egress before
/// owner-side promotion. Ordinary kind ids are tagged hashes, leaving this
/// sentinel outside actor vocabulary.
pub const ACTIVATION_BARRIER_KIND: KindId = KindId(u64::MAX);

pub fn barrier_token(mail: &Mail) -> Option<ActivationToken> {
    if mail.kind != ACTIVATION_BARRIER_KIND
        || mail.payload.bytes().len() != size_of::<u64>()
        || mail.mail_id.sender != mail.recipient
        || !matches!(mail.reply_to.addr, SourceAddr::Component(sender) if sender == mail.recipient)
    {
        return None;
    }
    let mut bytes = [0; size_of::<u64>()];
    bytes.copy_from_slice(mail.payload.bytes());
    ActivationToken::from_value(u64::from_le_bytes(bytes))
}

/// Route identity prepared away from the registry writer. It deliberately
/// names no storage representation.
pub struct PreparedRoute {
    pub id: MailboxId,
    pub canonical_name: String,
}

/// Exact actor-local cost cells carried into the fused owner commit.
pub struct PreparedCostCells {
    table: Arc<CostTable>,
    cells: Vec<(KindId, Arc<CostCell>)>,
}

impl PreparedCostCells {
    pub(crate) fn new(table: Arc<CostTable>, cells: Vec<(KindId, Arc<CostCell>)>) -> Self {
        Self { table, cells }
    }

    pub(super) fn prepare(&self, mailbox: MailboxId, token: ActivationToken) -> bool {
        self.table.prepare(mailbox, token, &self.cells)
    }

    pub(super) fn promote(&self, mailbox: MailboxId, token: ActivationToken) {
        self.table.promote(mailbox, token, &self.cells);
    }

    pub(super) fn rollback(&self, mailbox: MailboxId, token: ActivationToken) {
        self.table.rollback(mailbox, token, &self.cells);
    }
}

/// Storage-erased initialized actor awaiting an owner-assigned token.
pub trait PreparedSpawnActivation: Send {
    fn reserve(
        self: Box<Self>,
        token: ActivationToken,
    ) -> Result<Arc<dyn ActivationReservation>, Box<dyn PreparedSpawnActivation>>;

    /// Schedule destruction of initialized actor state at its execution
    /// home. Dropping the returned receiver is the nonblocking rejection
    /// path; shutdown retains it until the home-side drop completes.
    fn discard_at_home(self: Box<Self>) -> crossbeam_channel::Receiver<()>;
}

/// Owner-retained handle for one activation running at its execution home.
pub trait ActivationReservation: Send + Sync {
    fn schedule(&self);
    fn take_live(&self) -> Option<Box<dyn LiveActivation>>;
    fn cancel(&self);
    fn join(&self);
    fn barrier_matches(&self, mail_id: MailId) -> bool;

    fn cancel_and_join(&self) {
        self.cancel();
        self.join();
    }
}

/// Wired actor lease. Installing it invokes no actor-authored lifecycle code.
pub trait LiveActivation: Send {
    fn install(self: Box<Self>, bootstrap: Vec<PreparedMail>, parked: Vec<PreparedMail>) -> InstalledActivation;
    fn cancel_at_home(self: Box<Self>) -> crossbeam_channel::Receiver<()>;
}

pub struct InstalledActivation {
    pub(crate) entry: MailboxEntry,
    pub(crate) catch_up: Box<dyn FnOnce() + Send>,
}

pub struct PreparedMail {
    pub(crate) mail: Mail,
    pub(crate) kind_name: String,
    pub(crate) bootstrap: bool,
}

impl PreparedMail {
    pub(crate) fn bootstrap(mail: Mail, kind_name: String) -> Self {
        Self { mail, kind_name, bootstrap: true }
    }

    pub(super) fn parked(mail: Mail, kind_name: String) -> Self {
        Self { mail, kind_name, bootstrap: false }
    }
}

/// Private move-only birth committed by the registry owner.
pub struct PreparedSpawnCommit {
    pub(crate) route: PreparedRoute,
    activation: PreparedActivationGuard,
    pub(crate) costs: PreparedCostCells,
    pub(crate) after_init: Vec<PreparedMail>,
}

struct PreparedActivationGuard(Option<Box<dyn PreparedSpawnActivation>>);

impl PreparedActivationGuard {
    fn take(&mut self) -> Box<dyn PreparedSpawnActivation> {
        self.0.take().expect("prepared spawn activation consumed once")
    }

    fn discard(mut self) -> crossbeam_channel::Receiver<()> {
        self.take().discard_at_home()
    }
}

impl Drop for PreparedActivationGuard {
    fn drop(&mut self) {
        if let Some(activation) = self.0.take() {
            drop(activation.discard_at_home());
        }
    }
}

impl PreparedSpawnCommit {
    pub(crate) fn new(
        route: PreparedRoute,
        activation: Box<dyn PreparedSpawnActivation>,
        costs: PreparedCostCells,
        after_init: Vec<PreparedMail>,
    ) -> Self {
        Self { route, activation: PreparedActivationGuard(Some(activation)), costs, after_init }
    }

    pub fn take_activation(&mut self) -> Box<dyn PreparedSpawnActivation> {
        self.activation.take()
    }

    pub fn discard_at_home(self) -> crossbeam_channel::Receiver<()> {
        self.activation.discard()
    }
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
    PreparedSpawn(PreparedSpawnCommit),
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
    ActivationRejected,
    OwnerClosed,
}

impl fmt::Display for RegistryEffectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(error) => error.fmt(formatter),
            Self::Drop(error) => error.fmt(formatter),
            Self::Kind(error) => error.fmt(formatter),
            Self::ActivationRejected => {
                formatter.write_str("prepared actor activation could not reserve its lifecycle")
            }
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

    pub(super) fn discard_prepared(self) -> Vec<crossbeam_channel::Receiver<()>> {
        self.effects
            .into_iter()
            .filter_map(|effect| match effect {
                RegistryEffect::PreparedSpawn(commit) => Some(commit.discard_at_home()),
                _ => None,
            })
            .collect()
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
