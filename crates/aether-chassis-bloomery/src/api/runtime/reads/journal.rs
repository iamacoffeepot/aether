//! Page a decoded journal. The bloom and `contains` filters decode during a
//! reverse (or forward) scan; a filter whose match set has no recent activity
//! may walk the whole journal to fill one page or to learn the set is empty.
//!
//! Both filters run before the cursor, so `truncated` and `next_from_sequence`
//! are about matching records: following the cursor to exhaustion visits every
//! match exactly once instead of stepping over the ones a post-page filter
//! would have dropped.

use aether_bloomery::{
    BloomId, Digest, Event, Fact, JournalRecord, Outcome, decode_recorded_decisions, decode_recorded_event,
};
use serde::Serialize;
use serde_json::Value;

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
        if query.contains.as_deref().is_some_and(|needle| !entry_contains(&entry, needle)) {
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

/// Whether one record's summary contains `needle`.
///
/// The summary is the console's journal row without the parts only the console
/// can render: the sequence, the fact variant, the outcome variant, and the
/// idempotency key. Each field is matched on its own, so a needle can never
/// span a column separator the console paints and the route does not — every
/// record this filter keeps is one the console's own `record_matches` keeps
/// too, which is what lets the console fall back to filtering loaded rows
/// against a coordinator that predates this parameter.
fn entry_contains(entry: &JournalRecordResponse, needle: &str) -> bool {
    entry.sequence.to_string().contains(needle)
        || entry.idempotency_key.contains(needle)
        || variant_name(&entry.event.fact).is_some_and(|name| name.contains(needle))
        || variant_name(&entry.outcome).is_some_and(|name| name.contains(needle))
}

/// Serde's variant name for one externally tagged enum value: a variant with
/// fields serializes as a one-key object, a unit variant as that string.
fn variant_name<T: Serialize>(value: &T) -> Option<String> {
    match serde_json::to_value(value).ok()? {
        Value::String(name) => Some(name),
        Value::Object(map) => map.into_iter().next().map(|(name, _)| name),
        _ => None,
    }
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
        | Fact::MemberDeadlineExpired { bloom, .. }
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
        | Fact::HoldSharedRunCoalesce { bloom, .. }
        | Fact::AdminEnter { bloom, .. }
        | Fact::AdminExit { bloom, .. }
        | Fact::AdminCancelLane { bloom, .. }
        | Fact::AdminSetCandidate { bloom, .. }
        | Fact::AdminRerun { bloom, .. }
        | Fact::AdminWaive { bloom, .. }
        | Fact::AdminDropLap { bloom, .. } => vec![*bloom],
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
