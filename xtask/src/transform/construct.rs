//! The `construct.implement` lane (#3511): assemble the prompt from the
//! lane's in-repo instruction source plus the checked-out subject, run
//! headless Claude, gate on a produced candidate, and stamp the evidence.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::transform::claude::assemble_construct_prompt;
use crate::transform::fixers::{self, Report as FixerReport};
use crate::transform::{Measurements, TransformArgs, run_model_lane, write_evidence_json};

/// The typed id of the model-driven construct lane (#3511). Recognized here so
/// an unknown id stays unmapped exactly as in the verify lane.
pub(super) const CONSTRUCT_IMPLEMENT: &str = "construct.implement";

/// The file the agent writes its commit message to, relative to the run
/// worktree's root — the lane's one deliverable besides the candidate itself.
///
/// A single fixed path at the root, because the instruction text has to name it
/// literally and an agent that must first create a directory is one more way for
/// the deliverable to go missing. Deliberately *not* a gitignored path: the host
/// captures the candidate with `git add --all` after this process exits, so the
/// delete below is what keeps the deliverable out of the captured tree, and an
/// ignored path would make that delete unfalsifiable — a regression would leave
/// the file invisible to `git status` instead of visibly wrong.
const COMMIT_MESSAGE_DELIVERABLE: &str = ".bloomery-commit-message";

/// The lane-owned in-repo instruction source (#3572). Embedded at build time so
/// the construct lane owns its process natively — the prompt is assembled from
/// this text, never from `.claude/skills/implement` in the worker's checkout.
pub(super) const CONSTRUCT_INSTRUCTIONS: &str = include_str!("construct_instructions.md");

/// Stamp the broker-matched `nonce`, the command id, and the candidate-produced
/// signal onto the derived result `record`, producing the construct lane's
/// evidence envelope. `produced_candidate` records whether the run left a
/// candidate change in the working tree (#3596) — the completion gate reads it
/// alongside the terminal-`result`/`is_error` signal to demand a substantive
/// conclusion, not mere non-error. Pure so the binding is testable without
/// running Claude or git (#3572). `measured` carries what the host observed of
/// the run — what sccache served its builds and what it peaked at — each stamped
/// presence-driven by [`Measurements::stamp`]. `fixers` is whether the
/// mechanical fmt / clippy --fix pass ran and whether it moved the tree,
/// always present so a later reader can tell a model-authored line from a
/// fixer-authored one at the run level.
fn stamp_construct_evidence(
    nonce: Option<&str>,
    produced_candidate: bool,
    commit_message: Option<&str>,
    record: &serde_json::Value,
    measured: Measurements,
    fixers: FixerReport,
) -> serde_json::Value {
    let mut evidence = serde_json::json!({
        "command": CONSTRUCT_IMPLEMENT,
        "nonce": nonce,
        "produced_candidate": produced_candidate,
        "result_record": record,
    });
    fixers.stamp(&mut evidence);
    measured.stamp(&mut evidence);
    // Presence-driven, like the review lane's `findings`: a run that wrote no
    // deliverable stamps no key at all, so the host reads absence as "this run
    // named nothing" rather than as an empty message it might use as a subject.
    if let Some(message) = commit_message
        && let Some(object) = evidence.as_object_mut()
    {
        object.insert("commit_message".to_owned(), serde_json::Value::String(message.to_owned()));
    }
    evidence
}

/// Read the run's commit-message deliverable out of `worktree` and remove it,
/// returning the message when the agent left a non-empty one.
///
/// The remove is the load-bearing half and runs whether or not the read
/// succeeded: the host stages the whole worktree once this process exits, so a
/// deliverable still on disk would land inside the captured candidate tree. A
/// removal that itself fails warns rather than failing the lane — the message is
/// already in hand, and losing the run over a stray file would cost the whole
/// model attempt.
fn take_commit_message(worktree: &Path) -> Option<String> {
    let path = worktree.join(COMMIT_MESSAGE_DELIVERABLE);
    let read = fs::read_to_string(&path);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => eprintln!("construct lane: could not remove {}: {error}", path.display()),
    }
    read.ok().map(|message| message.trim().to_owned()).filter(|message| !message.is_empty())
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
    // worker's checkout. It is piped on the child's stdin. The curated lane
    // context rides along (#4647, #5141) so the conventions reach the agent
    // whichever harness is forked, rather than depending on the CLI to go and
    // find them — and rather than inlining the whole subject-tree CLAUDE.md.
    let prompt = assemble_construct_prompt(
        CONSTRUCT_INSTRUCTIONS,
        args.subject.as_deref(),
        args.task.as_deref(),
        args.seeded.as_deref(),
    );
    let run = run_model_lane(&prompt, args)?;

    // Take the commit-message deliverable before the candidate is inspected, and
    // in that order for two reasons: the file is gone by the time the host stages
    // the worktree, and a run whose only change *was* the deliverable does not
    // read as having produced a candidate.
    let commit_message = take_commit_message(Path::new("."));
    // The chassis stages with `git add --all` only after this process exits, so
    // this is the last moment the lane owns the tree. Fmt then a scoped
    // MachineApplicable clippy --fix apply the class of findings that otherwise
    // spend a Refine lap; they are best-effort and cannot fail the lane. The
    // candidate signal is read *after* so a fixer-authored edit is part of this
    // candidate rather than a second authorship.
    let fixers = fixers::apply(Path::new("."), &args.out);
    // Inspect the worktree (cwd) for the candidate change the run's whole job is
    // to leave (#3596): the gate demands a substantive conclusion, and an empty
    // diff is nothing to review. Captured after the child is reaped so it reflects
    // the run's final tree.
    let produced_candidate = capture_produced_candidate(&args.out);

    write_evidence_json(
        &args.out,
        &stamp_construct_evidence(
            args.nonce.as_deref(),
            produced_candidate,
            commit_message.as_deref(),
            &run.record,
            run.measured,
            fixers,
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::{env, fs, process};

    use aether_bloomery::{LANE_WORKPIECE_HEADER, pin_workpiece_description};

    use super::{
        COMMIT_MESSAGE_DELIVERABLE, CONSTRUCT_IMPLEMENT, CONSTRUCT_INSTRUCTIONS, FixerReport, Measurements,
        porcelain_signals_candidate, stamp_construct_evidence, take_commit_message,
    };
    use crate::transform::claude::assemble_construct_prompt;
    use crate::transform::conventions::LANE_CONTEXT;

    #[test]
    fn construct_evidence_binds_the_nonce_carries_the_record_and_the_candidate_signal() {
        let record = serde_json::json!({ "cost_usd": 0.42, "num_turns": 3, "input": 1000 });
        let evidence = stamp_construct_evidence(
            Some("nonce-7"),
            true,
            None,
            &record,
            Measurements::default(),
            FixerReport::default(),
        );
        assert_eq!(evidence["command"], CONSTRUCT_IMPLEMENT);
        assert_eq!(evidence["nonce"], "nonce-7", "the broker-matched nonce binds the evidence");
        assert_eq!(
            evidence["produced_candidate"], true,
            "the candidate-produced signal is stamped for the gate (#3596)"
        );
        assert_eq!(evidence["result_record"]["cost_usd"], 0.42, "the derived cost/turns record is carried");
        assert_eq!(evidence["result_record"]["num_turns"], 3);
        assert_eq!(evidence["fixers"]["ran"], false, "the fixer receipt is always present, even when they did not run");
        assert_eq!(evidence["fixers"]["changed"], false);

        assert!(
            evidence.get("commit_message").is_none(),
            "a run that wrote no deliverable stamps no commit_message key at all",
        );

        // An empty-candidate run stamps `false` so the gate can reject it while the
        // derived record is still carried whole.
        let no_nonce = stamp_construct_evidence(
            None,
            false,
            None,
            &serde_json::json!({ "no_result": true }),
            Measurements::default(),
            FixerReport::default(),
        );
        assert!(no_nonce["nonce"].is_null());
        assert_eq!(no_nonce["produced_candidate"], false, "an empty-candidate run stamps false");
        assert_eq!(no_nonce["result_record"]["no_result"], true, "the derived record is carried whole");
    }

    // The commit-message deliverable rides the evidence envelope whole — the host
    // reads its first line for the capture subject and the landing proposal's
    // title, so a stamp that dropped the body would silently cost the proposal
    // its prose.
    #[test]
    fn a_written_deliverable_rides_the_evidence_envelope_whole() {
        let message = "feat(crate:aether-render): draw the overlay pass\n\nThe world pass owns depth.";
        let evidence = stamp_construct_evidence(
            Some("n-1"),
            true,
            Some(message),
            &serde_json::json!({}),
            Measurements::default(),
            FixerReport::default(),
        );
        assert_eq!(evidence["commit_message"], message, "the message is carried verbatim, body included");
    }

    // Tripwire: a reader that has to infer whether fmt / clippy --fix ran
    // cannot tell a model-authored line from a fixer-authored one. The
    // receipt is therefore always present, including the ran-but-unchanged
    // case that would otherwise collapse into "never ran".
    #[test]
    fn fixer_receipt_rides_the_evidence_envelope() {
        let evidence = stamp_construct_evidence(
            Some("n-1"),
            true,
            None,
            &serde_json::json!({}),
            Measurements::default(),
            FixerReport { ran: true, changed: true },
        );
        assert_eq!(evidence["fixers"]["ran"], true);
        assert_eq!(evidence["fixers"]["changed"], true);
    }

    // The deliverable is read back *and removed*: the host stages the whole
    // worktree once this process exits, so a file left behind would land inside
    // the captured candidate tree (acceptance 4).
    #[test]
    fn taking_the_deliverable_returns_the_message_and_removes_the_file() {
        let worktree = scratch_dir("taken");
        let path = worktree.join(COMMIT_MESSAGE_DELIVERABLE);
        fs::write(&path, "fix(crate:aether-fs): reject a traversing path\n\nThe adapter joins.\n").expect("write");

        assert_eq!(
            take_commit_message(&worktree).as_deref(),
            Some("fix(crate:aether-fs): reject a traversing path\n\nThe adapter joins."),
            "the message comes back trimmed of trailing whitespace but otherwise whole",
        );
        assert!(!path.exists(), "the deliverable is removed before the host captures the worktree");
    }

    // Two shapes are the same absence: no file at all, and a file the agent
    // created but left blank. Neither may present itself as a message, because a
    // blank subject would assemble a landing title that is not lint-valid.
    #[test]
    fn an_absent_or_blank_deliverable_is_no_message() {
        let worktree = scratch_dir("blank");
        assert_eq!(take_commit_message(&worktree), None, "no deliverable is no message");

        let path = worktree.join(COMMIT_MESSAGE_DELIVERABLE);
        fs::write(&path, "  \n\n ").expect("write");
        assert_eq!(take_commit_message(&worktree), None, "a whitespace-only deliverable is no message");
        assert!(!path.exists(), "the blank deliverable is still removed");
    }

    /// A per-test scratch worktree under the system temp dir, unique per call so
    /// concurrent test threads never collide — the sibling lanes' convention.
    fn scratch_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-construct-deliverable-{tag}-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
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
            assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, Some("abc123"), Some("thread the work order"), None);
        assert!(prompt.contains(CONSTRUCT_INSTRUCTIONS), "the instruction source rides the prompt");
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
        assert!(!prompt.contains("\n## Lane\n"), "a header-less task grows no lane tail");

        // With no subject supplied, the prompt still stands and names no commit.
        let subjectless = assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, None, Some("still has a task"), None);
        assert!(subjectless.contains("## Subject"));
        assert!(subjectless.contains("\n## Task\n"));
        assert!(subjectless.contains(CONSTRUCT_INSTRUCTIONS));

        // With no task, the prompt still stands and appends no `## Task` section —
        // the fail-legible subject-only path for a member with no description.
        let taskless = assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, Some("abc123"), None, None);
        assert!(taskless.contains("## Subject"));
        assert!(!taskless.contains("\n## Task\n"), "no persisted description means no task section");
        assert!(taskless.contains(CONSTRUCT_INSTRUCTIONS));
    }

    // The curated lane context rides the prompt itself (#4647, #5141) rather
    // than being pointed at, because only one of the three harnesses the lane
    // can fork reads a conventions file on its own. They lead (#4985) so
    // construct and review share the lane-context prefix; the work order follows
    // the long general rules, and a per-lane tail may follow it.
    #[test]
    fn conventions_ride_the_prompt_ahead_of_the_work_order() {
        let prompt =
            assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, Some("abc123"), Some("thread the work order"), None);
        assert!(
            prompt.contains("Tests must earn their place"),
            "the curated lane context carries the testing doctrine",
        );
        assert!(prompt.starts_with("## Conventions\n"), "shared conventions lead so sibling lanes share a prefix");
        let conventions_at = prompt.find("## Conventions\n").expect("the conventions get their own section");
        let instructions_at = prompt.find(CONSTRUCT_INSTRUCTIONS).expect("the lane instructions still ride");
        let task_at = prompt.find("\n## Task\n").expect("the work order keeps its section");
        assert!(conventions_at < instructions_at, "lane context leads the lane-specific instructions");
        assert!(instructions_at < task_at, "the work order stays after the long general rules");
    }

    // Tripwire (#5141): the assembler used to inline the whole subject-tree
    // CLAUDE.md, so every turn re-read MCP / runtime / wasm authoring the lane
    // cannot act on. A wholesale embed returning is this test going red.
    #[test]
    fn assembled_construct_prompt_carries_lane_context_not_the_whole_claude_md() {
        let prompt =
            assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, Some("abc123"), Some("thread the work order"), None);

        assert!(prompt.contains(LANE_CONTEXT), "the curated lane context rides the prompt");
        assert!(prompt.contains("Tests must earn their place"), "testing doctrine stays");
        assert!(prompt.contains("spell units out in identifiers"), "code conventions stay");

        // Tripwire (#5254): construct used to learn the lint regime by failing
        // verify. The syllabus has to ride lane_context, after Commands and
        // before the testing surface, and name the measured shapes.
        let commands_at = LANE_CONTEXT.find("## Commands").expect("build/task framing stays");
        let lint_at = LANE_CONTEXT.find("## Lint expectations").expect("lint expectations are a named block");
        let harnesses_at = LANE_CONTEXT.find("## Test harnesses").expect("testing surface rules stay");
        assert!(
            commands_at < lint_at && lint_at < harnesses_at,
            "lint expectations sit after the build/task framing and before the surface rules",
        );
        assert!(
            LANE_CONTEXT.contains("disallowed-methods"),
            "lint expectations name the clippy methods that bite on the first pass",
        );
        assert!(
            LANE_CONTEXT.contains("wrong_self_convention"),
            "lint expectations name the measured function-length split knock-on",
        );
        assert!(
            LANE_CONTEXT.contains("#![allow(clippy::unwrap_used)]"),
            "lint expectations name the test-file unwrap header the suppression gate already permits",
        );

        for heading in [
            "## MCP harness",
            "## Runtime & subsystems",
            "## Writing components",
            "## Harness self-modification guardrail",
        ] {
            assert!(!prompt.contains(heading), "dropped CLAUDE.md section `{heading}` must not ride the prompt");
        }
    }

    // Tripwire: prompt caching is prefix-exact. A per-member `Workpiece:` header
    // ahead of the shared work order forfeits the cache for the whole body
    // (#4985). Two sibling construct dispatches of one bloom must share a
    // byte-identical prefix through the stable bulk.
    #[test]
    fn sibling_lane_prompts_share_a_byte_identical_prefix_through_the_work_order() {
        let body = "# Wave-3 member work order\n\nImplement the sealed plan.\n";
        let left = assemble_construct_prompt(
            CONSTRUCT_INSTRUCTIONS,
            Some("abc123"),
            Some(&pin_workpiece_description("issue-1111", body)),
            None,
        );
        let right = assemble_construct_prompt(
            CONSTRUCT_INSTRUCTIONS,
            Some("abc123"),
            Some(&pin_workpiece_description("issue-2222", body)),
            None,
        );

        let prefix_len = left.bytes().zip(right.bytes()).take_while(|(a, b)| a == b).count();
        let stable = assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, Some("abc123"), Some(body), None);
        assert!(
            prefix_len >= stable.len(),
            "common prefix ({prefix_len}) must cover the stable bulk ({})",
            stable.len(),
        );
        assert!(
            left[..prefix_len].contains(body.trim_end()),
            "the shared work order must sit inside the common prefix, not after a per-lane header",
        );
        assert!(left.contains(&format!("{LANE_WORKPIECE_HEADER} issue-1111")), "the left tail still names its member");
        assert!(
            right.contains(&format!("{LANE_WORKPIECE_HEADER} issue-2222")),
            "the right tail still names its member",
        );
        assert!(
            !left[..prefix_len].contains("issue-1111") && !left[..prefix_len].contains("issue-2222"),
            "member identity belongs in the tail after the common prefix",
        );
        assert!(left.contains("\n## Lane\n"), "the peeled header rides its own trailing section");
    }

    // Tripwire: construct vs review used to open with different instruction
    // files, so 99.8% overlapping ~59k-token prompts shared no cache (#4985).
    // Conventions lead, so distinct instruction texts still share lane context.
    #[test]
    fn conventions_lead_so_distinct_lane_instructions_still_share_them() {
        let construct = assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, Some("abc123"), Some("shared order"), None);
        let review = assemble_construct_prompt("REVIEW INSTRUCTIONS ONLY", Some("abc123"), Some("shared order"), None);
        let prefix_len = construct.bytes().zip(review.bytes()).take_while(|(a, b)| a == b).count();
        assert!(construct.starts_with("## Conventions\n"), "lane context leads the prompt");
        assert!(
            construct[..prefix_len].contains("Tests must earn their place"),
            "the shared conventions must sit inside the common prefix",
        );
        assert!(
            prefix_len > "Tests must earn their place".len(),
            "common prefix ({prefix_len}) must cover more than a single conventions phrase",
        );
    }

    // Tripwire for #4994 acceptance 4: the checkpoint's trust posture is a
    // property of *this* dispatch, not of every construct prompt. The plausible
    // bug is the original one — the paragraph lives in CONSTRUCT_INSTRUCTIONS
    // and a cold start is told it might be sitting on mid-refactor garbage.
    #[test]
    fn construct_prompt_names_the_seeded_state_only_when_seeded() {
        let cold =
            assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, Some("abc123"), Some("thread the work order"), None);
        assert!(!cold.contains("## Seeded state"), "a cold start grows no seeded-state section");
        assert!(!cold.contains("mid-refactor garbage"), "the trust posture is not static boilerplate on a cold start");
        assert!(
            !CONSTRUCT_INSTRUCTIONS.contains("## Seeded state")
                && !CONSTRUCT_INSTRUCTIONS.contains("mid-refactor garbage"),
            "the instruction source must not name a checkpoint the dispatch may not have",
        );

        let seeded = assemble_construct_prompt(
            CONSTRUCT_INSTRUCTIONS,
            Some("def456"),
            Some("thread the work order"),
            Some("def456"),
        );
        let task_at = seeded.find("\n## Task\n").expect("the work order keeps its section");
        let seeded_at = seeded.find("\n## Seeded state\n").expect("a seeded dispatch names the checkpoint");
        assert!(task_at < seeded_at, "the seeded-state section sits after the shared work order");
        assert!(seeded.contains("`def456`"), "the prompt names the checkpoint commit");
        assert!(seeded.contains("untrusted"), "the prompt names the checkpoint's trust posture");
        assert!(seeded.contains("mid-refactor garbage"), "the prompt states what a dead attempt's partial tree can be");

        let body = "shared order";
        let cold_sibling = assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, Some("abc123"), Some(body), None);
        let seeded_sibling =
            assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, Some("abc123"), Some(body), Some("def456"));
        let prefix_len = cold_sibling.bytes().zip(seeded_sibling.bytes()).take_while(|(a, b)| a == b).count();
        assert!(
            cold_sibling[..prefix_len].contains(body),
            "a seeded dispatch must not push the work order out of the shared prefix",
        );
    }

    // Tripwire for #5078: Construct used to restate Verify's full mechanical
    // matrix, so a successful construct occupied the serial path with work whose
    // verdict the reducer does not consume. Lightweight authoring checks stay;
    // the heavy argv must not ride the assembled prompt; fail-closed candidate
    // language is unchanged.
    #[test]
    fn construct_prompt_keeps_authoring_checks_and_leaves_verify_gates_to_verify() {
        let prompt =
            assemble_construct_prompt(CONSTRUCT_INSTRUCTIONS, Some("abc123"), Some("thread the work order"), None);

        assert!(prompt.contains("cargo fmt"), "formatting stays as cheap authoring feedback");
        assert!(prompt.contains("focused tests"), "focused behavior tests stay as cheap authoring feedback");
        assert!(
            prompt.contains("Dedicated Verify owns"),
            "the prompt must say Verify owns the mechanical matrix so the model does not volunteer it",
        );
        assert!(
            prompt.contains("Do not run workspace- or package-wide"),
            "the prompt must forbid volunteering the heavy gates, not merely omit their argv",
        );

        for needle in [
            "clippy --workspace --all-targets --keep-going --message-format=json",
            "doc --workspace --no-deps --document-private-items --all-features --keep-going",
            "nextest run --all-features --profile ci --no-fail-fast",
            "scripts/check-suppressions.py",
            "jscpd@5.0.12",
            "--no-ignore --skip-target-dir crates",
            "AETHER_REQUIRE_RUNTIME=1",
            "AETHER_STORE_PATH=:memory:",
            "RUSTDOCFLAGS=-D rustdoc::redundant_explicit_links",
        ] {
            assert!(!prompt.contains(needle), "heavy Verify argv `{needle}` must not ride the construct prompt");
        }

        assert!(
            prompt.contains("say so plainly"),
            "an unimplementable work order still fails closed instead of inventing a change",
        );
        assert!(prompt.contains("cannot proceed"), "the honest no-candidate exit remains named");
        assert!(
            prompt.contains("Stop at the candidate"),
            "the lane still ends on a working-tree candidate, not a merge",
        );
        assert!(
            prompt.contains("Leave the change in the working tree"),
            "an empty-diff run is still nothing to review",
        );
    }
}
