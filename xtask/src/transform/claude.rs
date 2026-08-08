//! The shared model-lane body: assemble the prompt, fork headless Claude,
//! and derive the result record both the `construct.implement` and
//! `review.critic` lanes run through.

use std::io::Write;
use std::process::{Command, Stdio};
use std::{fs, thread};

use anyhow::{Context, Result, bail};

use crate::transform::{TransformArgs, conventions};

/// The headless-Claude argv the `construct.implement` lane runs (#3511): `-p`
/// non-interactive, emitting the stream-json transcript the in-repo
/// result-record derivation reads. `--model` and `--effort` are the CLI's own
/// flags for the two axes an agent profile calibrates, each included only when
/// the caller resolved one — when the caller resolves neither, both are omitted
/// and `claude -p` falls back to the operator's ambient defaults (#3592). Pure
/// so the profile wiring is testable without spawning Claude; the assembled
/// prompt is piped on the child's stdin (not an argv positional).
fn construct_argv(model: Option<&str>, effort: Option<&str>) -> Vec<String> {
    let mut argv = vec!["-p".to_owned()];
    if let Some(model) = model {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    if let Some(effort) = effort {
        argv.push("--effort".to_owned());
        argv.push(effort.to_owned());
    }
    argv.push("--output-format".to_owned());
    argv.push("stream-json".to_owned());
    argv.push("--verbose".to_owned());
    argv
}

/// Assemble the headless-Claude prompt for the construct lane from the lane-owned
/// `instructions`, the subject tree's `conventions`, the checked-out `subject`,
/// and the work-order `task` — pure so the assembly is testable without spawning
/// Claude (#3572). The subject header names the exact sealed tree the worker is
/// on; the `## Task` section carries the operator's work-order description
/// (#3595) so the model is told *what* to build, not just *where*.
///
/// Both optional sections are presence-driven: a `None` task appends none (the
/// fail-legible path for a member with no persisted description), and `None`
/// conventions append none (a subject tree that carries no conventions file,
/// #4647). The order is deliberate — conventions are long and general, the work
/// order is short and specific, so the task stays last where the instructions
/// promise it.
pub(super) fn assemble_construct_prompt(
    instructions: &str,
    conventions: Option<&str>,
    subject: Option<&str>,
    task: Option<&str>,
) -> String {
    let subject_line = subject.map_or_else(
        || "You are working in the checked-out subject tree — the sealed source this work order named.".to_owned(),
        |subject| {
            format!(
                "You are working in the checked-out subject tree at commit `{subject}` — \
                 the exact sealed source this work order named.",
            )
        },
    );
    let conventions_section =
        conventions.map_or_else(String::new, |text| format!("\n{}\n", conventions::section(text)));
    let task_section = task.map_or_else(String::new, |task| format!("\n## Task\n\n{task}\n"));
    format!("{instructions}\n{conventions_section}\n## Subject\n\n{subject_line}\n{task_section}")
}

/// The last `max` bytes of `s`, snapped forward to a char boundary — for
/// carrying a bounded stderr tail into an operational-failure error.
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

/// Derive the construct lane's result record from the headless-Claude stream-json
/// `transcript` — the in-repo, node-free replacement (#3572) for the retired
/// `scripts/agent-usage-record.mjs` shell-out (#3565 deletes that script). Pure,
/// and faithful to the ledger derivation: the terminal `result` event carries
/// cost / turns / duration / usage, and the first non-haiku assistant call's usage
/// is the warm-resume cache-hit signal. A transcript with no terminal `result` (a
/// run that died early) yields a `no_result` record rather than an error, so
/// evidence is never dropped.
pub(super) fn derive_result_record(transcript: &str) -> serde_json::Value {
    use serde_json::{Map, Value, json};

    let mut result: Option<Value> = None;
    let mut first_main: Option<Value> = None;
    for line in transcript.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("result") => result = Some(event),
            Some("assistant") if first_main.is_none() => {
                let message = event.get("message").cloned().unwrap_or(Value::Null);
                let model = message.get("model").and_then(Value::as_str).unwrap_or_default();
                if model.contains("haiku") {
                    continue;
                }
                if let Some(usage) = message.get("usage") {
                    first_main = Some(
                        json!({ "model": message.get("model").cloned().unwrap_or(Value::Null), "usage": usage.clone() }),
                    );
                }
            }
            _ => {}
        }
    }

    let or_zero = |value: &Value, key: &str| value.get(key).cloned().unwrap_or_else(|| json!(0));
    let or_null = |value: &Value, key: &str| value.get(key).cloned().unwrap_or(Value::Null);

    let mut record = Map::new();
    record.insert("schema".to_owned(), json!(1));
    for field in ["task", "ref", "run_id", "conclusion", "model", "created_at", "pool"] {
        record.insert(field.to_owned(), Value::Null);
    }

    match &first_main {
        Some(first) => {
            let usage = first.get("usage").cloned().unwrap_or(Value::Null);
            record.insert("first_call_model".to_owned(), or_null(first, "model"));
            record.insert("first_call_cache_read".to_owned(), or_zero(&usage, "cache_read_input_tokens"));
            record.insert("first_call_cache_write".to_owned(), or_zero(&usage, "cache_creation_input_tokens"));
            record.insert("first_call_input".to_owned(), or_zero(&usage, "input_tokens"));
        }
        None => {
            for field in ["first_call_model", "first_call_cache_read", "first_call_cache_write", "first_call_input"] {
                record.insert(field.to_owned(), Value::Null);
            }
        }
    }

    let Some(result) = result else {
        // A run that died before the terminal record — legible, cost unknown.
        record.insert("no_result".to_owned(), json!(true));
        return Value::Object(record);
    };
    let usage = result.get("usage").cloned().unwrap_or(Value::Null);
    let cache_creation = usage.get("cache_creation").cloned().unwrap_or(Value::Null);
    record.insert("num_turns".to_owned(), or_null(&result, "num_turns"));
    record.insert("cost_usd".to_owned(), or_null(&result, "total_cost_usd"));
    record.insert("duration_ms".to_owned(), or_null(&result, "duration_ms"));
    record.insert("is_error".to_owned(), or_null(&result, "is_error"));
    record.insert("input".to_owned(), or_zero(&usage, "input_tokens"));
    record.insert("cache_write".to_owned(), or_zero(&usage, "cache_creation_input_tokens"));
    record.insert("cache_write_1h".to_owned(), or_zero(&cache_creation, "ephemeral_1h_input_tokens"));
    record.insert("cache_write_5m".to_owned(), or_zero(&cache_creation, "ephemeral_5m_input_tokens"));
    record.insert("cache_read".to_owned(), or_zero(&usage, "cache_read_input_tokens"));
    record.insert("output".to_owned(), or_zero(&usage, "output_tokens"));
    // The terminal record whole — keeps every meter on disk for downstream study.
    record.insert("result".to_owned(), result);
    Value::Object(record)
}

/// Fork headless Claude with `prompt` on stdin, capture its stream-json
/// transcript to `<out>/transcript.jsonl`, and derive the result record — the
/// shared body of both model lanes (`construct.implement` / `review.critic`).
pub(super) fn run_headless_claude(prompt: &str, args: &TransformArgs) -> Result<serde_json::Value> {
    fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out.display()))?;

    // Run headless Claude at the resolved model and reasoning effort — both the
    // CLI's own flags, so the calibrated profile reaches the child rather than an
    // env knob nothing on the other side reads.
    let mut claude = Command::new("claude");
    claude.args(construct_argv(args.model.as_deref(), args.effort.as_deref()));
    claude.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = claude.spawn().context("run headless claude")?;
    // Pipe the prompt on a separate thread and reap the child unconditionally: if
    // the write races an early exit (broken pipe) or the prompt outgrows the OS
    // pipe buffer, returning on the write error before `wait_with_output` would
    // drop `child` un-waited — `Child`'s `Drop` neither waits nor reaps — leaving a
    // zombie (or a live, still-billing process) behind. Waiting first, then
    // surfacing the writer's error, guarantees the child is reaped on every path.
    let mut stdin = child.stdin.take().context("headless claude stdin was not captured")?;
    let prompt_bytes = prompt.as_bytes().to_vec();
    // Infra thread in a build tool — no settlement/trace umbrella applies here;
    // it exists only to pipe stdin while the main thread reaps the child.
    #[allow(clippy::disallowed_methods)]
    let writer = thread::spawn(move || stdin.write_all(&prompt_bytes));
    let run = child.wait_with_output().context("await headless claude")?;
    // A non-zero exit is the CLI itself failing to run (auth, bad args, crash) —
    // an operational failure, distinct from a task-level error, which a completed
    // run records as `is_error` inside the transcript. Surface it rather than
    // writing an empty/garbage result record and reporting success. Check it
    // *before* the writer's result: an early child exit makes the stdin write race
    // a broken pipe, so joining the writer first would surface that downstream
    // symptom and mask the child's real exit cause.
    if !run.status.success() {
        bail!(
            "headless claude exited {}: {}",
            run.status.code().map_or_else(|| "by signal".to_owned(), |c| c.to_string()),
            tail(&String::from_utf8_lossy(&run.stderr), 1000),
        );
    }
    // The child exited zero, so a writer error here is an unexplained broken pipe
    // (or an OS-buffer overrun) worth propagating rather than a symptom of the exit
    // above.
    writer.join().expect("prompt-writer thread panicked").context("pipe the assembled prompt to headless claude")?;

    let transcript_path = args.out.join("transcript.jsonl");
    fs::write(&transcript_path, &run.stdout).with_context(|| format!("write {}", transcript_path.display()))?;

    // Derive the result record over the whole transcript, in-repo (no node
    // shell-out): the terminal `result` plus the first-call cache signal.
    Ok(derive_result_record(&String::from_utf8_lossy(&run.stdout)))
}

#[cfg(test)]
mod tests {
    use super::{construct_argv, derive_result_record, tail};

    #[test]
    fn tail_snaps_to_a_char_boundary_without_panicking() {
        assert_eq!(tail("short", 10), "short", "under the cap returns the whole string");
        assert_eq!(tail("abcdef", 3), "def", "the ascii byte cut is already a char boundary");
        // "aébc" is 5 bytes; the byte cut (5 - 3 = byte 2) lands inside the
        // 2-byte 'é'. Snapping forward to byte 3 drops the partial char rather
        // than slicing mid-char and panicking.
        assert_eq!(tail("aébc", 3), "bc");
    }

    // Tripwire: both calibrated axes reach the child as CLI flags. The effort used
    // to be exported as an `AETHER_CONSTRUCT_EFFORT` env var that neither this
    // repository nor the Claude Code CLI reads, so the lane ran at the operator's
    // ambient effort however the profile was calibrated (#4324).
    #[test]
    fn construct_argv_carries_the_resolved_model_and_effort_and_stream_json() {
        let argv = construct_argv(Some("claude-opus-4-8"), Some("high"));
        assert_eq!(argv.first().map(String::as_str), Some("-p"), "headless, non-interactive");
        let model_at = argv.iter().position(|a| a == "--model").expect("argv pins the model");
        assert_eq!(argv[model_at + 1], "claude-opus-4-8", "the resolved model rides argv");
        let effort_at = argv.iter().position(|a| a == "--effort").expect("argv pins the effort tier");
        assert_eq!(argv[effort_at + 1], "high", "the resolved effort rides argv, not an unread env var");
        // The stream-json transcript is what the result-record derivation reads.
        assert!(argv.windows(2).any(|w| w == ["--output-format", "stream-json"]), "emits the stream-json transcript");
    }

    #[test]
    fn construct_argv_omits_the_profile_flags_when_none_falls_back_to_ambient() {
        let argv = construct_argv(None, None);
        assert_eq!(argv.first().map(String::as_str), Some("-p"), "headless, non-interactive");
        assert!(!argv.iter().any(|a| a == "--model"), "no resolved model means no --model flag (ambient default)");
        assert!(!argv.iter().any(|a| a == "--effort"), "no resolved effort means no --effort flag (ambient default)");
        assert!(
            argv.windows(2).any(|w| w == ["--output-format", "stream-json"]),
            "still emits the stream-json transcript"
        );
    }

    // The result record is derived in-repo from the stream-json transcript — the
    // node-free replacement (#3572) for the `agent-usage-record.mjs` shell-out.
    #[test]
    fn result_record_derives_the_terminal_result_and_first_call_from_the_transcript() {
        let transcript = concat!(
            r#"{"type":"assistant","message":{"model":"claude-3-5-haiku","usage":{"input_tokens":9}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":100,"cache_read_input_tokens":40,"cache_creation_input_tokens":7}}}"#,
            "\n",
            "\n",
            r#"{"type":"result","num_turns":3,"total_cost_usd":0.42,"is_error":false,"usage":{"input_tokens":1000,"output_tokens":200,"cache_read_input_tokens":50,"cache_creation":{"ephemeral_1h_input_tokens":11,"ephemeral_5m_input_tokens":22}}}"#,
        );
        let record = derive_result_record(transcript);
        assert_eq!(record["schema"], 1);
        assert_eq!(record["num_turns"], 3);
        assert_eq!(record["cost_usd"], 0.42);
        assert_eq!(record["is_error"], false);
        assert_eq!(record["output"], 200);
        assert_eq!(record["cache_write_5m"], 22, "the ephemeral-5m split is flattened out of cache_creation");
        // The first non-haiku assistant call is the warm-resume cache signal; the
        // haiku line before it is skipped.
        assert_eq!(record["first_call_model"], "claude-opus-4-8");
        assert_eq!(record["first_call_cache_read"], 40);
        assert_eq!(record["result"]["num_turns"], 3, "the terminal record is carried whole");

        // A transcript with no terminal `result` is a legible `no_result` row,
        // never an error — evidence is never dropped.
        let died_early = r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":5}}}"#;
        let partial = derive_result_record(died_early);
        assert_eq!(partial["no_result"], true);
        assert_eq!(partial["first_call_input"], 5);
        assert!(partial.get("cost_usd").is_none(), "a died-early row carries no cost columns");
    }
}
