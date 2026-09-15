//! Page a decoded journal. The bloom filter decodes during a reverse (or
//! forward) scan; a filter whose bloom has no recent activity may walk the
//! whole journal to fill one page or to learn the match set is empty.

use aether_bloomery::{
    BloomId, Digest, Event, Fact, JournalRecord, Outcome, decode_recorded_decisions, decode_recorded_event,
};
use serde::Serialize;

use super::query::JournalQuery;

/// Why a journal page cannot be built.
#[derive(Debug)]
pub enum JournalPageError {
    /// A row's event bytes did not decode.
    Event { sequence: u64, error: String },
    /// A row's recorded decisions did not decode.
    Decisions { sequence: u64, error: String },
}

/// One decoded journal record as `GET /journal` renders it.
///
/// A local response shape rather than the shared journal-entry value type:
/// the store's `recorded_unix_millis` column rides beside the decoded event so
/// the console can paint a recorded-time column without touching the shared
/// type xtask also constructs.
#[derive(Debug, Clone, Serialize)]
pub struct JournalRecordResponse {
    /// The record's journal sequence.
    pub sequence: u64,
    /// The record's idempotency key.
    pub idempotency_key: String,
    /// The decoded event the record journaled.
    pub event: Event,
    /// The outcome the event reduced to when it was admitted.
    pub outcome: Outcome,
    /// The identity of the build whose reducer decided the event.
    pub decider: String,
    /// Host-clock stamp written at admission. `None` is a pre-column row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_unix_millis: Option<u64>,
}

/// One bounded `GET /journal` page as the route renders it.
#[derive(Debug, Clone, Serialize)]
pub struct JournalPageResponse {
    /// The page of journaled events, in the requested order.
    pub records: Vec<JournalRecordResponse>,
    /// How many records match the filter.
    pub total_matched: u64,
    /// How many records this page carries.
    pub shown: u64,
    /// True when more matching records remain after this page.
    pub truncated: bool,
    /// Exclusive cursor for the next page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_from_sequence: Option<u64>,
    /// Set when the caller named a `limit` above the clamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// Select and decode one page of `records` under `query`.
///
/// # Errors
///
/// [`JournalPageError`] when an unfiltered row cannot be decoded. A bloom
/// filter skips a row it cannot attribute rather than failing the read.
pub fn page_journal(records: &[JournalRecord], query: &JournalQuery) -> Result<JournalPageResponse, JournalPageError> {
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

    Ok(JournalPageResponse {
        records: page,
        total_matched,
        shown,
        truncated,
        next_from_sequence,
        notice: query.notice.clone(),
    })
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

fn decode_entry(record: &JournalRecord) -> Result<JournalRecordResponse, JournalPageError> {
    let event = decode_recorded_event(&record.event, record.event_schema.as_deref())
        .map_err(|error| JournalPageError::Event { sequence: record.sequence, error: error.to_string() })?;
    let decisions = decode_recorded_decisions(&record.decisions, record.decisions_schema_digest.as_deref())
        .map_err(|error| JournalPageError::Decisions { sequence: record.sequence, error: error.to_string() })?;
    Ok(JournalRecordResponse {
        sequence: record.sequence,
        idempotency_key: record.idempotency_key.clone(),
        event,
        outcome: decisions.outcome,
        decider: record.decider.clone(),
        recorded_unix_millis: record.recorded_unix_millis,
    })
}

fn entry_names_bloom(entry: &JournalRecordResponse, bloom: &Digest) -> bool {
    fact_blooms(&entry.event.fact).iter().any(|named| named.0 == *bloom)
}

fn fact_blooms(fact: &Fact) -> Vec<BloomId> {
    match fact {
        Fact::Seal(spec) => vec![spec.id()],
        Fact::Supersede { predecessor, successor } => vec![*predecessor, successor.id()],
        Fact::GraphSeal { predecessor, spec, .. } => {
            predecessor.map_or_else(|| vec![spec.id()], |predecessor| vec![predecessor, spec.id()])
        }
        Fact::ConstructionCheckpointObserved { checkpoint } => vec![checkpoint.bloom],
        Fact::RequestConstructionAdmission { admission } => vec![admission.dispatch.bloom],
        Fact::Integrate { bloom, .. }
        | Fact::AdmitEvidence { bloom, .. }
        | Fact::Resolve { bloom, .. }
        | Fact::Land { bloom, .. }
        | Fact::AdoptAnswer { bloom, .. }
        | Fact::AttemptCompleted { bloom, .. }
        | Fact::AggregateReviewCompleted { bloom, .. }
        | Fact::AggregateVerifyCompleted { bloom, .. }
        | Fact::PrecheckPrepared { bloom, .. }
        | Fact::RequestPrecheck { bloom, .. }
        | Fact::PrecheckCompleted { bloom, .. }
        | Fact::IntegrationAdvanced { bloom, .. }
        | Fact::IntegrationAppendConflicted { bloom, .. }
        | Fact::IntegrationAppendRefused { bloom, .. }
        | Fact::CandidatePrepared { bloom, .. }
        | Fact::ProposeSharedRun { bloom, .. }
        | Fact::SharedRunPrepared { bloom, .. }
        | Fact::SharedRunStarted { bloom, .. }
        | Fact::SharedRunCompleted { bloom, .. }
        | Fact::StableHeadReservationExpired { bloom, .. }
        | Fact::CompatibilityPreviewed { bloom, .. }
        | Fact::PartialHeadRepairCompleted { bloom, .. }
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
        | Fact::FoldRefused { bloom, .. }
        | Fact::ContainmentRefused { bloom, .. }
        | Fact::SurfaceRequested { bloom, .. }
        | Fact::Withdraw { bloom, .. }
        | Fact::LaneWritesObserved { bloom, .. }
        | Fact::SuppressionDisposition { bloom, .. }
        | Fact::CompositionNarrowed { bloom, .. }
        | Fact::SurfaceGranted { bloom, .. }
        | Fact::StudyCompleted { bloom, .. }
        | Fact::ProofReused { bloom, .. }
        | Fact::HoldSharedRunCoalesce { bloom, .. } => vec![*bloom],
        Fact::ObserveMainline { .. }
        | Fact::ObserveMainlineDiverged { .. }
        | Fact::SurfaceOverlap { .. }
        | Fact::RequestOrphanClaimRelease { .. }
        | Fact::CompleteOrphanClaimRelease { .. }
        | Fact::BaseVerifyCompleted { .. }
        | Fact::BaseReverify(_)
        | Fact::ProposeChange { .. } => Vec::new(),
    }
}
