//! The construct lane's post-fixer lint check and its one repair turn.
//!
//! The mechanical fixers apply what the toolchain can already write. What they
//! leave is the class `--fix` has no `MachineApplicable` suggestion for — the
//! rename and import-path pedantic lints — and until now the model never saw
//! those: construct handed off, dedicated Verify judged them, and the workpiece
//! paid a whole Refine lap to be told what a scoped `cargo clippy` already knew
//! while the lane still owned the tree.
//!
//! So this runs one scoped check over the candidate's crates and, if anything
//! remains, buys the model exactly one more turn with the distilled
//! diagnostics. Those crates are the diff's reverse-dependency closure,
//! resolved through the very function the verify gate resolves its own crate
//! set through (#6000) — the bar and the gate name one set, or a lane hands
//! off a candidate its own bar called clean and the gate calls red on the first
//! compile.
//!
//! It is a check, never a gate: nothing here fails the lane, a check that will
//! not run is reported as not-run and the lane hands off, and dedicated Verify
//! remains the pass that decides. The model's own prompt ban on running the
//! lint matrix (#5078) is unaffected — this is harness-side, closure-scoped,
//! and after the model's turn.

use std::fs::{self, File};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::transform::lane::Resumed;
use crate::transform::verify::scope::Scope;
use crate::transform::verify::{Judge, distil_diagnostics, judged_errors, judged_findings, render_diagnostics};
use crate::transform::{TransformArgs, fixers, run_model_lane, sccache};

/// How long the whole post-fixer lint round may take: both scoped checks and
/// the model's repair turn.
///
/// One deadline over the round rather than a budget each, sized as the fixers'
/// own `CLIPPY_FIX_BUDGET`. The passes compile into the target directory
/// `clippy --fix` just wrote, so the crates the run dirtied are a fingerprint
/// hit; their dependents are the part that can be a build, which is the price
/// of asking the question the gate asks. The repair turn edits a tree the model
/// still has in context. Past this the lane hands off with whatever it learned
/// — the stage budget belongs to producing a candidate, and a construct that
/// spent it on lint residue produced nothing.
const LINT_ROUND_BUDGET: Duration = Duration::from_mins(15);

/// The file the scoped check writes cargo's JSON diagnostic stream to, inside
/// the run's evidence directory.
///
/// A file rather than a pipe: the stream is unbounded and the wait loop that
/// enforces the budget does not drain pipes, so a talkative check would fill
/// the buffer and deadlock against its own deadline. Landing it in the
/// evidence tree also leaves the raw diagnostics beside the receipt that
/// counts them, so a reader can check the count rather than trust it.
const CHECK_STREAM_FILE: &str = "construct-lint.json";

/// The file the compile pass ahead of the lint check writes its own JSON
/// diagnostic stream to, for the same reasons [`CHECK_STREAM_FILE`] is a file:
/// its own name so a reader can tell "this candidate does not build" from
/// "this candidate builds with lint residue" without parsing either.
const TYPE_CHECK_STREAM_FILE: &str = "construct-check.json";

/// The evidence directory the one repair turn writes its transcript to.
///
/// Its own subdirectory of the run's `--out`, because the lane primitive
/// truncates `transcript.jsonl` on every launch: a repair turn writing beside
/// the construct turn would replace the transcript of the run that produced
/// the candidate with the tail that tidied it. Under `--out`, so the candidate
/// signal and the fixers both keep ignoring it as the run's own output.
const REPAIR_OUT_DIR: &str = "lint-repair";

/// What the construct evidence envelope records about the lint round.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Report {
    /// The scoped check ran to a verdict. `false` covers every way it did not
    /// — no owning packages, a cargo that would not start, a check that
    /// overran the round budget — because none of them says anything about the
    /// candidate's lint state, and reporting them as a clean check would be a
    /// false green in the receipt.
    pub ran: bool,
    /// Diagnostics the check attributed to the candidate's own packages.
    pub findings_before: usize,
    /// The one repair turn ran.
    pub resumed: bool,
    /// What the re-check after the repair turn found. `None` when no repair
    /// turn ran, and — read together with `resumed` — when one ran but the
    /// re-check itself could not, so "the repair left two findings" is never
    /// confused with "nobody looked again".
    pub findings_after: Option<usize>,
    /// What the round ran over, stated the way the gate states the same thing
    /// (#6000). `None` only when the round did not get as far as resolving it.
    pub scope: Option<String>,
}

impl Report {
    /// Stamp the round onto a construct evidence envelope. Always present, for
    /// the reason the fixer receipt is: a reader that cannot tell "the check
    /// found nothing" from "the check never ran" cannot tell whether a lint
    /// failure at Verify was one this lane had a chance to catch.
    pub(super) fn stamp(self, evidence: &mut serde_json::Value) {
        if let Some(object) = evidence.as_object_mut() {
            object.insert(
                "lint_check".to_owned(),
                serde_json::json!({
                    "ran": self.ran,
                    "findings_before": self.findings_before,
                    "resumed": self.resumed,
                    "findings_after": self.findings_after,
                    "scope": self.scope,
                }),
            );
        }
    }
}

/// The crates one bar is answerable for, and how it came by them.
///
/// The set is the candidate diff's reverse-dependency closure, resolved
/// through [`Scope::of_changed`] — the same function the verify gate resolves
/// its own crate set through (#6000). It used to be the narrower owning
/// packages of the files the run dirtied, on the argument that a dependent
/// crate's diagnostics are not this work order's to repair. That argument is
/// wrong about the class this bar exists to catch: an edit to a shared value
/// type compiles fine in its own crate and breaks an initializer in a
/// dependent's test target, which the bar never compiled and the gate failed
/// the member on a quarter of an hour later. A crate inside the closure is one
/// this candidate can have broken, so it is one the model is answerable for.
struct Bar {
    /// The gate's own resolution of this candidate's diff, kept whole so the
    /// bar's receipt can state it verbatim.
    scope: Scope,
    /// The crates the bar compiles and judges.
    packages: Vec<String>,
}

impl Bar {
    /// The bar for the tree `worktree` currently holds — the lane's candidate,
    /// which is uncommitted, so the diff is named from `git status` and handed
    /// to the gate's own closure function.
    fn resolve(worktree: &Path, out_dir: &Path) -> Self {
        let scope = Scope::of_changed(&fixers::dirty_paths(worktree, Some(out_dir)));
        // An unbounded scope is the gate's whole workspace, which this round
        // has neither the budget nor the mandate to compile; the bar falls back
        // to the packages the fixers were pointed at and the receipt says so.
        // A resolved closure is taken as it stands.
        let packages = scope.packages().map_or_else(|| fixers::scoped_packages(worktree, out_dir), <[String]>::to_vec);

        Self { scope, packages }
    }

    /// What the bar ran over, in the gate's own words.
    ///
    /// The gate's receipt verbatim, plus the crates this bar actually
    /// compiled. Recorded in the construct evidence beside the gate's
    /// `verify.scope.log` so the two sets are comparable as text: when they
    /// disagree, a lane that handed off clean and a gate that went red are one
    /// legible fact rather than two logs a reader has to correlate.
    fn receipt(&self) -> String {
        format!("{}bar crates ({}): {}\n", self.scope.receipt(), self.packages.len(), self.packages.join(" "))
    }
}

impl Judge for Bar {
    fn judges(&self, package: &str) -> bool {
        self.packages.iter().any(|name| name == package)
    }
}

#[cfg(test)]
impl Bar {
    /// A bar over the closure `packages`, for exercising the judge and the
    /// receipt without a git repository or a package graph.
    fn of(packages: &[&str]) -> Self {
        let packages: Vec<String> = packages.iter().map(|name| (*name).to_owned()).collect();
        let scope = Scope::Closure { packages: packages.clone(), skipped: Vec::new(), wasm_needed: false };
        Self { scope, packages }
    }
}

/// What one pass over the bar's crates learned.
struct Check {
    /// Diagnostics the bar's crates emitted.
    findings: usize,
    /// How many of those the build did not survive.
    errors: usize,
    /// Those diagnostics distilled to the findings budget, ready to hand a
    /// repair turn. `None` when there were none to render.
    distilled: Option<String>,
}

/// What the lint round left behind: the receipt, and the fixer pass the repair
/// turn's edits bought.
pub(super) struct Outcome {
    pub report: Report,
    /// The second [`fixers::apply`], present only when a repair turn ran. The
    /// caller folds it into the run's one fixer receipt.
    pub fixers: Option<fixers::Report>,
}

/// Run the post-fixer lint round over `worktree`.
///
/// Infallible by construction: every step that can fail — the package graph,
/// cargo, the harness, the budget — degrades to "did not run" and the lane
/// hands off. `session` is the handle the construct turn reported, which the
/// repair turn resumes so the model reads its findings with its own work still
/// in context rather than re-deriving it from a cold prompt.
pub(super) fn run(worktree: &Path, args: &TransformArgs, session: Option<&str>, repair_instructions: &str) -> Outcome {
    let deadline = Instant::now() + LINT_ROUND_BUDGET;
    let bar = Bar::resolve(worktree, &args.out);
    let scope = Some(bar.receipt());
    if bar.packages.is_empty() {
        return Outcome { report: Report { scope, ..Report::default() }, fixers: None };
    }

    let mut applied = None;
    let mut report = round(
        || check(worktree, &args.out, &bar, deadline),
        |found| {
            let repaired = repair(args, session, &bar.packages, found, deadline, repair_instructions);
            applied = repaired.then(|| fixers::apply(worktree, &args.out));
            repaired
        },
    );
    report.scope = scope;

    Outcome { report, fixers: applied }
}

/// One round: check, and at most one repair turn.
///
/// The two effects are injected so the decisions this function makes — that a
/// clean check buys no turn, that a check which could not run buys no turn
/// either, and above all that the turn is bought *once* — are testable without
/// a cargo build or a billed model turn. The cap is structural rather than a
/// counter: there is no loop here to bound.
fn round(mut check: impl FnMut() -> Option<Check>, mut repair: impl FnMut(&str) -> bool) -> Report {
    let Some(first) = check() else {
        return Report::default();
    };

    let mut report =
        Report { ran: true, findings_before: first.findings, resumed: false, findings_after: None, scope: None };
    let Some(distilled) = first.distilled.filter(|_| first.findings > 0) else {
        return report;
    };
    if !repair(&distilled) {
        return report;
    }

    report.resumed = true;
    report.findings_after = check().map(|second| second.findings);
    report
}

/// Run the bar once and read its verdict, or `None` when it did not reach one.
///
/// Two passes, compile before lint. A `cargo check --tests` over the closure is
/// the cheap question — does every target in what this diff can reach still
/// build — and it is the one the narrow bar never asked: the broken initializer
/// that failed #5963 at the gate lives in a dependent crate's test target. A
/// candidate that does not compile has nothing further to learn from clippy
/// over the same crates, which compiles the same units and restates the same
/// errors more slowly, so the bar stops there and spends the rest of its budget
/// on the repair turn.
fn check(worktree: &Path, out_dir: &Path, bar: &Bar, deadline: Instant) -> Option<Check> {
    compiled_then_linted(
        || pass(worktree, out_dir, TYPE_CHECK_STREAM_FILE, &type_check_argv(&bar.packages), bar, deadline),
        || pass(worktree, out_dir, CHECK_STREAM_FILE, &clippy_argv(&bar.packages), bar, deadline),
    )
}

/// Compile, then lint only what still builds. The two passes are injected so
/// the ordering decision is exercisable without a cargo build.
fn compiled_then_linted(
    compile: impl FnOnce() -> Option<Check>,
    lint: impl FnOnce() -> Option<Check>,
) -> Option<Check> {
    let compiled = compile()?;
    if compiled.errors > 0 {
        return Some(compiled);
    }

    lint()
}

/// One cargo pass over the bar's crates, its JSON stream landed in `file` and
/// read back as a verdict. `None` when it did not reach one.
fn pass(worktree: &Path, out_dir: &Path, file: &str, argv: &[String], bar: &Bar, deadline: Instant) -> Option<Check> {
    let budget = deadline.checked_duration_since(Instant::now()).unwrap_or(Duration::ZERO);
    if budget.is_zero() {
        eprintln!("construct lane: lint round out of budget before {file}; handing off");
        return None;
    }

    let stream = out_dir.join(file);
    fs::create_dir_all(out_dir).ok()?;
    let sink = File::create(&stream).ok()?;
    let status = fixers::spawn_and_wait(worktree, out_dir, "cargo", argv, budget, |command| {
        command.stdout(Stdio::from(sink)).stderr(Stdio::null());
        command.env("CARGO_INCREMENTAL", "0");
        sccache::export(sccache::detect().as_ref(), command);
    });
    // Exit code is not the verdict: the check does not deny warnings, so a
    // clean-compiling candidate with pedantic findings exits zero. Only a run
    // that could not produce a stream at all is unread. A cargo that exited
    // 101 still emitted every diagnostic it reached before the failing unit,
    // and those are exactly what the repair turn should see.
    if let Err(error) = status {
        eprintln!("construct lane: {file} {error}; handing off without it");
        return None;
    }

    let stdout = fs::read_to_string(&stream).ok()?;
    Some(Check {
        findings: judged_findings(&stdout, bar),
        errors: judged_errors(&stdout, bar),
        distilled: distil_diagnostics(&render_diagnostics(&stdout, bar)),
    })
}

/// The compile pass's argv: does everything the diff can reach still build,
/// test targets included.
///
/// `--tests` rather than `--all-targets` because the target class the narrow
/// bar was blind to is the integration-test binary nothing links against, and
/// it is the cheapest question that reaches it. No `-D warnings` and no
/// `--workspace`, for the reasons [`clippy_argv`] states.
fn type_check_argv(packages: &[String]) -> Vec<String> {
    let mut args = vec!["check".to_owned(), "--tests".to_owned(), "--message-format=json".to_owned()];
    args.extend(packages.iter().flat_map(|package| ["-p".to_owned(), package.clone()]));
    args
}

/// The lint pass's argv: the bar's crate set, judged rather than rewritten.
///
/// No `--fix`, because the pass that could apply anything already ran. No
/// `-D warnings`, for the reason `verify.clippy` omits it (#4706): denying
/// makes a lint a compile error, so a crate that trips one is never built and
/// the diagnostics underneath it never exist to be reported — and here it
/// would additionally turn a pedantic finding into a non-zero exit that reads
/// as a broken check. The JSON stream is the verdict. No `--workspace`: the
/// round is scoped to the candidate's closure.
fn clippy_argv(packages: &[String]) -> Vec<String> {
    let mut args = vec![
        "clippy".to_owned(),
        "--no-deps".to_owned(),
        "--all-targets".to_owned(),
        "--message-format=json".to_owned(),
    ];
    args.extend(packages.iter().flat_map(|package| ["-p".to_owned(), package.clone()]));
    args
}

/// Buy the model one turn on `findings`, returning whether it ran.
fn repair(
    args: &TransformArgs,
    session: Option<&str>,
    packages: &[String],
    findings: &str,
    deadline: Instant,
    repair_instructions: &str,
) -> bool {
    if Instant::now() >= deadline {
        eprintln!("construct lane: lint round out of budget before the repair turn; handing off");
        return false;
    }
    let Some(session) = session else {
        // No handle means no continuation: a cold turn would re-read the whole
        // work order to fix two renames, at the price of the turn that wrote
        // the candidate. Verify still owns the verdict.
        eprintln!("construct lane: the construct turn reported no session; handing off with the lint findings unfixed");
        return false;
    };

    let mut resumed = args.clone();
    resumed.out = args.out.join(REPAIR_OUT_DIR);
    resumed.resume = Some(session.to_owned());
    match run_model_lane(&repair_prompt(repair_instructions, packages, findings), &resumed, Resumed::SameTree) {
        Ok(_) => true,
        Err(error) => {
            eprintln!("construct lane: the lint repair turn did not run ({error:#}); handing off");
            false
        }
    }
}

/// The prompt the one repair turn receives.
///
/// It says what ran, what is left, and what the turn is for. The bound is
/// stated because it changes the right move: with one turn and no gate behind
/// it, arguing with a lint or re-reading the work order spends the lane's last
/// window for nothing, while an honest "not mine to fix" costs one line and
/// still reaches the reviewer.
fn repair_prompt(instructions: &str, packages: &[String], findings: &str) -> String {
    format!(
        "{instructions}\n\n## Lint packages\n\n{}\n\n## Remaining lint findings\n\n```\n{findings}\n```\n",
        packages.join(", "),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{Bar, Check, Judge, Report, clippy_argv, compiled_then_linted, repair_prompt, round, type_check_argv};
    use crate::transform::instructions::fixture_bundle;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// A check that reached a verdict of `findings` diagnostics, none of them
    /// fatal.
    fn found(findings: usize) -> Check {
        Check { findings, errors: 0, distilled: (findings > 0).then(|| "warning: field names".to_owned()) }
    }

    /// A check whose crates did not compile.
    fn broke(errors: usize) -> Check {
        Check { findings: errors, errors, distilled: Some("error[E0063]: missing field".to_owned()) }
    }

    // Tripwire: the bar judges every crate in the candidate's closure, not
    // just the ones the run's own files live in. #5963's lane reported zero
    // findings over a diff that broke two initializers in a dependent crate's
    // test target, and the gate failed the member on it fourteen minutes
    // later; a judge that narrows back to the dirtied packages restores
    // exactly that blind spot.
    #[test]
    fn the_bar_judges_every_crate_in_the_candidates_closure() {
        let bar = Bar::of(&["aether-math", "xtask"]);
        assert!(bar.judges("aether-math"), "the crate the diff changed");
        assert!(bar.judges("xtask"), "a dependent whose test target the change can break");
        assert!(!bar.judges("aether-render"), "a crate outside the closure cannot have been broken by this diff");
        assert!(!Bar::of(&[]).judges("aether-math"), "an empty set judges nothing");
    }

    // Tripwire: the receipt is what makes a disagreement between the bar and
    // the gate legible. Recording only the crate list would drop the gate's
    // own stated reason, and a bar that fell back to a narrower set would then
    // read exactly like one that resolved the closure.
    #[test]
    fn the_receipt_states_the_gates_closure_and_the_crates_the_bar_ran() {
        let bar = Bar::of(&["aether-math", "xtask"]);
        let receipt = bar.receipt();

        assert!(receipt.contains(&bar.scope.receipt()), "the gate's own words ride the bar's receipt verbatim");
        assert!(receipt.contains("bar crates (2): aether-math xtask"), "got: {receipt}");
    }

    // Tripwire: compile before lint, and stop at a compile that failed. Clippy
    // over the same crates compiles the same units and restates the same
    // errors, so running it anyway spends the round's remaining budget to
    // learn nothing and can leave no room for the repair turn the errors are
    // for.
    #[test]
    fn a_candidate_that_does_not_compile_never_reaches_the_lint_pass() {
        let linted = Cell::new(false);
        let verdict = compiled_then_linted(
            || Some(broke(2)),
            || {
                linted.set(true);
                Some(found(0))
            },
        );

        assert!(!linted.get(), "a tree that does not build has nothing further to learn from clippy");
        assert_eq!(verdict.expect("the compile pass reached a verdict").errors, 2);

        let linted = Cell::new(false);
        let verdict = compiled_then_linted(
            || Some(found(0)),
            || {
                linted.set(true);
                Some(found(3))
            },
        );

        assert!(linted.get(), "a tree that builds is still linted");
        assert_eq!(verdict.expect("the lint pass reached a verdict").findings, 3, "the lint pass owns the verdict");
        assert!(compiled_then_linted(|| None, || Some(found(1))).is_none(), "a compile pass with no verdict ends it");
    }

    // Tripwire: `--tests` is the whole point of the compile pass — the target
    // class the lint pass was blind to is the integration-test binary nothing
    // links against. The deny and workspace bans are [`clippy_argv`]'s, for
    // the same reasons.
    #[test]
    fn the_compile_pass_reaches_the_test_targets_of_every_crate_in_the_closure() {
        let argv = type_check_argv(&names(&["aether-math", "xtask"]));
        assert_eq!(argv[0], "check");
        assert!(argv.contains(&"--tests".to_owned()), "a dependent's test target is the class this pass exists for");
        assert!(argv.contains(&"--message-format=json".to_owned()), "the JSON stream is the verdict");
        assert!(!argv.iter().any(|arg| arg == "--workspace"), "a workspace check ignores the resolved closure");
        assert!(!argv.iter().any(|arg| arg.contains("-D") || arg == "warnings"), "the verdict is the JSON, not a deny");
        assert!(argv.windows(2).any(|pair| pair == ["-p", "aether-math"]));
        assert!(argv.windows(2).any(|pair| pair == ["-p", "xtask"]));
    }

    // Tripwire: `--fix` would rewrite the tree a second time under a pass whose
    // job is to read it; `-D warnings` would turn the pedantic findings this
    // round exists to surface into a non-zero exit that reads as a broken
    // check (and, upstream of that, stop the crates underneath from compiling
    // at all, #4706); `--workspace` would ignore the package set entirely.
    #[test]
    fn the_check_argv_judges_the_scoped_packages_and_rewrites_nothing() {
        let argv = clippy_argv(&names(&["aether-math", "xtask"]));
        assert_eq!(argv[0], "clippy");
        assert!(argv.contains(&"--message-format=json".to_owned()), "the JSON stream is the verdict");
        assert!(!argv.iter().any(|arg| arg == "--fix"), "the check reads the tree; the fixer already wrote it");
        assert!(!argv.iter().any(|arg| arg == "--workspace"), "a workspace check ignores the scoped package set");
        assert!(!argv.iter().any(|arg| arg.contains("-D") || arg == "warnings"), "the verdict is the JSON, not a deny");
        assert_eq!(argv.iter().filter(|arg| *arg == "-p").count(), 2, "each touched package is its own -p");
        assert!(argv.windows(2).any(|pair| pair == ["-p", "aether-math"]));
        assert!(argv.windows(2).any(|pair| pair == ["-p", "xtask"]));
    }

    // Tripwire: the whole point of the cap. A round that re-checked and
    // re-repaired while findings remained would spend the construct stage's
    // budget on a model that has already shown it cannot clear them, and the
    // lane would die mid-turn with no candidate instead of handing off a good
    // one with lint residue Verify can name.
    #[test]
    fn a_still_dirty_candidate_buys_exactly_one_repair_turn() {
        let checks = Cell::new(0);
        let repairs = Cell::new(0);
        let report = round(
            || {
                checks.set(checks.get() + 1);
                Some(found(3))
            },
            |_| {
                repairs.set(repairs.get() + 1);
                true
            },
        );

        assert_eq!(repairs.get(), 1, "one repair turn per construct invocation, however many findings survive it");
        assert_eq!(checks.get(), 2, "the check runs once before the turn and once after it");
        assert_eq!(
            report,
            Report { ran: true, findings_before: 3, resumed: true, findings_after: Some(3), scope: None },
            "a repair that cleared nothing is reported honestly rather than as a pass",
        );
    }

    #[test]
    fn a_clean_check_buys_no_turn_at_all() {
        let repairs = Cell::new(0);
        let report = round(
            || Some(found(0)),
            |_| {
                repairs.set(repairs.get() + 1);
                true
            },
        );

        assert_eq!(repairs.get(), 0, "there is nothing to repair");
        assert_eq!(report, Report { ran: true, findings_before: 0, resumed: false, findings_after: None, scope: None });
    }

    // Tripwire: a check that timed out or could not start knows nothing about
    // the candidate. Treating that absence as findings would buy a model turn
    // to fix diagnostics nobody has, and treating it as a clean check would
    // put `ran: true, findings_before: 0` in the evidence — indistinguishable
    // from a candidate that really is clean.
    #[test]
    fn a_check_that_could_not_run_is_reported_as_not_run_and_buys_no_turn() {
        let repairs = Cell::new(0);
        let report = round(
            || None,
            |_| {
                repairs.set(repairs.get() + 1);
                true
            },
        );

        assert_eq!(repairs.get(), 0, "no verdict is not a reason to spend a model turn");
        assert_eq!(report, Report::default());
        assert!(!report.ran, "an unread check must not stamp as a clean one");
    }

    // A repair turn the lane could not buy — no session handle, a harness that
    // would not start, the round out of budget — leaves the findings it did
    // measure in the evidence. Losing them would hide that the lane knew.
    #[test]
    fn a_refused_repair_turn_still_reports_what_the_check_found() {
        let report = round(|| Some(found(2)), |_| false);
        assert_eq!(report, Report { ran: true, findings_before: 2, resumed: false, findings_after: None, scope: None });
    }

    // `findings_after` is what tells a reader whether the turn worked, so the
    // re-check has to be the one that fills it — including when it clears them.
    #[test]
    fn a_repair_that_cleared_the_findings_says_so() {
        let remaining = Cell::new(2);
        let report = round(
            || {
                let now = remaining.get();
                remaining.set(0);
                Some(found(now))
            },
            |_| true,
        );
        assert_eq!(
            report,
            Report { ran: true, findings_before: 2, resumed: true, findings_after: Some(0), scope: None }
        );
    }

    #[test]
    fn the_lint_receipt_rides_the_evidence_envelope() {
        let mut evidence = serde_json::json!({ "command": "construct.implement" });
        let report = Report {
            ran: true,
            findings_before: 4,
            resumed: true,
            findings_after: Some(1),
            scope: Some(Bar::of(&["aether-math", "xtask"]).receipt()),
        };
        report.stamp(&mut evidence);

        assert_eq!(evidence["lint_check"]["ran"], true);
        assert_eq!(evidence["lint_check"]["findings_before"], 4);
        assert_eq!(evidence["lint_check"]["resumed"], true);
        assert_eq!(evidence["lint_check"]["findings_after"], 1);
        // The crate set rides the envelope so a reader comparing this receipt
        // with the gate's `verify.scope.log` can see a disagreement rather than
        // reconstruct one from two red logs.
        assert!(
            evidence["lint_check"]["scope"]
                .as_str()
                .is_some_and(|scope| scope.contains("bar crates (2): aether-math xtask")),
            "got: {}",
            evidence["lint_check"]["scope"],
        );

        // A round that never checked still stamps, and stamps null rather than
        // zero for the re-check it never made.
        let mut idle = serde_json::json!({ "command": "construct.implement" });
        Report::default().stamp(&mut idle);
        assert_eq!(idle["lint_check"]["ran"], false);
        assert_eq!(idle["lint_check"]["findings_before"], 0);
        assert!(idle["lint_check"]["findings_after"].is_null(), "no re-check is null, not a clean count");
    }

    // The repair turn resumes the construct conversation on the tree it just
    // wrote. A prompt that let it believe the tree was reset — the other
    // resume posture in this lane — would send it to redo the whole work order
    // against findings taken from the disk it was told to distrust. The
    // authorized bundle carries that posture; this function's job is to attach
    // the packages and findings without replacing it.
    #[test]
    fn the_repair_prompt_names_the_tree_the_findings_came_from() {
        let bundle = fixture_bundle();
        let prompt =
            repair_prompt(&bundle.construct_lint_repair, &names(&["aether-math", "xtask"]), "warning: field names");

        assert!(
            prompt.contains(&bundle.construct_lint_repair),
            "the authorized repair instructions ride the prompt; this function does not replace them with a reset \
             posture",
        );
        assert!(prompt.contains("aether-math, xtask"), "the prompt names the packages that were checked");
        assert!(prompt.contains("warning: field names"), "the distilled findings ride the prompt");
        assert!(prompt.contains("## Lint packages"), "packages are a context slot, not inlined into the instructions");
        assert!(
            prompt.contains("## Remaining lint findings"),
            "diagnostics are a context slot, not inlined into the instructions"
        );
    }
}
