//! End-to-end chassis: boot the journal owner, component host, and driver, and talk mail to them.
//!
//! This module is declared only by the end-to-end targets (`programs.rs` and
//! `reactors.rs`), so every item here is used by both and the dead-code gate
//! stays green. Items a single target needs live in that target's file instead.

use std::error::Error;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use aether_actor::ActorRef;
use aether_bloomery_driver::{BundleDriver, DriverParams};
use aether_bloomery_journal::{Entry, Journal, JournalActor, Seq};
use aether_bloomery_kinds::ClosureLimit;
use aether_component::{ComponentHostCapability, ComponentHostParams};
use aether_data::{Kind, MailId, MailboxId, Source, SourceAddr};
use aether_kinds::trace::Nanos;
use aether_substrate::PassiveChassis;
use aether_substrate::mail::MailRef;
use aether_substrate::mail::registry::{MailboxEntry, OwnedDispatch, Registry};
use aether_substrate::testing::{TestChassis, boot_authority, boot_test_chassis_with};
use aether_substrate::{Subname, SubstrateBoot};

/// Boot the component host, then spawn the journal owner and the driver over it.
#[must_use]
pub fn boot_driver(
    journal: &Path,
) -> (PassiveChassis<TestChassis>, Arc<Registry>, ActorRef<JournalActor>, ActorRef<BundleDriver>) {
    let boot = SubstrateBoot::build().expect("substrate boot");
    let chassis = boot_test_chassis_with::<ComponentHostCapability>(
        &boot.registry,
        &boot.queue,
        (),
        ComponentHostParams {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        },
    );
    let journal_id = chassis
        .spawn_actor::<JournalActor>(Subname::Named("journal"), journal.to_path_buf(), ())
        .finish()
        .expect("journal birth");
    let driver = chassis
        .spawn_actor::<BundleDriver>(
            Subname::Named("driver"),
            ClosureLimit::new(ClosureLimit::MAX_BYTES).expect("the ceiling is a valid limit"),
            DriverParams { journal: journal_id },
        )
        .finish()
        .expect("driver birth");
    (chassis, Arc::clone(&boot.registry), journal_id, driver)
}

/// Register a reply inbox; every reply it receives is forwarded to the receiver.
pub fn caller(registry: &Registry, name: &str) -> (MailboxId, mpsc::Receiver<OwnedDispatch>) {
    let (tx, rx) = mpsc::channel();
    let mailbox = registry.register_inbox(
        &boot_authority(),
        name,
        Arc::new(move |dispatch: OwnedDispatch| {
            dispatch.discharge();
            tx.send(dispatch).expect("capture reply");
        }),
    );
    (mailbox, rx)
}

/// Enqueue `mail` on `target` as if `caller` sent it with `correlation`.
pub fn request<R, K: Kind>(registry: &Registry, target: ActorRef<R>, caller: MailboxId, correlation: u64, mail: &K) {
    let MailboxEntry::Inbox { handler, .. } = registry.entry(target.erase()).expect("actor mailbox registered") else {
        panic!("actor mailbox is not an inbox");
    };
    handler.enqueue(OwnedDispatch::disarmed(
        K::ID,
        None,
        Source::with_correlation(SourceAddr::Component(caller), correlation),
        MailRef::from(mail.encode_into_bytes()),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));
}

/// Wait up to thirty seconds for the correlated reply of kind `K`; the first wait
/// covers the fixture's wasm compile on the load path.
pub fn reply<K: Kind>(rx: &mpsc::Receiver<OwnedDispatch>, correlation: u64) -> K {
    let dispatch = rx.recv_timeout(Duration::from_secs(30)).expect("reply within thirty seconds");
    assert_eq!(dispatch.kind, K::ID);
    assert_eq!(dispatch.sender.correlation_id, correlation);
    K::decode_from_bytes(dispatch.payload.bytes()).expect("decode reply")
}

/// Read the whole journal back through a fresh handle.
pub fn read_all(path: &Path) -> Result<Vec<Entry>, Box<dyn Error>> {
    Ok(Journal::open(path)?.read(Seq(0), 128)?)
}

/// Read the journal head through a fresh handle.
pub fn journal_head(path: &Path) -> Result<Seq, Box<dyn Error>> {
    Ok(Journal::open(path)?.head()?)
}
