use std::fs;
use std::process::{self, Command};

use anyhow::{Context, Result, bail};

use crate::cargo::{run_captured, write_json_pretty};
use crate::transform::{Evidence, TransformArgs, build_evidence};

/// One CI-mirroring cargo invocation for a `verify.*` command id.
struct VerifyInvocation {
    program: &'static str,
    args: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
}

impl VerifyInvocation {
    /// The [`Command`] this invocation runs — program, argv, and env — handed
    /// to [`run_captured`] for the captured-output spawn.
    fn command(&self) -> Command {
        let mut cmd = Command::new(self.program);
        cmd.args(self.args).envs(self.env.iter().copied());
        cmd
    }
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

/// The typed id of the verify umbrella (#3626) the reducer dispatches for the
/// Verify stage (`Transformation::for_member_stage`) — distinct from the three
/// concrete `verify.*` ids `verify_command` maps individually.
pub(super) const VERIFY_CHECK: &str = "verify.check";

/// The ordered member ids `verify.check` fans out to, in CI-parity order.
/// Pure so the umbrella membership is testable without spawning cargo; growing
/// this list (e.g. a future `verify.test`) needs no change to the reducer's
/// dispatched stage command.
fn verify_check_members() -> &'static [&'static str] {
    &["verify.fmt", "verify.clippy", "verify.docs"]
}

/// Aggregate `verify.check`'s member results: pass iff every member passed.
/// Pure so the aggregation is testable without spawning cargo.
fn all_passed(statuses: &[bool]) -> bool {
    statuses.iter().all(|&passed| passed)
}

/// The single mechanical-verify path: run the mapped command, capture
/// stdout+stderr, write evidence, and mirror the verify's own exit status. An
/// unrecognized command id is an operational failure — it exits non-zero with
/// no evidence written, distinct from a verify that ran and failed.
pub(super) fn run_single(args: &TransformArgs) -> Result<()> {
    let Some(invocation) = verify_command(&args.command) else {
        bail!("unrecognized transform command id: {} (verify.test is out of scope this slice)", args.command);
    };

    let output = run_captured(invocation.command())
        .with_context(|| format!("spawn {} {}", invocation.program, invocation.args.join(" ")))?;

    fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out.display()))?;

    let log_name = format!("{}.log", args.command);
    let log_path = args.out.join(&log_name);
    let mut log_bytes = output.stdout.clone();
    log_bytes.extend_from_slice(&output.stderr);
    fs::write(&log_path, &log_bytes).with_context(|| format!("write {}", log_path.display()))?;

    // A failing single verify contributes its own diagnostics on the same
    // channel the umbrella uses, so a lane run alone directs a Refine too.
    let findings = (!output.status.success())
        .then(|| verify_findings(&[(args.command.as_str(), false, String::from_utf8_lossy(&log_bytes).into_owned())]))
        .flatten();

    let evidence = build_evidence(&args.command, args.nonce.clone(), output.status, log_name, findings);
    write_json_pretty(&args.out.join("evidence.json"), &evidence)?;

    if output.status.success() {
        Ok(())
    } else {
        process::exit(output.status.code().unwrap_or(1));
    }
}

/// The line prefixes that open a diagnostic in the verify lanes' output — rustc
/// / clippy / rustdoc errors and warnings, their `-->` source locations, and
/// rustfmt's per-file diff header.
const DIAGNOSTIC_OPENERS: [&str; 4] = ["error", "warning:", "-->", "Diff in "];

/// How much distilled output one failing member may contribute to the findings.
/// A `Refine` prompt is read by a model with a finite budget, and a verify log
/// is mostly progress chatter, so the cap bounds the noise rather than the
/// signal.
const MAX_FINDING_LINES: usize = 40;

/// Distil one member's log down to the lines that carry a verdict.
///
/// A verify log is overwhelmingly `Compiling …` / `Checking …` progress with a
/// handful of diagnostics buried in it. Handing the whole thing to a `Refine`
/// re-entry would bury the finding it exists to deliver, which is the failure
/// mode #4628 describes for the boot log. So keep the diagnostic lines, and
/// fall back to the tail when nothing matches — an unrecognized failure shape
/// still says more than silence.
fn distil_diagnostics(log: &str) -> Option<String> {
    let matched: Vec<&str> = log.lines().filter(|line| opens_a_diagnostic(line)).collect();
    let selected = if matched.is_empty() {
        tail_lines(log)
    } else {
        matched
    };
    if selected.is_empty() {
        return None;
    }

    let kept = selected.len().min(MAX_FINDING_LINES);
    let rendered = selected[..kept].join("\n");
    let omitted = selected.len() - kept;
    if omitted == 0 {
        return Some(rendered);
    }

    Some(format!("{rendered}\n… {omitted} further diagnostic lines omitted"))
}

/// Whether a log line opens (or locates) a diagnostic rather than reporting
/// progress. Leading whitespace is ignored — rustc indents its `-->` locations.
fn opens_a_diagnostic(line: &str) -> bool {
    let trimmed = line.trim_start();
    DIAGNOSTIC_OPENERS.iter().any(|opener| trimmed.starts_with(opener))
}

/// The last few non-empty lines, for a failure whose shape none of the openers
/// recognize.
fn tail_lines(log: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = log.lines().filter(|line| !line.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(MAX_FINDING_LINES);
    lines.drain(..start);
    lines
}

/// Assemble the failing members' distilled diagnostics into the `findings`
/// prose a `Refine` re-entry is directed by (#4641).
///
/// The opening line is doing real work: the construct lane's prompt says
/// "implement the work order", and a re-entry checks out the previous candidate
/// — which already implements it. Without an explicit statement that the
/// candidate failed and why, the model correctly answers that there is nothing
/// to do, and the loop cannot converge.
fn verify_findings(members: &[(&str, bool, String)]) -> Option<String> {
    let failed: Vec<String> = members
        .iter()
        .filter(|(_, passed, _)| !passed)
        .filter_map(|(id, _, log)| distil_diagnostics(log).map(|body| format!("### {id}\n\n{body}")))
        .collect();
    if failed.is_empty() {
        return None;
    }

    Some(format!(
        "The previous candidate failed verification. It already carries the work order's change; \
         what follows is what verification said is wrong with it. Fix these.\n\n{}",
        failed.join("\n\n")
    ))
}

/// The `verify.check` umbrella (#3626): runs every member in
/// `verify_check_members()` unconditionally — no short-circuit on first
/// failure, so a partial failure still leaves every member's log for
/// diagnosis — then writes one aggregate `evidence.json` whose `status`
/// passes only when every member passed. Exit mirrors the aggregate, exactly
/// as the single-command path mirrors its own verify's exit.
pub(super) fn run_verify_check(args: &TransformArgs) -> Result<()> {
    fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out.display()))?;

    let mut log_names = Vec::with_capacity(verify_check_members().len());
    let mut passed = Vec::with_capacity(verify_check_members().len());
    let mut outcomes = Vec::with_capacity(verify_check_members().len());
    let mut first_failure_code: Option<i32> = None;

    for &id in verify_check_members() {
        let invocation = verify_command(id).expect("verify_check_members ids all resolve via verify_command");
        let output = run_captured(invocation.command())
            .with_context(|| format!("spawn {} {}", invocation.program, invocation.args.join(" ")))?;

        let log_name = format!("{id}.log");
        let log_path = args.out.join(&log_name);
        let mut log_bytes = output.stdout.clone();
        log_bytes.extend_from_slice(&output.stderr);
        fs::write(&log_path, &log_bytes).with_context(|| format!("write {}", log_path.display()))?;

        if !output.status.success() && first_failure_code.is_none() {
            first_failure_code = Some(output.status.code().unwrap_or(1));
        }
        outcomes.push((id, output.status.success(), String::from_utf8_lossy(&log_bytes).into_owned()));
        log_names.push(log_name);
        passed.push(output.status.success());
    }

    let all_pass = all_passed(&passed);
    let evidence = Evidence {
        findings: verify_findings(&outcomes),
        command: VERIFY_CHECK.to_owned(),
        nonce: args.nonce.clone(),
        status: if all_pass {
            "pass"
        } else {
            "fail"
        },
        exit_code: Some(first_failure_code.unwrap_or(0)),
        log: log_names.join(", "),
    };
    write_json_pretty(&args.out.join("evidence.json"), &evidence)?;

    if all_pass {
        Ok(())
    } else {
        process::exit(first_failure_code.unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_FINDING_LINES, VERIFY_CHECK, all_passed, distil_diagnostics, verify_check_members, verify_command,
        verify_findings,
    };
    use crate::transform::construct::CONSTRUCT_IMPLEMENT;
    use crate::transform::review::REVIEW_CRITIC;

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
    fn diagnostics_survive_a_log_that_is_mostly_progress() {
        // The whole point of distilling (#4641): a clippy log is thousands of
        // `Compiling …` lines around a handful of diagnostics. Handing the raw
        // log to a Refine buries the finding it exists to deliver.
        let log = format!(
            "{}error[E0308]: mismatched types\n  --> crates/a/src/lib.rs:4:9\n{}",
            "   Compiling aether-data v0.3.0\n".repeat(200),
            "   Compiling aether-http v0.3.0\n".repeat(200),
        );

        let distilled = distil_diagnostics(&log).expect("a log with diagnostics distils");

        assert_eq!(distilled, "error[E0308]: mismatched types\n  --> crates/a/src/lib.rs:4:9");
        assert!(!distilled.contains("Compiling"), "progress chatter must not survive");
    }

    #[test]
    fn an_unrecognized_failure_shape_falls_back_to_the_tail() {
        // Tripwire: a lane whose failure matches none of the openers must still
        // say something. Returning `None` here would restore the silent Refine
        // this change exists to end.
        let log = "something went sideways\nand then it stopped\n";

        let distilled = distil_diagnostics(log).expect("an unrecognized shape still yields findings");

        assert!(distilled.contains("and then it stopped"), "the tail stands in when nothing matches");
    }

    #[test]
    fn a_flood_of_diagnostics_is_capped_and_says_so() {
        let log = (0..MAX_FINDING_LINES + 10).map(|n| format!("error[E{n:04}]: boom")).collect::<Vec<_>>().join("\n");

        let distilled = distil_diagnostics(&log).expect("distils");

        assert_eq!(distilled.lines().count(), MAX_FINDING_LINES + 1, "the cap plus its own notice");
        assert!(distilled.ends_with("… 10 further diagnostic lines omitted"), "truncation is stated, not silent");
    }

    #[test]
    fn only_failing_members_contribute_findings() {
        let members = [
            ("verify.fmt", true, String::from("error[E0001]: from a lane that passed")),
            ("verify.clippy", false, String::from("error[E0308]: mismatched types")),
        ];

        let findings = verify_findings(&members).expect("a failing member yields findings");

        assert!(findings.contains("### verify.clippy"), "the failing member is named");
        assert!(!findings.contains("verify.fmt"), "a passing member contributes nothing");
    }

    #[test]
    fn an_all_pass_run_yields_no_findings() {
        // Tripwire: findings must be absent on a pass, or a Refine that never
        // happens would still carry a stale row, and `parse_findings` is
        // presence-driven with no lane flag to disambiguate.
        let members = [("verify.fmt", true, String::new()), ("verify.clippy", true, String::new())];

        assert!(verify_findings(&members).is_none(), "a clean run stamps no findings");
    }

    #[test]
    fn the_findings_state_the_candidate_already_carries_the_change() {
        // The framing is load-bearing, not decoration. A Refine checks out the
        // previous candidate, so a prompt that only says "implement the work
        // order" invites the correct-but-useless "already carries it" answer
        // that wedged both real runs.
        let members = [("verify.clippy", false, String::from("error[E0308]: mismatched types"))];

        let findings = verify_findings(&members).expect("findings");

        assert!(findings.contains("failed verification"), "the re-entry is told its candidate failed");
        assert!(findings.contains("already carries"), "and that the change being present is expected");
    }

    #[test]
    fn verify_check_members_are_the_three_ci_parity_ids_in_order() {
        // Tripwire: every id verify.check fans out to must resolve via
        // verify_command, and the order must match ci.yml's fmt/clippy/docs
        // jobs — a drift here breaks the umbrella-membership invariant.
        assert_eq!(verify_check_members(), &["verify.fmt", "verify.clippy", "verify.docs"]);
        for &id in verify_check_members() {
            assert!(verify_command(id).is_some(), "{id} must resolve via verify_command");
        }
    }

    #[test]
    fn all_passed_is_pass_only_when_every_member_passed() {
        assert!(all_passed(&[true, true, true]));
        assert!(!all_passed(&[true, false, true]));
        assert!(!all_passed(&[false, false, false]));
        assert!(all_passed(&[]), "no members is vacuously all-passed");
    }

    #[test]
    fn verify_check_is_the_umbrella_not_a_concrete_verify_id() {
        // run's dispatch must route VERIFY_CHECK to run_verify_check before
        // falling to the concrete verify_command lookup — verify_command itself
        // does not (and must not) recognize the umbrella id, else an unrouted
        // verify.check would silently run as a single (wrong) cargo invocation
        // instead of falling to the unrecognized-id bail!.
        assert!(verify_command(VERIFY_CHECK).is_none());
        assert_ne!(VERIFY_CHECK, CONSTRUCT_IMPLEMENT);
    }

    #[test]
    fn unknown_and_verify_test_ids_are_unmapped() {
        assert!(verify_command("verify.test").is_none());
        assert!(verify_command("verify.bogus").is_none());
        // construct.implement and review.critic are the model lanes' ids, not
        // verify ids — neither must resolve a verify invocation.
        assert!(verify_command(CONSTRUCT_IMPLEMENT).is_none());
        assert!(verify_command(REVIEW_CRITIC).is_none());
    }
}
