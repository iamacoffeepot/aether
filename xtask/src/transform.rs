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
//!   runs headless Claude at the resolved model + reasoning effort and writes
//!   the nonce-tagged **result record** (cost / tokens / turns), derived from
//!   the run transcript by `scripts/agent-usage-record.mjs`. Unlike the verify
//!   lane it needs a credential, so it runs **worker-side** (BYO); the
//!   coordinator never sees it.

use std::path::PathBuf;
use std::process::{Command, ExitStatus, Output};
use std::{fs, process};

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
    /// The model the `construct.implement` lane runs headless Claude under —
    /// the effective model the coordinator resolved from the sealed
    /// scope-revision (#3511). Ignored by the verify lane.
    #[arg(long)]
    model: Option<String>,
    /// The reasoning-effort tier the `construct.implement` lane runs at (the
    /// resolved effort, #3511). Ignored by the verify lane.
    #[arg(long)]
    effort: Option<String>,
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
/// non-interactive at the resolved `model`, emitting the stream-json transcript
/// `scripts/agent-usage-record.mjs` derives the result record from. Pure so the
/// model wiring is testable without spawning Claude; the reasoning effort rides
/// the `AETHER_CONSTRUCT_EFFORT` env (a worker-side knob), not an argv flag.
fn construct_argv(model: &str) -> Vec<String> {
    vec![
        "-p".to_owned(),
        "--model".to_owned(),
        model.to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--verbose".to_owned(),
    ]
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

/// Stamp the broker-matched `nonce` and the command id onto the result record
/// `agent-usage-record.mjs` derived, producing the construct lane's evidence
/// envelope. Pure so the nonce binding is testable without running Claude or
/// node; a malformed record is carried verbatim under `record_raw` so evidence
/// is never dropped.
fn stamp_construct_evidence(nonce: Option<&str>, record_json: &str) -> serde_json::Value {
    let record: serde_json::Value =
        serde_json::from_str(record_json).unwrap_or_else(|_| serde_json::json!({ "record_raw": record_json }));
    serde_json::json!({
        "command": CONSTRUCT_IMPLEMENT,
        "nonce": nonce,
        "result_record": record,
    })
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

/// The `construct.implement` lane: run headless Claude at the resolved model,
/// capture the stream-json transcript, derive the result record via
/// `agent-usage-record.mjs`, and write it as nonce-tagged evidence. This lane
/// needs a Claude credential, so it runs worker-side (BYO) — never on the
/// coordinator's zero-secret path.
fn run_construct(args: &TransformArgs) -> Result<()> {
    let model = args.model.as_deref().context("construct.implement requires --model (the resolved model)")?;
    fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out.display()))?;

    // Run headless Claude at the resolved model; the reasoning effort rides an
    // env knob. The prompt/subject is the worker's checked-out tree at the
    // pinned digest, piped on stdin by the wrapper.
    let mut claude = Command::new("claude");
    claude.args(construct_argv(model));
    if let Some(effort) = &args.effort {
        claude.env("AETHER_CONSTRUCT_EFFORT", effort);
    }
    let run = claude.output().context("run headless claude")?;
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

    // Derive the result record over the whole transcript (faithful reuse of the
    // fleet ledger's derivation, `scripts/agent-usage-record.mjs`).
    let record = Command::new("node")
        .args(["scripts/agent-usage-record.mjs", "--transcript"])
        .arg(&transcript_path)
        .output()
        .context("derive result record via agent-usage-record.mjs")?;
    // The derivation script never exits non-zero on a short transcript (it emits
    // a `no_result` envelope), so a non-zero exit is node itself failing —
    // surface it rather than stamping evidence over empty stdout.
    if !record.status.success() {
        bail!(
            "result-record derivation (agent-usage-record.mjs) failed: {}",
            tail(&String::from_utf8_lossy(&record.stderr), 1000),
        );
    }
    let record_json = String::from_utf8_lossy(&record.stdout);

    let evidence = stamp_construct_evidence(args.nonce.as_deref(), &record_json);
    let evidence_path = args.out.join("evidence.json");
    let mut json = serde_json::to_string_pretty(&evidence).context("serialize construct evidence")?;
    json.push('\n');
    fs::write(&evidence_path, json).with_context(|| format!("write {}", evidence_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CONSTRUCT_IMPLEMENT, build_evidence, construct_argv, stamp_construct_evidence, verify_command};

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
        let argv = construct_argv("claude-opus-4-8");
        assert_eq!(argv.first().map(String::as_str), Some("-p"), "headless, non-interactive");
        let model_at = argv.iter().position(|a| a == "--model").expect("argv pins the model");
        assert_eq!(argv[model_at + 1], "claude-opus-4-8", "the resolved model rides argv");
        // The stream-json transcript is what the result-record derivation reads.
        assert!(argv.windows(2).any(|w| w == ["--output-format", "stream-json"]), "emits the stream-json transcript");
    }

    #[test]
    fn construct_evidence_binds_the_nonce_and_carries_the_result_record() {
        let record = r#"{"cost_usd":0.42,"num_turns":3,"input":1000}"#;
        let evidence = stamp_construct_evidence(Some("nonce-7"), record);
        assert_eq!(evidence["command"], CONSTRUCT_IMPLEMENT);
        assert_eq!(evidence["nonce"], "nonce-7", "the broker-matched nonce binds the evidence");
        assert_eq!(evidence["result_record"]["cost_usd"], 0.42, "the derived cost/turns record is carried");
        assert_eq!(evidence["result_record"]["num_turns"], 3);

        // A malformed derivation is carried verbatim, never dropped.
        let raw = stamp_construct_evidence(None, "not json");
        assert_eq!(raw["result_record"]["record_raw"], "not json");
        assert!(raw["nonce"].is_null());
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
