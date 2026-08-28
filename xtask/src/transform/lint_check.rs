//! The construct lane's post-fixer lint check and its one repair turn.
//!
//! The mechanical fixers apply what the toolchain can already write. What they
//! leave is the class `--fix` has no `MachineApplicable` suggestion for — the
//! rename and import-path pedantic lints — and until now the model never saw
//! those: construct handed off, dedicated Verify judged them, and the workpiece
//! paid a whole Refine lap to be told what a scoped `cargo clippy` already knew
//! while the lane still owned the tree.
//!
//! So this runs one scoped check over the same owning packages the fixers were
//! pointed at and, if anything remains, buys the model exactly one more turn
//! with the distilled diagnostics. It is a check, never a gate: nothing here
//! fails the lane, a check that will not run is reported as not-run and the
//! lane hands off, and dedicated Verify remains the pass that decides. The
//! model's own prompt ban on running the lint matrix (#5078) is unaffected —
//! this is harness-side, package-scoped, and after the model's turn.

use std::fs::{self, File};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::transform::lane::Resumed;
use crate::transform::verify::{Judge, distil_diagnostics, judged_findings, render_diagnostics};
use crate::transform::{TransformArgs, fixers, run_model_lane, sccache};

/// How long the whole post-fixer lint round may take: both scoped checks and
/// the model's repair turn.
///
/// One deadline over the round rather than a budget each, sized as the fixers'
/// own `CLIPPY_FIX_BUDGET`. The check compiles the packages `clippy --fix` just
/// compiled, into the same target directory, so it is a fingerprint hit rather
/// than a build; the repair turn edits a tree the model still has in context.
/// Past this the lane hands off with whatever it learned — the stage budget
/// belongs to producing a candidate, and a construct that spent it on lint
/// residue produced nothing.
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

/// The evidence directory the one repair turn writes its transcript to.
///
/// Its own subdirectory of the run's `--out`, because the lane primitive
/// truncates `transcript.jsonl` on every launch: a repair turn writing beside
/// the construct turn would replace the transcript of the run that produced
/// the candidate with the tail that tidied it. Under `--out`, so the candidate
/// signal and the fixers both keep ignoring it as the run's own output.
const REPAIR_OUT_DIR: &str = "lint-repair";

/// What the construct evidence envelope records about the lint round.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
                }),
            );
        }
    }
}

/// The packages one scoped check is answerable for — the owning packages of
/// the files the run dirtied.
///
/// The [`Judge`] the umbrella satisfies with its resolved closure. Here the set
/// is deliberately narrower than a closure: the round exists to show the model
/// what `--fix` could not apply *in the files it wrote*, and a dependent
/// crate's diagnostics are not this work order's to repair.
struct OwningPackages(Vec<String>);

impl Judge for OwningPackages {
    fn judges(&self, package: &str) -> bool {
        self.0.iter().any(|name| name == package)
    }
}

/// What one scoped check learned.
struct Check {
    /// Diagnostics the candidate's own packages emitted.
    findings: usize,
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
pub(super) fn run(worktree: &Path, args: &TransformArgs, session: Option<&str>) -> Outcome {
    let deadline = Instant::now() + LINT_ROUND_BUDGET;
    let packages = OwningPackages(fixers::scoped_packages(worktree, &args.out));
    if packages.0.is_empty() {
        return Outcome { report: Report::default(), fixers: None };
    }

    let mut applied = None;
    let report = round(
        || check(worktree, &args.out, &packages, deadline),
        |found| {
            let repaired = repair(args, session, &packages.0, found, deadline);
            applied = repaired.then(|| fixers::apply(worktree, &args.out));
            repaired
        },
    );

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

    let mut report = Report { ran: true, findings_before: first.findings, resumed: false, findings_after: None };
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

/// Run the scoped check once and read its verdict, or `None` when it did not
/// reach one.
fn check(worktree: &Path, out_dir: &Path, packages: &OwningPackages, deadline: Instant) -> Option<Check> {
    let budget = deadline.checked_duration_since(Instant::now()).unwrap_or(Duration::ZERO);
    if budget.is_zero() {
        eprintln!("construct lane: lint round out of budget before the scoped check; handing off");
        return None;
    }

    let stream = out_dir.join(CHECK_STREAM_FILE);
    fs::create_dir_all(out_dir).ok()?;
    let sink = File::create(&stream).ok()?;
    let status = fixers::spawn_and_wait(worktree, out_dir, "cargo", &check_argv(&packages.0), budget, |command| {
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
        eprintln!("construct lane: scoped lint check {error}; handing off without it");
        return None;
    }

    let stdout = fs::read_to_string(&stream).ok()?;
    Some(Check {
        findings: judged_findings(&stdout, packages),
        distilled: distil_diagnostics(&render_diagnostics(&stdout, packages)),
    })
}

/// The scoped check's argv: the fixers' package set, judged rather than
/// rewritten.
///
/// No `--fix`, because the pass that could apply anything already ran. No
/// `-D warnings`, for the reason `verify.clippy` omits it (#4706): denying
/// makes a lint a compile error, so a crate that trips one is never built and
/// the diagnostics underneath it never exist to be reported — and here it
/// would additionally turn a pedantic finding into a non-zero exit that reads
/// as a broken check. The JSON stream is the verdict. No `--workspace`: the
/// round is scoped to what the run touched.
fn check_argv(packages: &[String]) -> Vec<String> {
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
fn repair(args: &TransformArgs, session: Option<&str>, packages: &[String], findings: &str, deadline: Instant) -> bool {
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
    match run_model_lane(&repair_prompt(packages, findings), &resumed, Resumed::SameTree) {
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
fn repair_prompt(packages: &[String], findings: &str) -> String {
    format!(
        "## Remaining lint findings\n\n\
         Your candidate is in the working tree, exactly as you left it plus whatever \
         the mechanical fixers rewrote: this lane ran `cargo fmt` over the files you changed and then \
         a `MachineApplicable` `cargo clippy --fix` over the packages that own them. Nothing was \
         reverted and nothing was reset.\n\n\
         A scoped `cargo clippy --no-deps --all-targets` over those same packages ({packages}) still \
         reports the diagnostics below. These are the ones `--fix` has no automatic suggestion for — \
         typically the pedantic rename and import-path lints — and the workspace denies warnings, so \
         each one is a failure of the gate that judges this candidate next.\n\n\
         Fix them in the working tree now. You get this one turn: nothing else runs after it, and the \
         authoritative lint verdict is dedicated Verify's, not this check's, so spend the turn on the \
         tree rather than on reporting back. Keep the change inside this work order's surface — if a \
         finding is genuinely not yours to fix here, leave it and say so in one line.\n\n\
         ```\n{findings}\n```\n",
        packages = packages.join(", "),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{Check, Judge, OwningPackages, Report, check_argv, repair_prompt, round};

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// A check that reached a verdict of `findings` diagnostics.
    fn found(findings: usize) -> Check {
        Check { findings, distilled: (findings > 0).then(|| "warning: field names".to_owned()) }
    }

    // Tripwire: the check judges only the packages the fixers rewrote. A judge
    // that answered for anything else would put a dependent crate's
    // diagnostics — which this work order's surface does not reach — in front
    // of the model as work, and the repair turn would either edit outside its
    // surface or spend itself arguing.
    #[test]
    fn the_check_judges_only_the_packages_the_run_dirtied() {
        let packages = OwningPackages(names(&["aether-math", "xtask"]));
        assert!(packages.judges("aether-math"));
        assert!(packages.judges("xtask"));
        assert!(!packages.judges("aether-render"), "a package the run did not touch is not this round's to judge");
        assert!(!OwningPackages(Vec::new()).judges("aether-math"), "an empty set judges nothing");
    }

    // Tripwire: `--fix` would rewrite the tree a second time under a pass whose
    // job is to read it; `-D warnings` would turn the pedantic findings this
    // round exists to surface into a non-zero exit that reads as a broken
    // check (and, upstream of that, stop the crates underneath from compiling
    // at all, #4706); `--workspace` would ignore the package set entirely.
    #[test]
    fn the_check_argv_judges_the_scoped_packages_and_rewrites_nothing() {
        let argv = check_argv(&names(&["aether-math", "xtask"]));
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
            Report { ran: true, findings_before: 3, resumed: true, findings_after: Some(3) },
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
        assert_eq!(report, Report { ran: true, findings_before: 0, resumed: false, findings_after: None });
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
        assert_eq!(report, Report { ran: true, findings_before: 2, resumed: false, findings_after: None });
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
        assert_eq!(report, Report { ran: true, findings_before: 2, resumed: true, findings_after: Some(0) });
    }

    #[test]
    fn the_lint_receipt_rides_the_evidence_envelope() {
        let mut evidence = serde_json::json!({ "command": "construct.implement" });
        Report { ran: true, findings_before: 4, resumed: true, findings_after: Some(1) }.stamp(&mut evidence);
        assert_eq!(evidence["lint_check"]["ran"], true);
        assert_eq!(evidence["lint_check"]["findings_before"], 4);
        assert_eq!(evidence["lint_check"]["resumed"], true);
        assert_eq!(evidence["lint_check"]["findings_after"], 1);

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
    // against findings taken from the disk it was told to distrust.
    #[test]
    fn the_repair_prompt_names_the_tree_the_findings_came_from() {
        let prompt = repair_prompt(&names(&["aether-math", "xtask"]), "warning: field names");

        assert!(prompt.contains("exactly as you left it"), "the turn continues on its own tree");
        assert!(prompt.contains("Nothing was reverted and nothing was reset."));
        assert!(prompt.contains("aether-math, xtask"), "the prompt names the packages that were checked");
        assert!(prompt.contains("warning: field names"), "the distilled findings ride the prompt");
        assert!(prompt.contains("one turn"), "the bound is stated: it changes what the turn should spend itself on");
    }
}
