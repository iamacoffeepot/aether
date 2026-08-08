//! The inward study-cost parser — the runner-output half of the inward channel.
//!
//! The small part of the inward channel that genuinely parses a runner's
//! output format: decoding the `scripts/agent-usage-record.mjs` JSON object
//! into gradeable [`StudyCost`] columns. The
//! platform-free stage-verdict vocabulary (`StageVerdict`, `StageResult`,
//! `InwardError`, `normalize_stage_result`, `StudyResult`,
//! `normalize_study_result`) lives in `aether-bloomery` as domain vocabulary;
//! this crate re-exports it for its own callers.

use core::fmt;
use std::error::Error;

use aether_bloomery::StudyCost;
use serde::Deserialize;

// Re-export the platform-free verdict vocabulary from the domain crate so
// existing callers through this crate keep compiling. The host must import
// directly from `aether_bloomery` instead.
pub use aether_bloomery::{
    InwardError, StageResult, StageVerdict, StudyResult, normalize_stage_result, normalize_study_result,
};

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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::parse_study_cost;

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
    fn a_well_formed_result_record_parses() {
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
    }

    #[test]
    fn a_dead_run_envelope_only_record_parses_to_zero_cost() {
        // A run that died before its terminal `result`: cost columns absent.
        // parse reads them as zero rather than failing — a dead attempt is
        // still legible study evidence.
        let dead = br#"{"schema": 1, "task": "implement", "ref": "3523", "no_result": true}"#;
        let cost = parse_study_cost(dead).expect("an envelope-only record still parses");
        assert_eq!(cost, aether_bloomery::StudyCost::default());
    }

    #[test]
    fn non_object_bytes_are_a_parse_error() {
        assert!(parse_study_cost(b"not json").is_err());
    }
}
