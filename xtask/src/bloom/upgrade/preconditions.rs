//! The screen that makes a refused upgrade a no-op.
//!
//! Every check runs and every failure is named, rather than stopping at the
//! first: an operator who drains the train, re-runs, and is only then told the
//! candidate path is wrong has been handed a drip-feed. All of it happens
//! before the store is copied or the running binary is touched, so a refusal
//! costs nothing to recover from — the alternative is a coordinator that
//! restarted onto a binary nobody has fold-tested.

use aether_bloomery::BloomStatus;
use anyhow::{Result, bail};

use super::paths::Paths;
use super::shell::Shell;
use crate::bloom::dto::ViewDocument;

/// Refuse unless every named precondition of the upgrade holds.
pub fn screen(view: &ViewDocument, shell: &impl Shell, paths: &Paths, log: &mut String) -> Result<()> {
    let refusals: Vec<String> =
        [undrained(view), candidate_missing(paths), supervisor_unreachable(shell, &paths.unit)?]
            .into_iter()
            .flatten()
            .collect();
    if !refusals.is_empty() {
        bail!("refusing to upgrade:\n  - {}", refusals.join("\n  - "));
    }

    super::checked(log, "no undrained blooms");
    super::checked(log, &format!("candidate {} exists", paths.candidate.display()));
    super::checked(log, &format!("supervisor unit {} is reachable", paths.unit));
    Ok(())
}

/// The blooms still in flight. An upgrade is a quiesce point: a bloom that has
/// not landed still needs the running coordinator, and replacing it mid-flight
/// is how a fold-test that would have refused is skipped because the process
/// is already gone.
fn undrained(view: &ViewDocument) -> Option<String> {
    let in_flight: Vec<String> = view
        .blooms
        .iter()
        .filter(|bloom| matches!(bloom.status, BloomStatus::Sealed | BloomStatus::Resolved))
        .map(|bloom| bloom.id.as_hex())
        .collect();
    (!in_flight.is_empty()).then(|| format!("{} bloom(s) undrained: {}", in_flight.len(), in_flight.join(" ")))
}

fn candidate_missing(paths: &Paths) -> Option<String> {
    (!paths.candidate.is_file()).then(|| format!("candidate {} does not exist", paths.candidate.display()))
}

fn supervisor_unreachable(shell: &impl Shell, unit: &str) -> Result<Option<String>> {
    let run = shell.capture("systemctl", &["--user", "show", "--property=LoadState", "--value", unit])?;
    let state = run.stdout.trim();
    Ok((!run.success || state != "loaded")
        .then(|| format!("supervisor unit {unit} is not reachable (LoadState={state})")))
}

#[cfg(test)]
mod tests {
    use aether_bloomery::BloomStatus;

    use std::fs;

    use super::screen;
    use crate::bloom::dto::{BloomView, DigestHex, MemberView, ViewDocument};
    use crate::bloom::upgrade::shell::Run;
    use crate::bloom::upgrade::shell::fake::Fake;
    use crate::bloom::upgrade::tests_support::{drained_view, test_paths, unique_temp};

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
                    }],
                })
                .collect(),
        }
    }

    fn reachable() -> Fake<'static> {
        Fake::new(|line| match line {
            line if line.starts_with("systemctl") => Run::ok("loaded"),
            _ => Run::ok(""),
        })
    }

    #[test]
    fn a_drained_day_with_a_present_candidate_and_a_loaded_unit_passes() {
        let mut paths = test_paths();
        let candidate = unique_temp("aether-xtask-upgrade-present");
        fs::write(&candidate, b"candidate").expect("write a present candidate");
        paths.candidate = candidate.clone();
        let mut log = String::new();
        screen(&drained_view(), &reachable(), &paths, &mut log).expect("a drained upgrade screens");
        assert!(log.contains("no undrained blooms"), "a pass names what it checked: {log}");
        assert!(log.contains("exists"), "the candidate is named: {log}");
        assert!(log.contains("reachable"), "the unit is named: {log}");
        let _ = fs::remove_file(&candidate);
    }

    // Tripwire: every precondition is reported from one screen, and the screen
    // is the whole of what a refusal costs. A first-failure return drip-feeds
    // an operator through three separate upgrades, and a check moved after the
    // install leaves the live coordinator on a binary nobody fold-tested.
    #[test]
    fn every_failing_precondition_is_named_at_once() {
        let mut paths = test_paths();
        paths.candidate = paths.scratch.join("missing-candidate");
        let shell = Fake::new(|line| match line {
            line if line.starts_with("systemctl") => Run::ok("not-found"),
            _ => Run::ok(""),
        });
        let mut log = String::new();

        let refusal = screen(&view(&[BloomStatus::Sealed, BloomStatus::Resolved]), &shell, &paths, &mut log)
            .expect_err("an undrained, missing-candidate, missing-unit upgrade is refused")
            .to_string();

        assert!(refusal.contains("2 bloom(s) undrained"), "undrained blooms are named: {refusal}");
        assert!(refusal.contains("does not exist"), "the missing candidate is named: {refusal}");
        assert!(
            refusal.contains("not reachable") && refusal.contains("not-found"),
            "the unreachable unit is named: {refusal}"
        );
        assert!(log.is_empty(), "a refusal does not claim the screen passed: {log}");
    }
}
