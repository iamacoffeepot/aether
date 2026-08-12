mod nextest;
mod tools;

use std::fs;
use std::process::{self, Command};

use aether_bloomery::{VerifyFailure, VerifyFailureSet};
use anyhow::{Context, Result, bail};

use crate::cargo::{WASM_TARGET, run_captured, write_json_pretty};
use crate::transform::{Evidence, TransformArgs, build_evidence};

/// One CI-mirroring invocation for a `verify.*` command id, plus the tools it
/// needs present to run at all (#4706).
struct VerifyInvocation {
    program: &'static str,
    args: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
    /// The programs [`tools::preflight`] resolves through the dependency graph
    /// before anything is dispatched against this host.
    requires: &'static [&'static str],
    /// The toolchain targets this member's work cross-compiles for, checked by
    /// [`tools::preflight_targets`] alongside the programs.
    ///
    /// CI states these as the toolchain action's `targets:` line, and no `PATH`
    /// probe can stand in for one: a host with every program installed and no
    /// wasm32 standard library builds no component wasm at all.
    requires_targets: &'static [&'static str],
    /// A cargo step this member needs run first, or `None` when it stands
    /// alone.
    ///
    /// The one genuine ordering edge in the lane: `verify.test`'s scenario
    /// tests load component wasm that `cargo xtask dist` builds, and CI
    /// pre-builds it for the same reason. Everything else here is independent
    /// and always runs, so a failure in one never suppresses another.
    prepare: Option<&'static [&'static str]>,
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

/// Maps a typed `verify.*` command id to the invocation that answers it, in
/// CI-parity terms.
///
/// `--document-private-items` is what the `Rustdoc` job passes, and rustdoc
/// does not descend into a private module without it (#4694). `--keep-going`
/// keeps cargo scheduling past the first failing crate (#4690), so one run
/// reports every *independent* unit rather than stopping at one.
///
/// **`verify.clippy` does not deny warnings, and that is the point** (#4706).
/// `-D warnings` makes a lint a compile error, so a lib that trips one is never
/// built and nothing depending on it is ever linted — its diagnostics do not
/// exist to be reported. `--keep-going` cannot recover that: a dependent target
/// has no artifact to link against. So the run stays non-denying, every unit
/// compiles, every lint in the workspace is emitted, and
/// [`clippy_verdict`] applies the *same* predicate `-D warnings` encodes —
/// fail if any warning appeared — over a complete list instead of a truncated
/// one. Keeping one flag shape also keeps cargo's fingerprint stable across
/// dispatches, which is what lets a repair round recompile nine crates instead
/// of ninety-six.
///
/// Tripwire: these argv + env pins are CI-parity invariants — a drift here
/// means this entrypoint no longer proves the laptop/Actions invocation
/// symmetry ADR-0149 §Execution requires.
fn verify_command(id: &str) -> Option<VerifyInvocation> {
    match id {
        "verify.fmt" => Some(VerifyInvocation {
            program: "cargo",
            args: &["fmt", "--all", "--", "--check"],
            env: &[],
            requires: &["cargo", "rustfmt"],
            requires_targets: &[],
            prepare: None,
        }),
        "verify.clippy" => Some(VerifyInvocation {
            program: "cargo",
            args: &["clippy", "--workspace", "--all-targets", "--keep-going", "--message-format=json"],
            env: &[],
            requires: &["cargo", "cargo-clippy"],
            requires_targets: &[],
            prepare: None,
        }),
        "verify.docs" => Some(VerifyInvocation {
            program: "cargo",
            args: &["doc", "--workspace", "--no-deps", "--document-private-items", "--keep-going"],
            env: &[(
                "RUSTDOCFLAGS",
                "-D rustdoc::redundant_explicit_links -D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links",
            )],
            requires: &["cargo"],
            requires_targets: &[],
            prepare: None,
        }),
        "verify.test" => Some(VerifyInvocation {
            program: "cargo",
            // `--all-features` and `--profile ci` are CI's, and
            // `AETHER_REQUIRE_RUNTIME` turns a missing component wasm into a
            // hard failure instead of a silent skip — without it the lane runs
            // strictly fewer tests than the gate it predicts, which is the
            // false-green direction.
            //
            // `AETHER_STORE_PATH` pins what a CI runner gets for free: nothing
            // there names a store, so the suite's bins fall to the `":memory:"`
            // default. Off Actions the gate can be reached from a shell — or
            // from a coordinator whose environment names the live journal — and
            // the store-backed tests would open it read-write (#4714). Stating
            // the value is what makes the two environments the same one, not a
            // divergence from CI.
            args: &["nextest", "run", "--all-features", "--profile", "ci", "--no-fail-fast"],
            env: &[("AETHER_REQUIRE_RUNTIME", "1"), ("AETHER_STORE_PATH", ":memory:")],
            requires: &["cargo", "cargo-nextest"],
            // The prepare cross-builds every component crate for wasm32, so the
            // target's standard library is as much a prerequisite as nextest
            // itself. Named through `WASM_TARGET` — the same const the dist
            // build passes to `--target`, so the check and the build cannot
            // drift onto different triples.
            requires_targets: &[WASM_TARGET],
            prepare: Some(&["xtask", "dist"]),
        }),
        "verify.dup" => Some(VerifyInvocation {
            program: "npx",
            args: &["--yes", "jscpd@5.0.12", "crates"],
            env: &[],
            requires: &["npx"],
            requires_targets: &[],
            prepare: None,
        }),
        "verify.deps" => Some(VerifyInvocation {
            // Invoked as the binary rather than through `cargo machete`, which
            // hands the subcommand its own name as argv[1]. cargo-machete reads
            // every positional as a directory to walk, so the CI spelling
            // scans a nonexistent `machete/` alongside `crates/` and fails on
            // every candidate for a reason unrelated to the candidate. Caught
            // by running the umbrella for real (#4706); `crates` is still
            // exactly the path CI scans.
            program: "cargo-machete",
            args: &["crates"],
            env: &[],
            requires: &["cargo", "cargo-machete"],
            requires_targets: &[],
            prepare: None,
        }),
        "verify.suppress" => Some(VerifyInvocation {
            program: "python3",
            args: &["scripts/check-suppressions.py"],
            env: &[],
            requires: &["git", "python3"],
            requires_targets: &[],
            prepare: None,
        }),
        _ => None,
    }
}

/// Whether a clippy run that was *not* asked to deny warnings should count as a
/// failure: it should exactly when it emitted a warning or an error, which is
/// what `-D warnings` means (#4706).
///
/// Reads cargo's JSON diagnostic stream rather than scanning rendered text, so
/// the verdict turns on a structured `level` rather than on the word "warning"
/// appearing in someone's identifier or doc comment.
fn clippy_verdict(stdout: &str) -> bool {
    !stdout.lines().any(|line| diagnostic_level(line).is_some_and(|level| level == "warning" || level == "error"))
}

/// The diagnostic level one `--message-format=json` line reports, or `None`
/// when the line is not a compiler message (cargo interleaves build progress on
/// the same stream).
fn diagnostic_level(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("reason")?.as_str()? != "compiler-message" {
        return None;
    }
    Some(value.get("message")?.get("level")?.as_str()?.to_owned())
}

/// The human-readable rendering of every diagnostic in a JSON stream, which is
/// what a `Refine` re-entry is handed. cargo puts the same text rustc would
/// have printed in each message's `rendered` field, so nothing is lost by
/// asking for JSON.
fn render_diagnostics(stdout: &str) -> String {
    stdout
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            (value.get("reason")?.as_str()? == "compiler-message")
                .then(|| value.get("message")?.get("rendered")?.as_str().map(str::to_owned))
                .flatten()
        })
        .collect::<Vec<String>>()
        .join("\n")
}

/// The typed id of the verify umbrella (#3626) the reducer dispatches for the
/// Verify stage (`Transformation::for_member_stage`) — distinct from the
/// concrete `verify.*` ids `verify_command` maps individually.
pub(super) const VERIFY_CHECK: &str = "verify.check";

/// The ordered member ids `verify.check` fans out to, in CI-parity order.
/// Pure so the umbrella membership is testable without spawning cargo; growing
/// this list (e.g. a future `verify.test`) needs no change to the reducer's
/// dispatched stage command.
fn verify_check_members() -> &'static [&'static str] {
    &["verify.suppress", "verify.fmt", "verify.clippy", "verify.docs", "verify.test", "verify.dup", "verify.deps"]
}

/// Aggregate `verify.check`'s member results: pass iff every member passed.
/// Pure so the aggregation is testable without spawning cargo.
fn all_passed(statuses: &[bool]) -> bool {
    statuses.iter().all(|&passed| passed)
}

/// Project failed member outcomes onto ADR-0178's closed canonical set.
fn failed_verifiers<'a>(members: impl IntoIterator<Item = (&'a str, bool)>) -> VerifyFailureSet {
    members.into_iter().filter_map(|(id, passed)| (!passed).then(|| VerifyFailure::from_name(id)).flatten()).collect()
}

/// Every program the umbrella's members need — the roots
/// [`tools::preflight`] resolves through the dependency graph.
fn required_tools() -> Vec<&'static str> {
    verify_check_members()
        .iter()
        .filter_map(|id| verify_command(id))
        .flat_map(|invocation| invocation.requires.iter().copied())
        .collect()
}

/// Standalone prerequisites this slice declares without extending the shared
/// cargo/node dependency graph. Both are host roots with no repository-known
/// prerequisite, so a direct `--version` probe is the complete check.
const STANDALONE_TOOLS: [(&str, &str); 2] =
    [("git", "install Git (https://git-scm.com)"), ("python3", "install Python 3 (https://www.python.org)")];

/// Resolve the dependency graph and the suppression scanner's standalone host
/// roots into one fail-closed preflight result.
fn preflight_tools() -> Vec<tools::Missing> {
    let required = required_tools();
    let mut missing = tools::preflight(&required);
    missing.extend(
        STANDALONE_TOOLS
            .iter()
            .filter(|(program, _)| required.contains(program))
            .filter(|(program, _)| {
                !Command::new(program).arg("--version").output().is_ok_and(|output| output.status.success())
            })
            .map(|(program, install)| tools::Missing { requirement: program, install: (*install).to_owned() }),
    );
    missing
}

/// Every toolchain target the umbrella's members cross-build for, checked
/// alongside the programs. Pure so the union is testable without probing a
/// host: a target declared on a member but never gathered here is a
/// prerequisite nothing verifies.
fn required_targets() -> Vec<&'static str> {
    verify_check_members()
        .iter()
        .filter_map(|id| verify_command(id))
        .flat_map(|invocation| invocation.requires_targets.iter().copied())
        .collect()
}

/// The log a member contributes when its prepare step failed, in place of the
/// output it never produced.
///
/// CI runs this pre-build as a job step, and a step that exits non-zero ends the
/// job. The lane's members are deliberately independent — one failing never
/// suppresses another — but a member's own prepare is not a sibling: it builds
/// the artifacts that member's tests load, so running the suite without it
/// reports one host or pre-build fault once per affected test, and every one of
/// those reads as a defect in code that is fine (#4717).
///
/// The opening line is doing the same work the findings preamble does: without
/// it the reader sees a build failure with no statement of which step produced
/// it, and attributes it to the member.
fn prepare_failure_log(id: &str, prepare: &[&str], captured: &str) -> String {
    format!(
        "error: {id} did not run — its pre-build step `cargo {}` failed, so the artifacts its tests \
         load were never built.\n{captured}",
        prepare.join(" ")
    )
}

/// Run one umbrella member and reduce its output to `(passed, log, exit_code)`.
fn run_member(id: &str, invocation: &VerifyInvocation) -> Result<(bool, Vec<u8>, i32)> {
    let output = run_captured(invocation.command())
        .with_context(|| format!("spawn {} {}", invocation.program, invocation.args.join(" ")))?;

    // A JSON-format member's verdict is ours to derive: its exit status is
    // success even with lints present, because the run was not asked to deny
    // them. Everything else mirrors its own exit.
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (passed, rendered) = if id == "verify.clippy" {
        (output.status.success() && clippy_verdict(&stdout), render_diagnostics(&stdout))
    } else {
        (output.status.success(), stdout)
    };

    let mut log = rendered.into_bytes();
    log.extend_from_slice(&output.stderr);
    Ok((passed, log, effective_exit_code(passed, output.status.code())))
}

// A derived verdict (currently clippy's structured warning predicate) may fail
// even when the child exited zero. The umbrella must still exit nonzero so its
// evidence status and the Actions step outcome cannot disagree.
fn effective_exit_code(passed: bool, exit_code: Option<i32>) -> i32 {
    if passed {
        exit_code.unwrap_or(0)
    } else {
        exit_code.filter(|code| *code != 0).unwrap_or(1)
    }
}

/// Run `invocation`'s prepare step, when it has one. `None` is a member clear to
/// run; `Some((log, exit_code))` is a prepare that failed, already framed as the
/// member's log by [`prepare_failure_log`].
fn run_prepare(id: &str, invocation: &VerifyInvocation) -> Result<Option<(String, i32)>> {
    let Some(prepare) = invocation.prepare else {
        return Ok(None);
    };

    let mut step = Command::new("cargo");
    step.args(prepare);
    let output = run_captured(step).with_context(|| format!("spawn cargo {}", prepare.join(" ")))?;
    if output.status.success() {
        return Ok(None);
    }

    let mut captured = String::from_utf8_lossy(&output.stdout).into_owned();
    captured.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(Some((prepare_failure_log(id, prepare, &captured), output.status.code().unwrap_or(1))))
}

/// The single mechanical-verify path: run the mapped command, capture
/// stdout+stderr, write evidence, and mirror the verify's own exit status. An
/// unrecognized command id is an operational failure — it exits non-zero with
/// no evidence written, distinct from a verify that ran and failed.
pub(super) fn run_single(args: &TransformArgs) -> Result<()> {
    let Some(invocation) = verify_command(&args.command) else {
        bail!("unrecognized transform command id: {}", args.command);
    };

    let output = run_captured(invocation.command())
        .with_context(|| format!("spawn {} {}", invocation.program, invocation.args.join(" ")))?;

    fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out.display()))?;

    let log_name = format!("{}.log", args.command);
    let log_path = args.out.join(&log_name);
    let mut log_bytes = output.stdout.clone();
    log_bytes.extend_from_slice(&output.stderr);
    fs::write(&log_path, &log_bytes).with_context(|| format!("write {}", log_path.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let passed = output.status.success() && (args.command != "verify.clippy" || clippy_verdict(&stdout));
    let exit_code = effective_exit_code(passed, output.status.code());

    // A failing single verify contributes its own diagnostics on the same
    // channel the umbrella uses, so a lane run alone directs a Refine too.
    let findings = (!passed)
        .then(|| verify_findings(&[(args.command.as_str(), false, String::from_utf8_lossy(&log_bytes).into_owned())]))
        .flatten();

    let failures = (!passed).then(|| VerifyFailure::from_name(&args.command).map(VerifyFailureSet::one)).flatten();
    let evidence =
        build_evidence(&args.command, args.nonce.clone(), passed, Some(exit_code), log_name, findings, failures);
    write_json_pretty(&args.out.join("evidence.json"), &evidence)?;

    if passed {
        Ok(())
    } else {
        process::exit(exit_code);
    }
}

/// The line prefixes that open a diagnostic in the verify lanes' output — rustc
/// / clippy / rustdoc errors and warnings, their `-->` source locations, and
/// rustfmt's per-file diff header. Suppression findings use their own
/// `path:line — token — source` shape and are recognized separately below.
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

/// Distil one member's log the way *that* member's failures are written.
///
/// The openers in [`DIAGNOSTIC_OPENERS`] are rustc's, and a failing test is not
/// a rustc diagnostic: the only line nextest leaves for them to match is its
/// closing `error: test run failed`, which names nothing (#4712). So
/// `verify.test` reads its own log first, and falls through to the generic
/// distiller when that log names no failing test — which is what a compile
/// error inside a test target produces, and that one *is* a rustc diagnostic
/// arriving on the channel the openers were written for.
fn distil_member(id: &str, log: &str) -> Option<String> {
    if id == "verify.test"
        && let Some(failures) = nextest::distil_test_failures(log)
    {
        return Some(failures);
    }

    distil_diagnostics(log)
}

/// Whether a log line opens (or locates) a diagnostic rather than reporting
/// progress. Leading whitespace is ignored — rustc indents its `-->` locations.
fn opens_a_diagnostic(line: &str) -> bool {
    let trimmed = line.trim_start();
    DIAGNOSTIC_OPENERS.iter().any(|opener| trimmed.starts_with(opener)) || opens_a_suppression_finding(trimmed)
}

/// Whether one scanner output line starts with a concrete `path:line` and the
/// suppression gate's delimiter. Keeping this narrow prevents ordinary prose
/// containing an em dash from displacing a real diagnostic in Refine evidence.
fn opens_a_suppression_finding(line: &str) -> bool {
    let Some((location, _)) = line.split_once(" — ") else {
        return false;
    };
    let Some((path, line_number)) = location.rsplit_once(':') else {
        return false;
    };
    !path.is_empty() && line_number.parse::<usize>().is_ok()
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
        .filter_map(|(id, _, log)| distil_member(id, log).map(|body| format!("### {id}\n\n{body}")))
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

    // Preflight before anything runs. A host missing a tool cannot compute what
    // the member would have said, and reporting that as a pass would let a
    // candidate integrate on the strength of a check that never happened.
    let mut missing = preflight_tools();
    missing.extend(tools::preflight_targets(&required_targets()));
    if !missing.is_empty() {
        let evidence = Evidence {
            findings: Some(tools::missing_findings(&missing)),
            failed_verifiers: Some(VerifyFailureSet::one(VerifyFailure::Preflight)),
            command: VERIFY_CHECK.to_owned(),
            nonce: args.nonce.clone(),
            status: "fail",
            exit_code: Some(1),
            log: String::new(),
        };
        write_json_pretty(&args.out.join("evidence.json"), &evidence)?;
        process::exit(1);
    }

    let mut log_names = Vec::with_capacity(verify_check_members().len());
    let mut passed = Vec::with_capacity(verify_check_members().len());
    let mut outcomes = Vec::with_capacity(verify_check_members().len());
    let mut first_failure_code: Option<i32> = None;

    for &id in verify_check_members() {
        let invocation = verify_command(id).expect("verify_check_members ids all resolve via verify_command");
        // The member's own prerequisite, run immediately before it rather than
        // once up front: it belongs to this member, and a member that is one day
        // removed should take its prepare step with it. A failed prepare fails
        // the member without running it — see `prepare_failure_log`.
        let (member_passed, log_bytes, exit_code) = match run_prepare(id, &invocation)? {
            Some((log, code)) => (false, log.into_bytes(), code),
            None => run_member(id, &invocation)?,
        };

        let log_name = format!("{id}.log");
        let log_path = args.out.join(&log_name);
        fs::write(&log_path, &log_bytes).with_context(|| format!("write {}", log_path.display()))?;

        if !member_passed && first_failure_code.is_none() {
            first_failure_code = Some(exit_code);
        }
        outcomes.push((id, member_passed, String::from_utf8_lossy(&log_bytes).into_owned()));
        log_names.push(log_name);
        passed.push(member_passed);
    }

    let all_pass = all_passed(&passed);
    let failures = failed_verifiers(outcomes.iter().map(|(id, passed, _)| (*id, *passed)));
    let evidence = Evidence {
        findings: verify_findings(&outcomes),
        failed_verifiers: (!failures.is_empty()).then_some(failures),
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
        MAX_FINDING_LINES, VERIFY_CHECK, all_passed, clippy_verdict, distil_diagnostics, effective_exit_code,
        failed_verifiers, preflight_tools, prepare_failure_log, render_diagnostics, required_targets, required_tools,
        verify_check_members, verify_command, verify_findings,
    };
    use crate::cargo::WASM_TARGET;
    use crate::transform::construct::CONSTRUCT_IMPLEMENT;
    use crate::transform::review::REVIEW_CRITIC;
    use aether_bloomery::{VerifyFailure, VerifyFailureSet};

    #[test]
    fn known_ids_map_to_ci_parity_argv() {
        let fmt = verify_command("verify.fmt").expect("verify.fmt mapped");
        assert_eq!(fmt.args, &["fmt", "--all", "--", "--check"]);

        // Tripwire: clippy must NOT deny (#4706). Denying makes a lint a
        // compile error, so a lib that trips one is never built and nothing
        // depending on it is ever linted — the repair loop then gets one layer
        // per round and wedges on a budget of three while genuinely converging.
        // `clippy_verdict` applies the same fail-on-any-warning predicate over
        // the complete list this run produces.
        let clippy = verify_command("verify.clippy").expect("verify.clippy mapped");
        assert_eq!(clippy.args, &["clippy", "--workspace", "--all-targets", "--keep-going", "--message-format=json"],);
        assert!(!clippy.args.contains(&"-D"), "denying re-creates the cascade the verdict change removed");

        // Tripwire: the test member must match CI's own invocation, including
        // the wasm pre-build its scenario tests load and the env that makes a
        // missing one loud. A lane running fewer tests than the gate it
        // predicts is a false green that surfaces at the landing pull request.
        let test = verify_command("verify.test").expect("verify.test mapped");
        assert_eq!(test.args, &["nextest", "run", "--all-features", "--profile", "ci", "--no-fail-fast"]);
        assert_eq!(test.prepare, Some(&["xtask", "dist"][..]), "scenario tests need the component wasm built");
        assert_eq!(
            test.env,
            &[("AETHER_REQUIRE_RUNTIME", "1"), ("AETHER_STORE_PATH", ":memory:")],
            "a missing wasm must fail rather than skip, and the suite must never inherit a store to open",
        );

        // Tripwire: `crates` is the path, and it is not optional — cargo-machete
        // reads its own subcommand name as the directory to walk without it and
        // fails on every candidate for a reason that has nothing to do with the
        // candidate. Caught by running the umbrella for real (#4706).
        let deps = verify_command("verify.deps").expect("verify.deps mapped");
        assert_eq!(deps.program, "cargo-machete", "going through `cargo machete` re-adds the bogus path");
        assert_eq!(deps.args, &["crates"]);

        let suppress = verify_command("verify.suppress").expect("verify.suppress mapped");
        assert_eq!(suppress.program, "python3");
        assert_eq!(suppress.args, &["scripts/check-suppressions.py"]);
        assert_eq!(suppress.requires, &["git", "python3"]);
        let tools = required_tools();
        assert!(tools.contains(&"git"));
        assert!(tools.contains(&"python3"));
        assert!(
            preflight_tools().iter().all(|missing| missing.requirement != "git" && missing.requirement != "python3"),
            "the host running the verifier tests must satisfy the scanner roots",
        );

        let docs = verify_command("verify.docs").expect("verify.docs mapped");
        assert_eq!(docs.args, &["doc", "--workspace", "--no-deps", "--document-private-items", "--keep-going"]);
        assert_eq!(docs.env.len(), 1);
        assert_eq!(docs.env[0].0, "RUSTDOCFLAGS");
    }

    // One cargo JSON line per diagnostic level, plus the build-progress noise
    // cargo interleaves on the same stream.
    fn json_line(level: &str, rendered: &str) -> String {
        format!(r#"{{"reason":"compiler-message","message":{{"level":"{level}","rendered":"{rendered}"}}}}"#)
    }

    #[test]
    fn a_clippy_run_that_emitted_a_warning_fails_even_though_cargo_exited_zero() {
        // Tripwire: the whole verdict change (#4706). The run is not asked to
        // deny warnings — that is what stops the compile cascade and keeps every
        // dependent target linted — so cargo exits 0 with lints present. If this
        // predicate regressed to trusting the exit status, every lint in the
        // workspace would pass verify silently, which is strictly worse than the
        // truncated reporting it replaced.
        let stream = [
            r#"{"reason":"compiler-artifact","target":{"name":"aether-bloomery"}}"#.to_owned(),
            json_line("warning", "warning: unnecessary qualification"),
        ]
        .join("\n");

        assert!(!clippy_verdict(&stream), "a warning is a failure exactly as `-D warnings` would make it");
        assert!(clippy_verdict(r#"{"reason":"compiler-artifact","target":{"name":"x"}}"#), "progress alone passes");
        assert!(clippy_verdict(""), "a silent run is a clean run");
        assert_eq!(effective_exit_code(false, Some(0)), 1, "a derived failure must make the umbrella exit nonzero");
    }

    #[test]
    fn a_note_level_message_is_not_a_failure() {
        // rustc emits `note` and `help` alongside real diagnostics. Counting
        // them would fail every candidate that has any diagnostic context at
        // all, including passing ones.
        assert!(clippy_verdict(&json_line("note", "note: required by a bound")));
    }

    #[test]
    fn the_rendered_text_survives_the_json_round_trip() {
        // The JSON format is for the verdict; the model still has to read the
        // diagnostics. cargo carries rustc's own rendering in `rendered`, so
        // asking for JSON must not cost the human-readable text.
        let stream = [
            json_line("warning", "warning: unused import"),
            r#"{"reason":"build-finished","success":true}"#.to_owned(),
            json_line("error", "error: could not compile"),
        ]
        .join("\n");

        let rendered = render_diagnostics(&stream);
        assert!(rendered.contains("unused import"));
        assert!(rendered.contains("could not compile"));
        assert!(!rendered.contains("build-finished"), "progress must not reach the findings");
    }

    #[test]
    fn the_cross_target_the_pre_build_needs_reaches_the_preflight() {
        // Tripwire: CI's toolchain step installs wasm32-unknown-unknown before
        // `cargo xtask dist`, and the lane has no equivalent unless the member's
        // declaration is gathered into the umbrella's preflight union. Declared
        // but ungathered is the silent half of the bug: a host without the
        // wasm32 standard library cross-builds no component wasm, the prepare
        // fails, and `AETHER_REQUIRE_RUNTIME=1` — set two fields above so a
        // missing wasm is loud — turns that one host fault into a failure per
        // scenario test, every one of them reported against a candidate that is
        // fine (#4717).
        let test = verify_command("verify.test").expect("verify.test mapped");
        assert_eq!(test.requires_targets, &[WASM_TARGET], "the dist pre-build cross-builds for this target");
        assert!(required_targets().contains(&WASM_TARGET), "a declared target the preflight never checks is inert");
    }

    #[test]
    fn a_failed_pre_build_says_which_step_failed_and_that_the_member_did_not_run() {
        // Tripwire: the previous shape printed a line to xtask's own stderr and
        // ran the suite anyway, so the member's log held thousands of tests
        // failing on artifacts that were never built. What a Refine needs from
        // this log is the pre-build's own diagnostics plus the fact that the
        // member never ran — attribute it to the member and the model chases
        // test failures whose cause is one line above them.
        let log = prepare_failure_log("verify.test", &["xtask", "dist"], "error: could not compile `aether-kit-mark`");

        let distilled = distil_diagnostics(&log).expect("a failed pre-build yields findings");

        assert!(distilled.contains("cargo xtask dist"), "the step that failed is named");
        assert!(distilled.contains("did not run"), "and the member's silence is stated, not inferred");
        assert!(distilled.contains("could not compile `aether-kit-mark`"), "the pre-build's diagnostics survive");
    }

    #[test]
    fn every_member_declares_the_tools_it_needs() {
        // Tripwire: preflight resolves the union of these. A member added
        // without them preflights as needing nothing, so a host missing its
        // tool discovers that by failing the check rather than by refusing —
        // which reports a candidate defect for a host fault.
        for id in verify_check_members() {
            let invocation = verify_command(id).expect("every umbrella member resolves");
            assert!(!invocation.requires.is_empty(), "{id} declares no tools");
        }
    }

    #[test]
    fn the_umbrella_covers_every_required_ci_job() {
        // Tripwire: a member missing here is a gate CI enforces and the lane
        // does not, and the lane exists to predict CI. The gap costs a whole
        // bloom re-entry, because the disagreement surfaces at the landing pull
        // request after integrate, aggregate verify, and review have all run.
        for id in [
            "verify.fmt",
            "verify.clippy",
            "verify.docs",
            "verify.test",
            "verify.dup",
            "verify.deps",
            "verify.suppress",
        ] {
            assert!(verify_check_members().contains(&id), "{id} is a required CI job the lane must run");
        }
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
    fn a_suppression_location_survives_findings_distillation() {
        let log = "scanning diff\ncrates/demo/src/lib.rs:17 — allow(clippy::all) — #[allow(clippy::all)]\ndone";

        let findings = verify_findings(&[("verify.suppress", false, log.to_owned())]).expect("findings");

        assert!(findings.contains("### verify.suppress"));
        assert!(findings.contains("crates/demo/src/lib.rs:17 — allow(clippy::all)"));
        assert!(!findings.contains("scanning diff"));
    }

    #[test]
    fn a_failing_test_member_names_the_test_rather_than_the_runner_summary() {
        // Tripwire for #4712 at the seam: `distil_member` has to route
        // verify.test through the nextest reader. Routed to the generic
        // distiller instead, this whole log yields `error: test run failed` —
        // the only line in it a rustc opener matches — and the model is asked
        // to repair a failure it cannot see.
        let log = "\
        FAIL [   0.008s] ( 156/3737) aether-actor::asset_sections asset_rides_a_named_custom_section_byte_exact

--- STDERR:              aether-actor::asset_sections asset_rides_a_named_custom_section_byte_exact ---
thread 'asset_rides_a_named_custom_section_byte_exact' panicked at crates/aether-actor/tests/asset_sections.rs:85:9:
AETHER_REQUIRE_RUNTIME=1 but aether_test_fixtures_bundle wasm not pre-built

     Summary [  74.644s] 3737 tests run: 3736 passed, 1 failed, 20 skipped
error: test run failed
";

        let findings = verify_findings(&[("verify.test", false, log.to_owned())]).expect("findings");

        assert!(findings.contains("### verify.test"));
        assert!(findings.contains("asset_rides_a_named_custom_section_byte_exact"), "the test is named");
        assert!(findings.contains("crates/aether-actor/tests/asset_sections.rs:85:9"), "with its file and line");
        assert!(findings.contains("wasm not pre-built"), "and what it said");
    }

    #[test]
    fn a_compile_error_in_a_test_target_still_surfaces_through_the_rustc_channel() {
        // Tripwire: a compile error inside a test target is a rustc diagnostic
        // and reaches findings today. Routing verify.test unconditionally to
        // the nextest reader would trade one blind failure shape for another —
        // the log names no failing test, so the reader has nothing to say and
        // the generic distiller must still get its turn.
        let log = "\
   Compiling aether-actor v0.3.0
error[E0308]: mismatched types
  --> crates/aether-actor/tests/asset_sections.rs:85:9
error: could not compile `aether-actor` (test \"asset_sections\") due to 1 previous error
";

        let findings = verify_findings(&[("verify.test", false, log.to_owned())]).expect("findings");

        assert!(findings.contains("error[E0308]: mismatched types"));
        assert!(findings.contains("--> crates/aether-actor/tests/asset_sections.rs:85:9"));
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
    fn verify_check_members_are_the_ci_parity_ids_in_order() {
        // Tripwire: every id verify.check fans out to must resolve via
        // verify_command, and the order must match ci.yml's job order — a drift
        // here breaks the umbrella-membership invariant. All seven of CI's
        // required gates, because a member CI enforces and the lane skips is a
        // false green that surfaces at the landing pull request (#4706).
        assert_eq!(
            verify_check_members(),
            &[
                "verify.suppress",
                "verify.fmt",
                "verify.clippy",
                "verify.docs",
                "verify.test",
                "verify.dup",
                "verify.deps",
            ],
        );
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
    fn failed_member_projection_is_exact_canonical_and_empty_on_pass() {
        let multi = failed_verifiers([
            ("verify.fmt", false),
            ("verify.clippy", true),
            ("verify.docs", false),
            ("verify.test", false),
        ]);
        assert_eq!(
            multi,
            [VerifyFailure::Fmt, VerifyFailure::Docs, VerifyFailure::Test].into_iter().collect(),
            "every failed command contributes its closed identity",
        );
        assert_eq!(failed_verifiers([("verify.test", false)]), VerifyFailureSet::one(VerifyFailure::Test));
        assert!(failed_verifiers([("verify.fmt", true), ("verify.deps", true)]).is_empty());
    }

    #[test]
    fn preflight_has_its_own_synthetic_failure_identity() {
        let failures = VerifyFailureSet::one(VerifyFailure::Preflight);
        assert_eq!(failures.to_mask(), "01");
        assert!(!failures.contains(VerifyFailure::Fmt), "missing tools are not attributed to a member");
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
    fn an_unknown_id_is_unmapped() {
        assert!(verify_command("verify.bogus").is_none());
        // construct.implement and review.critic are the model lanes' ids, not
        // verify ids — neither must resolve a verify invocation.
        assert!(verify_command(CONSTRUCT_IMPLEMENT).is_none());
        assert!(verify_command(REVIEW_CRITIC).is_none());
    }
}
