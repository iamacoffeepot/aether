//! The screen that makes a refused roll a no-op.
//!
//! Every check runs and every failure is named, rather than stopping at the
//! first: an operator who drains the train, re-runs, and is only then told the
//! working tree is dirty has been handed a drip-feed. All of it happens before
//! fleet main is compare-and-swapped, so a refusal costs nothing to recover
//! from — the alternative is a roll that advanced the day onto main and then
//! stopped short of cutting tomorrow.

use aether_bloomery::BloomStatus;
use aether_bloomery_git::command::{self, PORCELAIN_STATUS};
use anyhow::{Result, bail};

use super::MAIN;
use super::day::Day;
use super::shell::{Repo, Shell};
use crate::bloom::dto::ViewDocument;

/// Refuse unless every precondition of the day roll holds.
pub fn screen(view: &ViewDocument, shell: &impl Shell, repo: &Repo, day: &Day, remote: &str) -> Result<()> {
    if let Some(notice) = unwritable_replica(shell, repo, remote)? {
        println!("{notice}");
    }

    let refusals: Vec<String> = [undrained(view), dirty_tree(shell, repo)?, already_cut(shell, repo, day, remote)?]
        .into_iter()
        .flatten()
        .collect();
    if refusals.is_empty() {
        return Ok(());
    }
    bail!("refusing to roll:\n  - {}", refusals.join("\n  - "));
}

/// The blooms still in flight. The roll is a quiesce point: a bloom that has
/// not landed drains on the current day's branch, and the calendar waits for it
/// rather than orphaning it on a ref nothing observes any more.
fn undrained(view: &ViewDocument) -> Option<String> {
    let in_flight: Vec<String> = view
        .blooms
        .iter()
        .filter(|bloom| matches!(bloom.status, BloomStatus::Sealed | BloomStatus::Resolved))
        .map(|bloom| bloom.id.as_hex())
        .collect();
    (!in_flight.is_empty()).then(|| format!("{} bloom(s) undrained: {}", in_flight.len(), in_flight.join(" ")))
}

/// A dirty working tree, which the cut would carry into tomorrow's branch or
/// lose to a checkout.
fn dirty_tree(shell: &impl Shell, repo: &Repo) -> Result<Option<String>> {
    // A bare fleet repository has no working tree to be dirty, and every ref the
    // roll writes is written without one.
    if repo.checked(shell, &["rev-parse", "--is-bare-repository"])? == "true" {
        return Ok(None);
    }

    let porcelain = repo.checked(shell, PORCELAIN_STATUS)?;
    let entries = command::split_nul(&porcelain);
    Ok((!entries.is_empty()).then(|| format!("the working tree is dirty:\n      {}", entries.join("\n      "))))
}

/// Tomorrow's branch already on the remote, which means this day was rolled
/// already and a second roll would push onto someone else's cut.
fn already_cut(shell: &impl Shell, repo: &Repo, day: &Day, remote: &str) -> Result<Option<String>> {
    let branch = day.branch();
    let listed = repo.checked(shell, &["ls-remote", "--heads", remote, &branch])?;
    Ok((!listed.is_empty()).then(|| format!("{branch} already exists on {remote}")))
}

/// A replica that will not take a refspec push. This is a notice rather than a
/// refusal — ADR-0203 makes GitHub an output of the roll, never a gate — but an
/// operator should learn the mirror will lag here, before the advance runs,
/// rather than from a line in the middle of the log.
fn unwritable_replica(shell: &impl Shell, repo: &Repo, remote: &str) -> Result<Option<String>> {
    let run = repo.capture(shell, &["push", "--dry-run", remote, MAIN])?;
    Ok((!run.success).then(|| {
        let reason = if run.stderr.is_empty() {
            &run.stdout
        } else {
            &run.stderr
        };
        format!("notice: {remote} will not take a refspec push, so the replica lags this roll: {reason}")
    }))
}

#[cfg(test)]
mod tests {
    use aether_bloomery::BloomStatus;

    use super::screen;
    use crate::bloom::dto::{BloomView, DigestHex, MemberView, ViewDocument};
    use crate::bloom::roll::day::Day;
    use crate::bloom::roll::shell::Repo;
    use crate::bloom::roll::shell::Run;
    use crate::bloom::roll::shell::fake::Fake;

    fn view(statuses: &[BloomStatus]) -> ViewDocument {
        ViewDocument {
            mainline: DigestHex::from_bytes([1; 32]),
            observed: DigestHex::from_bytes([2; 32]),
            blooms: statuses
                .iter()
                .enumerate()
                .map(|(index, status)| BloomView {
                    id: DigestHex::from_bytes([u8::try_from(index).expect("few blooms"); 32]),
                    status: *status,
                    superseded_by: None,
                    members: vec![MemberView {
                        workpiece: format!("wp-{index}"),
                        scope_revision: DigestHex::from_bytes([7; 32]),
                        awaiting_surface: None,
                        withdrawn: None,
                        cursor: None,
                    }],
                })
                .collect(),
        }
    }

    fn day() -> Day {
        Day::parse("2026-08-15").expect("a well-formed day")
    }

    fn repo() -> Repo {
        Repo::new("/mnt/dev/bloomery/fleet.git")
    }

    #[test]
    fn a_drained_day_with_a_clean_tree_passes_the_screen() {
        let shell = Fake::new(|_| Run::ok(""));

        screen(&view(&[BloomStatus::Landed, BloomStatus::Superseded]), &shell, &repo(), &day(), "origin")
            .expect("a drained day rolls");
    }

    // Tripwire: every precondition is reported from one screen, and the screen
    // is the whole of what a refusal costs. A first-failure return drip-feeds an
    // operator through separate rolls, and a check moved after the advance
    // leaves the day on main with tomorrow uncut.
    #[test]
    fn every_failing_precondition_is_named_at_once() {
        let shell = Fake::new(|line| match line {
            line if line.contains(" status ") => Run::ok(" M crates/aether-bloomery/src/lib.rs"),
            line if line.contains(" ls-remote ") => Run::ok("cafe\trefs/heads/bloomery/daily/2026-08-15"),
            _ => Run::ok(""),
        });

        let refusal = screen(&view(&[BloomStatus::Sealed, BloomStatus::Resolved]), &shell, &repo(), &day(), "origin")
            .expect_err("an undrained, dirty, already-cut day is refused")
            .to_string();

        assert!(refusal.contains("2 bloom(s) undrained"), "undrained blooms are named: {refusal}");
        assert!(refusal.contains("working tree is dirty"), "the dirty tree is named: {refusal}");
        assert!(refusal.contains("already exists on origin"), "the existing cut is named: {refusal}");
        assert!(!refusal.contains("allow_rebase_merge"), "GitHub merge settings are not a roll gate: {refusal}");
    }
}
