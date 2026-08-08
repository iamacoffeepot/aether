//! The inward stage-result normalizer (#3459 step 6) — *port shape only*.
//!
//! The one inward channel ADR-0149 permits: a reviewer verdict or check-run
//! conclusion on work **Bloomery itself dispatched** normalizes into an
//! [`Evidence`] artifact bound to the exact digest Bloomery displayed, ready
//! to enter the reducer like any other attempt result. Free-form platform
//! activity is never translated — a comment never becomes a command; this
//! channel carries only a verdict over a digest Bloomery already put on the
//! wire.
//!
//! This slice ships the type + the pure normalizer + its tests. It does
//! **not** wire GitHub webhook / checks ingestion — that lands with the
//! migration step 2 executor/review bridge. The load-bearing invariant proven
//! here is ADR-0149 §The value vocabulary's: **evidence never validates a
//! digest it does not name.** A verdict whose subject is not the displayed
//! digest is rejected, not silently rebound.

use core::fmt;
use std::error::Error;

use aether_bloomery::{Digest, Evidence, StageVerdict, StudyCost};
use serde::Deserialize;

/// A stage result as the inward channel observed it: the digest the platform
/// object claimed to be about, the verdict, and the supporting artifact's
/// digest. `subject` is what the observation *claims*; the normalizer checks
/// it against what Bloomery actually displayed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StageResult {
    /// The digest the observed result claims to be about.
    pub subject: Digest,
    /// The verdict the result carries.
    pub verdict: StageVerdict,
    /// The supporting artifact (the check output, the review record).
    pub detail: Digest,
}

/// Why an inward stage result was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InwardError {
    /// The result's subject is not the digest Bloomery displayed. Binding it
    /// would produce evidence naming a digest the stage never ran against —
    /// the exact failure the value-vocabulary invariant forbids.
    DigestMismatch {
        /// The digest Bloomery displayed and expects the verdict to be about.
        displayed: Digest,
        /// The digest the observed result actually claimed.
        claimed: Digest,
    },
}

impl fmt::Display for InwardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DigestMismatch { .. } => {
                f.write_str("inward stage result names a digest other than the one displayed")
            }
        }
    }
}

impl Error for InwardError {}

/// Normalize an observed stage `result` into [`Evidence`] bound to the
/// `displayed` digest.
///
/// The result is accepted only when its `subject` is exactly the digest
/// Bloomery displayed; the produced evidence names that digest. A result
/// claiming any other digest is rejected with [`InwardError::DigestMismatch`]
/// — evidence never validates a digest it does not name.
///
/// # Errors
/// Returns [`InwardError::DigestMismatch`] when `result.subject != *displayed`.
pub fn normalize_stage_result(displayed: &Digest, result: &StageResult) -> Result<Evidence, InwardError> {
    if result.subject != *displayed {
        return Err(InwardError::DigestMismatch { displayed: *displayed, claimed: result.subject });
    }
    Ok(Evidence { subject: *displayed, kind: result.verdict.evidence_kind(), detail: result.detail })
}

/// A runner **result record** as the study intake observed it (issue #3523):
/// the attempt digest the upload claims its cost is about, plus the parsed cost
/// columns. The study sibling of [`StageResult`]: `subject` is what the upload
/// *claims*, and the normalizer checks it against the digest Bloomery displayed
/// for the order — a study record, like a verdict, never grades a digest it
/// does not name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StudyResult {
    /// The attempt digest the uploaded record claims to grade.
    pub subject: Digest,
    /// The cost columns the record carries.
    pub cost: StudyCost,
}

/// Why a runner result record could not be parsed into a [`StudyCost`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StudyRecordError {
    /// The uploaded result-record bytes are not a decodable
    /// `scripts/agent-usage-record.mjs` object.
    Parse(String),
}

impl fmt::Display for StudyRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(detail) => write!(f, "runner result record did not parse: {detail}"),
        }
    }
}

impl Error for StudyRecordError {}

/// The gradeable subset of a `scripts/agent-usage-record.mjs` object, as it
/// appears on the wire. Every column is optional: a run that died before its
/// terminal `result` emits an envelope-only record (`no_result: true`) with the
/// cost/turn/duration fields absent, and `parse_study_cost` reads those as zero
/// rather than failing — a dead attempt is still legible study evidence, cost
/// unknown (the same "never fail on a dead run" contract the script keeps).
#[derive(Deserialize)]
struct ResultRecordJson {
    #[serde(default)]
    cost_usd: Option<f64>,
    #[serde(default)]
    num_turns: Option<u64>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    input: u64,
    #[serde(default)]
    cache_write: u64,
    #[serde(default)]
    cache_write_1h: u64,
    #[serde(default)]
    cache_write_5m: u64,
    #[serde(default)]
    cache_read: u64,
    #[serde(default)]
    output: u64,
}

/// The dollar cost in micro-USD. A float dollar amount is not `Eq` and so not a
/// stable content address, so the study record carries `total_cost_usd` scaled
/// to integral micro-USD. A non-finite or non-positive cost (a null / absent /
/// negative field) reads as zero; the `f64 as u64` cast saturates by language
/// rule, so an absurd cost can never wrap.
fn micro_usd(cost_usd: Option<f64>) -> u64 {
    match cost_usd {
        // The guard admits only a finite, positive cost, and the `f64 as u64`
        // cast saturates by language rule, so neither truncation-to-wrap nor
        // sign-loss can occur — an absurd cost clamps to `u64::MAX`, a stable
        // (if useless) address, never a wrapped one.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(cost) if cost.is_finite() && cost > 0.0 => (cost * 1_000_000.0).round() as u64,
        _ => 0,
    }
}

/// Parse a runner result record's JSON bytes into the gradeable [`StudyCost`]
/// columns. Pure shape translation — the binding to an attempt is
/// [`normalize_study_result`]'s job.
///
/// # Errors
/// Returns [`StudyRecordError::Parse`] when `json` is not a decodable object.
pub fn parse_study_cost(json: &[u8]) -> Result<StudyCost, StudyRecordError> {
    let raw: ResultRecordJson =
        serde_json::from_slice(json).map_err(|error| StudyRecordError::Parse(error.to_string()))?;
    Ok(StudyCost {
        cost_micro_usd: micro_usd(raw.cost_usd),
        turns: raw.num_turns.unwrap_or(0),
        duration_millis: raw.duration_ms.unwrap_or(0),
        input_tokens: raw.input,
        cache_write_tokens: raw.cache_write,
        cache_write_1h_tokens: raw.cache_write_1h,
        cache_write_5m_tokens: raw.cache_write_5m,
        cache_read_tokens: raw.cache_read,
        output_tokens: raw.output,
    })
}

/// Bind an observed study `result` to the `displayed` digest, returning the
/// gradeable cost columns.
///
/// Accepted only when the result's `subject` is exactly the digest Bloomery
/// displayed; a record claiming any other digest is rejected with
/// [`InwardError::DigestMismatch`] — the same value-vocabulary invariant
/// [`normalize_stage_result`] enforces for verdicts, reused for study records.
///
/// # Errors
/// Returns [`InwardError::DigestMismatch`] when `result.subject != *displayed`.
pub fn normalize_study_result(displayed: &Digest, result: &StudyResult) -> Result<StudyCost, InwardError> {
    if result.subject != *displayed {
        return Err(InwardError::DigestMismatch { displayed: *displayed, claimed: result.subject });
    }
    Ok(result.cost)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_bloomery::{Digest, EvidenceKind, StageVerdict};

    use super::{InwardError, StageResult, normalize_stage_result};

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    #[test]
    fn a_verdict_over_the_displayed_digest_normalizes_to_evidence_naming_it() {
        let displayed = digest(1);
        let result = StageResult { subject: displayed, verdict: StageVerdict::Approved, detail: digest(9) };

        let evidence = normalize_stage_result(&displayed, &result).expect("matching subject normalizes");

        // The evidence names the exact displayed digest, and validates it.
        assert_eq!(evidence.subject, displayed);
        assert!(evidence.validates(&displayed));
        assert_eq!(evidence.kind, EvidenceKind::Approval);
        assert_eq!(evidence.detail, digest(9));
    }

    #[test]
    fn a_verdict_whose_digest_differs_is_rejected() {
        // The value-vocabulary invariant: evidence never validates a digest it
        // does not name. A verdict about digest(2) can never become evidence
        // bound to the displayed digest(1).
        let displayed = digest(1);
        let result = StageResult { subject: digest(2), verdict: StageVerdict::Approved, detail: digest(9) };

        let error = normalize_stage_result(&displayed, &result).unwrap_err();
        assert_eq!(error, InwardError::DigestMismatch { displayed, claimed: digest(2) });
    }

    #[test]
    fn verdict_variants_map_to_their_evidence_kinds() {
        let subject = digest(3);
        let cases = [
            (StageVerdict::Approved, EvidenceKind::Approval),
            (StageVerdict::VerificationPassed, EvidenceKind::VerificationResult),
            (StageVerdict::VerificationFailed, EvidenceKind::VerificationResult),
            (StageVerdict::ReviewFinding, EvidenceKind::ReviewFinding),
            (StageVerdict::Parked, EvidenceKind::Question),
        ];
        for (verdict, kind) in cases {
            let result = StageResult { subject, verdict, detail: digest(0) };
            let evidence = normalize_stage_result(&subject, &result).expect("matching subject");
            assert_eq!(evidence.kind, kind);
        }
    }

    mod study {
        use aether_bloomery::StudyCost;

        use super::super::{InwardError, StudyResult, normalize_study_result, parse_study_cost};
        use super::digest;

        // The gradeable columns of an `agent-usage-record.mjs` object for a
        // completed attempt.
        const WELL_FORMED: &[u8] = br#"{
            "schema": 1, "task": "implement", "ref": "3523", "conclusion": "success",
            "model": "claude-opus-4-8", "num_turns": 7, "cost_usd": 0.42, "duration_ms": 123456,
            "is_error": false, "input": 1000, "cache_write": 200, "cache_write_1h": 150,
            "cache_write_5m": 50, "cache_read": 8000, "output": 900,
            "result": {"num_turns": 7}
        }"#;

        #[test]
        fn a_well_formed_result_record_parses_and_binds_to_the_displayed_digest() {
            let cost = parse_study_cost(WELL_FORMED).expect("a well-formed record parses");
            // The dollar cost is carried as integral micro-USD, and every token
            // column round-trips off the object.
            assert_eq!(cost.cost_micro_usd, 420_000);
            assert_eq!(cost.turns, 7);
            assert_eq!(cost.duration_millis, 123_456);
            assert_eq!(cost.input_tokens, 1_000);
            assert_eq!(cost.cache_write_1h_tokens, 150);
            assert_eq!(cost.cache_read_tokens, 8_000);
            assert_eq!(cost.output_tokens, 900);

            let displayed = digest(1);
            let bound = normalize_study_result(&displayed, &StudyResult { subject: displayed, cost })
                .expect("a matching subject binds");
            assert_eq!(bound, cost);
        }

        #[test]
        fn a_record_claiming_the_wrong_digest_is_refused() {
            // The value-vocabulary invariant, reused for study records: a record
            // whose subject is not the displayed digest is refused, never rebound.
            let displayed = digest(1);
            let result = StudyResult { subject: digest(2), cost: StudyCost::default() };
            let error = normalize_study_result(&displayed, &result).unwrap_err();
            assert_eq!(error, InwardError::DigestMismatch { displayed, claimed: digest(2) });
        }

        #[test]
        fn a_dead_run_envelope_only_record_parses_to_zero_cost() {
            // A run that died before its terminal `result`: cost columns absent.
            // parse reads them as zero rather than failing — a dead attempt is
            // still legible study evidence.
            let dead = br#"{"schema": 1, "task": "implement", "ref": "3523", "no_result": true}"#;
            let cost = parse_study_cost(dead).expect("an envelope-only record still parses");
            assert_eq!(cost, StudyCost::default());
        }

        #[test]
        fn non_object_bytes_are_a_parse_error() {
            assert!(parse_study_cost(b"not json").is_err());
        }
    }
}
