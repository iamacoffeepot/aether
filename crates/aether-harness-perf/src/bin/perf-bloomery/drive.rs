//! The drive: activate the session loop, then open every session at once and
//! follow the journal until each has recorded its rest.

use std::time::{Duration, Instant};

use aether_bloomery_journal::{DecodeError, Digest, JournalReader};
use aether_bloomery_kinds::{
    Call, CallOutcome, Entry, Fault, MoveHead, MoveHeadResult, ReactorSet, Ref, Seq, Transition, WatchHeadResult,
};
use aether_bloomery_muse::{RestReason, Session};
use aether_harness_bloomery::BloomeryHarness;

use crate::Failure;

/// How long an open's outcome may take: it can queue behind every other
/// session's requests on the one bundle.
const OPEN_PATIENCE: Duration = Duration::from_mins(10);

/// How many journal entries one scan reads.
const PAGE: usize = 1024;

/// The program whose transition marks a session's rest.
const RECORD: &str = "muse.session.record";

/// What the measured window saw.
pub struct Window {
    /// From the first open's send to the last session's record.
    pub elapsed: Duration,
    /// Each rested session's recorded `muse.session` artifact.
    pub records: Vec<Digest>,
}

/// Settle the seed, move the reactor-set root to `set`, and settle again, so
/// the bundle's load and the session reactor's warmup finish before the
/// window opens.
pub fn activate(harness: &mut BloomeryHarness, set: Ref<ReactorSet>) -> Result<(), Failure> {
    let seeded = harness.settle(harness.head());
    match harness.move_head(&MoveHead::new(&ReactorSet::ROOT, set, seeded.0)) {
        MoveHeadResult::Committed { seq } => {
            harness.settle(Seq(seq));
            Ok(())
        }
        other => Err(Failure::unmeasured(format!("the reactor set did not take the root head: {other:?}"))),
    }
}

/// Send every open, wait for each to open its session, then scan the journal
/// through head watches until every session has recorded its rest; stop the
/// clock there, and settle once.
///
/// Settle is never called mid-run: a head a long session keeps moving can
/// outrun its bounded rounds.
pub fn sessions(harness: &mut BloomeryHarness, calls: &[Call]) -> Result<Window, Failure> {
    let reader = JournalReader::open(harness.journal_path())
        .map_err(|error| Failure::unmeasured(format!("open the journal for reading: {error}")))?;
    let mut after = harness.head();
    let started = Instant::now();

    let pending: Vec<_> = calls.iter().map(|call| harness.send_call(call)).collect();
    for (call, pending) in calls.iter().zip(pending) {
        match harness.wait_within(pending, OPEN_PATIENCE) {
            CallOutcome::Transition { .. } => {}
            other => return Err(Failure::session(format!("session {} did not open: {other:?}", call.key))),
        }
    }

    let mut records = Vec::with_capacity(calls.len());
    loop {
        let page = reader
            .read(after, PAGE)
            .map_err(|error| Failure::unmeasured(format!("read the journal after {after}: {error}")))?;
        for entry in &page {
            after = entry.seq;
            if let Some(record) = scan(entry)? {
                records.push(record);
            }
        }
        if records.len() >= calls.len() {
            break;
        }
        match harness.watch_head(after) {
            WatchHeadResult::Advanced { .. } => {}
            other => return Err(Failure::unmeasured(format!("the head watch after {after} ended: {other:?}"))),
        }
    }
    let elapsed = started.elapsed();

    harness.settle(harness.head());
    Ok(Window { elapsed, records })
}

/// Require every recorded session to have rested `Completed`, as the stub's
/// last reply rests it: any other rest means the loop did not run the workload
/// the knobs describe.
pub fn completed(harness: &BloomeryHarness, records: &[Digest]) -> Result<(), Failure> {
    let reader = JournalReader::open(harness.journal_path())
        .map_err(|error| Failure::unmeasured(format!("open the journal for reading: {error}")))?;
    for record in records {
        let session = reader
            .get::<Session>(record)
            .map_err(|error| Failure::unmeasured(format!("read session {record}: {error}")))?
            .ok_or_else(|| Failure::unmeasured(format!("session {record} is not stored")))?;
        if !matches!(session.rested(), RestReason::Completed) {
            return Err(Failure::session(format!("session {record} rested {:?}", session.rested())));
        }
    }
    Ok(())
}

/// The recorded session a `muse.session.record` transition cites, or a
/// failure for a fault.
fn scan(entry: &Entry) -> Result<Option<Digest>, Failure> {
    match entry.decode::<Transition>() {
        Ok(transition) if transition.program.name().as_str() == RECORD => return Ok(Some(transition.result)),
        Ok(_) => return Ok(None),
        Err(DecodeError::KindMismatch { .. }) => {}
        Err(error) => return Err(Failure::unmeasured(format!("decode transition {}: {error}", entry.seq))),
    }
    match entry.decode::<Fault>() {
        Ok(fault) => Err(Failure::session(format!(
            "{} faulted at {}: {:?}",
            fault.program.name().as_str(),
            entry.seq,
            fault.reason
        ))),
        Err(DecodeError::KindMismatch { .. }) => Ok(None),
        Err(error) => Err(Failure::unmeasured(format!("decode fault {}: {error}", entry.seq))),
    }
}
