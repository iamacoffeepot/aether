//! The day's sync-back: a linear image of the day branch, a coverage-barred
//! compare-and-swap of fleet `refs/heads/main`, and a best-effort GitHub replica
//! push (ADR-0203).

use std::env;

use aether_bloomery_git::DayCoverage;
use anyhow::{Context, Result, bail};

use super::MAIN;
use super::replica;
use super::shell::{Repo, Shell};

/// Build the day's linear image on fleet main, refuse unless `coverage` is
/// fully green, compare-and-swap `refs/heads/main` onto that image, and push
/// main to GitHub best-effort. Returns the advanced main sha for the roll's log.
///
/// `replay` chooses how the image is built: constructed as one sync commit over
/// the day's tree (the default, which cannot conflict) or replayed commit by
/// commit onto main, which keeps the day's authored history and is only
/// available to a day with no folds for the replay to trip on.
pub fn merge(
    shell: &impl Shell,
    repo: &Repo,
    remote: &str,
    from: &str,
    coverage: &DayCoverage,
    replay: bool,
) -> Result<String> {
    if !coverage.is_fully_green() {
        bail!(
            "refusing to advance main: coverage map is not green ({}); {from} stays bloomery's mainline until the \
             map is fully green",
            coverage.hold_detail()
        );
    }

    let (linearized, expected) = if replay {
        replay_commits(shell, repo, from, &sync_branch(from))?
    } else {
        construct(shell, repo, from)?
    };

    advance(shell, repo, &linearized, &expected)?;
    println!("advanced {MAIN} to {linearized} under compare-and-swap");
    replica::push(shell, repo, remote, MAIN);
    Ok(linearized)
}

/// Build the day's image as one commit carrying the day's tree over current
/// fleet main. Returns that commit and the main sha the compare-and-swap must
/// still observe.
///
/// ADR-0203's barrier names three properties of what main receives — linear, no
/// merge commit, byte-identical to the day — and a commit whose tree *is* the
/// day's tree and whose only parent is current main has all three by
/// construction. Being a construction rather than a replay, it also cannot
/// conflict.
///
/// A replay cannot deliver them at all for a fold-bearing day. The day carries
/// bloom fold merges, and an early commit whose hunks a later one rewrote is not
/// "already upstream" by patch id, so `git rebase` stops on it and asks a human
/// to reconcile a hunk that the day itself already superseded. The 2026-08-21
/// day was 105 commits and conflicted at 42, then again past that. Keeping the
/// authored history is what [`replay_commits`] is for, and it is for days with
/// no folds in them.
fn construct(shell: &impl Shell, repo: &Repo, from: &str) -> Result<(String, String)> {
    let main = format!("refs/heads/{MAIN}");
    let day = format!("refs/heads/{from}");

    let day_tree = tree_of(shell, repo, &day)?;
    let expected = repo.checked(shell, &["rev-parse", &main])?;

    // A construction cannot conflict, which also means it cannot notice that
    // the day is behind main: writing the day's tree over a main the day does
    // not contain silently reverts whatever main gained in between. A replay
    // refused that by conflicting, so the ancestry it implied is asked for
    // outright here.
    if !repo.capture(shell, &["merge-base", "--is-ancestor", &main, &day])?.success {
        bail!(
            "refusing to advance main: {from} does not contain {MAIN} ({expected}), so syncing its tree would \
             revert what {MAIN} gained since the day was cut"
        );
    }

    let subject = format!("chore(meta): sync day {} back to main", day_date(from));
    let synced = repo.checked(shell, &["commit-tree", &day_tree, "-p", &expected, "-m", &subject])?;
    println!("built {synced} carrying {from}'s tree {day_tree} over {MAIN} {expected}");

    Ok((synced, expected))
}

/// Replay `from` onto current fleet main as `sync` in a temporary worktree —
/// the opt-in image that keeps the day's authored commits. Returns the
/// linearized commit and the main sha the compare-and-swap must still observe.
/// A conflict or a tree that is not byte-identical to the day is a refusal, and
/// fleet main is not touched.
fn replay_commits(shell: &impl Shell, repo: &Repo, from: &str, sync: &str) -> Result<(String, String)> {
    let main = format!("refs/heads/{MAIN}");
    let day = format!("refs/heads/{from}");

    let day_tree = tree_of(shell, repo, &day)?;
    let main_tree = tree_of(shell, repo, &main)?;
    let expected = repo.checked(shell, &["rev-parse", &main])?;
    let day_merges = repo.checked(shell, &["rev-list", "--merges", &format!("{main}..{day}")])?;
    if !day_merges.is_empty() {
        println!("{from} carries merge commits; replaying it linearly onto {MAIN} as {sync}");
    }

    let path = worktree_dir(from)?;
    remove_worktree(shell, repo, &path);
    repo.checked(shell, &["worktree", "add", "-B", sync, &path, &day])?;

    // The rebase belongs on the operator's terminal: a conflict is a refusal,
    // and the conflict markers are the diagnosis, not something this crate restates.
    if !shell.stream("git", &["-C", &path, "rebase", &main])? {
        let _ = shell.capture("git", &["-C", &path, "rebase", "--abort"]);
        remove_worktree(shell, repo, &path);
        bail!(
            "refusing to advance main: linearizing {from} (tree {day_tree}) onto {MAIN} (tree {main_tree}) \
             conflicted; drop --replay to sync the day's tree as one commit instead"
        );
    }

    let sync_tree = tree_of(shell, repo, sync)?;
    if sync_tree != day_tree {
        remove_worktree(shell, repo, &path);
        bail!("refusing to advance main: {sync} tree {sync_tree} is not byte-identical to {from} tree {day_tree}");
    }

    let remaining = repo.checked(shell, &["rev-list", "--merges", &format!("{main}..{sync}")])?;
    let linearized = repo.checked(shell, &["rev-parse", sync])?;
    remove_worktree(shell, repo, &path);
    if !remaining.is_empty() {
        bail!("refusing to advance main: {sync} still contains a merge commit");
    }

    Ok((linearized, expected))
}

/// Compare-and-swap fleet `refs/heads/main` to the linearized commit. A lost
/// compare is a refusal — the same discipline as a landing.
fn advance(shell: &impl Shell, repo: &Repo, linearized: &str, expected: &str) -> Result<()> {
    repo.checked(shell, &["update-ref", &format!("refs/heads/{MAIN}"), linearized, expected])?;
    Ok(())
}

fn tree_of(shell: &impl Shell, repo: &Repo, rev: &str) -> Result<String> {
    repo.checked(shell, &["rev-parse", &format!("{rev}^{{tree}}")])
}

/// The date the day branch is named for, which is its last path segment.
fn day_date(from: &str) -> &str {
    from.rsplit('/').next().unwrap_or(from)
}

fn sync_branch(from: &str) -> String {
    format!("bloomery/sync/{}", day_date(from))
}

fn worktree_dir(from: &str) -> Result<String> {
    let path = env::temp_dir().join(format!("bloomery-sync-{}", day_date(from)));
    path.to_str().map(str::to_owned).with_context(|| format!("worktree path {} is not utf-8", path.display()))
}

fn remove_worktree(shell: &impl Shell, repo: &Repo, path: &str) {
    let _ = repo.capture(shell, &["worktree", "remove", "--force", path]);
}

#[cfg(test)]
mod tests {
    use super::{MAIN, merge, sync_branch};
    use crate::bloom::roll::shell::Repo;
    use crate::bloom::roll::shell::Run;
    use crate::bloom::roll::shell::fake::Fake;
    use aether_bloomery_git::DayCoverage;

    const DAY: &str = "bloomery/daily/2026-08-14";
    const ORIGIN: &str = "origin";
    const SYNC: &str = "bloomery/sync/2026-08-14";
    const LINEARIZED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const EXPECTED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const REPO: &str = "/mnt/dev/bloomery/fleet.git";

    fn repo() -> Repo {
        Repo::new(REPO)
    }

    /// A repository where the day is ahead of main and every read answers.
    fn green_fleet() -> Fake<'static> {
        Fake::new(|line| match line {
            line if line.contains("commit-tree") => Run::ok(LINEARIZED),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("tree-day"),
            line if line.contains("rev-parse") && line.contains(&format!("refs/heads/{MAIN}")) => Run::ok(EXPECTED),
            line if line.contains("rev-parse") => Run::ok(LINEARIZED),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        })
    }

    #[test]
    fn a_green_map_compare_and_swaps_fleet_main_then_replicates() {
        let shell = green_fleet();

        assert_eq!(
            merge(&shell, &repo(), ORIGIN, DAY, &DayCoverage::green(), false).expect("a green coverage map advances"),
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
        let pushed = calls.iter().position(|line| line.contains(&format!("push {ORIGIN} {MAIN}")));
        assert!(pushed.is_some_and(|index| cas < index), "the replica push follows the advance: {calls:?}");
        assert!(!calls.iter().any(|line| line.starts_with("gh ")), "GitHub is not a gate: {calls:?}");
        assert!(!calls.iter().any(|line| line.contains("fetch")), "the advance does not fetch GitHub: {calls:?}");
    }

    // Tripwire: every `git` the sync-back issues is rooted at the fleet
    // repository. The refs it reads and writes — the day, fleet main, the sync
    // commit — exist there and nowhere else, so a call that inherits the cwd
    // dies on `unknown revision` the moment the roll is driven from a plain
    // clone instead of a fleet worktree.
    #[test]
    fn every_call_is_rooted_at_the_fleet_repository() {
        let shell = green_fleet();

        merge(&shell, &repo(), ORIGIN, DAY, &DayCoverage::green(), false).expect("a green coverage map advances");

        for call in shell.calls() {
            assert!(call.starts_with(&format!("git -C {REPO} ")), "the cwd is not what resolves this ref: {call}");
        }
    }

    #[test]
    fn a_fold_bearing_day_whose_replay_would_conflict_still_advances_main() {
        // #5414. The 2026-08-21 day was 105 commits carrying bloom fold merges.
        // Replayed commit by commit it conflicted at 42 and again past that: an
        // old commit whose hunks a later one rewrote is not "already upstream"
        // by patch id, so a fold-bearing day can never be replayed linearly.
        // Constructing the sync commit over the day's tree cannot conflict,
        // because there is nothing to apply.
        let shell = Fake::new(|line| match line {
            line if line.contains(" rebase") && !line.contains("--abort") => Run::failed("could not apply"),
            line if line.contains("commit-tree") => Run::ok(LINEARIZED),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("tree-day"),
            line if line.contains("rev-parse") && line.contains(&format!("refs/heads/{MAIN}")) => Run::ok(EXPECTED),
            line if line.contains("rev-parse") => Run::ok(LINEARIZED),
            line if line.contains("rev-list") => Run::ok("cafemerge"),
            _ => Run::ok(""),
        });

        assert_eq!(
            merge(&shell, &repo(), ORIGIN, DAY, &DayCoverage::green(), false)
                .expect("a fold-bearing day advances without a replay"),
            LINEARIZED
        );

        let calls = shell.calls();
        let built = calls.iter().find(|line| line.contains("commit-tree")).expect("the image is constructed");
        assert!(built.contains("tree-day"), "over the day's own tree: {built}");
        assert!(built.contains(&format!("-p {EXPECTED}")), "with current fleet main as its only parent: {built}");
        assert!(
            built.contains("chore(meta): sync day 2026-08-14 back to main"),
            "under the day's sync subject: {built}"
        );
        assert!(!calls.iter().any(|line| line.contains(" rebase")), "nothing is replayed, so nothing can conflict");
        assert!(!calls.iter().any(|line| line.contains("worktree")), "and no worktree is needed: {calls:?}");
    }

    // Tripwire: a construction writes the day's tree over main whatever main
    // holds, so the ancestry a replay used to prove by conflicting has to be
    // asked for. A day that does not contain main is a day whose tree would
    // revert main's newer commits without a single conflict marker.
    #[test]
    fn a_day_that_does_not_contain_main_is_refused() {
        let shell = Fake::new(|line| match line {
            line if line.contains("merge-base") => Run::failed(""),
            line if line.contains("commit-tree") => Run::ok(LINEARIZED),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("tree-day"),
            line if line.contains("rev-parse") => Run::ok(EXPECTED),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, &repo(), ORIGIN, DAY, &DayCoverage::green(), false)
            .expect_err("a day behind main is refused")
            .to_string();

        assert!(refusal.contains("does not contain"), "the refusal names the missing ancestry: {refusal}");
        assert!(refusal.contains(DAY) && refusal.contains(EXPECTED), "and both sides of it: {refusal}");
        let calls = shell.calls();
        assert!(!calls.iter().any(|line| line.contains("commit-tree")), "nothing is built: {calls:?}");
        assert!(!calls.iter().any(|line| line.contains("update-ref")), "and main is untouched: {calls:?}");
    }

    #[test]
    fn a_non_green_map_refuses_without_touching_main() {
        let shell = green_fleet();
        let coverage = DayCoverage::hold("issue-a\nissue-b");

        let refusal =
            merge(&shell, &repo(), ORIGIN, DAY, &coverage, false).expect_err("a held map is refused").to_string();

        assert!(refusal.contains("not green"), "the refusal names the coverage bar: {refusal}");
        assert!(
            refusal.contains("issue-a") && refusal.contains("issue-b"),
            "the refusal names every uncovered workpiece: {refusal}"
        );
        assert!(refusal.contains(DAY), "the refusal names the branch that stays mainline: {refusal}");
        let calls = shell.calls();
        assert!(calls.is_empty(), "a held roll is a no-op: {calls:?}");
    }

    #[test]
    fn a_replica_push_failure_does_not_unwind_the_advance() {
        let shell = Fake::new(|line| match line {
            line if line.contains(&format!("push {ORIGIN} {MAIN}")) => Run::failed("protected branch"),
            line if line.contains("commit-tree") => Run::ok(LINEARIZED),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("tree-day"),
            line if line.contains("rev-parse") && line.contains(&format!("refs/heads/{MAIN}")) => Run::ok(EXPECTED),
            line if line.contains("rev-parse") => Run::ok(LINEARIZED),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        });

        assert_eq!(
            merge(&shell, &repo(), ORIGIN, DAY, &DayCoverage::green(), false).expect("a replica fault is best-effort"),
            LINEARIZED
        );
        assert!(
            shell.calls().iter().any(|line| line.contains("update-ref")),
            "the advance already landed: {:?}",
            shell.calls()
        );
    }

    #[test]
    fn the_replay_image_is_a_local_sync_branch_rebased_onto_main() {
        let shell = Fake::new(|line| match line {
            line if line.contains("rev-list") && line.contains("bloomery/daily") => Run::ok("cafemerge"),
            line if line.contains("rev-list") => Run::ok(""),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("tree-day"),
            line if line.contains("rev-parse") && line.contains(&format!("refs/heads/{MAIN}")) => Run::ok(EXPECTED),
            line if line.contains("rev-parse") => Run::ok(LINEARIZED),
            _ => Run::ok(""),
        });

        merge(&shell, &repo(), ORIGIN, DAY, &DayCoverage::green(), true).expect("a replayed day advances");

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
        assert!(!calls.iter().any(|line| line.contains("commit-tree")), "and no tree is synthesised: {calls:?}");
    }

    #[test]
    fn a_replay_conflict_refuses_with_the_trees_named() {
        let shell = Fake::new(|line| match line {
            line if line.contains(" rebase") && !line.contains("--abort") => Run::failed("could not apply"),
            line if line.contains("rev-parse") && line.contains("bloomery/daily") => Run::ok("day-tree"),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("main-tree"),
            line if line.contains("rev-parse") => Run::ok(EXPECTED),
            line if line.contains("rev-list") => Run::ok("cafemerge"),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, &repo(), ORIGIN, DAY, &DayCoverage::green(), true)
            .expect_err("a conflicted replay is refused")
            .to_string();

        assert!(refusal.contains("day-tree"), "the day tree is named: {refusal}");
        assert!(refusal.contains("main-tree"), "the main tree is named: {refusal}");
        assert!(refusal.contains("conflicted"), "the refusal names the conflict: {refusal}");
        assert!(refusal.contains("--replay"), "and points at the image that cannot conflict: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.contains("update-ref")), "{:?}", shell.calls());
        assert!(!shell.calls().iter().any(|line| line.contains(" push ")), "{:?}", shell.calls());
    }

    #[test]
    fn a_replay_tree_mismatch_refuses_with_the_trees_named() {
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

        let refusal = merge(&shell, &repo(), ORIGIN, DAY, &DayCoverage::green(), true)
            .expect_err("a drifted replay is refused")
            .to_string();

        assert!(refusal.contains("day-tree"), "the day tree is named: {refusal}");
        assert!(refusal.contains("sync-tree"), "the linearized tree is named: {refusal}");
        assert!(refusal.contains("not byte-identical"), "the refusal names the mismatch: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.contains("update-ref")), "{:?}", shell.calls());
    }

    #[test]
    fn a_replayed_range_that_still_contains_a_merge_is_not_advanced() {
        let shell = Fake::new(|line| match line {
            line if line.contains("rev-list") && line.contains("bloomery/sync") => Run::ok("stillamerge"),
            line if line.contains("rev-list") => Run::ok("cafemerge"),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("tree-day"),
            line if line.contains("rev-parse") && line.contains(&format!("refs/heads/{MAIN}")) => Run::ok(EXPECTED),
            line if line.contains("rev-parse") => Run::ok(LINEARIZED),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, &repo(), ORIGIN, DAY, &DayCoverage::green(), true)
            .expect_err("a merge still on the image is refused")
            .to_string();

        assert!(refusal.contains("still contains a merge commit"), "the leftover merge is named: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.contains("update-ref")), "{:?}", shell.calls());
    }

    #[test]
    fn a_lost_compare_is_a_nonzero_refusal() {
        let shell = Fake::new(|line| match line {
            line if line.contains("update-ref") => Run::failed("fatal: cannot lock ref"),
            line if line.contains("commit-tree") => Run::ok(LINEARIZED),
            line if line.contains("rev-parse") && line.contains("^{tree}") => Run::ok("tree-day"),
            line if line.contains("rev-parse") && line.contains(&format!("refs/heads/{MAIN}")) => Run::ok(EXPECTED),
            line if line.contains("rev-parse") => Run::ok(LINEARIZED),
            line if line.contains("rev-list") => Run::ok(""),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, &repo(), ORIGIN, DAY, &DayCoverage::green(), false)
            .expect_err("a lost compare-and-swap is a refusal")
            .to_string();

        assert!(refusal.contains("update-ref"), "the refusal names the compare: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.contains(" push ")), "a lost swap does not replicate");
    }

    #[test]
    fn the_sync_branch_is_the_day_date_under_the_sync_prefix() {
        assert_eq!(sync_branch(DAY), SYNC);
        assert_eq!(sync_branch("bloomery/daily/2026-08-15"), "bloomery/sync/2026-08-15");
    }
}
