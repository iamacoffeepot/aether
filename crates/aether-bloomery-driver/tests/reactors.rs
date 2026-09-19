//! End-to-end: the driver follows the reactor set over a live journal, activating bundles and recording reactions.

mod chassis;

use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::mpsc;

use aether_bloomery_journal::{Batch, Journal, Seq};
use aether_bloomery_kinds::{
    Activated, ActivationRejected, AwaitProcessed, Digest, Fault, Head, MoveHead, MoveHeadResult, OpaqueBytes,
    Processed, ReactionFailed, ReactorName, ReactorSet, RecordedHead, RecordedHeadMove, Ref, RequestSource, Requested,
    RuleName, Transition, Utf8Text,
};
use aether_data::{Kind, MailboxId};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_substrate::mail::registry::{OwnedDispatch, Registry};
use aether_test_fixtures_kinds::{SUMMARIZE_BUNDLE, SUMMARIZE_PROGRAM, SummarizeInput};
use chassis::{boot_driver, caller, journal_head, read_all, reply, request};

/// Digests the seed wrote: the program and reactor bundle wasms, the unmoved set, and the call input.
struct Seed {
    program: Digest,
    reactor: Digest,
    set: Digest,
    input: Digest,
    member: Head<OpaqueBytes>,
}

/// Stage both fixture wasms under their heads, plus the unmoved set and one call input.
fn seed_reactor_journal(path: &Path, program_wasm: &[u8], reactor_wasm: &[u8]) -> Result<Seed, Box<dyn Error>> {
    let mut journal = Journal::open(path)?;
    let mut batch = Batch::new();
    let program = batch.stage_bytes(program_wasm);
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&SUMMARIZE_BUNDLE), program.digest()), None)?;
    let member: Head<OpaqueBytes> = Head::new("test.bloomery.summarize.caller");
    let reactor = batch.stage_bytes(reactor_wasm);
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&member), reactor.digest()), None)?;
    let set = batch.stage_encoded(&ReactorSet::new(vec![member.clone()])?)?;
    let text = batch.stage_text("hello");
    let input = batch.stage_encoded(&SummarizeInput { text })?;
    journal.append(Seq(0), &batch)?;
    Ok(Seed { program: program.digest(), reactor: reactor.digest(), set: set.digest(), input: input.digest(), member })
}

/// Stage two texts and move one head, returning the staged-but-unmoved text for the live move.
fn seed_barrier_journal(path: &Path) -> Result<Digest, Box<dyn Error>> {
    let mut journal = Journal::open(path)?;
    let mut batch = Batch::new();
    let first = batch.stage_text("one");
    let second = batch.stage_text("two");
    let head: Head<Utf8Text> = Head::new("test.bloomery.barrier.first");
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&head), first.digest()), None)?;
    journal.append(Seq(0), &batch)?;
    Ok(second.digest())
}

/// Drive the barrier at `through` to quiescence, following the `Processed` protocol.
fn settle(
    registry: &Registry,
    driver: MailboxId,
    inbox: MailboxId,
    rx: &mpsc::Receiver<OwnedDispatch>,
    correlations: &mut u64,
    through: u64,
) -> u64 {
    let mut current = through;
    for _ in 0..8 {
        *correlations += 1;
        let correlation = *correlations;
        request(registry, driver, inbox, correlation, &AwaitProcessed { through: current });
        let head = reply::<Processed>(rx, correlation).head;
        if head == current {
            return head;
        }
        current = head;
    }
    panic!("settle did not quiesce within 8 rounds from {through}");
}

/// Send one fenced head move, returning the committed seq.
fn move_head(
    registry: &Registry,
    journal: MailboxId,
    inbox: MailboxId,
    rx: &mpsc::Receiver<OwnedDispatch>,
    correlations: &mut u64,
    mail: &MoveHead,
) -> u64 {
    *correlations += 1;
    let correlation = *correlations;
    request(registry, journal, inbox, correlation, mail);
    match reply::<MoveHeadResult>(rx, correlation) {
        MoveHeadResult::Committed { seq } => seq,
        other => panic!("expected a committed move, got {other:?}"),
    }
}

#[test]
fn await_processed_waits_for_a_live_append_it_is_woken_for() -> Result<(), Box<dyn Error>> {
    // Catches a warn-dropped `WatchHeadResult` (routing never wakes for another
    // writer's append, so the barrier never answers), an unhandled
    // `AwaitProcessed`, and a barrier answered before its bound is routed.
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("driver.sqlite");
    let staged = seed_barrier_journal(&path)?;
    let (_chassis, registry, journal, driver) = boot_driver(&path);
    let (journal_inbox, journal_rx) = caller(&registry, "test.bloomery.driver.journal");
    let (driver_inbox, driver_rx) = caller(&registry, "test.bloomery.driver.caller");
    let mut correlations = 0u64;
    assert_eq!(settle(&registry, driver, driver_inbox, &driver_rx, &mut correlations, 1), 1);
    correlations += 1;
    let barrier = correlations;
    request(&registry, driver, driver_inbox, barrier, &AwaitProcessed { through: 2 });
    let head = Head::<Utf8Text>::new("test.bloomery.barrier.second");
    assert_eq!(
        move_head(
            &registry,
            journal,
            journal_inbox,
            &journal_rx,
            &mut correlations,
            &MoveHead::new(&head, Ref::from_digest(staged), 1)
        ),
        2
    );
    assert_eq!(reply::<Processed>(&driver_rx, barrier).head, 2);
    Ok(())
}

#[test]
fn reactor_set_move_activates_and_its_call_program_records_requested_then_transition() -> Result<(), Box<dyn Error>> {
    // Catches a warn-dropped `Warmed` / `Evaluated` (routing stalls and `settle`
    // times out), a reactor loaded under the program export, a reply routed to
    // the wrong continuation, a reaction `Requested` that is uncaused, carries a
    // `Native` source, or resolves its bundle at the wrong boundary, a
    // `Transition` not caused by the reaction's `Requested`, and a barrier that
    // answers between `Requested` and `Transition`.
    let Some(reactor_path) = require_wasm("aether_test_fixtures_reactor_call") else {
        return Ok(());
    };
    let Some(program_path) = require_wasm("aether_test_fixtures_program") else {
        return Ok(());
    };
    let reactor_wasm = fs::read(&reactor_path)?;
    let program_wasm = fs::read(&program_path)?;
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("driver.sqlite");
    let seed = seed_reactor_journal(&path, &program_wasm, &reactor_wasm)?;
    let (_chassis, registry, journal, driver) = boot_driver(&path);
    let (journal_inbox, journal_rx) = caller(&registry, "test.bloomery.driver.journal");
    let (driver_inbox, driver_rx) = caller(&registry, "test.bloomery.driver.caller");
    let mut correlations = 0u64;
    assert_eq!(settle(&registry, driver, driver_inbox, &driver_rx, &mut correlations, 2), 2);
    assert_eq!(
        move_head(
            &registry,
            journal,
            journal_inbox,
            &journal_rx,
            &mut correlations,
            &MoveHead::new(&ReactorSet::ROOT, Ref::from_digest(seed.set), 2)
        ),
        3
    );
    let activated = settle(&registry, driver, driver_inbox, &driver_rx, &mut correlations, 3);
    assert_eq!(activated, 4);
    let entries = read_all(&path)?;
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[3].seq, Seq(4));
    assert_eq!(entries[3].kind, Activated::ID);
    assert_eq!(entries[3].cause, Some(Seq(3)));
    assert_eq!(Journal::decode::<Activated>(&entries[3])?, Activated::new(seed.member.clone(), seed.reactor, Seq(4))?);
    let input_head = Head::<SummarizeInput>::new("test.bloomery.summarize.input");
    assert_eq!(
        move_head(
            &registry,
            journal,
            journal_inbox,
            &journal_rx,
            &mut correlations,
            &MoveHead::new(&input_head, Ref::from_digest(seed.input), activated)
        ),
        activated + 1
    );
    let final_head = settle(&registry, driver, driver_inbox, &driver_rx, &mut correlations, activated + 1);
    assert_eq!(final_head, activated + 3);
    assert_eq!(journal_head(&path)?, Seq(final_head));
    let entries = read_all(&path)?;
    let requested_entry = &entries[usize::try_from(activated + 1)?];
    assert_eq!(requested_entry.seq, Seq(activated + 2));
    assert_eq!(requested_entry.kind, Requested::ID);
    assert_eq!(requested_entry.cause, Some(Seq(activated + 1)));
    let requested = Journal::decode::<Requested>(requested_entry)?;
    assert_eq!(
        requested.source,
        RequestSource::Reaction {
            bundle: seed.reactor,
            reactor: ReactorName::new("test.bloomery.summarize.caller")?,
            rule: RuleName::new("call_summarize")?,
            ordinal: 0,
        }
    );
    assert_eq!(requested.program.bundle(), seed.program);
    assert_eq!(requested.program.name().as_str(), SUMMARIZE_PROGRAM);
    assert_eq!(requested.input, seed.input);
    let transition_entry = &entries[usize::try_from(activated + 2)?];
    assert_eq!(transition_entry.seq, Seq(activated + 3));
    assert_eq!(transition_entry.kind, Transition::ID);
    assert_eq!(transition_entry.cause, Some(Seq(activated + 2)));
    let transition = Journal::decode::<Transition>(transition_entry)?;
    assert_eq!(transition.program, requested.program);
    assert!(Journal::open(&path)?.get_bytes(&transition.result)?.is_some(), "the staged result is stored");
    assert_eq!(entries.iter().filter(|entry| entry.kind == Requested::ID).count(), 1);
    let failures = [ReactionFailed::ID, ActivationRejected::ID, Fault::ID];
    assert!(entries.iter().all(|entry| !failures.contains(&entry.kind)), "no failure records");
    Ok(())
}
