//! `cargo xtask transform` — ADR-0149 §Execution's portable execution
//! unit: a typed `command` id maps to the exact invocation the lane runs,
//! executes it, and writes nonce-tagged evidence bytes a broker can
//! validate. Two lanes share this entrypoint:
//!
//! - The **mechanical verify lane** (`verify.fmt`, `verify.clippy`,
//!   `verify.docs`, `verify.test`, `verify.dup`, `verify.deps`, and
//!   `verify.suppress`, #3501) — zero-secret invocations byte-for-byte with CI.
//!   The `verify.check` umbrella runs all seven without short-circuiting.
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
mod codex;
mod construct;
mod conventions;
mod grok;
mod lane;
mod messages;
mod muse;
mod peak_memory;
mod review;
mod sccache;
mod scratch;
mod verify;

use std::path::{Path, PathBuf};

use aether_bloomery::{Harness, VerifyFailureSet};
use anyhow::{Result, bail};
use clap::Args;
use serde::Serialize;

use crate::cargo::write_json_pretty;
use crate::transform::construct::CONSTRUCT_IMPLEMENT;
use crate::transform::peak_memory::PeakMemory;
use crate::transform::review::REVIEW_CRITIC;
use crate::transform::sccache::{CompilerCache, Counters};
use crate::transform::scratch::Scratch;
use crate::transform::verify::VERIFY_CHECK;

#[derive(Args, Clone)]
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
    /// The commit the reviewed candidate's diff is taken against (#4723) — the
    /// `review.critic` lane's diff source, threaded from the work order's
    /// `diff_base`. Absent names the working-tree contract every member lane
    /// runs under; present names the committed range `<diff-base>..HEAD` an
    /// aggregate review judges. Ignored by every other lane.
    #[arg(long)]
    diff_base: Option<String>,
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
    /// The harness session a retry lap resumes, in whatever the resolved
    /// harness calls it — a Claude or Grok session id (`--resume`), a Codex
    /// thread id (`exec … resume <id>`), or a Muse session uuid
    /// (`--session-id`). Absent launches a fresh session; the Muse arm mints
    /// its own uuid in that case, because Muse addresses a new and a continued
    /// session through the same flag. Ignored by the verify lane.
    #[arg(long)]
    resume: Option<String>,
    /// The construct dispatch checks out a prior attempt's checkpoint rather
    /// than the sealed base (#4994). Names the seeded state and trust posture
    /// in the assembled prompt; absent (the cold path) omits that note.
    /// Ignored by every other lane.
    #[arg(long)]
    seeded: bool,
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
    /// A failing run's distilled diagnostics (#4641), read back host-side and
    /// persisted per member so a `Refine` re-entry is directed by them — the
    /// same top-level `findings` channel the review critic stamps.
    ///
    /// Absent on a pass, and absent on a lane that produces no diagnostics, so
    /// the channel stays presence-driven rather than needing a lane flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    findings: Option<String>,
    /// The exact failed `verify.check` members (ADR-0178). Absent on a pass;
    /// present and nonempty on a failed umbrella run.
    #[serde(skip_serializing_if = "Option::is_none")]
    failed_verifiers: Option<VerifyFailureSet>,
    /// What the run declined to charge the candidate for, and why (#4895): the
    /// failures it read as the host's rather than the work's, the closure it
    /// judged them against, and what the rerun then said.
    ///
    /// Its own channel rather than a section of `findings`, because the two are
    /// read by different parties. Findings are handed to a repair lap as work;
    /// this is a receipt for the lane, and a model given it would spend a
    /// bounded repair roll on a host it cannot reach.
    #[serde(skip_serializing_if = "Option::is_none")]
    environment: Option<String>,
    /// What sccache served this run's compilations (#4894) — the receipts that
    /// make the reclaimed seconds countable rather than anecdotal.
    ///
    /// Absent on a host with no sccache, where the lane builds exactly as it did
    /// before: a zeroed reading there would say the cache served nothing, which
    /// is the opposite conclusion about the host from the true one.
    #[serde(skip_serializing_if = "Option::is_none")]
    sccache: Option<Counters>,
    /// The largest resident set any of this run's commands reached, in bytes
    /// (#4912) — what the lane concurrency ceiling is calibrated from, measured
    /// on production laps instead of estimated.
    ///
    /// Absent on a host whose `/usr/bin/time` cannot report it, for the reason
    /// the counters above are absent without sccache: a zero would claim a run
    /// that allocated nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    peak_resident_bytes: Option<u64>,
}

impl Evidence {
    /// Stamp what the host measured about this run — what `cache` served it and
    /// what it peaked at. Reads both at the moment it is called, so it belongs at
    /// the end of a lane rather than beside the record's other fields.
    fn measured_by(mut self, cache: Option<&CompilerCache>, peak: &PeakMemory) -> Self {
        self.sccache = cache.and_then(CompilerCache::served);
        self.peak_resident_bytes = peak.peak_resident_bytes();
        self
    }
}

/// Assembles the evidence record from a captured run's status — pure
/// so it's testable without spawning a process.
fn build_evidence(
    command: &str,
    nonce: Option<String>,
    passed: bool,
    exit_code: Option<i32>,
    log_file: String,
    findings: Option<String>,
    failed_verifiers: Option<VerifyFailureSet>,
) -> Evidence {
    Evidence {
        findings,
        failed_verifiers,
        // The single-command path discriminates nothing: only the umbrella
        // resolves a closure, so only the umbrella can report against one.
        environment: None,
        sccache: None,
        peak_resident_bytes: None,
        command: command.to_string(),
        nonce,
        status: if passed {
            "pass"
        } else {
            "fail"
        },
        exit_code,
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
///
/// The run's [`Scratch`] directory is prepared here and dropped when the lane
/// returns, so every arm hands its child the same place to build throwaway
/// target directories and every run reaps its own. The host's [`CompilerCache`]
/// is resolved beside it and rides the same child environment, so a run that
/// builds where an earlier one did draws on what that one compiled instead of
/// re-paying for it, and the host's [`PeakMemory`] wrapper is resolved with them
/// so the child's own peak is measured rather than modelled.
fn run_model_lane(prompt: &str, args: &TransformArgs) -> Result<LaneRun> {
    let harness = resolve_harness(args.harness.as_deref())?;
    let scratch = Scratch::prepare(&args.out, args.nonce.as_deref())?;
    let cache = sccache::detect();
    let peak = peak_memory::detect();

    let record = match harness {
        Harness::Claude => claude::run_headless_claude(prompt, args, &scratch, cache.as_ref(), &peak)?,
        Harness::Codex => codex::run(prompt, args, &scratch, cache.as_ref(), &peak)?,
        Harness::Muse => muse::run(prompt, args, &scratch, cache.as_ref(), &peak)?,
        Harness::Grok => grok::run(prompt, args, &scratch, cache.as_ref(), &peak)?,
    };

    Ok(LaneRun {
        record,
        measured: Measurements {
            sccache: cache.as_ref().and_then(CompilerCache::served),
            peak_resident_bytes: peak.peak_resident_bytes(),
        },
    })
}

/// What one model lane's run produced.
///
/// The record is the harness's and the measurements are the host's — taken after
/// the child is reaped, so they cover everything the run's agent did rather than
/// only what this process did.
struct LaneRun {
    record: serde_json::Value,
    measured: Measurements,
}

/// What the host measured about a run, in one value.
///
/// One value rather than a parameter each, because both lanes' evidence stampers
/// carry them together and neither reads either: a third reading arriving later
/// should not re-open two signatures to add itself.
#[derive(Clone, Copy, Default)]
struct Measurements {
    sccache: Option<Counters>,
    peak_resident_bytes: Option<u64>,
}

impl Measurements {
    /// Stamp both readings onto a model lane's evidence envelope, each
    /// presence-driven: a host that cannot measure one stamps no key for it.
    fn stamp(self, evidence: &mut serde_json::Value) {
        sccache::stamp(evidence, self.sccache);
        peak_memory::stamp(evidence, self.peak_resident_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::build_evidence;

    #[test]
    fn evidence_assembly_carries_status_nonce_and_exit_code() {
        let evidence = build_evidence(
            "verify.fmt",
            Some("nonce-1".to_string()),
            true,
            Some(0),
            "verify.fmt.log".to_string(),
            None,
            None,
        );
        assert_eq!(evidence.command, "verify.fmt");
        assert_eq!(evidence.nonce, Some("nonce-1".to_string()));
        assert_eq!(evidence.status, "pass");
        assert_eq!(evidence.exit_code, Some(0));
        assert_eq!(evidence.log, "verify.fmt.log");

        let failures = aether_bloomery::VerifyFailureSet::one(aether_bloomery::VerifyFailure::Clippy);
        let evidence = build_evidence(
            "verify.clippy",
            None,
            false,
            Some(1),
            "verify.clippy.log".to_string(),
            None,
            Some(failures),
        );
        assert_eq!(evidence.status, "fail");
        assert_eq!(evidence.exit_code, Some(1));
        assert_eq!(evidence.nonce, None);
        assert_eq!(evidence.failed_verifiers, Some(failures));
        assert_eq!(
            serde_json::to_value(&evidence).expect("evidence serializes")["failed_verifiers"],
            serde_json::json!(["verify.clippy"]),
        );
    }
}
