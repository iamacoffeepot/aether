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

use aether_bloomery::{StudyCall, StudyCost};
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
    num_turns: Option<u64>,
    #[serde(default)]
    duration_ms: Option<u64>,
    // Every token column is `Option`, not a `#[serde(default)] u64`, because
    // the xtask lanes render an unreported column as an explicit JSON `null`
    // rather than omitting the key — they keep the same key set whichever
    // harness arm ran. `#[serde(default)]` fills a *missing* field and does
    // nothing for a present `null`, so a non-optional column here fails the
    // whole parse on the record shape the lanes actually emit; `cache_write_1h`
    // and `cache_write_5m` are unconditionally null outside the Claude arm, so
    // that failure would be total rather than occasional.
    #[serde(default)]
    input: Option<u64>,
    #[serde(default)]
    cache_write: Option<u64>,
    #[serde(default)]
    cache_write_1h: Option<u64>,
    #[serde(default)]
    cache_write_5m: Option<u64>,
    #[serde(default)]
    cache_read: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
    // Present when the messages-format arms derived per-call usage; null or
    // absent on every other harness, and on any record sealed before the
    // long-context band needed it.
    #[serde(default)]
    calls: Option<Vec<CallJson>>,
}

#[derive(Deserialize)]
struct CallJson {
    #[serde(default)]
    input: Option<u64>,
    #[serde(default)]
    cache_write: Option<u64>,
    #[serde(default)]
    cache_write_1h: Option<u64>,
    #[serde(default)]
    cache_write_5m: Option<u64>,
    #[serde(default)]
    cache_read: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

/// Parse a runner result record's JSON bytes into the gradeable [`StudyCost`]
/// columns. Pure shape translation — the binding to an attempt is
/// [`normalize_study_result`]'s job.
///
/// # Errors
/// Returns [`StudyRecordError::Parse`] when `json` is not a decodable object.
pub fn parse_study_cost(json: &[u8]) -> Result<StudyCost, StudyRecordError> {
    Ok(parse_study(json)?.0)
}

/// Parse a runner result record into the aggregate cost columns and, when the
/// record carried them, the per-call usages a long-context band selects on.
///
/// # Errors
/// Returns [`StudyRecordError::Parse`] when `json` is not a decodable object.
pub fn parse_study(json: &[u8]) -> Result<(StudyCost, Option<Vec<StudyCall>>), StudyRecordError> {
    let raw: ResultRecordJson =
        serde_json::from_slice(json).map_err(|error| StudyRecordError::Parse(error.to_string()))?;
    let cost = StudyCost {
        // Never parsed: the price is computed from the token columns against a
        // sealed `PriceTable` (#4679), because a harness's self-reported bottom
        // line is its own cost model and two of them are not comparable.
        cost_micro_usd: 0,
        turns: raw.num_turns.unwrap_or(0),
        duration_millis: raw.duration_ms.unwrap_or(0),
        input_tokens: raw.input.unwrap_or(0),
        cache_write_tokens: raw.cache_write.unwrap_or(0),
        cache_write_1h_tokens: raw.cache_write_1h.unwrap_or(0),
        cache_write_5m_tokens: raw.cache_write_5m.unwrap_or(0),
        cache_read_tokens: raw.cache_read.unwrap_or(0),
        output_tokens: raw.output.unwrap_or(0),
    };
    let calls = raw.calls.filter(|calls| !calls.is_empty()).map(|calls| {
        calls
            .into_iter()
            .map(|call| StudyCall {
                input_tokens: call.input.unwrap_or(0),
                cache_write_tokens: call.cache_write.unwrap_or(0),
                cache_write_1h_tokens: call.cache_write_1h.unwrap_or(0),
                cache_write_5m_tokens: call.cache_write_5m.unwrap_or(0),
                cache_read_tokens: call.cache_read.unwrap_or(0),
                output_tokens: call.output.unwrap_or(0),
            })
            .collect()
    });
    Ok((cost, calls))
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
        // The record's own `cost_usd` is deliberately ignored — pricing is the
        // `PriceTable`'s job, applied to these token columns.
        assert_eq!(cost.cost_micro_usd, 0, "a self-reported bottom line is never trusted");
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
    fn an_unreported_column_is_null_rather_than_absent_and_still_parses() {
        // Tripwire: the shape `xtask`'s non-Claude lanes actually emit. They
        // keep one key set whichever harness arm ran, so an unreported column
        // is present-and-null, not missing — and `cache_write_1h` / `_5m` are
        // null unconditionally outside the Claude arm. A `#[serde(default)]`
        // over a non-`Option` column fills only a *missing* field, so pinning
        // this shape is what keeps the study lane from failing every parse on
        // every muse and codex attempt (the two arms it exists to measure).
        let nulled = br#"{
            "schema": 1, "num_turns": null, "cost_usd": null, "duration_ms": null,
            "is_error": false, "input": 1000, "cache_write": null, "cache_write_1h": null,
            "cache_write_5m": null, "cache_read": 8000, "output": 900
        }"#;
        let cost = parse_study_cost(nulled).expect("a null-column record parses");

        assert_eq!(cost.input_tokens, 1_000, "a reported column survives its null siblings");
        assert_eq!(cost.output_tokens, 900);
        assert_eq!(cost.cache_write_1h_tokens, 0, "an unreported split reads as zero, not a parse failure");
    }

    #[test]
    fn non_object_bytes_are_a_parse_error() {
        assert!(parse_study_cost(b"not json").is_err());
    }

    #[test]
    fn a_record_with_per_call_usage_keeps_the_calls_off_the_aggregate() {
        // The ledger bands per call. A record that only summed these two into
        // the aggregate columns would force the price table to pick one band
        // for both, which is the overcount the call list exists to prevent.
        let json = br#"{
            "input": 350000, "output": 2000,
            "calls": [
                {"input": 100000, "output": 1000},
                {"input": 250000, "output": 1000, "cache_read": 0}
            ]
        }"#;
        let (cost, calls) = super::parse_study(json).expect("a two-call record parses");
        assert_eq!(cost.input_tokens, 350_000, "the aggregate is still the dispatch total");
        let calls = calls.expect("the call list survived");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].input_tokens, 100_000);
        assert_eq!(calls[1].prompt_tokens(), 250_000);
    }
}
