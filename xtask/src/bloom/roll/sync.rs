//! The day's sync-back: one integration pull request from the day branch onto
//! main, gated by the required checks and merged by rebase (ADR-0186).

use anyhow::{Context, Result, bail};

use super::MAIN;
use super::shell::{Shell, checked};

/// The body the sync-back pull request opens with. It says what the pull
/// request is for a human scanning the day's history, and why it is the one
/// pull request on this repository that does not squash.
const BODY: &str = "The day's landed blooms returning to main as one integration pull request (ADR-0186). \
                    Merged by rebase so each bloom's authored subject and closing lines land on main verbatim.";

/// Open or reuse the sync-back pull request, wait for its required checks, and
/// rebase-merge it. Returns the pull request number for the roll's log.
pub fn merge(shell: &impl Shell, from: &str) -> Result<String> {
    let number = open_or_reuse(shell, from)?;

    println!("waiting for the required checks on sync-back pull request #{number}");
    if !shell.stream("gh", &["pr", "checks", &number, "--required", "--watch", "--fail-fast"])? {
        bail!(
            "sync-back pull request #{number} is not green; {from} stays bloomery's mainline until a repair bloom \
             fixes it and the sync merges"
        );
    }

    // `--rebase`, never `--squash`: the carve-out ADR-0186 grants the sync pull
    // request is the whole reason each bloom's model-authored commit survives
    // onto main instead of arriving as one flattened blob.
    checked(shell, "gh", &["pr", "merge", &number, "--rebase"])?;
    println!("merged #{number} onto {MAIN} by rebase");
    Ok(number)
}

/// The day's pull request, opened if it is not already there.
///
/// Reused rather than re-opened, because the roll's own failure modes land
/// here: a red check run or an interrupted wait leaves an open pull request
/// that the re-run must merge, not duplicate.
fn open_or_reuse(shell: &impl Shell, from: &str) -> Result<String> {
    if let Some(number) = open_pull_request(shell, from)? {
        println!("reusing open sync-back pull request #{number}");
        return Ok(number);
    }

    let title = format!("chore(meta): sync {from} back to {MAIN}");
    checked(shell, "gh", &["pr", "create", "--base", MAIN, "--head", from, "--title", &title, "--body", BODY])?;
    open_pull_request(shell, from)?
        .with_context(|| format!("gh pr create left no open pull request from {from} onto {MAIN}"))
}

/// The number of the open pull request from `from` onto main, if there is one.
fn open_pull_request(shell: &impl Shell, from: &str) -> Result<Option<String>> {
    let listed = checked(
        shell,
        "gh",
        &["pr", "list", "--head", from, "--base", MAIN, "--state", "open", "--json", "number", "--jq", ".[0].number"],
    )?;
    Ok((!listed.is_empty()).then_some(listed))
}

#[cfg(test)]
mod tests {
    use super::merge;
    use crate::bloom::roll::shell::Run;
    use crate::bloom::roll::shell::fake::Fake;

    const DAY: &str = "bloomery/daily/2026-08-14";

    // Tripwire: the sync-back merges by rebase. A squash here — the merge method
    // every other pull request in this repository uses — flattens the day into
    // one commit and destroys each bloom's authored subject and `Closes` lines,
    // which is the whole reason the day is a branch rather than a queue.
    #[test]
    fn the_sync_back_merges_by_rebase_after_the_required_checks_pass() {
        let shell = Fake::new(|line| match line {
            line if line.starts_with("gh pr list") => Run::ok("4990"),
            _ => Run::ok(""),
        });

        assert_eq!(merge(&shell, DAY).expect("a green sync-back merges"), "4990");

        let calls = shell.calls();
        let checks = calls.iter().position(|line| line.starts_with("gh pr checks")).expect("the checks are awaited");
        let merged = calls.iter().position(|line| line.starts_with("gh pr merge")).expect("the pull request is merged");
        assert!(checks < merged, "the checks are awaited before the merge: {calls:?}");
        assert!(calls[merged].contains("--rebase"), "the sync-back merges by rebase: {}", calls[merged]);
        assert!(!calls.iter().any(|line| line.contains("--squash")), "nothing squashes the day: {calls:?}");
    }

    // Tripwire: a re-run after a red or interrupted check wait merges the pull
    // request that is already open. Opening a second one splits the day's
    // sync-back in two, and only one of them can carry the day.
    #[test]
    fn an_open_sync_back_is_reused_rather_than_duplicated() {
        let shell = Fake::new(|line| match line {
            line if line.starts_with("gh pr list") => Run::ok("4990"),
            _ => Run::ok(""),
        });

        merge(&shell, DAY).expect("a green sync-back merges");

        assert!(!shell.calls().iter().any(|line| line.starts_with("gh pr create")), "{:?}", shell.calls());
    }

    #[test]
    fn a_red_sync_back_refuses_instead_of_merging() {
        let shell = Fake::new(|line| match line {
            line if line.starts_with("gh pr list") => Run::ok("4990"),
            line if line.starts_with("gh pr checks") => Run::failed("2 required checks failed"),
            _ => Run::ok(""),
        });

        let refusal = merge(&shell, DAY).expect_err("a red sync-back is refused").to_string();

        assert!(refusal.contains("not green"), "the refusal names the red checks: {refusal}");
        assert!(refusal.contains(DAY), "the refusal names the branch that stays mainline: {refusal}");
        assert!(!shell.calls().iter().any(|line| line.starts_with("gh pr merge")), "{:?}", shell.calls());
    }
}
