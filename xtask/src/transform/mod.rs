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

mod claude;
mod construct;
mod review;
mod verify;

use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use aether_bloomery::Harness;
use anyhow::{Result, bail};
use clap::Args;
use serde::Serialize;

use crate::cargo::write_json_pretty;
use crate::transform::construct::CONSTRUCT_IMPLEMENT;
use crate::transform::review::REVIEW_CRITIC;
use crate::transform::verify::VERIFY_CHECK;

#[derive(Args)]
pub struct TransformArgs {
    /// Typed command id — a `verify.*` mechanical id, `construct.implement`, or
    /// `review.critic`.
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
    /// Which agent CLI the model lanes fork — the harness the coordinator
    /// resolved from the stage's sealed `AgentProfile` (#4578). Ignored by the
    /// verify lane, which runs a compiler. Absent when the coordinator resolved
    /// none, which falls back to the lane's default harness.
    #[arg(long)]
    harness: Option<String>,
    /// The model the `construct.implement` lane runs its harness under —
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
        return construct::run_construct(args);
    }
    if args.command == REVIEW_CRITIC {
        return review::run_review(args);
    }
    if args.command == VERIFY_CHECK {
        return verify::run_verify_check(args);
    }
    verify::run_single(args)
}

/// Serialize `evidence` to `<out>/evidence.json` — the one write both model
/// lanes end on.
fn write_evidence_json(out: &Path, evidence: &serde_json::Value) -> Result<()> {
    write_json_pretty(&out.join("evidence.json"), evidence)
}

/// The lane default when the coordinator resolved no harness — the operator's
/// ambient CLI, matching how an absent `--model` / `--effort` falls back to the
/// child's own defaults (#3592) rather than refusing the run.
const DEFAULT_HARNESS: Harness = Harness::Claude;

/// The harness a model lane forks for this run: the resolved `--harness` when
/// the coordinator named one, [`DEFAULT_HARNESS`] when it did not.
///
/// An unrecognized spelling is a hard error rather than a fallback. A dispatch
/// that names a harness this binary cannot parse is a version skew between the
/// coordinator and the worker's checkout, and silently running the default
/// would produce evidence attributed to a harness that never ran — the exact
/// claim the sealed profile digest is supposed to make verifiable.
fn resolve_harness(harness: Option<&str>) -> Result<Harness> {
    let Some(name) = harness else {
        return Ok(DEFAULT_HARNESS);
    };
    Harness::from_name(name).map_or_else(|| bail!("unrecognized harness `{name}`"), Ok)
}

/// Run one model lane's `prompt` under the resolved harness and return the
/// derived result record — the seam both model lanes (`construct.implement` and
/// `review.critic`) go through, so a harness is chosen once rather than per
/// lane.
///
/// Every arm returns the same record envelope, which is what lets the lanes
/// stay harness-agnostic: `construct.rs` reads `result_record.is_error` and
/// `review.rs` reads `result.result` for the critic's verdict text, neither
/// knowing which CLI produced them.
fn run_model_lane(prompt: &str, args: &TransformArgs) -> Result<serde_json::Value> {
    match resolve_harness(args.harness.as_deref())? {
        Harness::Claude => claude::run_headless_claude(prompt, args),
        harness @ (Harness::Codex | Harness::Muse) => {
            bail!("harness `{}` has no lane arm in this worker build", harness.as_str())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::build_evidence;

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
