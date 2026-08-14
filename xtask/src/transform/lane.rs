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
use std::process::{Command, ExitStatus, Output};
use std::{fs, io};

use anyhow::{Context, Result, bail};

use crate::transform::TransformArgs;
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
/// `session` is the handle a later lap resumes this run's conversation with —
/// the codex arm's `thread_id`, the muse arm's session uuid — under the
/// `session_id` key the Anthropic-Messages derivation already writes, because
/// the session pool reads one key whichever arm produced the record.
///
/// `cost_usd` is always null: neither non-Claude harness reports a price, and
/// zero would read as free rather than unmeasured.
pub(super) fn record(terminal: Option<Terminal>, session: Option<String>) -> serde_json::Value {
    use serde_json::{Map, Value, json};

    let mut record = Map::new();
    record.insert("schema".to_owned(), json!(1));
    // Ahead of the `no_result` return below: a run that died mid-lap still has a
    // session to resume, and a retry that had to relaunch cold because the
    // handle was dropped here is exactly the spend the pool exists to avoid.
    record.insert("session_id".to_owned(), session.map_or(Value::Null, Value::String));
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
        "calls",
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
pub(super) fn capture(command: Command, out: &Path, harness: &str, peak: &PeakMemory) -> Result<String> {
    let run = execute(command, out, harness, peak)?;
    exit_check(&run, harness)?;
    Ok(String::from_utf8_lossy(&run.stdout).into_owned())
}

/// [`capture`] for a run whose argv carried a resume handle.
///
/// `Ok(None)` is its one extra outcome: the CLI refused the handle before
/// starting a billed turn, which the arm answers by relaunching cold. Every
/// other non-zero exit still fails the lane, because a crash that had already
/// spent tokens must not be paid for twice.
pub(super) fn capture_resumed(
    command: Command,
    out: &Path,
    harness: &str,
    peak: &PeakMemory,
) -> Result<Option<String>> {
    let run = execute(command, out, harness, peak)?;
    if resume_handle_rejected(run.status, &run.stdout, &run.stderr) {
        return Ok(None);
    }
    exit_check(&run, harness)?;
    Ok(Some(String::from_utf8_lossy(&run.stdout).into_owned()))
}

/// Spawn `command`, read its peak, and write its stdout to
/// `<out>/transcript.jsonl` — everything both capture entry points do before
/// they part ways over how the exit is read.
///
/// The transcript is written before either caller judges the exit, so a failed
/// run still leaves whatever it managed to emit on disk for an operator to read.
/// `peak` is read first for the same reason: a run that died still peaked at
/// something, and the reading is what the concurrency model is calibrated from
/// either way (#4912).
fn execute(mut command: Command, out: &Path, harness: &str, peak: &PeakMemory) -> Result<Output> {
    fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    let run = command.output().map_err(|error| spawn_context(error, harness))?;
    peak.observe(&run.stderr);

    let path = out.join("transcript.jsonl");
    fs::write(&path, &run.stdout).with_context(|| format!("write {}", path.display()))?;
    Ok(run)
}

/// Fail the lane when the harness exited non-zero, naming a bounded stderr tail.
fn exit_check(run: &Output, harness: &str) -> Result<()> {
    if !run.status.success() {
        bail!(
            "{harness} exited {}: {}",
            run.status.code().map_or_else(|| "by signal".to_owned(), |code| code.to_string()),
            tail(&String::from_utf8_lossy(&run.stderr), 1000),
        );
    }
    Ok(())
}

/// A copy of `args` with the resume handle cleared — the fresh-launch fallback
/// an arm relaunches under when its harness rejects the handle.
pub(super) fn without_resume(args: &TransformArgs) -> TransformArgs {
    let mut args = args.clone();
    args.resume = None;
    args
}

/// Did the CLI refuse the resume handle *before* starting a billed turn?
///
/// A non-zero exit after the CLI emitted a transcript is an operational
/// failure (auth, crash, SIGKILL) — not a missing session — and must not
/// launch a second full-cost cold run. Spawn failures never reach here.
///
/// Shared across the arms deliberately: each harness spells its refusal
/// differently, but the conservative shape that makes degrading safe — an empty
/// transcript plus a stderr that both names the handle and says it was refused —
/// is the same judgement, and a per-arm copy would drift into a looser one.
pub(super) fn resume_handle_rejected(status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> bool {
    if status.success() || !stdout.is_empty() {
        return false;
    }
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    let names_the_handle = stderr.contains("session")
        || stderr.contains("resume")
        || stderr.contains("conversation")
        || stderr.contains("thread");
    let rejected = stderr.contains("not found")
        || stderr.contains("unknown")
        || stderr.contains("invalid")
        || stderr.contains("no conversation");
    names_the_handle && rejected
}

/// The prompt a resumed lap receives: the assembled prompt plus the one fact the
/// resumed conversation cannot see for itself — the working tree was reset
/// between laps, so the files it edited last time are gone. A cold launch gets
/// the prompt unchanged.
pub(super) fn resumed_prompt(prompt: &str, resume: Option<&str>) -> String {
    if resume.is_none() {
        return prompt.to_owned();
    }
    format!(
        "{prompt}\nThe working tree was reset since the previous attempt; do not assume files you edited last time are still there.\n"
    )
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
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::{Terminal, Usage, record, resume_handle_rejected, resumed_prompt};

    // Tripwire: the envelope's two cross-lane fields. `result_record.is_error`
    // is what the construct lane's completion gate tests `== Some(false)`, and
    // `result.result` is where the review lane finds the critic's VERDICT line.
    // A record that renamed or dropped either fails the review gate closed,
    // which surfaces as a critic finding rather than a harness bug.
    #[test]
    fn the_envelope_carries_the_two_fields_the_lanes_read() {
        let clean = record(
            Some(Terminal { is_error: false, text: "all pillars clean.\nVERDICT: pass".to_owned(), usage: None }),
            None,
        );
        assert_eq!(clean["is_error"], false, "the construct gate reads this");
        assert_eq!(clean["result"]["is_error"], false, "the review gate reads this");
        assert_eq!(clean["result"]["result"], "all pillars clean.\nVERDICT: pass", "the verdict text");

        let errored = record(Some(Terminal { is_error: true, text: String::new(), usage: None }), None);
        assert_eq!(errored["is_error"], true);
        assert_eq!(errored["result"]["is_error"], true);
    }

    // Tripwire: the session handle rides the same `session_id` key the
    // Anthropic-Messages derivation writes, because the pool's deposit reads
    // exactly one key whichever arm produced the record — a handle parked under
    // an arm-specific name is a handle the pool never stores, so every lap
    // relaunches cold at full price while reporting success.
    #[test]
    fn the_session_handle_rides_the_key_the_pool_reads() {
        let terminal = || Some(Terminal { is_error: false, text: "ok".to_owned(), usage: None });
        assert_eq!(record(terminal(), Some("019f-thread".to_owned()))["session_id"], "019f-thread");
        assert!(record(terminal(), None)["session_id"].is_null(), "an arm that names no session says so");

        // A run that died mid-lap is exactly the one a later lap wants to
        // resume, so the handle survives the `no_result` row.
        let partial = record(None, Some("019f-thread".to_owned()));
        assert_eq!(partial["no_result"], true);
        assert_eq!(partial["session_id"], "019f-thread");
    }

    #[test]
    fn a_missing_session_is_a_resume_reject_and_a_crash_after_tokens_is_not() {
        // Tripwire: a non-zero exit used to relaunch cold whenever a resume
        // handle was on argv, so an auth failure or a crash that had already
        // billed doubled the spend. Only a handle the CLI refused *before*
        // emitting a transcript degrades.
        let failed = ExitStatus::from_raw(1 << 8);
        assert!(resume_handle_rejected(failed, b"", b"No conversation found with session ID sess-1"));
        assert!(resume_handle_rejected(failed, b"", b"error: unknown session"));
        assert!(
            resume_handle_rejected(failed, b"", b"thread 019f not found"),
            "codex addresses its session as a thread"
        );
        assert!(
            !resume_handle_rejected(failed, br#"{"type":"result"}"#, b"No conversation found"),
            "a transcript means the CLI ran — do not double-spend"
        );
        assert!(
            !resume_handle_rejected(failed, b"", b"authentication failed"),
            "an auth failure is not a missing session file"
        );
        assert!(!resume_handle_rejected(ExitStatus::from_raw(0), b"", b"No conversation found"));
    }

    // The reset tree is the one thing a resumed conversation is wrong about by
    // default: it remembers editing files the lap-boundary reset removed, and a
    // lap that trusts that memory reports work it never redid.
    #[test]
    fn only_a_resumed_lap_is_told_its_tree_was_reset() {
        assert_eq!(resumed_prompt("build it", None), "build it", "a cold launch gets the prompt unchanged");
        assert!(resumed_prompt("build it", Some("sess-1")).starts_with("build it\n"));
        assert!(resumed_prompt("build it", Some("sess-1")).contains("working tree was reset"));
    }

    // A harness that reports no counts renders them null, not zero: a study
    // grading such a bloom must read "unmeasured" rather than "free".
    #[test]
    fn an_unmetered_harness_renders_null_columns_never_zero() {
        let unmetered = record(Some(Terminal { is_error: false, text: "ok".to_owned(), usage: None }), None);
        for column in ["cost_usd", "input", "output", "cache_read", "cache_write"] {
            assert!(unmetered[column].is_null(), "{column} must be null, not zero, when unmeasured");
        }

        let metered = record(
            Some(Terminal {
                is_error: false,
                text: "ok".to_owned(),
                usage: Some(Usage { input: 16147, cache_read: 11008, cache_write: 0, output: 5 }),
            }),
            None,
        );
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
        let partial = record(None, None);
        assert_eq!(partial["no_result"], true);
        assert!(partial.get("is_error").is_none(), "no is_error means the construct gate fails closed");
        assert!(partial.get("result").is_none(), "and the review lane finds no verdict text");
    }
}
