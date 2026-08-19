//! The day's sync-back: a linear image of the day branch, a coverage-barred
//! compare-and-swap of fleet `refs/heads/main`, and a best-effort GitHub replica
//! push (ADR-0203).

use std::env;

use aether_bloomery_git::DayCoverage;
use anyhow::{Context, Result, bail};

use super::MAIN;
use super::shell::{Shell, checked};

/// Linearize `from` onto fleet main, refuse unless `coverage` is fully green,
/// compare-and-swap `refs/heads/main` onto the linearized head, and push main
/// to GitHub best-effort. Returns the advanced main sha for the roll's log.
pub fn merge(shell: &impl Shell, remote: &str, from: &str, coverage: &DayCoverage) -> Result<String> {
    if !coverage.is_fully_green() {
        bail!(
            "refusing to advance main: coverage map is not green ({}); {from} stays bloomery's mainline until the \
             map is fully green",
            coverage.hold_detail()
        );
    }

    let sync = sync_branch(from);
    let (linearized, expected) = linearize(shell, from, &sync)?;

    advance(shell, &linearized, &expected)?;
    println!("advanced {MAIN} to {linearized} under compare-and-swap");
    replicate(shell, remote);
    Ok(linearized)
}

/// Replay `from` onto current fleet main as `sync` in a temporary worktree.
/// Returns the linearized commit and the main sha the compare-and-swap must
/// still observe. A conflict or a tree that is not byte-identical to the day
/// is a refusal, and fleet main is not touched.
fn linearize(shell: &impl Shell, from: &str, sync: &str) -> Result<(String, String)> {
    let main = format!("refs/heads/{MAIN}");
    let day = format!("refs/heads/{from}");

    let day_tree = tree_of(shell, &day)?;
    let main_tree = tree_of(shell, &main)?;
    let expected = checked(shell, "git", &["rev-parse", &main])?;
    let day_merges = checked(shell, "git", &["rev-list", "--merges", &format!("{main}..{day}")])?;
    if !day_merges.is_empty() {
        println!("{from} carries merge commits; replaying it linearly onto {MAIN} as {sync}");
    }

    let path = worktree_dir(from)?;
    remove_worktree(shell, &path);
    checked(shell, "git", &["worktree", "add", "-B", sync, &path, &day])?;

    // The rebase belongs on the operator's terminal: a conflict is a refusal,
    // and the conflict markers are the diagnosis, not something this crate restates.
    if !shell.stream("git", &["-C", &path, "rebase", &main])? {
        let _ = shell.capture("git", &["-C", &path, "rebase", "--abort"]);
        remove_worktree(shell, &path);
        bail!(
            "refusing to advance main: linearizing {from} (tree {day_tree}) onto {MAIN} (tree {main_tree}) conflicted"
        );
    }

    let sync_tree = tree_of(shell, sync)?;
    if sync_tree != day_tree {
        remove_worktree(shell, &path);
        bail!("refusing to advance main: {sync} tree {sync_tree} is not byte-identical to {from} tree {day_tree}");
    }

    let remaining = checked(shell, "git", &["rev-list", "--merges", &format!("{main}..{sync}")])?;
    let linearized = checked(shell, "git", &["rev-parse", sync])?;
    remove_worktree(shell, &path);
    if !remaining.is_empty() {
        bail!("refusing to advance main: {sync} still contains a merge commit");
    }

    Ok((linearized, expected))
}

/// Compare-and-swap fleet `refs/heads/main` to the linearized commit. A lost
/// compare is a refusal — the same discipline as a landing.
fn advance(shell: &impl Shell, linearized: &str, expected: &str) -> Result<()> {
    checked(shell, "git", &["update-ref", &format!("refs/heads/{MAIN}"), linearized, expected])?;
    Ok(())
}

/// Push fleet main to GitHub the same one-way, best-effort way as the daily
/// refs. A rejected or unreachable replica does not unwind the advance: GitHub
/// is an output, never a gate.
fn replicate(shell: &impl Shell, remote: &str) {
    let run = match shell.capture("git", &["push", remote, MAIN]) {
        Ok(run) => run,
        Err(error) => {
            println!("best-effort GitHub replica push of {MAIN} failed: {error}");
            return;
        }
    };
    if run.success {
        println!("replicated {MAIN} to {remote}");
        return;
    }
    let reason = if run.stderr.is_empty() {
        &run.stdout
    } else {
        &run.stderr
    };
    println!("best-effort GitHub replica push of {MAIN} failed: {reason}");
}

fn tree_of(shell: &impl Shell, rev: &str) -> Result<String> {
    checked(shell, "git", &["rev-parse", &format!("{rev}^{{tree}}")])
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

#[cfg(test)]
mod tests {
    use super::{MAIN, merge, sync_branch};
    use crate::bloom::roll::shell::Run;
    use crate::bloom::roll::shell::fake::Fake;
    use aether_bloomery_git::DayCoverage;

    const DAY: &str = "bloomery/daily/2026-08-14";
    const ORIGIN: &str = "origin";
    const SYNC: &str = "bloomery/sync/2026-08-14";
    const LINEARIZED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const EXPECTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn green_linear() -> Fake<'static> {
        Fake::new(|line| match line {
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("tree-day"),
            line if line.contains("rev-parse") && line.contains(&format!("refs/heads/{MAIN}")) => Run::ok(EXPECTED),
            line if line.contains("rev-parse") => Run::ok(LINEARIZED),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        })
    }

    #[test]
    fn a_green_map_compare_and_swaps_fleet_main_then_replicates() {
        let shell = green_linear();

        assert_eq!(
            merge(&shell, ORIGIN, DAY, &DayCoverage::green()).expect("a green coverage map advances"),
            LINEARIZED
        );

        let calls = shell.calls();
        let cas = calls
            .iter()
            .position(|line| line.contains("update-ref") && line.contains(&format!("refs/heads/{MAIN}")))
            .expect("fleet main is compare-and-swapped");
        assert!(
            calls[cas].contains(LINEARIZED) && calls[cas].contains(EXPECTED),
            "the swap is expected-value, not a blind set: {}",
            calls[cas]
        );
        let pushed = calls.iter().position(|line| line.starts_with(&format!("git push {ORIGIN} {MAIN}")));
        assert!(pushed.is_some_and(|index| cas < index), "the replica push follows the advance: {calls:?}");
        assert!(!calls.iter().any(|line| line.starts_with("gh ")), "GitHub is not a gate: {calls:?}");
        assert!(!calls.iter().any(|line| line.contains("fetch")), "the advance does not fetch GitHub: {calls:?}");
    }

    #[test]
    fn a_non_green_map_refuses_without_touching_main() {
        let shell = green_linear();
        let coverage = DayCoverage::hold("red test crate::day_head");

        let refusal = merge(&shell, ORIGIN, DAY, &coverage).expect_err("a held map is refused").to_string();

        assert!(refusal.contains("not green"), "the refusal names the coverage bar: {refusal}");
        assert!(refusal.contains("crate::day_head"), "the refusal forwards the hold: {refusal}");
        assert!(refusal.contains(DAY), "the refusal names the branch that stays mainline: {refusal}");
        let calls = shell.calls();
        assert!(calls.is_empty(), "a held roll is a no-op: {calls:?}");
    }

    #[test]
    fn a_replica_push_failure_does_not_unwind_the_advance() {
        let shell = Fake::new(|line| match line {
            line if line.starts_with(&format!("git push {ORIGIN} {MAIN}")) => Run::failed("protected branch"),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("tree-day"),
            line if line.contains("rev-parse") && line.contains(&format!("refs/heads/{MAIN}")) => Run::ok(EXPECTED),
            line if line.contains("rev-parse") => Run::ok(LINEARIZED),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        });

        assert_eq!(
            merge(&shell, ORIGIN, DAY, &DayCoverage::green()).expect("a replica fault is best-effort"),
            LINEARIZED
        );
        assert!(
            shell.calls().iter().any(|line| line.contains("update-ref")),
            "the advance already landed: {:?}",
            shell.calls()
        );
    }

    #[test]
    fn a_day_with_merge_commits_is_linearized_before_the_swap() {
        let shell = Fake::new(|line| match line {
            line if line.contains("rev-list") && line.contains("bloomery/daily") => Run::ok("cafemerge"),
            line if line.contains("rev-list") => Run::ok(""),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("tree-day"),
            line if line.contains("rev-parse") && line.contains(&format!("refs/heads/{MAIN}")) => Run::ok(EXPECTED),
            line if line.contains("rev-parse") => Run::ok(LINEARIZED),
            _ => Run::ok(""),
        });

        merge(&shell, ORIGIN, DAY, &DayCoverage::green()).expect("a linearized day advances");

        let calls = shell.calls();
        let rebase = calls
            .iter()
            .position(|line| line.contains(" rebase") && !line.contains("--abort"))
            .expect("the day is rebased onto fleet main");
        let cas = calls.iter().position(|line| line.contains("update-ref")).expect("the linearized image is swapped");
        assert!(rebase < cas, "the image is built before the compare-and-swap: {calls:?}");
        assert!(
            calls.iter().any(|line| line.contains("worktree add") && line.contains(DAY)),
            "the worktree starts at the day, not a GitHub fetch: {calls:?}"
        );
        assert!(
            !calls.iter().any(|line| line.contains("--rebase-merges")),
            "merge commits are dropped, not replayed: {calls:?}"
        );
        assert!(
            calls.iter().any(|line| line.contains(&format!("-B {SYNC}"))),
            "the linearized image is a local sync branch: {calls:?}"
        );
    }

    #[test]
    fn a_linearization_conflict_refuses_with_the_trees_named() {
        let shell = Fake::new(|line| match line {
            line if line.contains(" rebase") && !line.contains("--abort") => Run::failed("could not apply"),
            line if line.contains("rev-parse") && line.contains("bloomery/daily") => Run::ok("day-tree"),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("main-tree"),
            line if line.contains("rev-parse") => Run::ok(EXPECTED),
            line if line.contains("rev-list") => Run::ok("cafemerge"),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, ORIGIN, DAY, &DayCoverage::green())
            .expect_err("a conflicted linearization is refused")
            .to_string();

        assert!(refusal.contains("day-tree"), "the day tree is named: {refusal}");
        assert!(refusal.contains("main-tree"), "the main tree is named: {refusal}");
        assert!(refusal.contains("conflicted"), "the refusal names the conflict: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.contains("update-ref")), "{:?}", shell.calls());
        assert!(!shell.calls().iter().any(|line| line.starts_with("git push")), "{:?}", shell.calls());
    }

    #[test]
    fn a_tree_mismatch_refuses_with_the_trees_named() {
        let shell = Fake::new(|line| match line {
            line if line.contains("rev-parse") && line.contains("bloomery/daily") && line.contains("^{tree}") => {
                Run::ok("day-tree")
            }
            line if line.contains("rev-parse") && line.contains("bloomery/sync") => Run::ok("sync-tree"),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("main-tree"),
            line if line.contains("rev-parse") => Run::ok(EXPECTED),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, ORIGIN, DAY, &DayCoverage::green())
            .expect_err("a drifted linearization is refused")
            .to_string();

        assert!(refusal.contains("day-tree"), "the day tree is named: {refusal}");
        assert!(refusal.contains("sync-tree"), "the linearized tree is named: {refusal}");
        assert!(refusal.contains("not byte-identical"), "the refusal names the mismatch: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.contains("update-ref")), "{:?}", shell.calls());
    }

    #[test]
    fn a_range_that_still_contains_a_merge_is_not_advanced() {
        let shell = Fake::new(|line| match line {
            line if line.contains("rev-list") && line.contains("bloomery/sync") => Run::ok("stillamerge"),
            line if line.contains("rev-list") => Run::ok("cafemerge"),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("tree-day"),
            line if line.contains("rev-parse") && line.contains(&format!("refs/heads/{MAIN}")) => Run::ok(EXPECTED),
            line if line.contains("rev-parse") => Run::ok(LINEARIZED),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, ORIGIN, DAY, &DayCoverage::green())
            .expect_err("a merge still on the image is refused")
            .to_string();

        assert!(refusal.contains("still contains a merge commit"), "the leftover merge is named: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.contains("update-ref")), "{:?}", shell.calls());
    }

    #[test]
    fn a_lost_compare_is_a_nonzero_refusal() {
        let shell = Fake::new(|line| match line {
            line if line.contains("update-ref") => Run::failed("fatal: cannot lock ref"),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("tree-day"),
            line if line.contains("rev-parse") && line.contains(&format!("refs/heads/{MAIN}")) => Run::ok(EXPECTED),
            line if line.contains("rev-parse") => Run::ok(LINEARIZED),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, ORIGIN, DAY, &DayCoverage::green())
            .expect_err("a lost compare-and-swap is a refusal")
            .to_string();

        assert!(refusal.contains("update-ref"), "the refusal names the compare: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.starts_with("git push")), "a lost swap does not replicate");
    }

    #[test]
    fn the_sync_branch_is_the_day_date_under_the_sync_prefix() {
        assert_eq!(sync_branch(DAY), SYNC);
        assert_eq!(sync_branch("bloomery/daily/2026-08-15"), "bloomery/sync/2026-08-15");
    }
}
