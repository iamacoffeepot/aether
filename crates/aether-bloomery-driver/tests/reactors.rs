//! End-to-end: the driver follows the reactor set over a live journal, activating bundles and recording reactions.

mod chassis;

use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::mpsc;

use aether_actor::ActorRef;
use aether_bloomery_driver::BundleDriver;
use aether_bloomery_journal::{Batch, Entry, Journal, JournalActor, Seq};
use aether_bloomery_kinds::{
    Activated, ActivationRejected, AwaitProcessed, Digest, Fault, Head, MoveHead, MoveHeadResult, OpaqueBytes,
    Processed, ReactionFailed, ReactorName, ReactorSet, RecordedHead, RecordedHeadMove, Ref, RequestSource, Requested,
    RuleName, Transition, Utf8Text,
};
use aether_data::{Kind, MailboxId};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_substrate::mail::registry::{OwnedDispatch, Registry};
use aether_test_fixtures_kinds::{MIXED_BUNDLE, SUMMARIZE_BUNDLE, SUMMARIZE_PROGRAM, SummarizeInput};
use chassis::{boot_driver, caller, journal_head, read_all, reply, request};

/// Which fixture wasms to seed: split program/reactor bundles, or one mixed bundle.
#[derive(Clone, Copy)]
enum Bundles<'a> {
    Split { program: &'a [u8], reactor: &'a [u8] },
    Mixed(&'a [u8]),
}

/// Digests the seed wrote: the program and reactor bundle digests (one digest for a mixed bundle),
/// the unmoved set, and the call input.
struct Seed {
    program: Digest,
    reactor: Digest,
    set: Digest,
    input: Digest,
    member: Head<OpaqueBytes>,
}

/// Stage the fixture wasm(s) under their heads, plus the unmoved set and one call input.
fn seed_journal(path: &Path, bundles: Bundles<'_>) -> Result<Seed, Box<dyn Error>> {
    let mut journal = Journal::open(path)?;
    let mut batch = Batch::new();
    let (program, reactor, member) = match bundles {
        Bundles::Split { program, reactor } => {
            let program = batch.stage_bytes(program);
            batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&SUMMARIZE_BUNDLE), program.digest()), None)?;
            let member: Head<OpaqueBytes> = Head::new("test.bloomery.summarize.caller");
            let reactor = batch.stage_bytes(reactor);
            batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&member), reactor.digest()), None)?;
            (program.digest(), reactor.digest(), member)
        }
        Bundles::Mixed(wasm) => {
            let mixed = batch.stage_bytes(wasm);
            batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&MIXED_BUNDLE), mixed.digest()), None)?;
            let member: Head<OpaqueBytes> = Head::new("test.bloomery.mixed.caller");
            batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&member), mixed.digest()), None)?;
            (mixed.digest(), mixed.digest(), member)
        }
    };
    let set = batch.stage_encoded(&ReactorSet::new(vec![member.clone()])?)?;
    let text = batch.stage_text("hello");
    let input = batch.stage_encoded(&SummarizeInput { text })?;
    journal.append(Seq(0), &batch)?;
    Ok(Seed { program, reactor, set: set.digest(), input: input.digest(), member })
}

/// Run the shared reaction flow: settle, set move, settle, input move on
/// `test.bloomery.summarize.input`, settle. Returns the `Activated` seq and the journal entries.
fn drive_reaction(path: &Path, seed: &Seed) -> Result<(u64, Vec<Entry>), Box<dyn Error>> {
    let (_chassis, registry, journal, driver) = boot_driver(path);
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
    settle(&registry, driver, driver_inbox, &driver_rx, &mut correlations, activated + 1);
    let entries = read_all(path)?;
    Ok((activated, entries))
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
    driver: ActorRef<BundleDriver>,
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
    journal: ActorRef<JournalActor>,
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
    let seed = seed_journal(&path, Bundles::Split { program: &program_wasm, reactor: &reactor_wasm })?;
    let (activated, entries) = drive_reaction(&path, &seed)?;
    assert_eq!(activated, 4);
    assert_eq!(entries.len(), 7);
    assert_eq!(entries[3].seq, Seq(4));
    assert_eq!(entries[3].kind, Activated::ID);
    assert_eq!(entries[3].cause, Some(Seq(3)));
    assert_eq!(Journal::decode::<Activated>(&entries[3])?, Activated::new(seed.member.clone(), seed.reactor, Seq(4))?);
    let final_head = u64::try_from(entries.len())?;
    assert_eq!(final_head, activated + 3);
    assert_eq!(journal_head(&path)?, Seq(final_head));
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

#[test]
fn a_mixed_bundle_serves_its_reactor_and_its_program_from_one_load() -> Result<(), Box<dyn Error>> {
    // Catches a second `LoadComponent` for a digest one role already loaded: the component host refuses the digest name with `SubnameInUse`, which the driver records as a `BundleUnavailable` `Fault`, so one load is the only way both roles succeed.
    let Some(mixed_path) = require_wasm("aether_test_fixtures_mixed_bundle") else {
        return Ok(());
    };
    let mixed_wasm = fs::read(&mixed_path)?;
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("driver.sqlite");
    let seed = seed_journal(&path, Bundles::Mixed(&mixed_wasm))?;
    let mixed = seed.reactor;
    assert_eq!(seed.program, mixed);
    let (activated, entries) = drive_reaction(&path, &seed)?;
    assert_eq!(activated, 4);
    assert_eq!(Journal::decode::<Activated>(&entries[3])?, Activated::new(seed.member, mixed, Seq(4))?);
    let requested_entries: Vec<_> = entries.iter().filter(|entry| entry.kind == Requested::ID).collect();
    assert_eq!(requested_entries.len(), 1);
    let requested = Journal::decode::<Requested>(requested_entries[0])?;
    assert_eq!(
        requested.source,
        RequestSource::Reaction {
            bundle: mixed,
            reactor: ReactorName::new("test.bloomery.mixed.caller")?,
            rule: RuleName::new("call_summarize")?,
            ordinal: 0,
        }
    );
    assert_eq!(requested.program.bundle(), mixed);
    let transition_entries: Vec<_> = entries.iter().filter(|entry| entry.kind == Transition::ID).collect();
    assert_eq!(transition_entries.len(), 1);
    assert_eq!(transition_entries[0].cause, Some(requested_entries[0].seq));
    let transition = Journal::decode::<Transition>(transition_entries[0])?;
    assert_eq!(transition.program, requested.program);
    assert!(Journal::open(&path)?.get_bytes(&transition.result)?.is_some(), "the staged result is stored");
    let failures = [ReactionFailed::ID, ActivationRejected::ID, Fault::ID];
    assert!(entries.iter().all(|entry| !failures.contains(&entry.kind)), "no failure records");
    Ok(())
}
