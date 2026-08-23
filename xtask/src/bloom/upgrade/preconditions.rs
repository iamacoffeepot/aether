//! The screen that makes a refused upgrade a no-op.
//!
//! Every check runs and every failure is named, rather than stopping at the
//! first: an operator who drains the train, re-runs, and is only then told the
//! candidate path is wrong has been handed a drip-feed. All of it happens
//! before the store is copied or the running binary is touched, so a refusal
//! costs nothing to recover from — the alternative is a coordinator that
//! restarted onto a binary nobody has fold-tested. The candidate's `--doctor`
//! runs here under the live unit's `PATH` so a missing kit tool is the same
//! kind of no-op as a missing binary.

use aether_bloomery::BloomStatus;
use anyhow::{Result, bail};

use super::paths::Paths;
use super::shell::Shell;
use crate::bloom::dto::ViewDocument;

/// Refuse unless every named precondition of the upgrade holds.
pub fn screen(view: &ViewDocument, shell: &impl Shell, paths: &Paths, log: &mut String) -> Result<()> {
    let mut refusals: Vec<String> =
        [undrained(view), candidate_missing(paths), supervisor_unreachable(shell, &paths.unit)?]
            .into_iter()
            .flatten()
            .collect();
    refusals.extend(doctor(shell, paths)?);
    if !refusals.is_empty() {
        bail!("refusing to upgrade:\n  - {}", refusals.join("\n  - "));
    }

    super::checked(log, "no undrained blooms");
    super::checked(log, &format!("candidate {} exists", paths.candidate.display()));
    super::checked(log, &format!("supervisor unit {} is reachable", paths.unit));
    super::checked(log, "candidate doctor passed on service PATH");
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

/// The candidate doctor under the live unit PATH. Missing pid, missing
/// environment, and a red doctor are collected as refusals; a missing
/// candidate is already named and is not also a spawn failure.
fn doctor(shell: &impl Shell, paths: &Paths) -> Result<Vec<String>> {
    let mut refusals = Vec::new();
    let Some(pid) = live_pid(shell, &paths.unit)? else {
        refusals.push(format!("supervisor unit {} has no MainPID", paths.unit));
        return Ok(refusals);
    };
    let service_path = match paths.process_path(&pid) {
        Ok(path) => path,
        Err(error) => {
            refusals.push(error.to_string());
            return Ok(refusals);
        }
    };
    if paths.candidate.is_file()
        && let Some(refusal) = doctor_failed(shell, paths, &service_path)?
    {
        refusals.push(refusal);
    }
    Ok(refusals)
}

fn live_pid(shell: &impl Shell, unit: &str) -> Result<Option<String>> {
    let run = shell.capture("systemctl", &["--user", "show", "--property=MainPID", "--value", unit])?;
    let pid = run.stdout.trim();
    Ok((run.success && is_live_pid(pid)).then(|| pid.to_owned()))
}

fn is_live_pid(pid: &str) -> bool {
    !pid.is_empty() && pid != "0" && pid.bytes().all(|b| b.is_ascii_digit())
}

fn doctor_failed(shell: &impl Shell, paths: &Paths, service_path: &str) -> Result<Option<String>> {
    let Some(candidate) = paths.candidate.to_str() else {
        return Ok(Some(format!("candidate {} is not UTF-8", paths.candidate.display())));
    };
    let run = shell.capture_with_env(candidate, &["--doctor"], &[("PATH", service_path)])?;
    if run.success {
        return Ok(None);
    }
    let report = if run.stdout.is_empty() {
        &run.stderr
    } else {
        &run.stdout
    };
    Ok(Some(format!("candidate doctor failed on service PATH: {report}")))
}

#[cfg(test)]
mod tests {
    use aether_bloomery::BloomStatus;

    use std::fs;

    use super::screen;
    use crate::bloom::dto::{BloomView, DigestHex, MemberView, ViewDocument};
    use crate::bloom::upgrade::paths::Paths;
    use crate::bloom::upgrade::shell::Run;
    use crate::bloom::upgrade::shell::fake::Fake;
    use crate::bloom::upgrade::tests_support::{drained_view, install_service_environ, test_paths, unique_temp};

    const SERVICE_PATH: &str = "/service/bin";
    const SERVICE_PID: &str = "4321";
    const DOCTOR_MISSING: &str = "jscpd            MISSING  npm install -g jscpd";

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

    fn reachable() -> Fake<'static> {
        Fake::new(|line| match line {
            line if line.contains("LoadState") => Run::ok("loaded"),
            line if line.contains("MainPID") => Run::ok(SERVICE_PID),
            line if line.contains("--doctor") => Run::ok("lane host kit is complete"),
            _ => Run::ok(""),
        })
    }

    fn present_paths() -> Paths {
        let mut paths = test_paths();
        let candidate = unique_temp("aether-xtask-upgrade-present");
        fs::write(&candidate, b"candidate").expect("write a present candidate");
        paths.candidate = candidate;
        install_service_environ(&mut paths.proc_exe, SERVICE_PID, SERVICE_PATH);
        paths
    }

    #[test]
    fn a_drained_day_with_a_present_candidate_and_a_loaded_unit_passes() {
        let paths = present_paths();
        let shell = reachable();
        let mut log = String::new();
        screen(&drained_view(), &shell, &paths, &mut log).expect("a drained upgrade screens");
        assert!(log.contains("no undrained blooms"), "a pass names what it checked: {log}");
        assert!(log.contains("exists"), "the candidate is named: {log}");
        assert!(log.contains("reachable"), "the unit is named: {log}");
        assert!(log.contains("candidate doctor passed on service PATH"), "the doctor is a named check: {log}");
        assert_eq!(
            shell.overlays(),
            vec![vec![("PATH".to_owned(), SERVICE_PATH.to_owned())]],
            "the doctor received the service PATH and nothing else: {:?}",
            shell.overlays()
        );
        let _ = fs::remove_file(&paths.candidate);
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
        assert!(refusal.contains("no MainPID"), "a missing pid is named with the rest: {refusal}");
        assert!(log.is_empty(), "a refusal does not claim the screen passed: {log}");
    }

    // Tripwire: the doctor's own install lines are what the operator needs.
    // Wrapping them as a generic "doctor failed" without the tool name is
    // how a missing jscpd becomes another upgrade to diagnose.
    #[test]
    fn a_red_doctor_is_named_with_its_actionable_output() {
        let paths = present_paths();
        let shell = Fake::new(|line| match line {
            line if line.contains("LoadState") => Run::ok("loaded"),
            line if line.contains("MainPID") => Run::ok(SERVICE_PID),
            line if line.contains("--doctor") => Run::failed(DOCTOR_MISSING),
            _ => Run::ok(""),
        });
        let mut log = String::new();

        let refusal = screen(&drained_view(), &shell, &paths, &mut log).expect_err("a red doctor refuses").to_string();

        assert!(refusal.contains("candidate doctor failed on service PATH"), "the doctor is named: {refusal}");
        assert!(refusal.contains("jscpd") && refusal.contains("MISSING"), "the kit line is forwarded: {refusal}");
        assert!(log.is_empty(), "a refusal does not claim the screen passed: {log}");
        let _ = fs::remove_file(&paths.candidate);
    }

    // Tripwire: a red doctor is one more refusal on the same screen, not a
    // second upgrade after the operator drained. Reporting only undrained
    // would drip-feed the kit miss the way the old screen drip-fed the
    // missing candidate.
    #[test]
    fn undrained_and_a_red_doctor_are_named_together() {
        let paths = present_paths();
        let shell = Fake::new(|line| match line {
            line if line.contains("LoadState") => Run::ok("loaded"),
            line if line.contains("MainPID") => Run::ok(SERVICE_PID),
            line if line.contains("--doctor") => Run::failed(DOCTOR_MISSING),
            _ => Run::ok(""),
        });
        let mut log = String::new();

        let refusal = screen(&view(&[BloomStatus::Sealed]), &shell, &paths, &mut log)
            .expect_err("undrained plus red doctor is refused")
            .to_string();

        assert!(refusal.contains("1 bloom(s) undrained"), "undrained blooms are named: {refusal}");
        assert!(refusal.contains("jscpd"), "the doctor output is named with them: {refusal}");
        assert!(log.is_empty(), "a refusal does not claim the screen passed: {log}");
        let _ = fs::remove_file(&paths.candidate);
    }

    #[test]
    fn a_missing_process_environment_is_named() {
        let mut paths = present_paths();
        paths.proc_exe = format!("{}/$pid/exe", unique_temp("aether-xtask-upgrade-no-environ").display());
        let mut log = String::new();

        let refusal =
            screen(&drained_view(), &reachable(), &paths, &mut log).expect_err("a missing environ refuses").to_string();

        assert!(refusal.contains("missing"), "the missing process state is named: {refusal}");
        assert!(log.is_empty(), "a refusal does not claim the screen passed: {log}");
        let _ = fs::remove_file(&paths.candidate);
    }
}
