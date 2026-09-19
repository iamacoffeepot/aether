//! End-to-end: the driver loads the program fixture bundle by digest and records caused outcomes.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use aether_bloomery_driver::{BundleDriver, DriverParams};
use aether_bloomery_journal::{Batch, Entry, Journal, JournalActor, Seq};
use aether_bloomery_kinds::{
    Call, CallOutcome, CallRefusal, ClosureLimit, Digest, FaultReason, Head, NativeOrigin, OpaqueBytes, ProgramName,
    RecordedHead, RecordedHeadMove, Ref, RequestSource, Requested, Transition, Utf8Text, artifact_digest,
};
use aether_component::{ComponentHostCapability, ComponentHostParams};
use aether_data::{Kind, MailId, MailboxId, Source, SourceAddr};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_kinds::trace::Nanos;
use aether_substrate::PassiveChassis;
use aether_substrate::mail::MailRef;
use aether_substrate::mail::registry::{MailboxEntry, OwnedDispatch, Registry};
use aether_substrate::testing::{TestChassis, boot_authority, boot_test_chassis_with};
use aether_substrate::{Subname, SubstrateBoot};

/// Local mirror of the fixture's `test.program.summarize.input`: same kind
/// name, same shape, so it encodes to the same digest the guest expects.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.summarize.input")]
struct SummarizeInput {
    text: Ref<Utf8Text>,
}

/// Local mirror of the fixture's `test.program.refuse.input`.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.refuse.input")]
struct RefuseInput {
    marker: u32,
}

/// Digests the seed wrote: the bundle wasm and the two call inputs.
struct Seed {
    bundle: Digest,
    summarize: Digest,
    refuse: Digest,
}

/// Stage the fixture wasm under a `program` head plus both call inputs.
fn seed_program_journal(path: &Path, wasm: &[u8]) -> Result<Seed, Box<dyn Error>> {
    let mut journal = Journal::open(path)?;
    let mut batch = Batch::new();
    let bundle = batch.stage_bytes(wasm);
    let head: Head<OpaqueBytes> = Head::new("program");
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&head), bundle.digest()), None)?;
    let text = batch.stage_text("hello");
    let summarize = batch.stage_encoded(&SummarizeInput { text })?;
    let refuse = batch.stage_encoded(&RefuseInput { marker: 1 })?;
    journal.append(Seq(0), &batch)?;
    Ok(Seed { bundle: bundle.digest(), summarize: summarize.digest(), refuse: refuse.digest() })
}

/// Boot the component host, then spawn the journal owner and the driver over it.
fn boot_driver(journal: &Path) -> (PassiveChassis<TestChassis>, Arc<Registry>, MailboxId) {
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
    (chassis, Arc::clone(&boot.registry), driver)
}

/// Register a reply inbox; every reply it receives is forwarded to the receiver.
fn caller(registry: &Registry, name: &str) -> (MailboxId, mpsc::Receiver<OwnedDispatch>) {
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
fn request<K: Kind>(registry: &Registry, target: MailboxId, caller: MailboxId, correlation: u64, mail: &K) {
    let MailboxEntry::Inbox { handler, .. } = registry.entry(target).expect("actor mailbox registered") else {
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

/// Wait up to thirty seconds for the correlated `CallOutcome`; the first wait
/// covers the fixture's wasm compile on the load path.
fn reply(rx: &mpsc::Receiver<OwnedDispatch>, correlation: u64) -> CallOutcome {
    let dispatch = rx.recv_timeout(Duration::from_secs(30)).expect("reply within thirty seconds");
    assert_eq!(dispatch.kind, CallOutcome::ID);
    assert_eq!(dispatch.sender.correlation_id, correlation);
    CallOutcome::decode_from_bytes(dispatch.payload.bytes()).expect("decode reply")
}

/// Send one `Call` and wait for its outcome.
fn send_call(
    registry: &Registry,
    driver: MailboxId,
    inbox: MailboxId,
    rx: &mpsc::Receiver<OwnedDispatch>,
    correlation: u64,
    call: &Call,
) -> CallOutcome {
    request(registry, driver, inbox, correlation, call);
    reply(rx, correlation)
}

/// Read the whole journal back through a fresh handle.
fn read_all(path: &Path) -> Result<Vec<Entry>, Box<dyn Error>> {
    Ok(Journal::open(path)?.read(Seq(0), 128)?)
}

/// Read the journal head through a fresh handle.
fn journal_head(path: &Path) -> Result<Seq, Box<dyn Error>> {
    Ok(Journal::open(path)?.head()?)
}

#[test]
fn program_calls_record_caused_outcomes_from_one_loaded_root() -> Result<(), Box<dyn Error>> {
    // Catches a broken cause link, an unpinned digest, unstored staged artifacts, a reload per call, and re-running a repeated key.
    let Some(wasm_path) = require_wasm("aether_test_fixtures_program") else {
        return Ok(());
    };
    let wasm = fs::read(&wasm_path)?;
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("driver.sqlite");
    let seeded = seed_program_journal(&path, &wasm)?;
    assert_eq!(seeded.bundle, artifact_digest(OpaqueBytes::ID, &wasm));
    let (_chassis, registry, driver) = boot_driver(&path);
    let (inbox, rx) = caller(&registry, "test.bloomery.driver.caller");
    let origin = NativeOrigin::new("test.driver")?;
    let summarize = Call {
        program: Head::<OpaqueBytes>::new("program"),
        name: ProgramName::new("test.program.summarize")?,
        input: seeded.summarize,
        origin: origin.clone(),
        key: 1,
    };
    let outcome = send_call(&registry, driver, inbox, &rx, 1, &summarize);
    let CallOutcome::Transition { key: 1, seq, transition } = outcome else {
        panic!("expected a Transition outcome, got {outcome:?}");
    };
    let entries = read_all(&path)?;
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[1].kind, Requested::ID);
    assert_eq!(entries[1].cause, None);
    let requested = Journal::decode::<Requested>(&entries[1])?;
    assert_eq!(requested.source, RequestSource::Native { origin: origin.clone(), key: 1 });
    assert_eq!(requested.program.bundle(), seeded.bundle);
    assert_eq!(requested.program.name().as_str(), "test.program.summarize");
    assert_eq!(requested.input, seeded.summarize);
    assert_eq!(entries[2].kind, Transition::ID);
    assert_eq!(entries[2].cause, Some(Seq(2)));
    let recorded = Journal::decode::<Transition>(&entries[2])?;
    assert_eq!(recorded.program, requested.program);
    assert_eq!(recorded.input, seeded.summarize);
    assert!(Journal::open(&path)?.get_bytes(&recorded.result)?.is_some(), "the staged result is stored");
    assert_eq!(seq, 3);
    assert_eq!(transition, recorded);

    let refuse = Call {
        program: Head::<OpaqueBytes>::new("program"),
        name: ProgramName::new("test.program.refuse")?,
        input: seeded.refuse,
        origin,
        key: 2,
    };
    match send_call(&registry, driver, inbox, &rx, 2, &refuse) {
        CallOutcome::Fault { key: 2, fault, .. } => {
            assert!(
                matches!(fault.reason, FaultReason::Refused { .. }),
                "a refusing program faults Refused, got {:?}",
                fault.reason
            );
            assert_eq!(fault.program.bundle(), seeded.bundle);
        }
        other => panic!("expected a Refused fault, got {other:?}"),
    }

    let before = journal_head(&path)?;
    match send_call(&registry, driver, inbox, &rx, 3, &summarize) {
        CallOutcome::Transition { key: 1, seq: repeated, .. } => assert_eq!(repeated, seq),
        other => panic!("expected the recorded transition, got {other:?}"),
    }
    assert_eq!(journal_head(&path)?, before);
    Ok(())
}

#[test]
fn unbound_head_is_refused_and_records_nothing() -> Result<(), Box<dyn Error>> {
    // Catches recording a request that can't resolve.
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("driver.sqlite");
    let (_chassis, registry, driver) = boot_driver(&path);
    let (inbox, rx) = caller(&registry, "test.bloomery.driver.caller");
    let before = journal_head(&path)?;
    let call = Call {
        program: Head::<OpaqueBytes>::new("missing"),
        name: ProgramName::new("test.program.summarize")?,
        input: Digest::from_bytes([7; 32]),
        origin: NativeOrigin::new("test.driver")?,
        key: 1,
    };
    match send_call(&registry, driver, inbox, &rx, 1, &call) {
        CallOutcome::Refused { key: 1, reason: CallRefusal::HeadUnbound } => {}
        other => panic!("expected a HeadUnbound refusal, got {other:?}"),
    }
    assert_eq!(journal_head(&path)?, before);
    Ok(())
}
