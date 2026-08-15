//! The day's sync-back: a linear image of the day branch, one integration pull
//! request onto main, gated by the named branch-protection aggregate, and
//! merged by rebase (ADR-0186).

use std::env;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::MAIN;
use super::shell::{Shell, checked};

/// The body the sync-back pull request opens with. It says what the pull
/// request is for a human scanning the day's history, and why it is the one
/// pull request on this repository that does not squash.
const BODY: &str = "The day's landed blooms returning to main as one integration pull request (ADR-0186). \
                    Merged by rebase so each bloom's authored subject and closing lines land on main verbatim.";

/// The branch-protection aggregate. Absence is "not yet"; a partial required
/// set that does not include this name is not green.
const GATE: &str = "CI pass";

const POLL_INTERVAL_SECS: u64 = 10;
const GATE_TIMEOUT_SECS: u64 = 45 * 60;
const GATE_POLLS: u64 = GATE_TIMEOUT_SECS / POLL_INTERVAL_SECS;

/// Open or reuse the sync-back pull request from a linearized image of `from`,
/// wait for the named aggregate to conclude, and rebase-merge it. Returns the
/// pull request number for the roll's log.
pub fn merge(shell: &impl Shell, remote: &str, from: &str) -> Result<String> {
    let sync = sync_branch(from);
    let day_tree = linearize(shell, remote, from, &sync)?;

    let existing = open_pull_request(shell, &sync)?;
    publish(shell, remote, &sync, &day_tree, existing.is_some())?;

    let number = match existing {
        Some(number) => {
            println!("reusing open sync-back pull request #{number}");
            number
        }
        None => create_pull_request(shell, from, &sync)?,
    };

    wait_for_gate(shell, &number, from)?;

    // `--rebase`, never `--squash`: the carve-out ADR-0186 grants the sync pull
    // request is the whole reason each bloom's model-authored commit survives
    // onto main instead of arriving as one flattened blob.
    checked(shell, "gh", &["pr", "merge", &number, "--rebase"])?;
    println!("merged #{number} onto {MAIN} by rebase");
    Ok(number)
}

/// Replay `from` onto current main as `sync` in a temporary worktree. Returns
/// the day head's tree. A conflict or a tree that is not byte-identical to the
/// day is a refusal, and no pull request is opened from the result.
fn linearize(shell: &impl Shell, remote: &str, from: &str, sync: &str) -> Result<String> {
    let remote_main = format!("{remote}/{MAIN}");
    let remote_day = format!("{remote}/{from}");

    checked(shell, "git", &["fetch", remote, MAIN])?;
    checked(shell, "git", &["fetch", remote, from])?;

    let day_tree = tree_of(shell, &remote_day)?;
    let main_tree = tree_of(shell, &remote_main)?;
    let day_merges = checked(shell, "git", &["rev-list", "--merges", &format!("{remote_main}..{remote_day}")])?;
    if !day_merges.is_empty() {
        println!("{from} carries merge commits; replaying it linearly onto {MAIN} as {sync}");
    }

    let path = worktree_dir(from)?;
    remove_worktree(shell, &path);
    checked(shell, "git", &["worktree", "add", "-B", sync, &path, &remote_day])?;

    // The rebase belongs on the operator's terminal: a conflict is a refusal,
    // and the conflict markers are the diagnosis, not something this crate restates.
    if !shell.stream("git", &["-C", &path, "rebase", &remote_main])? {
        let _ = shell.capture("git", &["-C", &path, "rebase", "--abort"]);
        remove_worktree(shell, &path);
        bail!(
            "refusing to open a sync-back: linearizing {from} (tree {day_tree}) onto {MAIN} (tree {main_tree}) \
             conflicted"
        );
    }

    let sync_tree = tree_of(shell, sync)?;
    if sync_tree != day_tree {
        remove_worktree(shell, &path);
        bail!("refusing to open a sync-back: {sync} tree {sync_tree} is not byte-identical to {from} tree {day_tree}");
    }

    let remaining = checked(shell, "git", &["rev-list", "--merges", &format!("{remote_main}..{sync}")])?;
    remove_worktree(shell, &path);
    if !remaining.is_empty() {
        bail!("refusing to open a sync-back: {sync} still contains a merge commit");
    }

    Ok(day_tree)
}

/// Push the linearized image. A first publish is a regular push; a stale
/// already-published image (open pull request, day tree moved) is refreshed
/// with `--force-with-lease`. A published tree that still matches the day is
/// left alone so a re-run does not restart the gate.
fn publish(shell: &impl Shell, remote: &str, sync: &str, day_tree: &str, refreshing: bool) -> Result<()> {
    if refreshing {
        if published_tree(shell, remote, sync)?.as_deref() == Some(day_tree) {
            return Ok(());
        }
        checked(shell, "git", &["push", "--force-with-lease", remote, sync])?;
        return Ok(());
    }

    checked(shell, "git", &["push", remote, sync])?;
    Ok(())
}

fn create_pull_request(shell: &impl Shell, from: &str, sync: &str) -> Result<String> {
    let title = format!("chore(meta): sync {from} back to {MAIN}");
    checked(shell, "gh", &["pr", "create", "--base", MAIN, "--head", sync, "--title", &title, "--body", BODY])?;
    open_pull_request(shell, sync)?
        .with_context(|| format!("gh pr create left no open pull request from {sync} onto {MAIN}"))
}

/// The number of the open pull request from `head` onto main, if there is one.
fn open_pull_request(shell: &impl Shell, head: &str) -> Result<Option<String>> {
    let listed = checked(
        shell,
        "gh",
        &["pr", "list", "--head", head, "--base", MAIN, "--state", "open", "--json", "number", "--jq", ".[0].number"],
    )?;
    Ok((!listed.is_empty()).then_some(listed))
}

fn wait_for_gate(shell: &impl Shell, number: &str, from: &str) -> Result<()> {
    println!("waiting for `{GATE}` on sync-back pull request #{number}");

    let mut saw_checks = false;
    for poll in 0..GATE_POLLS {
        let checks = list_checks(shell, number)?;
        saw_checks |= !checks.is_empty();

        match conclusion(&checks) {
            Conclusion::Passed => return Ok(()),
            Conclusion::Failed(detail) => bail!(
                "sync-back pull request #{number} is not green ({detail}); {from} stays bloomery's mainline until a \
                 repair bloom fixes it and the sync merges"
            ),
            Conclusion::Pending if poll + 1 == GATE_POLLS => {}
            Conclusion::Pending => {
                checked(shell, "sleep", &[&POLL_INTERVAL_SECS.to_string()])?;
            }
        }
    }

    bail!("{}", timeout_refusal(number, from, saw_checks));
}

fn list_checks(shell: &impl Shell, number: &str) -> Result<Vec<Check>> {
    // `gh pr checks` exits non-zero for "no checks yet", for pending (exit 8),
    // and for a failing set. The JSON, when present, is the source of truth;
    // a missing list is absence, never a conclusion.
    let run = shell.capture("gh", &["pr", "checks", number, "--json", "name,state,bucket"])?;
    let stdout = run.stdout.trim();
    if stdout.starts_with('[') {
        return serde_json::from_str(stdout).context("parse gh pr checks json");
    }
    Ok(Vec::new())
}

fn conclusion(checks: &[Check]) -> Conclusion {
    match checks.iter().find(|check| check.name == GATE) {
        Some(check) if check.passed() => Conclusion::Passed,
        Some(check) if check.failed() => Conclusion::Failed(check.summary()),
        _ => Conclusion::Pending,
    }
}

fn timeout_refusal(number: &str, from: &str, saw_checks: bool) -> String {
    if saw_checks {
        format!(
            "sync-back pull request #{number} never reported `{GATE}` within {GATE_TIMEOUT_SECS} seconds; {from} stays \
             bloomery's mainline until the aggregate registers and the roll is re-run"
        )
    } else {
        format!(
            "sync-back pull request #{number} reported no checks within {GATE_TIMEOUT_SECS} seconds; {from} stays \
             bloomery's mainline until the checks register and the roll is re-run"
        )
    }
}

fn tree_of(shell: &impl Shell, rev: &str) -> Result<String> {
    checked(shell, "git", &["rev-parse", &format!("{rev}^{{tree}}")])
}

fn published_tree(shell: &impl Shell, remote: &str, sync: &str) -> Result<Option<String>> {
    if !shell.capture("git", &["fetch", remote, sync])?.success {
        return Ok(None);
    }
    Ok(Some(tree_of(shell, &format!("{remote}/{sync}"))?))
}

fn sync_branch(from: &str) -> String {
    format!("bloomery/sync/{}", from.rsplit('/').next().unwrap_or(from))
}

fn worktree_dir(from: &str) -> Result<String> {
    let date = from.rsplit('/').next().unwrap_or(from);
    let path = env::temp_dir().join(format!("bloomery-sync-{date}"));
    path.to_str().map(str::to_owned).with_context(|| format!("worktree path {} is not utf-8", path.display()))
}

fn remove_worktree(shell: &impl Shell, path: &str) {
    let _ = shell.capture("git", &["worktree", "remove", "--force", path]);
}

#[derive(Debug, Deserialize)]
struct Check {
    name: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    bucket: String,
}

impl Check {
    fn passed(&self) -> bool {
        self.bucket == "pass" || (self.bucket.is_empty() && self.state.eq_ignore_ascii_case("success"))
    }

    fn failed(&self) -> bool {
        matches!(self.bucket.as_str(), "fail" | "cancel")
            || (self.bucket.is_empty()
                && matches!(
                    self.state.to_ascii_uppercase().as_str(),
                    "FAILURE" | "CANCELLED" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED"
                ))
    }

    fn summary(&self) -> String {
        let status = if self.bucket.is_empty() {
            self.state.as_str()
        } else {
            self.bucket.as_str()
        };
        format!("{} {status}", self.name)
    }
}

enum Conclusion {
    Passed,
    Failed(String),
    Pending,
}

#[cfg(test)]
pub(super) const GREEN_GATE_JSON: &str = r#"[{"name":"CI pass","state":"SUCCESS","bucket":"pass"}]"#;

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{GATE, GREEN_GATE_JSON, merge, sync_branch};
    use crate::bloom::roll::shell::Run;
    use crate::bloom::roll::shell::fake::Fake;

    const DAY: &str = "bloomery/daily/2026-08-14";
    const ORIGIN: &str = "origin";
    const SYNC: &str = "bloomery/sync/2026-08-14";

    fn green_checks() -> Run {
        Run::ok(GREEN_GATE_JSON)
    }

    fn lint_only() -> Run {
        Run::ok(r#"[{"name":"Lint title","state":"SUCCESS","bucket":"pass"}]"#)
    }

    fn red_gate() -> Run {
        Run {
            success: false,
            stdout: r#"[{"name":"CI pass","state":"FAILURE","bucket":"fail"}]"#.to_owned(),
            stderr: "1 failing check".to_owned(),
        }
    }

    fn reuse_green() -> Fake<'static> {
        Fake::new(|line| match line {
            line if line.starts_with("gh pr list") => Run::ok("4990"),
            line if line.starts_with("gh pr checks") => green_checks(),
            line if line.contains("rev-parse") => Run::ok("tree-day"),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        })
    }

    // Tripwire: the sync-back merges by rebase. A squash here — the merge method
    // every other pull request in this repository uses — flattens the day into
    // one commit and destroys each bloom's authored subject and `Closes` lines,
    // which is the whole reason the day is a branch rather than a queue.
    #[test]
    fn the_sync_back_merges_by_rebase_after_the_named_gate_passes() {
        let shell = reuse_green();

        assert_eq!(merge(&shell, ORIGIN, DAY).expect("a green sync-back merges"), "4990");

        let calls = shell.calls();
        let checks = calls.iter().position(|line| line.starts_with("gh pr checks")).expect("the checks are awaited");
        let merged = calls.iter().position(|line| line.starts_with("gh pr merge")).expect("the pull request is merged");
        assert!(checks < merged, "the checks are awaited before the merge: {calls:?}");
        assert!(calls[merged].contains("--rebase"), "the sync-back merges by rebase: {}", calls[merged]);
        assert!(!calls.iter().any(|line| line.contains("--squash")), "nothing squashes the day: {calls:?}");
        assert!(
            !calls.iter().any(|line| line.contains("--admin") || line.contains("--auto")),
            "the merge does not route around branch protection: {calls:?}"
        );
        assert!(
            !calls.iter().any(|line| line.contains("--watch") || line.contains("--fail-fast")),
            "the wait does not watch an incomplete check set: {calls:?}"
        );
    }

    // Tripwire: a re-run after a red or interrupted check wait merges the pull
    // request that is already open. Opening a second one splits the day's
    // sync-back in two, and only one of them can carry the day.
    #[test]
    fn an_open_sync_back_is_reused_rather_than_duplicated() {
        let shell = reuse_green();

        merge(&shell, ORIGIN, DAY).expect("a green sync-back merges");

        assert!(!shell.calls().iter().any(|line| line.starts_with("gh pr create")), "{:?}", shell.calls());
    }

    #[test]
    fn a_red_sync_back_refuses_instead_of_merging() {
        let shell = Fake::new(|line| match line {
            line if line.starts_with("gh pr list") => Run::ok("4990"),
            line if line.starts_with("gh pr checks") => red_gate(),
            line if line.contains("rev-parse") => Run::ok("tree-day"),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, ORIGIN, DAY).expect_err("a red sync-back is refused").to_string();

        assert!(refusal.contains("not green"), "the refusal names the red checks: {refusal}");
        assert!(refusal.contains(GATE), "the refusal names the aggregate that failed: {refusal}");
        assert!(refusal.contains(DAY), "the refusal names the branch that stays mainline: {refusal}");
        assert!(!refusal.contains("no checks"), "a failed run is not reported as unregistered: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.starts_with("gh pr merge")), "{:?}", shell.calls());
    }

    // The first 2026-08-14 roll shape: GitHub had registered nothing, `gh`
    // exited non-zero, and the roll treated absence as a failed run.
    #[test]
    fn an_empty_check_list_is_waited_out_until_the_gate_arrives() {
        let polls = Cell::new(0);
        let shell = Fake::new(move |line| match line {
            line if line.starts_with("gh pr list") => Run::ok("4990"),
            line if line.starts_with("gh pr checks") => {
                let n = polls.get();
                polls.set(n + 1);
                if n == 0 {
                    Run::failed("no checks reported on the 'bloomery/sync/2026-08-14' branch")
                } else {
                    green_checks()
                }
            }
            line if line.contains("rev-parse") => Run::ok("tree-day"),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        });

        merge(&shell, ORIGIN, DAY).expect("the gate arriving after absence is a wait, not a refusal");

        let calls = shell.calls();
        let check_calls = calls.iter().filter(|line| line.starts_with("gh pr checks")).count();
        assert!(check_calls >= 2, "absence is polled again: {calls:?}");
        assert!(
            calls.iter().any(|line| line.starts_with("sleep")),
            "the wait sleeps rather than treating absence as terminal: {calls:?}"
        );
        assert!(calls.iter().any(|line| line.starts_with("gh pr merge")), "the arrived gate is merged: {calls:?}");
    }

    // The second 2026-08-14 roll shape: `Lint title` had registered and was
    // green, `--required --watch` returned success, and the roll tried to merge
    // a check set that did not yet include the aggregate.
    #[test]
    fn a_partial_required_set_is_not_green_until_the_aggregate_arrives() {
        let polls = Cell::new(0);
        let shell = Fake::new(move |line| match line {
            line if line.starts_with("gh pr list") => Run::ok("4990"),
            line if line.starts_with("gh pr checks") => {
                let n = polls.get();
                polls.set(n + 1);
                if n == 0 {
                    lint_only()
                } else {
                    green_checks()
                }
            }
            line if line.contains("rev-parse") => Run::ok("tree-day"),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        });

        merge(&shell, ORIGIN, DAY).expect("the aggregate arriving after Lint title is a wait, not a merge");

        let calls = shell.calls();
        let check_calls = calls.iter().filter(|line| line.starts_with("gh pr checks")).count();
        assert!(check_calls >= 2, "a partial set is polled again: {calls:?}");
        let merged = calls.iter().position(|line| line.starts_with("gh pr merge")).expect("the pull request is merged");
        let second = calls
            .iter()
            .enumerate()
            .filter(|(_, line)| line.starts_with("gh pr checks"))
            .nth(1)
            .map(|(index, _)| index)
            .expect("the aggregate is observed");
        assert!(second < merged, "merge waits for the aggregate, not the first required check: {calls:?}");
    }

    #[test]
    fn a_timeout_with_no_checks_does_not_claim_they_failed() {
        let shell = Fake::new(|line| match line {
            line if line.starts_with("gh pr list") => Run::ok("4990"),
            line if line.starts_with("gh pr checks") => {
                Run::failed("no checks reported on the 'bloomery/sync/2026-08-14' branch")
            }
            line if line.contains("rev-parse") => Run::ok("tree-day"),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, ORIGIN, DAY).expect_err("unregistered checks time out").to_string();

        assert!(refusal.contains("no checks"), "absence is named as unregistered: {refusal}");
        assert!(!refusal.contains("not green"), "unregistered checks are not a failed run: {refusal}");
        assert!(!refusal.contains("repair bloom"), "there is nothing for a repair bloom to fix: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.starts_with("gh pr merge")), "{:?}", shell.calls());
    }

    // Forward syncs leave `main → day` merge commits on the day branch. GitHub
    // refuses to rebase-merge a range that still contains them, so the pull
    // request has to open from the linearized image, not the day head.
    #[test]
    fn a_day_with_merge_commits_opens_the_sync_back_from_a_linear_image() {
        let created = Cell::new(false);
        let shell = Fake::new(move |line| match line {
            line if line.starts_with("gh pr create") => {
                created.set(true);
                Run::ok("")
            }
            line if line.starts_with("gh pr list") => {
                if created.get() {
                    Run::ok("5004")
                } else {
                    Run::ok("")
                }
            }
            line if line.starts_with("gh pr checks") => green_checks(),
            line if line.contains("rev-list") && line.contains("bloomery/daily") => Run::ok("cafemerge"),
            line if line.contains("rev-list") => Run::ok(""),
            line if line.contains("rev-parse") => Run::ok("tree-day"),
            _ => Run::ok(""),
        });

        assert_eq!(merge(&shell, ORIGIN, DAY).expect("a linearized day rolls"), "5004");

        let calls = shell.calls();
        assert!(
            calls.iter().any(|line| line.contains("rev-list") && line.contains("--merges") && line.contains(DAY)),
            "the day range is inspected for merge commits: {calls:?}"
        );
        let rebase = calls
            .iter()
            .position(|line| line.contains(" rebase") && !line.contains("--abort"))
            .expect("the day is rebased onto main");
        let trees = calls.iter().position(|line| line.contains("rev-parse") && line.contains("^{tree}"));
        assert!(trees.is_some(), "the linearized tree is compared to the day: {calls:?}");
        let created_at =
            calls.iter().position(|line| line.starts_with("gh pr create")).expect("the pull request is opened");
        assert!(rebase < created_at, "the image is built before the pull request exists: {calls:?}");
        assert!(
            calls[created_at].contains(&format!("--head {SYNC}")),
            "the pull request opens from the sync branch: {}",
            calls[created_at]
        );
        assert!(
            !calls[created_at].contains(&format!("--head {DAY}")),
            "the pull request does not open from the merge-commit day: {}",
            calls[created_at]
        );
        assert!(
            !calls.iter().any(|line| line.contains("--rebase-merges")),
            "merge commits are dropped, not replayed: {calls:?}"
        );
    }

    #[test]
    fn a_linearization_conflict_refuses_with_the_trees_named() {
        let shell = Fake::new(|line| match line {
            line if line.contains(" rebase") && !line.contains("--abort") => Run::failed("could not apply"),
            line if line.contains("rev-parse") && line.contains("bloomery/daily") => Run::ok("day-tree"),
            line if line.contains("rev-parse") => Run::ok("main-tree"),
            line if line.contains("rev-list") => Run::ok("cafemerge"),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, ORIGIN, DAY).expect_err("a conflicted linearization is refused").to_string();

        assert!(refusal.contains("day-tree"), "the day tree is named: {refusal}");
        assert!(refusal.contains("main-tree"), "the main tree is named: {refusal}");
        assert!(refusal.contains("conflicted"), "the refusal names the conflict: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.starts_with("gh pr create")), "{:?}", shell.calls());
        assert!(!shell.calls().iter().any(|line| line.starts_with("gh pr merge")), "{:?}", shell.calls());
    }

    #[test]
    fn a_tree_mismatch_refuses_with_the_trees_named() {
        let shell = Fake::new(|line| match line {
            line if line.contains("rev-parse") && line.contains("bloomery/daily") => Run::ok("day-tree"),
            line if line.contains("rev-parse") && line.contains("bloomery/sync") => Run::ok("sync-tree"),
            line if line.contains("rev-parse") => Run::ok("main-tree"),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, ORIGIN, DAY).expect_err("a drifted linearization is refused").to_string();

        assert!(refusal.contains("day-tree"), "the day tree is named: {refusal}");
        assert!(refusal.contains("sync-tree"), "the linearized tree is named: {refusal}");
        assert!(refusal.contains("not byte-identical"), "the refusal names the mismatch: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.starts_with("gh pr create")), "{:?}", shell.calls());
        assert!(!shell.calls().iter().any(|line| line.starts_with("gh pr merge")), "{:?}", shell.calls());
    }

    #[test]
    fn a_range_that_still_contains_a_merge_is_not_opened() {
        let shell = Fake::new(|line| match line {
            line if line.contains("rev-list") && line.contains("bloomery/sync") => Run::ok("stillamerge"),
            line if line.contains("rev-list") => Run::ok("cafemerge"),
            line if line.contains("rev-parse") => Run::ok("tree-day"),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, ORIGIN, DAY).expect_err("a merge still on the image is refused").to_string();

        assert!(refusal.contains("still contains a merge commit"), "the leftover merge is named: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.starts_with("gh pr create")), "{:?}", shell.calls());
    }

    #[test]
    fn a_stale_sync_back_is_refreshed_rather_than_merged() {
        let shell = Fake::new(|line| match line {
            line if line.starts_with("gh pr list") => Run::ok("4990"),
            line if line.starts_with("gh pr checks") => green_checks(),
            line if line.contains("rev-parse") && line.contains(&format!("{ORIGIN}/{SYNC}")) => Run::ok("old-tree"),
            line if line.contains("rev-parse") => Run::ok("day-tree"),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        });

        merge(&shell, ORIGIN, DAY).expect("a stale image is refreshed and then merged");

        let calls = shell.calls();
        assert!(
            calls
                .iter()
                .any(|line| line.contains("push") && line.contains("--force-with-lease") && line.contains(SYNC)),
            "the stale sync branch is refreshed: {calls:?}"
        );
        assert!(!calls.iter().any(|line| line.starts_with("gh pr create")), "the open pull request is kept: {calls:?}");
        assert!(
            calls.iter().any(|line| line.starts_with("gh pr merge")),
            "the refreshed pull request is merged: {calls:?}"
        );
    }

    #[test]
    fn the_sync_branch_is_the_day_date_under_the_sync_prefix() {
        assert_eq!(sync_branch(DAY), SYNC);
        assert_eq!(sync_branch("bloomery/daily/2026-08-15"), "bloomery/sync/2026-08-15");
    }
}
