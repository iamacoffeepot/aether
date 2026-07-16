//! `cargo xtask transform` — ADR-0149 §Execution's portable execution
//! unit: a typed `command` id maps to the exact cargo invocation CI
//! runs, executes it, and writes nonce-tagged evidence bytes a broker
//! can validate. This is the mechanical-verify lane only
//! (`verify.fmt` / `verify.clippy` / `verify.docs`) — `verify.test`
//! is deliberately out of scope (issue #3501 Design notes: CI's
//! actual test lane pre-builds with `cargo xtask dist` and runs under
//! nextest with a heavier toolchain, so mirroring it here would break
//! this slice's byte-for-byte parity claim).

use std::path::PathBuf;
use std::process::{Command, ExitStatus, Output};
use std::{fs, process};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Serialize;

#[derive(Args)]
pub struct TransformArgs {
    /// Typed verify command id (`verify.fmt` / `verify.clippy` / `verify.docs`).
    command: String,
    /// Directory evidence bytes are written to (created if missing).
    #[arg(long)]
    out: PathBuf,
    /// Idempotency nonce the broker matches against the work order,
    /// stamped into `evidence.json`.
    #[arg(long)]
    nonce: Option<String>,
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

/// `<out>/evidence.json` schema — the untrusted claim a broker
/// validates by `nonce` and re-checks against `status`.
#[derive(Serialize)]
struct Evidence {
    command: String,
    nonce: Option<String>,
    status: &'static str,
    exit_code: Option<i32>,
    log: String,
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

#[cfg(test)]
mod tests {
    use super::{build_evidence, verify_command};

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
