mod closure;
mod nextest;
mod scope;
mod tools;
#[cfg(test)]
mod workflow;

use std::fs;
use std::process::{self, Command};

use aether_bloomery::{VerifyFailure, VerifyFailureSet};
use anyhow::{Context, Result, bail};

use crate::cargo::{WASM_TARGET, run_captured, write_json_pretty};
use crate::transform::peak_memory::{self, PeakMemory};
use crate::transform::sccache::{self, CompilerCache};
use crate::transform::verify::closure::Closure;
use crate::transform::verify::scope::Scope;
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
    /// The exit codes with which this member's program reports a **finding
    /// about the candidate**, as distinct from every other nonzero exit, which
    /// says the member could not compute a verdict at all (#4845).
    ///
    /// The distinction is the difference between blaming the work and blaming
    /// the host. `verify.suppress` exits 2 for an `OperationalError` — an
    /// unreadable blob, a malformed `.jscpd.json`, a git invocation that would
    /// not run — and `cargo-machete` exits 2 for a path it cannot walk. Read as
    /// a finding, either one is forgiven once and then charges a repair roll
    /// against a candidate that did nothing wrong, eventually wedging the
    /// member on `repeated_verifiers` it could never clear.
    ///
    /// Stated per member because the codes are each program's own contract, not
    /// a shared convention: nextest reports test failures with 100 and a failed
    /// build with 101, cargo reports "could not compile" with 101, and the
    /// exit-1 members have no other code to say anything with. A code absent
    /// here — and a child that died on a signal, which has no code at all — is
    /// attributed to [`VerifyFailure::Preflight`] instead, so an unrecognized
    /// exit fails towards the host rather than towards the work.
    ///
    /// Tripwire: these are the observed codes of the programs
    /// [`verify_command`] pins. A member whose list drifts off its program's
    /// contract mis-files every one of its failures in one direction or the
    /// other.
    finding_exit_codes: &'static [i32],
    /// How much of the workspace this member looks at when the run computed a
    /// closure to narrow to (#4890).
    breadth: Breadth,
}

/// Whether a member's work narrows to the candidate diff's reverse-dependency
/// closure, or covers the workspace whatever the diff touched.
///
/// Stated per member rather than derived from the argv, because the reason a
/// member stays wide is never visible in its flags: `verify.dup` compares every
/// crate against every other and `verify.deps` walks `crates/` from the
/// filesystem rather than from the package graph, so both are cross-crate by
/// construction, while `verify.fmt` and `verify.suppress` are seconds either
/// way and narrowing them would buy nothing for the risk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Breadth {
    /// The whole workspace, whatever the run's scope says.
    Workspace,
    /// The run's closure, when it computed one.
    Closure,
}

/// The build environment every verify member runs under, whatever the host's
/// tooling (#4912).
///
/// Incremental compilation is off for the same two reasons CI has it off. It is
/// what the gate this lane predicts does, so a lane that compiled incrementally
/// would be judging a candidate under flags the gate never uses; and sccache
/// cannot cache an incremental compilation, so leaving it on would make the
/// wrapper pure overhead on every member that builds.
///
/// Not shared with the model lanes, deliberately. A construct lane's child is an
/// edit loop over one tree, where incremental is worth more than a cache hit
/// rate — so this rides the verify invocation rather than the cache export both
/// lanes go through.
const CI_BUILD_ENV: [(&str, &str); 1] = [("CARGO_INCREMENTAL", "0")];

impl VerifyInvocation {
    /// The [`Command`] this invocation runs under `scope` — program, argv, and
    /// env — handed to [`run_captured`] for the captured-output spawn.
    ///
    /// The host's compiler cache and peak-memory wrapper ride every member
    /// rather than only the ones that compile: naming which members build would
    /// be a second list to keep true, and both are inert in a member that never
    /// invokes rustc.
    ///
    /// What the member does *not* set is its build directory or its job cap.
    /// `CARGO_TARGET_DIR` and `CARGO_BUILD_JOBS` are inherited from the lane the
    /// coordinator dispatched (#4912), which is the whole point of them: the gate
    /// has to build where the lane that produced the candidate built, in the slot
    /// that lane holds, under the same share of the host. Setting either here
    /// would override the dispatch that knows which slot this is.
    fn command(&self, scope: &Scope, cache: Option<&CompilerCache>, peak: &PeakMemory) -> Command {
        let mut cmd = peak.command(self.program);
        cmd.args(self.args_under(scope)).envs(self.env.iter().copied()).envs(CI_BUILD_ENV.iter().copied());
        sccache::export(cache, &mut cmd);
        cmd
    }

    /// The argv this invocation runs under `scope`: the stated workspace-wide
    /// argv, or that argv with `--workspace` traded for one `-p <crate>` per
    /// crate in the closure.
    ///
    /// Traded rather than appended, because `--workspace` and `-p` are cargo's
    /// two spellings of the same choice and a command carrying both selects the
    /// whole workspace regardless — the run would compile every crate while
    /// reporting itself as narrowed, which is worse than not narrowing at all.
    /// `verify.test` states no `--workspace` (nextest's own default is the
    /// workspace), so there the filter is a no-op and the package flags are the
    /// whole change.
    fn args_under(&self, scope: &Scope) -> Vec<String> {
        let Some(packages) = scope.packages().filter(|_| self.breadth == Breadth::Closure) else {
            return self.args.iter().map(|arg| (*arg).to_owned()).collect();
        };

        self.args
            .iter()
            .filter(|arg| **arg != "--workspace")
            .map(|arg| (*arg).to_owned())
            .chain(packages.iter().flat_map(|package| ["-p".to_owned(), package.clone()]))
            .collect()
    }
}

/// Maps a typed `verify.*` command id to the invocation that answers it, in
/// CI-parity terms.
///
/// `--document-private-items` is what the `Rustdoc` job passes, and rustdoc
/// does not descend into a private module without it (#4694). `--keep-going`
/// keeps cargo scheduling past the first failing crate (#4690), so one run
/// reports every *independent* unit rather than stopping at one.
/// `--all-features` (#4836) is what the `Rustdoc` job also passes: a module
/// behind a non-default feature is otherwise never compiled by `cargo doc`,
/// so a denied lint inside it never runs, and the job stays green over code
/// it has not actually looked at.
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
/// symmetry ADR-0149 §Execution requires. The `workflow` module reads
/// `.github/workflows/ci.yml` so the comparison is against the command the
/// gate runs rather than against a second literal in this file (#4843).
fn verify_command(id: &str) -> Option<VerifyInvocation> {
    match id {
        "verify.fmt" => Some(VerifyInvocation {
            program: "cargo",
            args: &["fmt", "--all", "--", "--check"],
            env: &[],
            requires: &["cargo", "rustfmt"],
            requires_targets: &[],
            prepare: None,
            // rustfmt exits 1 for a diff under `--check`, and cargo fmt
            // forwards it. It has no second code: a manifest it cannot read
            // exits 1 too, so that conflation survives this change unresolved.
            finding_exit_codes: &[1],
            breadth: Breadth::Workspace,
        }),
        "verify.clippy" => Some(VerifyInvocation {
            program: "cargo",
            args: &["clippy", "--workspace", "--all-targets", "--keep-going", "--message-format=json"],
            env: &[],
            requires: &["cargo", "cargo-clippy"],
            requires_targets: &[],
            prepare: None,
            // A lint is a finding this run derives from the JSON stream at exit
            // zero; 101 is cargo's "could not compile", which is a finding too.
            finding_exit_codes: &[101],
            breadth: Breadth::Closure,
        }),
        "verify.docs" => Some(VerifyInvocation {
            program: "cargo",
            args: &["doc", "--workspace", "--no-deps", "--document-private-items", "--all-features", "--keep-going"],
            env: &[(
                "RUSTDOCFLAGS",
                "-D rustdoc::redundant_explicit_links -D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links",
            )],
            requires: &["cargo"],
            requires_targets: &[],
            prepare: None,
            // A denied rustdoc lint reaches cargo as a failed build, so the
            // documentation findings arrive on the same 101 a compile error
            // does.
            finding_exit_codes: &[101],
            breadth: Breadth::Closure,
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
            // nextest states its own codes rather than cargo's: 100 is a test
            // run with failures, 101 is a build that did not produce the
            // binaries to run. Its other codes are about nextest itself — an
            // unusable profile exits 96, an unparsable argv exits 2 — and none
            // of those is a statement about the candidate.
            finding_exit_codes: &[100, 101],
            breadth: Breadth::Closure,
        }),
        "verify.dup" => Some(VerifyInvocation {
            // The settings ride the argv rather than a `.jscpd.json` (#4856).
            // jscpd answers a malformed config file with one stderr warning
            // and a silent fall back to built-in defaults — no format list, no
            // `minTokens`, and above all no threshold — so the gate stopped
            // failing on duplication at the moment its config broke. A bad
            // flag it refuses outright (exit 2). This argv is the CI step's,
            // verbatim, so the lane predicts the gate.
            program: "npx",
            args: &["--yes", "jscpd@5.0.12", "-f", "rust", "-k", "150", "-t", "0.5", "-r", "consoleFull", "crates"],
            env: &[],
            requires: &["npx"],
            requires_targets: &[],
            prepare: None,
            // jscpd exits 1 when the duplication threshold is exceeded, and
            // npx exits 1 when it cannot fetch the package at all — the one
            // member whose host fault is genuinely indistinguishable from its
            // finding, so it keeps the candidate-blaming reading rather than
            // routing every real duplication report to the host. A bad flag is
            // exit 2, which lands on the operational side where it belongs.
            finding_exit_codes: &[1],
            breadth: Breadth::Workspace,
        }),
        "verify.deps" => Some(VerifyInvocation {
            // Invoked as the binary rather than through `cargo machete`, which
            // hands the subcommand its own name as argv[1]. cargo-machete reads
            // every positional as a directory to walk, so the CI spelling
            // scans a nonexistent `machete/` alongside `crates/` and fails on
            // every candidate for a reason unrelated to the candidate. Caught
            // by running the umbrella for real (#4706); `crates` is still
            // exactly the path CI scans.
            //
            // `--no-ignore` because the walk skips gitignored files by
            // default, so a `.gitignore` pattern matching a crate path drops
            // it from the scan and the gate reports "no unused dependencies"
            // over a crate it never opened (#4863). `--skip-target-dir` then
            // re-excludes `target/`, which is all `.gitignore` was excluding
            // here.
            program: "cargo-machete",
            args: &["--no-ignore", "--skip-target-dir", "crates"],
            env: &[],
            requires: &["cargo", "cargo-machete"],
            requires_targets: &[],
            prepare: None,
            // cargo-machete exits 1 for unused dependencies it found and 2 for
            // a path it could not walk — the same split the suppression
            // scanner draws, from a different program.
            finding_exit_codes: &[1],
            breadth: Breadth::Workspace,
        }),
        "verify.suppress" => Some(VerifyInvocation {
            program: "python3",
            args: &["scripts/check-suppressions.py"],
            env: &[],
            requires: &["git", "python3"],
            requires_targets: &[],
            prepare: None,
            // The scanner prints its suppressions and exits 1; every
            // `OperationalError` it raises — and python3's own 2 for a script
            // it cannot open — leaves by the 2 that says it never reached a
            // verdict. This is the split #4845 was filed for.
            finding_exit_codes: &[1],
            breadth: Breadth::Workspace,
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

/// The log the umbrella writes its computed scope to, alongside its members'
/// own logs — the receipt that makes a wrong closure visible in the envelope
/// rather than silent (#4890).
const SCOPE_LOG: &str = "verify.scope.log";

/// The ordered member ids `verify.check` fans out to, in CI-parity order.
/// Pure so the umbrella membership is testable without spawning cargo; growing
/// this list (e.g. a future `verify.test`) needs no change to the reducer's
/// dispatched stage command.
fn verify_check_members() -> &'static [&'static str] {
    &["verify.suppress", "verify.fmt", "verify.clippy", "verify.docs", "verify.test", "verify.dup", "verify.deps"]
}

/// The status the umbrella stamps, from what its members said (ADR-0176's
/// three-valued lane vocabulary). Pure, so the aggregation is testable without
/// spawning cargo.
///
/// `fail` the moment any member judged the candidate and found something,
/// `environment` when the only thing that went wrong went wrong outside
/// anything the candidate can reach, `pass` otherwise. The order matters in one
/// direction: a real finding outranks an environment fault, because a run that
/// found a defect has judged the candidate whatever else the host was doing.
fn umbrella_status(outcomes: &[MemberOutcome]) -> &'static str {
    if outcomes.iter().any(|outcome| matches!(outcome, MemberOutcome::Failed | MemberOutcome::Operational)) {
        return "fail";
    }
    if outcomes.contains(&MemberOutcome::Environment) {
        return "environment";
    }
    "pass"
}

/// What one umbrella member's run said.
///
/// The third case is the one that had to exist (#4845): a member that could not
/// compute a verdict has said nothing about the candidate, and folding it into
/// `Failed` charges the candidate for a broken scanner config or an unreadable
/// blob. The fourth is the same argument one level out (#4895): a member that
/// judged something the candidate cannot reach has said nothing about it
/// either, and the host is the one being reported on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MemberOutcome {
    /// The member ran and found nothing.
    Passed,
    /// The member ran and reported a defect in the candidate.
    Failed,
    /// The member could not reach a verdict — a host or tooling fault.
    Operational,
    /// The member reported only failures outside the candidate's
    /// reverse-dependency closure, twice — an environment fault that did not
    /// clear on its one repeat.
    Environment,
}

impl MemberOutcome {
    /// Whether this outcome contributes to an all-pass umbrella. Only a real
    /// verdict does: an operational fault leaves the check unperformed, and
    /// reporting an unperformed check as a pass is the false-green direction.
    fn passed(self) -> bool {
        matches!(self, Self::Passed)
    }

    /// The ADR-0178 identity this outcome contributes to `failed_verifiers`.
    ///
    /// A finding is the member's own. A fault that stopped the member reaching
    /// one is [`VerifyFailure::Preflight`] — the umbrella's synthetic identity
    /// for a prerequisite the host did not meet, which is exactly what an
    /// unreadable input or a scanner that could not run is. Routing it there
    /// keeps the failure visible and accountable without attributing it to a
    /// candidate that could never repair it, and adds no identity, so
    /// ADR-0178's `N + B` bound is unchanged.
    ///
    /// An environment fault carries no identity at all: ADR-0178's set names
    /// the verifiers that found something wrong with the candidate, and this
    /// member found nothing wrong with it. Charging it to
    /// [`VerifyFailure::Preflight`] would put a host outage into the repair
    /// ledger the member's stuckness is measured from.
    fn failure(self, id: &str) -> Option<VerifyFailure> {
        match self {
            Self::Passed | Self::Environment => None,
            Self::Failed => VerifyFailure::from_name(id),
            Self::Operational => Some(VerifyFailure::Preflight),
        }
    }
}

/// Classify one member's run from the code it exited with and, for a member
/// whose verdict this lane derives rather than reads, that derived verdict.
///
/// A zero exit is the member speaking: it either found nothing or — clippy's
/// case — emitted diagnostics the run was not asked to deny. Every other exit
/// is read against the codes the member states its findings with.
fn member_outcome(invocation: &VerifyInvocation, derived_pass: bool, code: Option<i32>) -> MemberOutcome {
    if code == Some(0) {
        return if derived_pass {
            MemberOutcome::Passed
        } else {
            MemberOutcome::Failed
        };
    }

    if code.is_some_and(|code| invocation.finding_exit_codes.contains(&code)) {
        MemberOutcome::Failed
    } else {
        MemberOutcome::Operational
    }
}

/// Project failed member outcomes onto ADR-0178's closed canonical set.
fn failed_verifiers<'a>(members: impl IntoIterator<Item = (&'a str, MemberOutcome)>) -> VerifyFailureSet {
    members.into_iter().filter_map(|(id, outcome)| outcome.failure(id)).collect()
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

/// The line a member's log opens with when it could not reach a verdict, in
/// place of the finding it never produced.
///
/// The same work `prepare_failure_log`'s opening line does, for the other way a
/// member can fail without saying anything about the candidate. A `Refine` is
/// handed these logs to repair, and an unframed host fault reads as a defect —
/// the model then edits working code until the scanner it cannot see stops
/// being broken. Naming the identity the run is accounted to also lets an
/// operator match the log against the `failed_verifiers` set without inferring
/// the mapping.
fn operational_failure_notice(id: &str, invocation: &VerifyInvocation, code: Option<i32>) -> String {
    let exited = code.map_or_else(|| String::from("died on a signal"), |code| format!("exited {code}"));
    let stated = invocation.finding_exit_codes.iter().map(i32::to_string).collect::<Vec<String>>().join(", ");
    format!(
        "error: {id} reported nothing about the candidate — `{}` {exited}, which is not one of the codes it \
         states a finding with ({stated}). A verdict it could not reach is a host or tooling fault, so this \
         run is accounted to {} rather than to {id}.\n",
        invocation.program,
        VerifyFailure::Preflight,
    )
}

/// One member run's captured output, in the shape the umbrella reads it.
///
/// A struct rather than a [`std::process::Output`] because this is the seam a
/// test scripts (#4895): the rerun policy's whole subject is what a *second*
/// run of the same member said, and no fixture can make cargo say it twice.
struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: Option<i32>,
}

/// How the umbrella obtains one member's captured output.
///
/// `scope` rides the call rather than the runner because it is a property of
/// *what* is run — it decides the member's argv (#4890) — while the runner's
/// own cache is a property of *how*: the environment the spawn happens under,
/// which a rerun repeats unchanged.
trait MemberRunner {
    fn run(&mut self, invocation: &VerifyInvocation, scope: &Scope) -> Result<Captured>;
}

/// The runner the lane uses: spawn the member's own command, through the host's
/// compiler cache when it has one (#4894).
///
/// The cache belongs to the runner rather than to the policy above it: which
/// environment a member is spawned under is a property of *how* it is run, and
/// a rerun is the same spawn a second time.
struct SpawnRunner<'a> {
    cache: Option<&'a CompilerCache>,
    peak: &'a PeakMemory,
}

impl MemberRunner for SpawnRunner<'_> {
    fn run(&mut self, invocation: &VerifyInvocation, scope: &Scope) -> Result<Captured> {
        let output = run_captured(invocation.command(scope, self.cache, self.peak))
            .with_context(|| format!("spawn {} {}", invocation.program, invocation.args.join(" ")))?;
        // The wrapper's report is taken off the stderr the member's log keeps:
        // the reading belongs in the evidence record, and the log belongs to
        // whoever reads the failure.
        Ok(Captured { stdout: output.stdout, stderr: self.peak.take_report(output.stderr), code: output.status.code() })
    }
}

/// Run one umbrella member under `scope` and reduce its output to `(outcome,
/// log, exit_code)`.
fn run_member(
    id: &str,
    invocation: &VerifyInvocation,
    scope: &Scope,
    runner: &mut dyn MemberRunner,
) -> Result<(MemberOutcome, Vec<u8>, i32)> {
    let output = runner.run(invocation, scope)?;

    // A JSON-format member's verdict is ours to derive: its exit status is
    // success even with lints present, because the run was not asked to deny
    // them. Everything else has nothing to derive and passes its zero exit
    // through.
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (derived_pass, rendered) = if id == "verify.clippy" {
        (clippy_verdict(&stdout), render_diagnostics(&stdout))
    } else {
        (true, stdout)
    };

    let code = output.code;
    let outcome = member_outcome(invocation, derived_pass, code);

    let mut log = Vec::new();
    // The narrowing states itself where the reader is: a log that names a
    // handful of crates is otherwise indistinguishable from a workspace run
    // that happened to compile nothing else.
    if let Some(notice) = member_scope_notice(invocation, scope) {
        log.extend_from_slice(notice.as_bytes());
    }
    if outcome == MemberOutcome::Operational {
        log.extend_from_slice(operational_failure_notice(id, invocation, code).as_bytes());
    }
    log.extend_from_slice(rendered.as_bytes());
    log.extend_from_slice(&output.stderr);

    Ok((outcome, log, effective_exit_code(outcome.passed(), code)))
}

/// One member's contribution to the umbrella: what it concluded, the log the
/// evidence directory keeps, the findings a repair lap is directed by, and the
/// environment observation the lane reports *instead of* handing over.
///
/// The two prose channels are separate on purpose (#4895). Findings are work: a
/// `Refine` re-entry is told to fix them. An observation is a receipt: it names
/// what this run declined to blame the candidate for, so a misclassification is
/// visible to a reader instead of being a silence.
struct MemberRun {
    id: String,
    outcome: MemberOutcome,
    log: Vec<u8>,
    exit_code: i32,
    findings: Option<String>,
    observation: Option<String>,
}

impl MemberRun {
    /// A member whose log the distillers read as they always have — everything
    /// but a `verify.test` run whose failures were classified.
    fn plain(id: &str, outcome: MemberOutcome, log: Vec<u8>, exit_code: i32) -> Self {
        let findings = (!outcome.passed()).then(|| distil_member(id, &String::from_utf8_lossy(&log))).flatten();
        Self { id: id.to_owned(), outcome, log, exit_code, findings, observation: None }
    }
}

/// The classification a member's run admits, or `None` for a run there is
/// nothing to classify: a member other than `verify.test`, an outcome that is
/// not a finding about the candidate, or a log naming no failing test (a
/// compile error inside a test target, which is the rustc channel's to report).
fn classify_failures(
    id: &str,
    outcome: MemberOutcome,
    log: &[u8],
    closure: Option<&Closure>,
) -> Option<nextest::ClassifiedRun> {
    (id == "verify.test" && outcome == MemberOutcome::Failed)
        .then(|| nextest::classify(&String::from_utf8_lossy(log), closure))
        .flatten()
}

/// The line separating a member's first run from its repeat inside the one log
/// the evidence keeps.
///
/// Both runs stay in that log. The receipt for a rerun is what each of the two
/// runs said, and a log holding only the second cannot show that the first was
/// environmental.
fn rerun_notice(id: &str) -> String {
    format!(
        "\n\n{id}: every failure above lies outside the candidate's reverse-dependency closure — a transient \
         host fault far more often than a candidate defect, so the member is being rerun once. Everything \
         below is that second run.\n\n"
    )
}

/// The environment observation for a member whose first run reported only
/// out-of-closure failures, framed by what its one repeat then did.
fn rerun_observation(outcome: MemberOutcome, block: Option<String>) -> Option<String> {
    let verdict = match outcome {
        MemberOutcome::Passed => {
            "The repeat came back green, so the block did not repeat: it was the host, and the member passes \
             without spending a repair lap."
        }
        MemberOutcome::Environment => {
            "The repeat reported the same out-of-closure block, so this member escalates as an environment \
             fault on the bloom rather than as findings against a candidate that cannot reach it."
        }
        _ => {
            "The repeat reported failures inside the closure, so the member is judged on those; the block \
             above is not among them."
        }
    };
    Some(format!("{}\n\n{verdict}", block?))
}

/// Run one member, discriminating an environment fault from a candidate defect
/// (#4895).
///
/// Only a failing `verify.test` run can be discriminated: a failing test names
/// the package it lives in, and that package either links something the diff
/// touched or it does not. Every other member reports on the workspace as a
/// whole and has no such axis, so it passes straight through.
///
/// A run whose failures are *all* out of closure is worth exactly one repeat
/// before anything is charged to anyone — the fault class is transient by
/// nature (contention, a fork that failed, a box out of memory), so a green
/// repeat is the honest verdict and a red one is a host still broken. A mixed
/// run is not repeated at all: it found a real defect, and the repair lap it
/// earns is handed the in-closure half alone.
fn run_member_discriminated(
    id: &str,
    invocation: &VerifyInvocation,
    scope: &Scope,
    closure: Option<&Closure>,
    runner: &mut dyn MemberRunner,
) -> Result<MemberRun> {
    let (outcome, log, exit_code) = run_member(id, invocation, scope, runner)?;
    let Some(classified) = classify_failures(id, outcome, &log, closure) else {
        return Ok(MemberRun::plain(id, outcome, log, exit_code));
    };

    if !classified.is_environmental() {
        let findings = classified.findings().or_else(|| distil_diagnostics(&String::from_utf8_lossy(&log)));
        return Ok(MemberRun {
            id: id.to_owned(),
            outcome,
            log,
            exit_code,
            findings,
            observation: classified.observation(),
        });
    }

    let (repeat_outcome, repeat_log, repeat_exit) = run_member(id, invocation, scope, runner)?;
    let repeat = classify_failures(id, repeat_outcome, &repeat_log, closure);

    // The repeat is the run this member reports: the first one judged nothing
    // about the candidate, whichever way the second went.
    let outcome = if repeat_outcome.passed() {
        MemberOutcome::Passed
    } else if repeat.as_ref().is_some_and(nextest::ClassifiedRun::is_environmental) {
        MemberOutcome::Environment
    } else {
        repeat_outcome
    };
    let findings = matches!(outcome, MemberOutcome::Failed | MemberOutcome::Operational)
        .then(|| {
            repeat
                .as_ref()
                .and_then(nextest::ClassifiedRun::findings)
                .or_else(|| distil_diagnostics(&String::from_utf8_lossy(&repeat_log)))
        })
        .flatten();

    Ok(MemberRun {
        id: id.to_owned(),
        outcome,
        log: [log, rerun_notice(id).into_bytes(), repeat_log].concat(),
        exit_code: repeat_exit,
        findings,
        observation: rerun_observation(outcome, classified.observation()),
    })
}

/// The scope line this member's log opens with, when the member is one the run
/// narrowed. A workspace-wide member under a narrowed run gets none — claiming
/// a closure it did not honor would misdirect the reader of a `verify.dup`
/// failure to crates that were never the reason.
fn member_scope_notice(invocation: &VerifyInvocation, scope: &Scope) -> Option<String> {
    (invocation.breadth == Breadth::Closure).then(|| scope.member_notice()).flatten()
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
fn run_prepare(
    id: &str,
    invocation: &VerifyInvocation,
    cache: Option<&CompilerCache>,
    peak: &PeakMemory,
) -> Result<Option<(String, i32)>> {
    let Some(prepare) = invocation.prepare else {
        return Ok(None);
    };

    // The prepare cross-builds every component crate for wasm32 — the largest
    // single build in the lane — so it goes through the cache, the CI build
    // environment, and the peak-memory wrapper like the member it belongs to.
    let mut step = peak.command("cargo");
    step.args(prepare).envs(CI_BUILD_ENV.iter().copied());
    sccache::export(cache, &mut step);
    let output = run_captured(step).with_context(|| format!("spawn cargo {}", prepare.join(" ")))?;
    // Read before the success check: the pre-build is the lane's largest single
    // compile, so a prepare that *worked* is exactly the run whose peak the
    // concurrency model wants.
    let stderr = peak.take_report(output.stderr);
    if output.status.success() {
        return Ok(None);
    }

    let mut captured = String::from_utf8_lossy(&output.stdout).into_owned();
    captured.push_str(&String::from_utf8_lossy(&stderr));
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

    // The same narrowing seam the umbrella runs through (#4890), so a member
    // dispatched alone answers a work order's diff base the way it does inside
    // `verify.check` rather than by a second rule. An order that names none —
    // every current caller of this path — resolves the whole workspace.
    let scope = Scope::resolve(args.diff_base.as_deref());
    let cache = sccache::detect();
    let peak = peak_memory::detect();
    let output = run_captured(invocation.command(&scope, cache.as_ref(), &peak))
        .with_context(|| format!("spawn {} {}", invocation.program, invocation.args.join(" ")))?;
    let stderr = peak.take_report(output.stderr);

    fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let derived_pass = args.command != "verify.clippy" || clippy_verdict(&stdout);
    let code = output.status.code();
    let outcome = member_outcome(&invocation, derived_pass, code);

    let log_name = format!("{}.log", args.command);
    let log_path = args.out.join(&log_name);
    let mut log_bytes = Vec::new();
    if let Some(notice) = member_scope_notice(&invocation, &scope) {
        log_bytes.extend_from_slice(notice.as_bytes());
    }
    if outcome == MemberOutcome::Operational {
        log_bytes.extend_from_slice(operational_failure_notice(&args.command, &invocation, code).as_bytes());
    }
    log_bytes.extend_from_slice(&output.stdout);
    log_bytes.extend_from_slice(&stderr);
    fs::write(&log_path, &log_bytes).with_context(|| format!("write {}", log_path.display()))?;

    let passed = outcome.passed();
    let exit_code = effective_exit_code(passed, code);

    // A failing single verify contributes its own diagnostics on the same
    // channel the umbrella uses, so a lane run alone directs a Refine too.
    let findings =
        (!passed).then(|| verify_findings(&[MemberRun::plain(&args.command, outcome, log_bytes, exit_code)])).flatten();

    let failures = outcome.failure(&args.command).map(VerifyFailureSet::one);
    let evidence =
        build_evidence(&args.command, args.nonce.clone(), passed, Some(exit_code), log_name, findings, failures)
            .measured_by(cache.as_ref(), &peak);
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
///
/// The classification is against no closure here (#4895): this is the path a
/// member takes when the umbrella has nothing to discriminate with — a single
/// `verify.test` invocation, or a log the classifier already read — and a run
/// that cannot see the candidate's diff must blame the candidate.
fn distil_member(id: &str, log: &str) -> Option<String> {
    if id == "verify.test"
        && let Some(failures) = nextest::classify(log, None).as_ref().and_then(nextest::ClassifiedRun::findings)
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
///
/// A member that reported an environment fault contributes nothing here, by
/// construction rather than by filter: its findings channel is empty, because
/// what it saw was never the candidate's to fix (#4895).
fn verify_findings(members: &[MemberRun]) -> Option<String> {
    let failed: Vec<String> = members
        .iter()
        .filter_map(|member| member.findings.as_ref().map(|body| format!("### {}\n\n{body}", member.id)))
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

/// Assemble the members' environment observations into the evidence's own
/// channel — the receipts for everything this run declined to charge the
/// candidate for (#4895).
///
/// Deliberately not merged into `findings`: these are reported *on the lane*,
/// and a `Refine` re-entry handed them would spend its lap chasing a host.
fn environment_observations(members: &[MemberRun]) -> Option<String> {
    let observed: Vec<String> = members
        .iter()
        .filter_map(|member| member.observation.as_ref().map(|body| format!("### {}\n\n{body}", member.id)))
        .collect();

    (!observed.is_empty()).then(|| observed.join("\n\n"))
}

/// The `verify.check` umbrella (#3626): runs every member in
/// `verify_check_members()` unconditionally — no short-circuit on first
/// failure, so a partial failure still leaves every member's log for
/// diagnosis — then writes one aggregate `evidence.json` whose `status`
/// passes only when every member passed. Exit mirrors the aggregate, exactly
/// as the single-command path mirrors its own verify's exit.
///
/// The candidate's reverse-dependency closure is resolved once, before any
/// member runs, and every member is discriminated against that one answer
/// (#4895) — recomputing it per member would let a mid-run repository change
/// give two members two different candidates.
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
            // A run that refused before its first member compiled nothing, so
            // there is nothing for a cache to have served and nothing whose
            // memory there was to measure.
            sccache: None,
            peak_resident_bytes: None,
            command: VERIFY_CHECK.to_owned(),
            nonce: args.nonce.clone(),
            status: "fail",
            exit_code: Some(1),
            log: String::new(),
            environment: None,
        };
        write_json_pretty(&args.out.join("evidence.json"), &evidence)?;
        process::exit(1);
    }

    // Resolved once for the whole umbrella, so the counters the evidence carries
    // cover every member's build rather than one member's slice of it, and the
    // peak it reports is the high-water mark of the whole lane.
    let cache = sccache::detect();
    let peak = peak_memory::detect();
    let closure = closure::resolve(args.diff_base.as_deref());
    let mut runner = SpawnRunner { cache: cache.as_ref(), peak: &peak };

    // Resolved once, before any member runs, and written out as its own log:
    // the compiling members are only comparable across a run if they all
    // narrowed to the same closure, and a receipt no one can read is not a
    // receipt (#4890).
    let scope = Scope::resolve(args.diff_base.as_deref());
    let scope_log = String::from(SCOPE_LOG);
    let scope_path = args.out.join(&scope_log);
    fs::write(&scope_path, scope.receipt()).with_context(|| format!("write {}", scope_path.display()))?;

    let mut log_names = vec![scope_log];
    let mut runs = Vec::with_capacity(verify_check_members().len());
    let mut first_failure_code: Option<i32> = None;

    for &id in verify_check_members() {
        let invocation = verify_command(id).expect("verify_check_members ids all resolve via verify_command");
        // The member's own prerequisite, run immediately before it rather than
        // once up front: it belongs to this member, and a member that is one day
        // removed should take its prepare step with it. A failed prepare fails
        // the member without running it — see `prepare_failure_log`.
        //
        // It is the member's own failure, not an operational one, even though
        // the member never ran: ADR-0178 rules that "a failed member
        // preparation step belongs to that member's identity". The host half of
        // it is already covered — the preflight checks the cross-target the
        // pre-build needs — so what is left is the candidate breaking its own
        // build.
        let run = match run_prepare(id, &invocation, cache.as_ref(), &peak)? {
            Some((log, code)) => MemberRun::plain(id, MemberOutcome::Failed, log.into_bytes(), code),
            None => run_member_discriminated(id, &invocation, &scope, closure.as_ref(), &mut runner)?,
        };

        let log_name = format!("{id}.log");
        let log_path = args.out.join(&log_name);
        fs::write(&log_path, &run.log).with_context(|| format!("write {}", log_path.display()))?;

        if !run.outcome.passed() && first_failure_code.is_none() {
            first_failure_code = Some(run.exit_code);
        }
        log_names.push(log_name);
        runs.push(run);
    }

    let status = umbrella_status(&runs.iter().map(|run| run.outcome).collect::<Vec<MemberOutcome>>());
    let failures = failed_verifiers(runs.iter().map(|run| (run.id.as_str(), run.outcome)));
    let evidence = Evidence {
        findings: verify_findings(&runs),
        failed_verifiers: (!failures.is_empty()).then_some(failures),
        sccache: cache.as_ref().and_then(CompilerCache::served),
        peak_resident_bytes: peak.peak_resident_bytes(),
        command: VERIFY_CHECK.to_owned(),
        nonce: args.nonce.clone(),
        status,
        exit_code: Some(first_failure_code.unwrap_or(0)),
        log: log_names.join(", "),
        environment: environment_observations(&runs),
    };
    write_json_pretty(&args.out.join("evidence.json"), &evidence)?;

    if status == "pass" {
        Ok(())
    } else {
        process::exit(first_failure_code.unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Captured, MAX_FINDING_LINES, MemberOutcome, MemberRun, MemberRunner, Scope, VERIFY_CHECK, VerifyInvocation,
        clippy_verdict, closure, distil_diagnostics, effective_exit_code, environment_observations, failed_verifiers,
        member_outcome, member_scope_notice, operational_failure_notice, preflight_tools, prepare_failure_log,
        render_diagnostics, required_targets, required_tools, run_member_discriminated, umbrella_status,
        verify_check_members, verify_command, verify_findings, workflow,
    };
    use std::iter;

    use crate::cargo::WASM_TARGET;
    use crate::transform::construct::{CONSTRUCT_IMPLEMENT, CONSTRUCT_INSTRUCTIONS};
    use crate::transform::review::REVIEW_CRITIC;
    use aether_bloomery::{VerifyFailure, VerifyFailureSet};

    /// The full command line an invocation dispatches, program first, in the
    /// shape a workflow `run:` line is read into.
    fn argv(invocation: &VerifyInvocation) -> Vec<String> {
        iter::once(invocation.program).chain(invocation.args.iter().copied()).map(str::to_owned).collect()
    }

    fn owned(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| (*token).to_owned()).collect()
    }

    fn owned_pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect()
    }

    /// One member's run as the umbrella records it, from a canned log.
    fn member(id: &str, outcome: MemberOutcome, log: &str) -> MemberRun {
        MemberRun::plain(id, outcome, log.as_bytes().to_vec(), 1)
    }

    /// A runner that hands out canned captures in order — the seam standing in
    /// for the member's own command, so a rerun policy whose whole subject is
    /// the *second* run of a member is exercisable without a cargo invocation.
    struct ScriptedRunner {
        scripted: Vec<Captured>,
        runs: usize,
    }

    impl ScriptedRunner {
        /// A runner scripted with one `(log, exit code)` per run, in order.
        fn new(scripted: &[(&str, i32)]) -> Self {
            let scripted = scripted
                .iter()
                .map(|(log, code)| Captured { stdout: log.as_bytes().to_vec(), stderr: Vec::new(), code: Some(*code) })
                .collect();
            Self { scripted, runs: 0 }
        }
    }

    impl MemberRunner for ScriptedRunner {
        fn run(&mut self, _invocation: &VerifyInvocation, _scope: &Scope) -> anyhow::Result<Captured> {
            let captured = self.scripted.get(self.runs).expect("the policy ran the member more times than scripted");
            self.runs += 1;
            Ok(Captured { stdout: captured.stdout.clone(), stderr: captured.stderr.clone(), code: captured.code })
        }
    }

    /// A nextest log reporting `failures` as failing tests, in the shape the
    /// runner prints them: a status line, a captured-output banner, and a panic.
    fn failing_run(failures: &[&str]) -> String {
        let body = failures
            .iter()
            .enumerate()
            .map(|(index, test)| {
                format!(
                    "        FAIL [   0.008s] ({index:3}/900) {test}\n\n\
                     --- STDERR:              {test} ---\n\
                     thread 'test' panicked at crates/somewhere/src/lib.rs:{index}:9:\n\
                     boom\n"
                )
            })
            .collect::<Vec<String>>()
            .join("\n");

        format!(
            " Starting 900 tests across 312 binaries\n{body}\n     Summary [  74.6s] 900 tests run: {} failed, \
             0 skipped\nerror: test run failed\n",
            failures.len(),
        )
    }

    /// A clean nextest run — what a repeat on a quiet box prints.
    fn passing_run() -> String {
        " Starting 900 tests across 312 binaries\n     Summary [  71.2s] 900 tests run: 900 passed, 0 skipped\n"
            .to_owned()
    }

    #[test]
    fn ci_parity_argv_matches_the_command_the_workflow_runs() {
        // Tripwire: every argv here is a second spelling of a command Actions
        // runs, and only `.github/workflows/ci.yml` is the copy the gate
        // executes. Comparing the two Rust literals proved nothing about that
        // — the workflow could be trimmed back to default features, or lose a
        // flag, with the whole suite still green (#4843). Each assertion below
        // reads the workflow, so drift on either side fails here.
        let fmt = verify_command("verify.fmt").expect("verify.fmt mapped");
        assert_eq!(argv(&fmt), workflow::gate_step("fmt", &["cargo", "fmt"]).run);

        // The duplicate-code gate states its whole calibration on the argv
        // (#4856), so the argv is the gate: a lane running a different format,
        // threshold, or min-tokens predicts a different check than CI applies.
        // Selected on `npx` alone, since every token that carries calibration
        // has to stay on the asserted side rather than the selecting side.
        let dup = verify_command("verify.dup").expect("verify.dup mapped");
        assert_eq!(argv(&dup), workflow::gate_step("dup-check", &["npx"]).run);

        // The lane asks cargo for JSON diagnostics and deliberately does not
        // deny (#4706); `clippy_verdict` applies the same fail-on-any-warning
        // predicate over the complete list a non-denying run produces. That
        // trailing `-- -D warnings` is the whole permitted divergence —
        // everything before it is the shared invocation.
        let clippy = verify_command("verify.clippy").expect("verify.clippy mapped");
        let ci_clippy = workflow::gate_step("clippy", &["cargo", "clippy"]).run;
        let (shared, deny) = ci_clippy.split_at(ci_clippy.len() - 3);
        assert_eq!(
            deny,
            owned(&["--", "-D", "warnings"]),
            "CI's clippy gate denies through a trailing `-- -D warnings`"
        );
        assert_eq!(argv(&clippy), [shared.to_vec(), owned(&["--message-format=json"])].concat());

        // Docs is a straight mirror, env included: RUSTDOCFLAGS is what
        // escalates the tracked lints to failures, so a lint denied on one
        // side and not the other is a gate the lane cannot predict.
        let docs = verify_command("verify.docs").expect("verify.docs mapped");
        let ci_docs = workflow::gate_step("docs", &["cargo", "doc"]);
        assert_eq!(argv(&docs), ci_docs.run);
        assert_eq!(owned_pairs(docs.env), ci_docs.env);

        // cargo-machete is invoked as the binary rather than through `cargo
        // machete`, which hands the subcommand its own name as argv[1] and
        // walks a nonexistent `machete/` alongside `crates/` (#4706). The
        // directories it scans are still CI's, and they are not optional.
        let deps = verify_command("verify.deps").expect("verify.deps mapped");
        let ci_deps = workflow::gate_step("unused-deps", &["cargo", "machete"]).run;
        assert_eq!(deps.program, "cargo-machete", "going through `cargo machete` re-adds the bogus path");
        assert_eq!(owned(deps.args), ci_deps[2..].to_vec());

        // The test member drops CI's shard partition — the lane runs the suite
        // whole — and keeps every flag before it, `--all-features` above all: a
        // lane running fewer tests than the gate it predicts is a false green
        // that surfaces at the landing pull request. The wasm pre-build its
        // scenario tests load is CI's own step, in the same job.
        let test = verify_command("verify.test").expect("verify.test mapped");
        let ci_test = workflow::named_step("test", "Run tests (workspace, parallel)");
        let partition = ci_test.run.iter().position(|token| token == "--partition").expect("CI shards the full suite");
        assert_eq!(argv(&test), [ci_test.run[..partition].to_vec(), owned(&["--no-fail-fast"])].concat());
        assert_eq!(
            owned(test.prepare.expect("scenario tests need the component wasm built")),
            workflow::gate_step("test", &["cargo", "xtask", "dist"]).run[1..].to_vec(),
        );
        for pair in &ci_test.env {
            assert!(owned_pairs(test.env).contains(pair), "CI sets {pair:?} for the gate, so the lane must too");
        }
    }

    #[test]
    fn the_construct_instructions_state_the_gate_argv_a_lane_has_to_run() {
        // Tripwire: the construct lane's pre-exit checklist is a second
        // spelling of these invocations, and a near-miss one predicts nothing —
        // rustdoc without `--document-private-items` passes over the private
        // items the gate fails on, so the lane goes green and bounces anyway
        // (#4951). Flags move here often (#4836, #4863) and prose nobody
        // compiles goes stale silently, so the instructions are asserted
        // against the arms rather than proofread against them.
        for id in ["verify.fmt", "verify.clippy", "verify.docs", "verify.test", "verify.suppress"] {
            let invocation = verify_command(id).expect("member mapped");

            let stated = argv(&invocation).join(" ");
            assert!(
                CONSTRUCT_INSTRUCTIONS.contains(&stated),
                "{id} runs `{stated}`, which construct_instructions.md has to state for a lane to run it",
            );
            for &(key, value) in invocation.env {
                let setting = format!("{key}={value}");
                assert!(
                    CONSTRUCT_INSTRUCTIONS.contains(&setting),
                    "{id} runs under {setting}, and a lane that omits it runs a different check",
                );
            }
            if let Some(prepare) = invocation.prepare {
                let prepare = format!("cargo {}", prepare.join(" "));
                assert!(
                    CONSTRUCT_INSTRUCTIONS.contains(&prepare),
                    "{id} is preceded by `{prepare}`, without which the lane's run is missing its inputs",
                );
            }
        }
    }

    /// A computed closure over two crates, for the argv assertions.
    fn closure_scope(packages: &[&str]) -> Scope {
        Scope::Closure {
            packages: packages.iter().map(|name| (*name).to_owned()).collect(),
            skipped: vec![String::from("aether-render")],
        }
    }

    #[test]
    fn a_scoped_member_trades_the_workspace_flag_for_package_flags() {
        // Tripwire for #4890's whole mechanism. `--workspace` and `-p` are
        // cargo's two spellings of the same choice, and a command carrying
        // both selects the whole workspace regardless — so a narrowing that
        // appended instead of replacing would compile every crate while its
        // receipt claimed a closure, which is worse than never narrowing.
        let scope = closure_scope(&["aether-chassis-bloomery", "aether-math"]);

        let clippy = verify_command("verify.clippy").expect("verify.clippy mapped");
        let args = clippy.args_under(&scope);
        assert!(
            !args.contains(&String::from("--workspace")),
            "the workspace flag would re-select everything: {args:?}"
        );
        assert_eq!(
            args,
            owned(&[
                "clippy",
                "--all-targets",
                "--keep-going",
                "--message-format=json",
                "-p",
                "aether-chassis-bloomery",
                "-p",
                "aether-math",
            ]),
        );

        // nextest states no `--workspace` at all — the workspace is its own
        // default — so there the package flags are the entire narrowing.
        let test = verify_command("verify.test").expect("verify.test mapped");
        assert_eq!(
            test.args_under(&scope),
            [owned(test.args), owned(&["-p", "aether-chassis-bloomery", "-p", "aether-math"])].concat(),
        );
    }

    #[test]
    fn a_workspace_breadth_member_runs_the_whole_tree_under_a_closure() {
        // Tripwire: the members that stay wide are wide *because* their work
        // is cross-crate. jscpd compares every crate against every other, so a
        // narrowed run reports no duplication between a changed crate and the
        // one it was copied from; cargo-machete walks `crates/` from the
        // filesystem and has no package selection to narrow at all.
        let scope = closure_scope(&["aether-chassis-bloomery"]);

        for id in ["verify.fmt", "verify.dup", "verify.deps", "verify.suppress"] {
            let invocation = verify_command(id).expect("member mapped");
            assert_eq!(invocation.args_under(&scope), owned(invocation.args), "{id} must not narrow");
            assert_eq!(member_scope_notice(&invocation, &scope), None, "{id} must not claim a closure it ignored");
        }
    }

    #[test]
    fn a_run_without_a_closure_dispatches_every_members_stated_argv() {
        // Tripwire for acceptance 3: the aggregate verify names no diff base,
        // and its every member must therefore dispatch the exact argv the
        // CI-parity assertions above pin. A narrowing that leaked into the
        // unscoped path would move the stage that proves the landing.
        let scope = Scope::resolve(None);

        for &id in verify_check_members() {
            let invocation = verify_command(id).expect("member mapped");
            assert_eq!(invocation.args_under(&scope), owned(invocation.args), "{id} must dispatch its stated argv");
        }
    }

    #[test]
    fn lane_only_details_pin_what_the_workflow_does_not_state() {
        // Tripwire: the rest of each invocation, where the lane is deliberately
        // not a copy of a workflow step. Nothing here is derivable from
        // ci.yml, so each pin has to carry the reason it diverges.

        // Denying makes a lint a compile error, so a lib that trips one is
        // never built and nothing depending on it is ever linted — the repair
        // loop then gets one layer per round and wedges on a budget of three
        // while genuinely converging (#4706).
        let clippy = verify_command("verify.clippy").expect("verify.clippy mapped");
        assert!(!clippy.args.contains(&"-D"), "denying re-creates the cascade the verdict change removed");

        // `AETHER_STORE_PATH` pins what a CI runner gets for free: nothing
        // there names a store, so the suite falls to the `":memory:"` default.
        // Off Actions the gate can be reached from a coordinator whose
        // environment names the live journal (#4714).
        let test = verify_command("verify.test").expect("verify.test mapped");
        assert!(test.env.contains(&("AETHER_STORE_PATH", ":memory:")), "the suite must never inherit a store to open");

        // The suppressions job copies the scanner to a runner temp path and
        // invokes it with pull-request shas, so its argv has no lane
        // counterpart at all; what must hold is that the scanner roots the
        // host running these tests resolves are the ones it declares.
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
            member("verify.fmt", MemberOutcome::Passed, "error[E0001]: from a lane that passed"),
            member("verify.clippy", MemberOutcome::Failed, "error[E0308]: mismatched types"),
        ];

        let findings = verify_findings(&members).expect("a failing member yields findings");

        assert!(findings.contains("### verify.clippy"), "the failing member is named");
        assert!(!findings.contains("verify.fmt"), "a passing member contributes nothing");
    }

    #[test]
    fn a_suppression_location_survives_findings_distillation() {
        let log = "scanning diff\ncrates/demo/src/lib.rs:17 — allow(clippy::all) — #[allow(clippy::all)]\ndone";

        let findings = verify_findings(&[member("verify.suppress", MemberOutcome::Failed, log)]).expect("findings");

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

        let findings = verify_findings(&[member("verify.test", MemberOutcome::Failed, log)]).expect("findings");

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

        let findings = verify_findings(&[member("verify.test", MemberOutcome::Failed, log)]).expect("findings");

        assert!(findings.contains("error[E0308]: mismatched types"));
        assert!(findings.contains("--> crates/aether-actor/tests/asset_sections.rs:85:9"));
    }

    #[test]
    fn an_all_pass_run_yields_no_findings() {
        // Tripwire: findings must be absent on a pass, or a Refine that never
        // happens would still carry a stale row, and `parse_findings` is
        // presence-driven with no lane flag to disambiguate.
        let members =
            [member("verify.fmt", MemberOutcome::Passed, ""), member("verify.clippy", MemberOutcome::Passed, "")];

        assert!(verify_findings(&members).is_none(), "a clean run stamps no findings");
    }

    #[test]
    fn the_findings_state_the_candidate_already_carries_the_change() {
        // The framing is load-bearing, not decoration. A Refine checks out the
        // previous candidate, so a prompt that only says "implement the work
        // order" invites the correct-but-useless "already carries it" answer
        // that wedged both real runs.
        let members = [member("verify.clippy", MemberOutcome::Failed, "error[E0308]: mismatched types")];

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
    fn every_umbrella_member_has_a_typed_failure_identity() {
        // Tripwire: an umbrella member that VerifyFailure::from_name cannot
        // decode is dropped from the projected set, so a run in which it is the
        // only failure emits status "fail" with no failed_verifiers — a shape
        // both transports refuse, stalling the dispatch to its deadline rather
        // than charging the member. verify.suppress was exactly that (#4807).
        for &id in verify_check_members() {
            assert!(VerifyFailure::from_name(id).is_some(), "{id} must carry a typed failure identity");
        }
    }

    #[test]
    fn the_umbrella_status_passes_only_on_a_clean_run_and_never_hides_a_finding() {
        // Tripwire: `environment` is the status that costs no repair lap, so a
        // member that found a real defect must outrank one that reported a host
        // fault. Ordering these the other way would stamp a candidate's own
        // failing test as weather and integrate it.
        assert_eq!(umbrella_status(&[MemberOutcome::Passed, MemberOutcome::Passed]), "pass");
        assert_eq!(umbrella_status(&[]), "pass", "no members is vacuously a pass");
        assert_eq!(umbrella_status(&[MemberOutcome::Passed, MemberOutcome::Environment]), "environment");
        assert_eq!(
            umbrella_status(&[MemberOutcome::Environment, MemberOutcome::Failed]),
            "fail",
            "a finding outranks an environment fault: that member did judge the candidate",
        );
        assert_eq!(
            umbrella_status(&[MemberOutcome::Environment, MemberOutcome::Operational]),
            "fail",
            "an unperformed check is not a host fault the bloom may retry past",
        );
    }

    #[test]
    fn an_out_of_closure_block_that_clears_on_the_repeat_costs_no_repair_lap() {
        // Acceptance 1 (#4895): 36 fleetharness failures on a memory-starved box
        // are not a candidate defect. The member is rerun once, the repeat comes
        // back green, and what the member reports is the repeat — no findings,
        // no verifier identity, no repair lap — with the block kept as an
        // observation so the excuse is on the record rather than invisible.
        let invocation = verify_command("verify.test").expect("verify.test mapped");
        let mut runner = ScriptedRunner::new(&[
            (&failing_run(&["aether-actor::fleetharness_binary_store store_round_trips"]), 100),
            (&passing_run(), 0),
        ]);

        let run = run_member_discriminated(
            "verify.test",
            &invocation,
            &Scope::resolve(None),
            Some(&closure::Closure::of(&["aether-render"])),
            &mut runner,
        )
        .expect("the policy runs");

        assert_eq!(runner.runs, 2, "an out-of-closure-only run is repeated exactly once");
        assert_eq!(run.outcome, MemberOutcome::Passed);
        assert!(run.findings.is_none(), "a repair lap must not be handed failures the candidate cannot reach");
        assert!(run.outcome.failure(&run.id).is_none(), "and no verifier identity is charged for a recovered host");
        let observation = run.observation.expect("the block is reported on the lane");
        assert!(observation.contains("aether-actor"), "the observation names what was classified out");
        assert!(observation.contains("did not repeat"), "and what the repeat then said");
        assert!(
            String::from_utf8_lossy(&run.log).contains("being rerun once"),
            "both runs stay in the log, separated by the notice that says why there are two",
        );
    }

    #[test]
    fn a_mixed_run_hands_the_repair_lap_only_the_in_closure_failures() {
        // Acceptance 2 (#4895): the 2026-08-13 shape — one real defect beside a
        // 36-test environmental block. The repair lap is warranted, and the
        // findings it is directed by must carry the defect alone: a model asked
        // to repair 36 red herrings reasons its way past them at best, and
        // "fixes" tests its surface does not permit it to touch at worst.
        let invocation = verify_command("verify.test").expect("verify.test mapped");
        let mut runner = ScriptedRunner::new(&[(
            &failing_run(&[
                "aether-render::pipeline blend_state_survives_a_resize",
                "aether-chassis-hub::fleetharness_engines spawn_headless_connects",
            ]),
            100,
        )]);

        let run = run_member_discriminated(
            "verify.test",
            &invocation,
            &Scope::resolve(None),
            Some(&closure::Closure::of(&["aether-render"])),
            &mut runner,
        )
        .expect("the policy runs");

        assert_eq!(runner.runs, 1, "a run that found a real defect is not repeated");
        assert_eq!(run.outcome, MemberOutcome::Failed);
        let findings = run.findings.expect("the in-closure defect is handed over");
        assert!(findings.contains("blend_state_survives_a_resize"), "the in-closure failure reaches the repair lap");
        assert!(!findings.contains("spawn_headless_connects"), "and the out-of-closure block does not");
        assert!(
            run.observation.expect("the block is still reported").contains("spawn_headless_connects"),
            "the out-of-closure half is an observation, not silence",
        );
    }

    #[test]
    fn an_out_of_closure_block_that_repeats_escalates_instead_of_blaming_the_candidate() {
        // Acceptance 3 (#4895): a host that is still broken on the repeat is a
        // host, not a candidate. The member reports an environment fault, which
        // the umbrella stamps as ADR-0176's `environment` status and the local
        // executor reads as an ExecutorFault — never as findings, and never as a
        // verifier identity in the repair ledger a member's stuckness is
        // measured from.
        let invocation = verify_command("verify.test").expect("verify.test mapped");
        let block = failing_run(&["aether-component::fleetharness_load loads_over_the_wire"]);
        let mut runner = ScriptedRunner::new(&[(&block, 100), (&block, 100)]);

        let run = run_member_discriminated(
            "verify.test",
            &invocation,
            &Scope::resolve(None),
            Some(&closure::Closure::of(&["aether-render"])),
            &mut runner,
        )
        .expect("the policy runs");

        assert_eq!(runner.runs, 2, "the repeat is bounded at one — a broken host is not retried forever");
        assert_eq!(run.outcome, MemberOutcome::Environment);
        assert!(run.findings.is_none(), "nothing here judged the candidate");
        assert!(run.outcome.failure(&run.id).is_none(), "so nothing is charged to a verifier identity");
        assert_eq!(umbrella_status(&[MemberOutcome::Passed, run.outcome]), "environment");
        let observation = run.observation.expect("the escalation states its grounds");
        assert!(observation.contains("aether-component"), "naming the block it escalated on");
        assert!(observation.contains("environment fault"), "and what it escalated as");
    }

    #[test]
    fn a_run_with_no_closure_classifies_nothing_out() {
        // Tripwire: the fail-towards-the-candidate direction. A diff nobody
        // could resolve, an unreadable package graph, or a change whose blast
        // radius the graph cannot bound all arrive here as `None` — and a run
        // that cannot see the candidate must blame the candidate, because the
        // opposite error excuses a real defect as weather and lands it.
        let invocation = verify_command("verify.test").expect("verify.test mapped");
        let mut runner =
            ScriptedRunner::new(&[(&failing_run(&["aether-actor::fleetharness_binary_store store_round_trips"]), 100)]);

        let run = run_member_discriminated("verify.test", &invocation, &Scope::resolve(None), None, &mut runner)
            .expect("the policy runs");

        assert_eq!(runner.runs, 1, "with no closure there is nothing to classify out, so nothing to rerun for");
        assert_eq!(run.outcome, MemberOutcome::Failed);
        assert!(run.findings.expect("findings").contains("store_round_trips"), "the failure is the candidate's");
        assert!(run.observation.is_none(), "and there is no observation to make");
    }

    #[test]
    fn observations_are_a_separate_channel_from_the_findings_a_refine_is_handed() {
        // Tripwire: the two channels must not merge. `findings` is work — a
        // Refine re-entry is told to fix what is in it — and an observation is a
        // receipt. Folded together, a bounded repair roll gets spent on a host
        // the candidate cannot reach, which is the whole failure #4895 exists to
        // end.
        let mut failing = member("verify.test", MemberOutcome::Failed, "error: something in closure");
        failing.observation = Some("36 failing tests lie outside the closure".to_owned());

        let observations = environment_observations(&[failing]).expect("the observation is reported");

        assert!(observations.contains("### verify.test"), "attributed to the member that made it");
        assert!(observations.contains("outside the closure"));
        assert!(
            environment_observations(&[member("verify.fmt", MemberOutcome::Failed, "Diff in x")]).is_none(),
            "a member with nothing to observe contributes no channel at all",
        );
    }

    #[test]
    fn failed_member_projection_is_exact_canonical_and_empty_on_pass() {
        let multi = failed_verifiers([
            ("verify.fmt", MemberOutcome::Failed),
            ("verify.clippy", MemberOutcome::Passed),
            ("verify.docs", MemberOutcome::Failed),
            ("verify.test", MemberOutcome::Failed),
        ]);
        assert_eq!(
            multi,
            [VerifyFailure::Fmt, VerifyFailure::Docs, VerifyFailure::Test].into_iter().collect(),
            "every failed command contributes its closed identity",
        );
        assert_eq!(
            failed_verifiers([("verify.test", MemberOutcome::Failed)]),
            VerifyFailureSet::one(VerifyFailure::Test)
        );
        assert!(
            failed_verifiers([("verify.fmt", MemberOutcome::Passed), ("verify.deps", MemberOutcome::Passed)])
                .is_empty()
        );
    }

    #[test]
    fn an_exit_the_member_does_not_find_with_is_charged_to_preflight() {
        // Tripwire for #4845, at both halves of the seam. The suppression
        // scanner exits 2 for an `OperationalError` — an unreadable blob, a
        // malformed .jscpd.json, a git call that would not run — and read as a
        // finding it is forgiven once, charges a repair roll on repeat, and
        // wedges the member with repeated_verifiers = {verify.suppress}
        // against a candidate that did nothing wrong. cargo-machete draws the
        // same 1-vs-2 split, and every member shares the signal-death case: a
        // child with no exit code at all said nothing about the candidate.
        let suppress = verify_command("verify.suppress").expect("verify.suppress mapped");
        assert_eq!(
            member_outcome(&suppress, true, Some(1)),
            MemberOutcome::Failed,
            "a printed suppression is a finding"
        );
        assert_eq!(member_outcome(&suppress, true, Some(2)), MemberOutcome::Operational);
        assert_eq!(
            member_outcome(&suppress, true, None),
            MemberOutcome::Operational,
            "a signal death is not a verdict"
        );

        let deps = verify_command("verify.deps").expect("verify.deps mapped");
        assert_eq!(member_outcome(&deps, true, Some(1)), MemberOutcome::Failed, "an unused dependency is a finding");
        assert_eq!(member_outcome(&deps, true, Some(2)), MemberOutcome::Operational, "a path it cannot walk is not");

        assert_eq!(
            failed_verifiers([("verify.suppress", MemberOutcome::Operational)]),
            VerifyFailureSet::one(VerifyFailure::Preflight),
            "a host fault reaches the ledger as the umbrella's synthetic identity, never the member's own",
        );
        assert_eq!(
            failed_verifiers([("verify.suppress", MemberOutcome::Failed)]),
            VerifyFailureSet::one(VerifyFailure::Suppress),
            "and a real finding still lands on the member, or the repair ledger stops measuring stuckness",
        );
    }

    #[test]
    fn a_members_finding_exits_are_its_own_programs_codes() {
        // Tripwire: these lists decide, per member, which failures blame the
        // candidate. nextest is the one that matters most — its codes are its
        // own rather than cargo's, and a list narrowed to cargo's 101 would
        // route every genuine test failure to verify.preflight, forgiving the
        // exact defect the loop exists to charge for.
        let test = verify_command("verify.test").expect("verify.test mapped");
        assert_eq!(test.finding_exit_codes, &[100, 101], "100 is a failing test run, 101 a failing build");
        assert_eq!(member_outcome(&test, true, Some(100)), MemberOutcome::Failed);
        assert_eq!(
            member_outcome(&test, true, Some(96)),
            MemberOutcome::Operational,
            "an unusable profile is nextest's"
        );

        for id in verify_check_members() {
            let invocation = verify_command(id).expect("every umbrella member resolves");
            assert!(!invocation.finding_exit_codes.is_empty(), "{id} states no finding exit, so nothing can blame it");
            assert!(!invocation.finding_exit_codes.contains(&0), "{id} claims a clean exit as a finding");
        }
    }

    #[test]
    fn a_derived_clippy_failure_at_a_clean_exit_is_still_the_candidates() {
        // Tripwire: clippy is the one member whose failure arrives at exit
        // zero, because the run is deliberately not asked to deny warnings.
        // Classifying by exit code alone would read that as a pass; routing it
        // through the operational branch would read it as a host fault. Both
        // stop lints being charged to the candidate that wrote them.
        let clippy = verify_command("verify.clippy").expect("verify.clippy mapped");

        assert_eq!(member_outcome(&clippy, false, Some(0)), MemberOutcome::Failed);
        assert_eq!(member_outcome(&clippy, true, Some(0)), MemberOutcome::Passed);
        assert_eq!(member_outcome(&clippy, true, Some(101)), MemberOutcome::Failed, "could not compile is a finding");
    }

    #[test]
    fn an_operational_log_says_the_member_reported_nothing_and_names_where_it_landed() {
        // Tripwire: a Refine is handed these logs to repair. Unframed, a
        // scanner traceback reads as a defect and the model edits working code
        // until a broken host stops complaining — the same failure
        // `prepare_failure_log` exists to prevent, for the other way a member
        // can fail without saying anything about the candidate.
        let suppress = verify_command("verify.suppress").expect("verify.suppress mapped");
        let log = format!(
            "{}suppression scan error: malformed .jscpd.json: Expecting value",
            operational_failure_notice("verify.suppress", &suppress, Some(2)),
        );

        let findings =
            verify_findings(&[member("verify.suppress", MemberOutcome::Operational, &log)]).expect("findings");

        assert!(findings.contains("reported nothing about the candidate"), "the member's silence is stated");
        assert!(findings.contains("exited 2"), "with the code that said so");
        assert!(findings.contains("verify.preflight"), "and the identity the run was accounted to");
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
