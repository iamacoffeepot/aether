//! Page a decoded journal. The bloom filter decodes during a reverse (or
//! forward) scan; a filter whose bloom has no recent activity may walk the
//! whole journal to fill one page or to learn the match set is empty.

use aether_bloomery::{BloomId, Digest, Event, Fact, JournalRecord, decode_recorded_decisions};
use aether_data::wire::from_bytes;

use super::query::JournalQuery;
use crate::api::dto::{JournalEntry, JournalView};

/// Why a journal page cannot be built.
#[derive(Debug)]
pub enum JournalPageError {
    /// A row's event bytes did not decode.
    Event { sequence: u64, error: String },
    /// A row's recorded decisions did not decode.
    Decisions { sequence: u64, error: String },
}

/// Select and decode one page of `records` under `query`.
///
/// # Errors
///
/// [`JournalPageError`] when an unfiltered row cannot be decoded. A bloom
/// filter skips a row it cannot attribute rather than failing the read.
pub fn page_journal(records: &[JournalRecord], query: &JournalQuery) -> Result<JournalView, JournalPageError> {
    let mut total_matched = 0_u64;
    let mut after_cursor = 0_u64;
    let mut page = Vec::new();
    let limit = query.limit;

    for record in ordered(records, query.descending) {
        let entry = match decode_entry(record) {
            Ok(entry) => entry,
            Err(error) if query.bloom.is_some() => {
                let _ = error;
                continue;
            }
            Err(error) => return Err(error),
        };
        if query.bloom.is_some_and(|bloom| !entry_names_bloom(&entry, &bloom)) {
            continue;
        }
        total_matched += 1;
        if !past_cursor(entry.sequence, query) {
            continue;
        }
        after_cursor += 1;
        if u64::try_from(page.len()).unwrap_or(u64::MAX) < limit {
            page.push(entry);
        }
    }

    let shown = u64::try_from(page.len()).unwrap_or(u64::MAX);
    let truncated = after_cursor > shown;
    let next_from_sequence = truncated.then(|| page.last().map(|entry| entry.sequence)).flatten();

    Ok(JournalView { records: page, total_matched, shown, truncated, next_from_sequence, notice: query.notice.clone() })
}

fn ordered(records: &[JournalRecord], descending: bool) -> impl Iterator<Item = &JournalRecord> {
    let len = records.len();
    (0..len).map(move |index| {
        if descending {
            &records[len - 1 - index]
        } else {
            &records[index]
        }
    })
}

fn past_cursor(sequence: u64, query: &JournalQuery) -> bool {
    match query.from_sequence {
        None => true,
        Some(from) if query.descending => sequence < from,
        Some(from) => sequence > from,
    }
}

fn decode_entry(record: &JournalRecord) -> Result<JournalEntry, JournalPageError> {
    let event = from_bytes::<Event>(&record.event)
        .map_err(|error| JournalPageError::Event { sequence: record.sequence, error: error.to_string() })?;
    let decisions = decode_recorded_decisions(&record.decisions, record.decisions_schema.as_deref())
        .map_err(|error| JournalPageError::Decisions { sequence: record.sequence, error: error.to_string() })?;
    Ok(JournalEntry {
        sequence: record.sequence,
        idempotency_key: record.idempotency_key.clone(),
        event,
        outcome: decisions.outcome,
        decider: record.decider.clone(),
    })
}

fn entry_names_bloom(entry: &JournalEntry, bloom: &Digest) -> bool {
    fact_blooms(&entry.event.fact).iter().any(|named| named.0 == *bloom)
}

fn fact_blooms(fact: &Fact) -> Vec<BloomId> {
    match fact {
        Fact::Seal(spec) => vec![spec.id()],
        Fact::Supersede { predecessor, successor } => vec![*predecessor, successor.id()],
        Fact::GraphSeal { predecessor, spec, .. } => {
            predecessor.map_or_else(|| vec![spec.id()], |predecessor| vec![predecessor, spec.id()])
        }
        Fact::Integrate { bloom, .. }
        | Fact::AdmitEvidence { bloom, .. }
        | Fact::Resolve { bloom, .. }
        | Fact::Land { bloom, .. }
        | Fact::AdoptAnswer { bloom, .. }
        | Fact::AttemptCompleted { bloom, .. }
        | Fact::AggregateReviewCompleted { bloom, .. }
        | Fact::AggregateVerifyCompleted { bloom, .. }
        | Fact::LandingRejected { bloom, .. }
        | Fact::GrantAttempts { bloom, .. }
        | Fact::VerifyFailed { bloom, .. }
        | Fact::AggregateReviewExecutorFault { bloom, .. }
        | Fact::FoldConflict { bloom, .. }
        | Fact::OperatorAdjudication { bloom, .. }
        | Fact::OperatorRepair { bloom, .. }
        | Fact::OperatorHold { bloom, .. }
        | Fact::OperatorRelease { bloom, .. }
        | Fact::VerifyHostFault { bloom, .. }
        | Fact::ResumeHostFault { bloom, .. }
        | Fact::SpliceAssembled { bloom, .. }
        | Fact::MemberExecutorFault { bloom, .. }
        | Fact::FoldRefused { bloom, .. } => vec![*bloom],
        Fact::ObserveMainline { .. }
        | Fact::ObserveMainlineDiverged { .. }
        | Fact::SurfaceOverlap { .. }
        | Fact::RequestOrphanClaimRelease { .. }
        | Fact::CompleteOrphanClaimRelease { .. } => Vec::new(),
    }
}
