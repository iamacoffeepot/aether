//! The `construct.implement` lane (#3511): assemble the prompt from the
//! lane's in-repo instruction source plus the checked-out subject, run
//! headless Claude, gate on a produced candidate, and stamp the evidence.

use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::transform::claude::assemble_construct_prompt;
use crate::transform::{TransformArgs, conventions, run_model_lane, write_evidence_json};

/// The typed id of the model-driven construct lane (#3511). Recognized here so
/// an unknown id stays unmapped exactly as in the verify lane.
pub(super) const CONSTRUCT_IMPLEMENT: &str = "construct.implement";

/// The lane-owned in-repo instruction source (#3572). Embedded at build time so
/// the construct lane owns its process natively — the prompt is assembled from
/// this text, never from `.claude/skills/implement` in the worker's checkout.
const CONSTRUCT_INSTRUCTIONS: &str = include_str!("construct_instructions.md");

/// Stamp the broker-matched `nonce`, the command id, and the candidate-produced
/// signal onto the derived result `record`, producing the construct lane's
/// evidence envelope. `produced_candidate` records whether the run left a
/// candidate change in the working tree (#3596) — the completion gate reads it
/// alongside the terminal-`result`/`is_error` signal to demand a substantive
/// conclusion, not mere non-error. Pure so the binding is testable without
/// running Claude or git (#3572).
fn stamp_construct_evidence(
    nonce: Option<&str>,
    produced_candidate: bool,
    record: &serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "command": CONSTRUCT_IMPLEMENT,
        "nonce": nonce,
        "produced_candidate": produced_candidate,
        "result_record": record,
    })
}

/// Whether `git status --porcelain` stdout signals a candidate change in the
/// working tree: a non-empty output means the construct run left something to
/// review, an empty one means it did nothing (#3596). Entries whose path is under
/// `out_dir` — the run's own evidence output tree — are ignored: the local base is
/// relative (`.bloomery/local-worktrees`), so the `--out` dir can resolve inside
/// the worktree cwd and its untracked output would otherwise read as a candidate
/// (#3632). Pure so the mapping is testable without spawning git.
fn porcelain_signals_candidate(stdout: &str, out_dir: &Path) -> bool {
    stdout.lines().any(|line| {
        if line.trim().is_empty() {
            return false;
        }
        // Porcelain v1: two status chars, a space, then the path. A rename renders
        // as `orig -> new`; the candidate is the new path.
        let path = line.get(3..).unwrap_or("").trim();
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        !path.is_empty() && !Path::new(path).starts_with(out_dir)
    })
}

/// Inspect the working tree (cwd — the checked-out worktree the construct lane
/// runs in) for a candidate change the run left, via `git status --porcelain`,
/// ignoring entries under the run's own `out_dir` evidence tree (#3632).
/// Fail-closed: a git that will not run cannot prove a candidate, so it reads as
/// none (the completion gate then rejects the attempt).
fn capture_produced_candidate(out_dir: &Path) -> bool {
    Command::new("git").args(["status", "--porcelain"]).output().is_ok_and(|output| {
        output.status.success() && porcelain_signals_candidate(&String::from_utf8_lossy(&output.stdout), out_dir)
    })
}

/// The `construct.implement` lane: assemble the prompt from the lane's in-repo
/// instruction source plus the checked-out subject, run headless Claude at the
/// resolved model (or the operator's ambient default when none is resolved,
/// #3592), capture the stream-json transcript, derive the result record
/// in-repo, and write it as nonce-tagged evidence (#3572). This lane needs a
/// Claude credential, so it runs worker-side (BYO) — never on the coordinator's
/// zero-secret path.
pub(super) fn run_construct(args: &TransformArgs) -> Result<()> {
    // The lane owns its process: the prompt is assembled from the in-repo
    // instruction source and the checked-out subject, never from a skill in the
    // worker's checkout. It is piped on the child's stdin. The subject tree's own
    // conventions ride along (#4647) — read from the cwd the lane runs in, which
    // is that checkout — so they reach the agent whichever harness is forked,
    // rather than depending on the CLI to go and find them.
    let prompt = assemble_construct_prompt(
        CONSTRUCT_INSTRUCTIONS,
        conventions::read(Path::new(".")).as_deref(),
        args.subject.as_deref(),
        args.task.as_deref(),
    );
    let record = run_model_lane(&prompt, args)?;

    // Inspect the worktree (cwd) for the candidate change the run's whole job is
    // to leave (#3596): the gate demands a substantive conclusion, and an empty
    // diff is nothing to review. Captured after the child is reaped so it reflects
    // the run's final tree.
    let produced_candidate = capture_produced_candidate(&args.out);

    write_evidence_json(&args.out, &stamp_construct_evidence(args.nonce.as_deref(), produced_candidate, &record))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{CONSTRUCT_IMPLEMENT, CONSTRUCT_INSTRUCTIONS, porcelain_signals_candidate, stamp_construct_evidence};
    use crate::transform::claude::assemble_construct_prompt;

    #[test]
    fn construct_evidence_binds_the_nonce_carries_the_record_and_the_candidate_signal() {
        let record = serde_json::json!({ "cost_usd": 0.42, "num_turns": 3, "input": 1000 });
        let evidence = stamp_construct_evidence(Some("nonce-7"), true, &record);
        assert_eq!(evidence["command"], CONSTRUCT_IMPLEMENT);
        assert_eq!(evidence["nonce"], "nonce-7", "the broker-matched nonce binds the evidence");
        assert_eq!(
            evidence["produced_candidate"], true,
            "the candidate-produced signal is stamped for the gate (#3596)"
        );
        assert_eq!(evidence["result_record"]["cost_usd"], 0.42, "the derived cost/turns record is carried");
        assert_eq!(evidence["result_record"]["num_turns"], 3);

        // An empty-candidate run stamps `false` so the gate can reject it while the
        // derived record is still carried whole.
        let no_nonce = stamp_construct_evidence(None, false, &serde_json::json!({ "no_result": true }));
        assert!(no_nonce["nonce"].is_null());
        assert_eq!(no_nonce["produced_candidate"], false, "an empty-candidate run stamps false");
        assert_eq!(no_nonce["result_record"]["no_result"], true, "the derived record is carried whole");
    }

    // The candidate signal is a pure map of `git status --porcelain` stdout: a
    // non-empty output is a candidate change, an empty one is not (#3596), and the
    // run's own evidence output tree under `out_dir` never counts (#3632).
    #[test]
    fn porcelain_signals_a_candidate_only_when_the_worktree_is_dirty() {
        let out = Path::new(".bloomery/local-worktrees/n-evidence");
        assert!(!porcelain_signals_candidate("", out), "a clean worktree left no candidate");
        assert!(!porcelain_signals_candidate("   \n  \n", out), "whitespace-only output is still no candidate");
        assert!(porcelain_signals_candidate(" M xtask/src/transform.rs\n", out), "a modified file is a candidate");
        assert!(porcelain_signals_candidate("?? new_file.rs\n", out), "a new untracked file is a candidate");
    }

    // The run's own evidence tree (and any other output under `out_dir`) is not a
    // candidate; a real source change alongside it still is (#3632).
    #[test]
    fn porcelain_ignores_the_runs_own_output_tree() {
        let out = Path::new(".bloomery/local-worktrees/n-evidence");
        assert!(
            !porcelain_signals_candidate("?? .bloomery/local-worktrees/n-evidence/evidence.json\n", out),
            "the run's own evidence output is not a candidate",
        );
        assert!(
            !porcelain_signals_candidate("?? .bloomery/local-worktrees/n-evidence/\n", out),
            "the untracked evidence directory itself is not a candidate",
        );
        assert!(
            porcelain_signals_candidate("?? .bloomery/local-worktrees/n-evidence/evidence.json\n M src/lib.rs\n", out,),
            "a real source change alongside the evidence tree is still a candidate",
        );
        assert!(
            porcelain_signals_candidate("R  old_name.rs -> new_name.rs\n", out),
            "a rename to a path outside the out dir is a candidate (new-path parse)",
        );
    }

    // The prompt is assembled from the lane's own in-repo instruction source plus
    // the checked-out subject — the construct lane owns its process natively and
    // never reads `.claude/skills/implement` (#3572). Pure: no Claude spawn.
    #[test]
    fn construct_prompt_assembles_from_the_in_repo_instructions_and_subject() {
        let prompt =
            assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, None, Some("abc123"), Some("thread the work order"));
        assert!(prompt.starts_with(CONSTRUCT_INSTRUCTIONS), "the lane's own instruction source leads the prompt");
        assert!(prompt.contains("## Subject"), "the checked-out subject is appended as its own section");
        assert!(prompt.contains("abc123"), "the exact checked-out commit is named in the prompt");
        assert!(
            !prompt.contains(".claude/skills/implement"),
            "the native lane never delegates to the retired implement skill",
        );

        // The work-order description is appended under its own `## Task` section
        // (#3595) so the model is told what to build, not just where. Match the
        // heading on its own line (`\n## Task\n`), since the instruction text
        // itself references the section name inline as a code span.
        assert!(prompt.contains("\n## Task\n"), "the work-order description is appended as its own section");
        assert!(prompt.contains("thread the work order"), "the task text is named in the prompt");

        // With no subject supplied, the prompt still stands and names no commit.
        let subjectless = assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, None, None, Some("still has a task"));
        assert!(subjectless.contains("## Subject"));
        assert!(subjectless.contains("\n## Task\n"));
        assert!(subjectless.starts_with(CONSTRUCT_INSTRUCTIONS));

        // With no task, the prompt still stands and appends no `## Task` section —
        // the fail-legible subject-only path for a member with no description.
        let taskless = assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, None, Some("abc123"), None);
        assert!(taskless.contains("## Subject"));
        assert!(!taskless.contains("\n## Task\n"), "no persisted description means no task section");
        assert!(taskless.starts_with(CONSTRUCT_INSTRUCTIONS));
    }

    // The conventions the subject tree carries ride the prompt itself (#4647)
    // rather than being pointed at, because only one of the three harnesses the
    // lane can fork reads a conventions file on its own. The order is what the
    // instructions promise: the work order stays last, after the long general
    // rules, so `## Task` is still the section at the end.
    #[test]
    fn conventions_ride_the_prompt_ahead_of_the_work_order() {
        let prompt = assemble_construct_prompt(
            CONSTRUCT_INSTRUCTIONS,
            Some("Tests must earn their place."),
            Some("abc123"),
            Some("thread the work order"),
        );
        assert!(prompt.contains("Tests must earn their place."), "the tree's conventions are carried verbatim");
        let conventions_at = prompt.find("\n## Conventions\n").expect("the conventions get their own section");
        let task_at = prompt.find("\n## Task\n").expect("the work order keeps its section");
        assert!(conventions_at < task_at, "the work order stays last, where the instructions say it is");

        // A subject tree carrying no conventions file drops the section rather
        // than failing the dispatch or emitting an empty heading.
        let bare = assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, None, Some("abc123"), Some("build it"));
        assert!(!bare.contains("\n## Conventions\n"), "no conventions file means no conventions section");
        assert!(bare.contains("\n## Task\n"), "the rest of the prompt is unaffected");
    }
}
