//! What every model-lane harness arm shares: the spawn-and-capture, and the
//! result-record envelope the lanes read.
//!
//! The envelope is the load-bearing contract. Two consumers pin its shape and
//! neither knows which harness produced it:
//!
//! - the construct lane's completion gate reads `result_record.is_error`
//!   (`LocalExecutor::stream_evidence`, #3596),
//! - the review lane reads `result.result` for the critic's `VERDICT:` line.
//!
//! An arm that returned a differently-shaped record would fail the review gate
//! closed, which reads as a critic *finding* rather than a harness bug — so the
//! arms converge here rather than each assembling their own.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fs, io};

use anyhow::{Context, Result, bail};

use crate::transform::peak_memory::PeakMemory;

/// What a harness reported when its run ended.
pub(super) struct Terminal {
    /// Whether the run ended in error. The construct gate demands `false`.
    pub is_error: bool,
    /// The run's final message text — what the review lane parses its
    /// `VERDICT:` line out of.
    pub text: String,
    /// The token counts the harness reported, or `None` when it reports none.
    /// `None` renders the token columns null rather than zero, so a study reads
    /// "unmeasured" instead of "free".
    pub usage: Option<Usage>,
}

/// The token counts a harness reported for a run.
pub(super) struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

/// Assemble the result-record envelope from a harness's `terminal`, or the
/// `no_result` record when the run died before reporting one.
///
/// `cost_usd` is always null: neither non-Claude harness reports a price, and
/// zero would read as free rather than unmeasured.
pub(super) fn record(terminal: Option<Terminal>) -> serde_json::Value {
    use serde_json::{Map, Value, json};

    let mut record = Map::new();
    record.insert("schema".to_owned(), json!(1));
    // The ledger columns a non-Claude harness cannot fill. Present and null so a
    // downstream reader sees the same key set whichever arm ran.
    for field in [
        "task",
        "ref",
        "run_id",
        "conclusion",
        "model",
        "created_at",
        "pool",
        "first_call_model",
        "first_call_cache_read",
        "first_call_cache_write",
        "first_call_input",
    ] {
        record.insert(field.to_owned(), Value::Null);
    }

    let Some(terminal) = terminal else {
        // A run that died before reporting a terminal — legible, cost unknown.
        // Deliberately carries no `is_error`, so the construct gate's
        // `== Some(false)` test fails closed exactly as it does for Claude.
        record.insert("no_result".to_owned(), json!(true));
        return Value::Object(record);
    };

    record.insert("num_turns".to_owned(), Value::Null);
    record.insert("cost_usd".to_owned(), Value::Null);
    record.insert("duration_ms".to_owned(), Value::Null);
    record.insert("is_error".to_owned(), json!(terminal.is_error));
    let (input, cache_read, cache_write, output) =
        terminal.usage.map_or((Value::Null, Value::Null, Value::Null, Value::Null), |usage| {
            (json!(usage.input), json!(usage.cache_read), json!(usage.cache_write), json!(usage.output))
        });
    record.insert("input".to_owned(), input);
    record.insert("cache_read".to_owned(), cache_read);
    record.insert("cache_write".to_owned(), cache_write);
    record.insert("cache_write_1h".to_owned(), Value::Null);
    record.insert("cache_write_5m".to_owned(), Value::Null);
    record.insert("output".to_owned(), output);
    // The nested terminal the review lane reads its verdict text out of, shaped
    // like the Claude arm's carried-whole `result` event.
    record.insert("result".to_owned(), json!({ "is_error": terminal.is_error, "result": terminal.text }));
    Value::Object(record)
}

/// Write the assembled `prompt` to `<out>/prompt.md` and return its path — both
/// non-Claude arms hand their child a prompt file rather than piping stdin, so
/// neither repeats the pipe-on-a-thread dance the Claude arm needs, and the
/// exact prompt a run received stays on disk beside its transcript.
pub(super) fn write_prompt(out: &Path, prompt: &str) -> Result<PathBuf> {
    fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    let path = out.join("prompt.md");
    fs::write(&path, prompt).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Run `command` to completion, capture its stdout to `<out>/transcript.jsonl`,
/// and return the captured text.
///
/// A non-zero exit is the CLI itself failing to run (auth, bad args, crash) — an
/// operational failure, distinct from a task-level error, which a completed run
/// records inside its transcript. It is surfaced rather than folded into an
/// empty record reported as success.
///
/// `peak` reads the run's peak memory off its stderr, where the host's wrapper
/// reports it (#4912) — before the exit check, because a run that died still
/// peaked at something and the reading is what the concurrency model is
/// calibrated from either way.
pub(super) fn capture(mut command: Command, out: &Path, harness: &str, peak: &PeakMemory) -> Result<String> {
    fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    let run = command.output().map_err(|error| spawn_context(error, harness))?;
    peak.observe(&run.stderr);
    let transcript = String::from_utf8_lossy(&run.stdout).into_owned();

    // Write the transcript before the exit check, so a failed run still leaves
    // whatever it managed to emit on disk for an operator to read.
    let path = out.join("transcript.jsonl");
    fs::write(&path, &run.stdout).with_context(|| format!("write {}", path.display()))?;

    if !run.status.success() {
        bail!(
            "{harness} exited {}: {}",
            run.status.code().map_or_else(|| "by signal".to_owned(), |code| code.to_string()),
            tail(&String::from_utf8_lossy(&run.stderr), 1000),
        );
    }
    Ok(transcript)
}

// A missing harness binary is the failure an operator hits first when a stage is
// calibrated onto a CLI their machine does not have, so it is named rather than
// left as a bare "No such file or directory".
fn spawn_context(error: io::Error, harness: &str) -> anyhow::Error {
    if error.kind() == io::ErrorKind::NotFound {
        return anyhow::anyhow!(
            "`{harness}` is not on PATH — this stage is calibrated onto a harness this worker lacks"
        );
    }
    anyhow::Error::new(error).context(format!("run {harness}"))
}

/// The last `max` bytes of `s`, snapped forward to a char boundary — a bounded
/// stderr tail for an operational failure.
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    use super::{Terminal, Usage, record};

    // Tripwire: the envelope's two cross-lane fields. `result_record.is_error`
    // is what the construct lane's completion gate tests `== Some(false)`, and
    // `result.result` is where the review lane finds the critic's VERDICT line.
    // A record that renamed or dropped either fails the review gate closed,
    // which surfaces as a critic finding rather than a harness bug.
    #[test]
    fn the_envelope_carries_the_two_fields_the_lanes_read() {
        let clean = record(Some(Terminal {
            is_error: false,
            text: "all pillars clean.\nVERDICT: pass".to_owned(),
            usage: None,
        }));
        assert_eq!(clean["is_error"], false, "the construct gate reads this");
        assert_eq!(clean["result"]["is_error"], false, "the review gate reads this");
        assert_eq!(clean["result"]["result"], "all pillars clean.\nVERDICT: pass", "the verdict text");

        let errored = record(Some(Terminal { is_error: true, text: String::new(), usage: None }));
        assert_eq!(errored["is_error"], true);
        assert_eq!(errored["result"]["is_error"], true);
    }

    // A harness that reports no counts renders them null, not zero: a study
    // grading such a bloom must read "unmeasured" rather than "free".
    #[test]
    fn an_unmetered_harness_renders_null_columns_never_zero() {
        let unmetered = record(Some(Terminal { is_error: false, text: "ok".to_owned(), usage: None }));
        for column in ["cost_usd", "input", "output", "cache_read", "cache_write"] {
            assert!(unmetered[column].is_null(), "{column} must be null, not zero, when unmeasured");
        }

        let metered = record(Some(Terminal {
            is_error: false,
            text: "ok".to_owned(),
            usage: Some(Usage { input: 16147, cache_read: 11008, cache_write: 0, output: 5 }),
        }));
        assert_eq!(metered["input"], 16147);
        assert_eq!(metered["output"], 5);
        assert_eq!(metered["cache_write"], 0, "a reported zero is a zero");
        assert!(metered["cost_usd"].is_null(), "no harness here reports a price");
    }

    // A run that died before its terminal is a legible `no_result` row carrying
    // no `is_error`, so the construct gate's `== Some(false)` test fails closed —
    // the same shape the Claude arm produces for a died-early run.
    #[test]
    fn a_died_early_run_is_a_no_result_row_that_fails_the_gate_closed() {
        let partial = record(None);
        assert_eq!(partial["no_result"], true);
        assert!(partial.get("is_error").is_none(), "no is_error means the construct gate fails closed");
        assert!(partial.get("result").is_none(), "and the review lane finds no verdict text");
    }
}
