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

    let evidence = build_evidence(&args.command, args.nonce.clone(), output.status, log_name);
    write_json_pretty(&args.out.join("evidence.json"), &evidence)?;

    if output.status.success() {
        Ok(())
    } else {
        process::exit(output.status.code().unwrap_or(1));
    }
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
        log_names.push(log_name);
        passed.push(output.status.success());
    }

    let all_pass = all_passed(&passed);
    let evidence = Evidence {
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
    use super::{VERIFY_CHECK, all_passed, verify_check_members, verify_command};
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
