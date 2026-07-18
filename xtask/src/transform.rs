//! `cargo xtask transform` — ADR-0149 §Execution's portable execution
//! unit: a typed `command` id maps to the exact invocation the lane runs,
//! executes it, and writes nonce-tagged evidence bytes a broker can
//! validate. Two lanes share this entrypoint:
//!
//! - The **mechanical verify lane** (`verify.fmt` / `verify.clippy` /
//!   `verify.docs`, #3501) — a zero-secret cargo invocation byte-for-byte
//!   with CI. `verify.test` is deliberately out of scope (CI's test lane
//!   pre-builds with `cargo xtask dist` under a heavier toolchain).
//! - The **model-driven construct lane** (`construct.implement`, #3511) —
//!   runs headless Claude at the resolved model + reasoning effort against the
//!   checked-out **subject** tree, and writes the nonce-tagged **result record**
//!   (cost / tokens / turns) derived in-repo from the run transcript (#3572; the
//!   lane no longer shells out to `scripts/agent-usage-record.mjs`, which #3565
//!   deletes). The lane assembles its prompt from its own in-repo instruction
//!   source (`construct_instructions.md`) plus the subject — it owns its process
//!   natively rather than delegating to `.claude/skills/implement`. Unlike the
//!   verify lane it needs a credential, so it runs **worker-side** (BYO); the
//!   coordinator never sees it.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::{fs, process, thread};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Serialize;

#[derive(Args)]
pub struct TransformArgs {
    /// Typed command id — a `verify.*` mechanical id or `construct.implement`.
    command: String,
    /// Directory evidence bytes are written to (created if missing).
    #[arg(long)]
    out: PathBuf,
    /// Idempotency nonce the broker matches against the work order,
    /// stamped into `evidence.json`.
    #[arg(long)]
    nonce: Option<String>,
    /// The git commit this attempt's worker checked out — the sealed subject the
    /// `construct.implement` lane builds against (#3572). Threaded end-to-end from
    /// the executor's `subject` dispatch input; named in the assembled prompt so
    /// the transcript records which tree the work ran on. Ignored by the verify
    /// lane.
    #[arg(long)]
    subject: Option<String>,
    /// The model the `construct.implement` lane runs headless Claude under —
    /// the effective model the coordinator resolved from the sealed
    /// scope-revision (#3511). Ignored by the verify lane.
    #[arg(long)]
    model: Option<String>,
    /// The reasoning-effort tier the `construct.implement` lane runs at (the
    /// resolved effort, #3511). Ignored by the verify lane.
    #[arg(long)]
    effort: Option<String>,
    /// The advisory, human-readable work-order description the
    /// `construct.implement` lane names in its prompt's `## Task` section (#3595)
    /// — the operator-supplied text the coordinator persisted at seal and the
    /// executor threaded onto the dispatch. Absent when none was persisted (a
    /// subject-only prompt); ignored by the verify lane.
    #[arg(long)]
    task: Option<String>,
}

/// One CI-mirroring cargo invocation for a `verify.*` command id.
struct VerifyInvocation {
    program: &'static str,
    args: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
}

/// Maps a typed `verify.*` command id to the exact cargo invocation
/// `ci.yml`'s `fmt` / `clippy` / `docs` jobs run.
///
/// Tripwire: these argv + env pins are CI-parity invariants — a drift
/// here means this entrypoint no longer proves the laptop/Actions
/// invocation symmetry ADR-0149 §Execution requires. `verify.test` is
/// deliberately absent, not merely unrecognized: it names the one
/// command this slice explicitly declines to cover.
fn verify_command(id: &str) -> Option<VerifyInvocation> {
    match id {
        "verify.fmt" => Some(VerifyInvocation { program: "cargo", args: &["fmt", "--all", "--", "--check"], env: &[] }),
        "verify.clippy" => Some(VerifyInvocation {
            program: "cargo",
            args: &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"],
            env: &[],
        }),
        "verify.docs" => Some(VerifyInvocation {
            program: "cargo",
            args: &["doc", "--workspace", "--no-deps"],
            env: &[(
                "RUSTDOCFLAGS",
                "-D rustdoc::redundant_explicit_links -D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links",
            )],
        }),
        _ => None,
    }
}

/// The typed id of the model-driven construct lane (#3511). Recognized here so
/// an unknown id stays unmapped exactly as in the verify lane.
const CONSTRUCT_IMPLEMENT: &str = "construct.implement";

/// The headless-Claude argv the `construct.implement` lane runs (#3511): `-p`
/// non-interactive, emitting the stream-json transcript the in-repo
/// result-record derivation reads. `--model` is included only when `model` is
/// `Some` — when the caller resolves no model, the flag is omitted and `claude
/// -p` falls back to the operator's ambient default model (#3592). Pure so the
/// model wiring is testable without spawning Claude; the reasoning effort rides
/// the `AETHER_CONSTRUCT_EFFORT` env (a worker-side knob), not an argv flag, and
/// the assembled prompt is piped on the child's stdin (not an argv positional).
fn construct_argv(model: Option<&str>) -> Vec<String> {
    let mut argv = vec!["-p".to_owned()];
    if let Some(model) = model {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    argv.push("--output-format".to_owned());
    argv.push("stream-json".to_owned());
    argv.push("--verbose".to_owned());
    argv
}

/// The lane-owned in-repo instruction source (#3572). Embedded at build time so
/// the construct lane owns its process natively — the prompt is assembled from
/// this text, never from `.claude/skills/implement` in the worker's checkout.
const CONSTRUCT_INSTRUCTIONS: &str = include_str!("construct_instructions.md");

/// Assemble the headless-Claude prompt for the construct lane from the lane-owned
/// `instructions`, the checked-out `subject`, and the work-order `task` — pure so
/// the assembly is testable without spawning Claude (#3572). The subject header
/// names the exact sealed tree the worker is on; the `## Task` section carries the
/// operator's work-order description (#3595) so the model is told *what* to build,
/// not just *where*. A `None` task appends no section — the subject-only prompt
/// still stands (the fail-legible path for a member with no persisted description).
fn assemble_construct_prompt(instructions: &str, subject: Option<&str>, task: Option<&str>) -> String {
    let subject_line = subject.map_or_else(
        || "You are working in the checked-out subject tree — the sealed source this work order named.".to_owned(),
        |subject| {
            format!(
                "You are working in the checked-out subject tree at commit `{subject}` — \
                 the exact sealed source this work order named.",
            )
        },
    );
    let task_section = task.map_or_else(String::new, |task| format!("\n## Task\n\n{task}\n"));
    format!("{instructions}\n\n## Subject\n\n{subject_line}\n{task_section}")
}

/// `<out>/evidence.json` schema for the verify lane — the untrusted claim a
/// broker validates by `nonce` and re-checks against `status`.
#[derive(Serialize)]
struct Evidence {
    command: String,
    nonce: Option<String>,
    status: &'static str,
    exit_code: Option<i32>,
    log: String,
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

/// Stamp the broker-matched `nonce`, the command id, and the candidate-produced
/// signal onto the derived result `record`, producing the construct lane's
/// evidence envelope. `produced_candidate` records whether the run left a
/// candidate change in the working tree (#3596) — the completion gate reads it
/// alongside the terminal-`result`/`is_error` signal to demand a substantive
/// conclusion, not mere non-error. Pure so the binding is testable without
/// running Claude or git (#3572).
fn stamp_construct_evidence(
    nonce: Option<&str>,
    produced_candidate: bool,
    record: &serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "command": CONSTRUCT_IMPLEMENT,
        "nonce": nonce,
        "produced_candidate": produced_candidate,
        "result_record": record,
    })
}

/// Whether `git status --porcelain` stdout signals a candidate change in the
/// working tree: a non-empty (whitespace-trimmed) output means the construct run
/// left something to review, an empty one means it did nothing (#3596). Pure so
/// the mapping is testable without spawning git.
fn porcelain_signals_candidate(stdout: &str) -> bool {
    !stdout.trim().is_empty()
}

/// Inspect the working tree (cwd — the checked-out worktree the construct lane
/// runs in) for a candidate change the run left, via `git status --porcelain`.
/// Fail-closed: a git that will not run cannot prove a candidate, so it reads as
/// none (the completion gate then rejects the attempt).
fn capture_produced_candidate() -> bool {
    Command::new("git").args(["status", "--porcelain"]).output().is_ok_and(|output| {
        output.status.success() && porcelain_signals_candidate(&String::from_utf8_lossy(&output.stdout))
    })
}

/// Derive the construct lane's result record from the headless-Claude stream-json
/// `transcript` — the in-repo, node-free replacement (#3572) for the retired
/// `scripts/agent-usage-record.mjs` shell-out (#3565 deletes that script). Pure,
/// and faithful to the ledger derivation: the terminal `result` event carries
/// cost / turns / duration / usage, and the first non-haiku assistant call's usage
/// is the warm-resume cache-hit signal. A transcript with no terminal `result` (a
/// run that died early) yields a `no_result` record rather than an error, so
/// evidence is never dropped.
fn derive_result_record(transcript: &str) -> serde_json::Value {
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

/// Assembles the evidence record from a captured run's status — pure
/// so it's testable without spawning a process.
fn build_evidence(command: &str, nonce: Option<String>, status: ExitStatus, log_file: String) -> Evidence {
    Evidence {
        command: command.to_string(),
        nonce,
        status: if status.success() {
            "pass"
        } else {
            "fail"
        },
        exit_code: status.code(),
        log: log_file,
    }
}

/// Runs the mapped command, capturing stdout+stderr, and writes
/// evidence before mirroring the verify's own exit status. An
/// unrecognized command id is an operational failure — it exits
/// non-zero with no evidence written, distinct from a verify that ran
/// and failed.
pub fn run(args: &TransformArgs) -> Result<()> {
    if args.command == CONSTRUCT_IMPLEMENT {
        return run_construct(args);
    }
    let Some(invocation) = verify_command(&args.command) else {
        bail!("unrecognized transform command id: {} (verify.test is out of scope this slice)", args.command);
    };

    let output =
        spawn(&invocation).with_context(|| format!("spawn {} {}", invocation.program, invocation.args.join(" ")))?;

    fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out.display()))?;

    let log_name = format!("{}.log", args.command);
    let log_path = args.out.join(&log_name);
    let mut log_bytes = output.stdout.clone();
    log_bytes.extend_from_slice(&output.stderr);
    fs::write(&log_path, &log_bytes).with_context(|| format!("write {}", log_path.display()))?;

    let evidence = build_evidence(&args.command, args.nonce.clone(), output.status, log_name);
    let evidence_path = args.out.join("evidence.json");
    let mut json = serde_json::to_string_pretty(&evidence).context("serialize evidence")?;
    json.push('\n');
    fs::write(&evidence_path, json).with_context(|| format!("write {}", evidence_path.display()))?;

    if output.status.success() {
        Ok(())
    } else {
        process::exit(output.status.code().unwrap_or(1));
    }
}

fn spawn(invocation: &VerifyInvocation) -> Result<Output> {
    Command::new(invocation.program)
        .args(invocation.args)
        .envs(invocation.env.iter().copied())
        .output()
        .context("run command")
}

/// The `construct.implement` lane: assemble the prompt from the lane's in-repo
/// instruction source plus the checked-out subject, run headless Claude at the
/// resolved model (or the operator's ambient default when none is resolved,
/// #3592), capture the stream-json transcript, derive the result record
/// in-repo, and write it as nonce-tagged evidence (#3572). This lane needs a
/// Claude credential, so it runs worker-side (BYO) — never on the coordinator's
/// zero-secret path.
fn run_construct(args: &TransformArgs) -> Result<()> {
    let model = args.model.as_deref();
    fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out.display()))?;

    // The lane owns its process: the prompt is assembled from the in-repo
    // instruction source and the checked-out subject, never from a skill in the
    // worker's checkout. It is piped on the child's stdin.
    let prompt = assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, args.subject.as_deref(), args.task.as_deref());

    // Run headless Claude at the resolved model; the reasoning effort rides an
    // env knob.
    let mut claude = Command::new("claude");
    claude.args(construct_argv(model));
    if let Some(effort) = &args.effort {
        claude.env("AETHER_CONSTRUCT_EFFORT", effort);
    }
    claude.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = claude.spawn().context("run headless claude")?;
    // Pipe the prompt on a separate thread and reap the child unconditionally: if
    // the write races an early exit (broken pipe) or the prompt outgrows the OS
    // pipe buffer, returning on the write error before `wait_with_output` would
    // drop `child` un-waited — `Child`'s `Drop` neither waits nor reaps — leaving a
    // zombie (or a live, still-billing process) behind. Waiting first, then
    // surfacing the writer's error, guarantees the child is reaped on every path.
    let mut stdin = child.stdin.take().context("headless claude stdin was not captured")?;
    let prompt_bytes = prompt.into_bytes();
    // Infra thread in a build tool — no settlement/trace umbrella applies here;
    // it exists only to pipe stdin while the main thread reaps the child.
    #[allow(clippy::disallowed_methods)]
    let writer = thread::spawn(move || stdin.write_all(&prompt_bytes));
    let run = child.wait_with_output().context("await headless claude")?;
    writer.join().expect("prompt-writer thread panicked").context("pipe the assembled prompt to headless claude")?;
    // A non-zero exit is the CLI itself failing to run (auth, bad args, crash) —
    // an operational failure, distinct from a task-level error, which a completed
    // run records as `is_error` inside the transcript. Surface it rather than
    // writing an empty/garbage result record and reporting success.
    if !run.status.success() {
        bail!(
            "headless claude exited {}: {}",
            run.status.code().map_or_else(|| "by signal".to_owned(), |c| c.to_string()),
            tail(&String::from_utf8_lossy(&run.stderr), 1000),
        );
    }

    let transcript_path = args.out.join("transcript.jsonl");
    fs::write(&transcript_path, &run.stdout).with_context(|| format!("write {}", transcript_path.display()))?;

    // Derive the result record over the whole transcript, in-repo (no node
    // shell-out): the terminal `result` plus the first-call cache signal.
    let record = derive_result_record(&String::from_utf8_lossy(&run.stdout));

    // Inspect the worktree (cwd) for the candidate change the run's whole job is
    // to leave (#3596): the gate demands a substantive conclusion, and an empty
    // diff is nothing to review. Captured after the child is reaped so it reflects
    // the run's final tree.
    let produced_candidate = capture_produced_candidate();

    let evidence = stamp_construct_evidence(args.nonce.as_deref(), produced_candidate, &record);
    let evidence_path = args.out.join("evidence.json");
    let mut json = serde_json::to_string_pretty(&evidence).context("serialize construct evidence")?;
    json.push('\n');
    fs::write(&evidence_path, json).with_context(|| format!("write {}", evidence_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CONSTRUCT_IMPLEMENT, CONSTRUCT_INSTRUCTIONS, assemble_construct_prompt, build_evidence, construct_argv,
        derive_result_record, porcelain_signals_candidate, stamp_construct_evidence, verify_command,
    };

    #[test]
    fn known_ids_map_to_ci_parity_argv() {
        let fmt = verify_command("verify.fmt").expect("verify.fmt mapped");
        assert_eq!(fmt.args, &["fmt", "--all", "--", "--check"]);

        let clippy = verify_command("verify.clippy").expect("verify.clippy mapped");
        assert_eq!(clippy.args, &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]);

        let docs = verify_command("verify.docs").expect("verify.docs mapped");
        assert_eq!(docs.args, &["doc", "--workspace", "--no-deps"]);
        assert_eq!(docs.env.len(), 1);
        assert_eq!(docs.env[0].0, "RUSTDOCFLAGS");
    }

    #[test]
    fn unknown_and_verify_test_ids_are_unmapped() {
        assert!(verify_command("verify.test").is_none());
        assert!(verify_command("verify.bogus").is_none());
        // construct.implement is the model lane's id, not a verify id — it must
        // not resolve a verify invocation.
        assert!(verify_command(CONSTRUCT_IMPLEMENT).is_none());
    }

    #[test]
    fn tail_snaps_to_a_char_boundary_without_panicking() {
        use super::tail;
        assert_eq!(tail("short", 10), "short", "under the cap returns the whole string");
        assert_eq!(tail("abcdef", 3), "def", "the ascii byte cut is already a char boundary");
        // "aébc" is 5 bytes; the byte cut (5 - 3 = byte 2) lands inside the
        // 2-byte 'é'. Snapping forward to byte 3 drops the partial char rather
        // than slicing mid-char and panicking.
        assert_eq!(tail("aébc", 3), "bc");
    }

    #[test]
    fn construct_argv_carries_the_resolved_model_and_stream_json() {
        let argv = construct_argv(Some("claude-opus-4-8"));
        assert_eq!(argv.first().map(String::as_str), Some("-p"), "headless, non-interactive");
        let model_at = argv.iter().position(|a| a == "--model").expect("argv pins the model");
        assert_eq!(argv[model_at + 1], "claude-opus-4-8", "the resolved model rides argv");
        // The stream-json transcript is what the result-record derivation reads.
        assert!(argv.windows(2).any(|w| w == ["--output-format", "stream-json"]), "emits the stream-json transcript");
    }

    #[test]
    fn construct_argv_omits_model_when_none_falls_back_to_ambient() {
        let argv = construct_argv(None);
        assert_eq!(argv.first().map(String::as_str), Some("-p"), "headless, non-interactive");
        assert!(!argv.iter().any(|a| a == "--model"), "no resolved model means no --model flag (ambient default)");
        assert!(
            argv.windows(2).any(|w| w == ["--output-format", "stream-json"]),
            "still emits the stream-json transcript"
        );
    }

    #[test]
    fn construct_evidence_binds_the_nonce_carries_the_record_and_the_candidate_signal() {
        let record = serde_json::json!({ "cost_usd": 0.42, "num_turns": 3, "input": 1000 });
        let evidence = stamp_construct_evidence(Some("nonce-7"), true, &record);
        assert_eq!(evidence["command"], CONSTRUCT_IMPLEMENT);
        assert_eq!(evidence["nonce"], "nonce-7", "the broker-matched nonce binds the evidence");
        assert_eq!(
            evidence["produced_candidate"], true,
            "the candidate-produced signal is stamped for the gate (#3596)"
        );
        assert_eq!(evidence["result_record"]["cost_usd"], 0.42, "the derived cost/turns record is carried");
        assert_eq!(evidence["result_record"]["num_turns"], 3);

        // An empty-candidate run stamps `false` so the gate can reject it while the
        // derived record is still carried whole.
        let no_nonce = stamp_construct_evidence(None, false, &serde_json::json!({ "no_result": true }));
        assert!(no_nonce["nonce"].is_null());
        assert_eq!(no_nonce["produced_candidate"], false, "an empty-candidate run stamps false");
        assert_eq!(no_nonce["result_record"]["no_result"], true, "the derived record is carried whole");
    }

    // The candidate signal is a pure map of `git status --porcelain` stdout: any
    // non-empty (trimmed) output is a candidate change, an empty one is not (#3596).
    #[test]
    fn porcelain_signals_a_candidate_only_when_the_worktree_is_dirty() {
        assert!(!porcelain_signals_candidate(""), "a clean worktree left no candidate");
        assert!(!porcelain_signals_candidate("   \n  \n"), "whitespace-only output is still no candidate");
        assert!(porcelain_signals_candidate(" M xtask/src/transform.rs\n"), "a modified file is a candidate");
        assert!(porcelain_signals_candidate("?? new_file.rs\n"), "a new untracked file is a candidate");
    }

    // The prompt is assembled from the lane's own in-repo instruction source plus
    // the checked-out subject — the construct lane owns its process natively and
    // never reads `.claude/skills/implement` (#3572). Pure: no Claude spawn.
    #[test]
    fn construct_prompt_assembles_from_the_in_repo_instructions_and_subject() {
        let prompt = assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, Some("abc123"), Some("thread the work order"));
        assert!(prompt.starts_with(CONSTRUCT_INSTRUCTIONS), "the lane's own instruction source leads the prompt");
        assert!(prompt.contains("## Subject"), "the checked-out subject is appended as its own section");
        assert!(prompt.contains("abc123"), "the exact checked-out commit is named in the prompt");
        assert!(
            !prompt.contains(".claude/skills/implement"),
            "the native lane never delegates to the retired implement skill",
        );

        // The work-order description is appended under its own `## Task` section
        // (#3595) so the model is told what to build, not just where. Match the
        // heading on its own line (`\n## Task\n`), since the instruction text
        // itself references the section name inline as a code span.
        assert!(prompt.contains("\n## Task\n"), "the work-order description is appended as its own section");
        assert!(prompt.contains("thread the work order"), "the task text is named in the prompt");

        // With no subject supplied, the prompt still stands and names no commit.
        let subjectless = assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, None, Some("still has a task"));
        assert!(subjectless.contains("## Subject"));
        assert!(subjectless.contains("\n## Task\n"));
        assert!(subjectless.starts_with(CONSTRUCT_INSTRUCTIONS));

        // With no task, the prompt still stands and appends no `## Task` section —
        // the fail-legible subject-only path for a member with no description.
        let taskless = assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, Some("abc123"), None);
        assert!(taskless.contains("## Subject"));
        assert!(!taskless.contains("\n## Task\n"), "no persisted description means no task section");
        assert!(taskless.starts_with(CONSTRUCT_INSTRUCTIONS));
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

    #[test]
    fn evidence_assembly_carries_status_nonce_and_exit_code() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;

        let pass = ExitStatus::from_raw(0);
        let evidence = build_evidence("verify.fmt", Some("nonce-1".to_string()), pass, "verify.fmt.log".to_string());
        assert_eq!(evidence.command, "verify.fmt");
        assert_eq!(evidence.nonce, Some("nonce-1".to_string()));
        assert_eq!(evidence.status, "pass");
        assert_eq!(evidence.exit_code, Some(0));
        assert_eq!(evidence.log, "verify.fmt.log");

        let fail = ExitStatus::from_raw(1 << 8);
        let evidence = build_evidence("verify.clippy", None, fail, "verify.clippy.log".to_string());
        assert_eq!(evidence.status, "fail");
        assert_eq!(evidence.exit_code, Some(1));
        assert_eq!(evidence.nonce, None);
    }
}
