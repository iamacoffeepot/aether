//! Install the candidate and restart through the supervisor.
//!
//! The running binary is copied beside itself before it is replaced, the
//! restart is a `systemctl --user restart` (never a raw kill), and the new
//! process is not trusted until its executable identity matches the installed
//! path and `/view` is serving again. A timeout while the unit is still up is
//! "not yet"; a unit that has gone `failed` is a failed restart.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::paths::Paths;
use super::shell::{Shell, checked};
use super::{Views, checked as note};
use crate::bloom::dto::ViewDocument;

/// Back up, install, restart, and wait for the new process to serve `/view`.
pub fn apply(
    views: &impl Views,
    shell: &impl Shell,
    paths: &Paths,
    prior: &ViewDocument,
    log: &mut String,
) -> Result<ViewDocument> {
    let bin = path_arg(&paths.bin)?;
    let candidate = path_arg(&paths.candidate)?;
    let backup = format!("{}{}", paths.bin.display(), paths.backup_suffix);

    checked(shell, "cp", &[bin, &backup])?;
    note(log, &format!("backed up {} to {backup}", paths.bin.display()));

    // A direct `cp` onto the running path is ETXTBSY on Linux. Stage beside
    // the binary (same filesystem) and rename over it so the live inode is
    // never opened for write.
    let staging = format!("{bin}.new");
    checked(shell, "cp", &[candidate, &staging])?;
    checked(shell, "mv", &["-f", &staging, bin])?;
    note(log, &format!("installed candidate at {}", paths.bin.display()));

    checked(shell, "systemctl", &["--user", "restart", &paths.unit])?;
    note(log, &format!("restarted unit {} through the supervisor", paths.unit));

    wait_for_observation(views, shell, paths, prior, log)
}

fn wait_for_observation(
    views: &impl Views,
    shell: &impl Shell,
    paths: &Paths,
    prior: &ViewDocument,
    log: &mut String,
) -> Result<ViewDocument> {
    let deadline = Instant::now() + Duration::from_millis(paths.observe_timeout_millis);
    loop {
        let state = active_state(shell, &paths.unit)?;
        if state == "failed" {
            bail!("refusing to upgrade: supervisor unit {} is failed; observation did not advance", paths.unit);
        }

        let timed_out = Instant::now() >= deadline;
        match (identity(shell, paths)?, views.live()) {
            (Some(exe), Ok(view)) if view.observed == prior.observed => {
                note(log, &format!("process executable is {exe}"));
                note(log, &format!("observation advanced (observed={} mainline={})", view.observed, view.mainline));
                return Ok(view);
            }
            (Some(_), Ok(_)) | (None, _) | (_, Err(_)) if timed_out => {
                bail!(
                    "timed out after {} millis waiting for /view observation to advance (not yet); \
                     unit {} is {state}, not failed",
                    paths.observe_timeout_millis,
                    paths.unit
                );
            }
            _ => shell.pause(paths.observe_poll_millis),
        }
    }
}

fn identity(shell: &impl Shell, paths: &Paths) -> Result<Option<String>> {
    let Ok(pid) = checked(shell, "systemctl", &["--user", "show", "--property=MainPID", "--value", &paths.unit]) else {
        return Ok(None);
    };
    if pid.is_empty() || pid == "0" {
        return Ok(None);
    }
    let exe_path = paths.proc_exe.replace("$pid", &pid);
    let Ok(exe) = checked(shell, "readlink", &["-f", &exe_path]) else {
        return Ok(None);
    };
    let observed = exe.trim().trim_end_matches(" (deleted)");
    // `readlink -f` the installed path too: the operator's `--bin` may be
    // relative or carry a `..`, and comparing that spelling to the process
    // image would refuse a restart that did land on the file we installed.
    let expected = checked(shell, "readlink", &["-f", path_arg(&paths.bin)?])?;
    if observed != expected.trim() {
        bail!("new process executable is {observed}, expected {expected}");
    }
    Ok(Some(observed.to_owned()))
}

fn active_state(shell: &impl Shell, unit: &str) -> Result<String> {
    let run = shell.capture("systemctl", &["--user", "show", "--property=ActiveState", "--value", unit])?;
    if !run.success && run.stdout.is_empty() {
        return Ok("unknown".to_owned());
    }
    Ok(run.stdout)
}

fn path_arg(path: &Path) -> Result<&str> {
    path.to_str().with_context(|| format!("{} is not UTF-8", path.display()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::apply;
    use crate::bloom::dto::DigestHex;
    use crate::bloom::upgrade::shell::Run;
    use crate::bloom::upgrade::shell::fake::Fake;
    use crate::bloom::upgrade::tests_support::{Scripted, drained_view, test_paths};

    fn live_views() -> Scripted {
        Scripted::matching()
    }

    fn installed() -> Fake<'static> {
        Fake::new(|line| match line {
            line if line.contains("ActiveState") => Run::ok("active"),
            line if line.contains("MainPID") => Run::ok("4321"),
            line if line.starts_with("readlink") && line.contains("/proc/") => Run::ok("/opt/bloomery"),
            line if line.starts_with("readlink") => Run::ok("/opt/bloomery"),
            _ => Run::ok(""),
        })
    }

    #[test]
    fn the_happy_path_backs_up_installs_and_restarts_through_the_supervisor() {
        let shell = installed();
        let mut log = String::new();
        apply(&live_views(), &shell, &test_paths(), &drained_view(), &mut log).expect("a matching identity deploys");

        let calls = shell.calls();
        assert!(
            calls.iter().any(|line| line == "cp /opt/bloomery /opt/bloomery.prev"),
            "the running binary is backed up beside itself: {calls:?}"
        );
        assert!(
            calls.iter().any(|line| line == "cp /opt/candidate /opt/bloomery.new"),
            "the candidate is staged beside the running path: {calls:?}"
        );
        assert!(
            calls.iter().any(|line| line == "mv -f /opt/bloomery.new /opt/bloomery"),
            "the staged candidate is renamed over the running path: {calls:?}"
        );
        assert!(
            !calls.iter().any(|line| line == "cp /opt/candidate /opt/bloomery"),
            "the running path is never opened for write: {calls:?}"
        );
        assert!(
            calls.iter().any(|line| line == "systemctl --user restart bloomery"),
            "the restart goes through the supervisor: {calls:?}"
        );
        assert!(log.contains("process executable is /opt/bloomery"), "identity is printed: {log}");
        assert!(log.contains("observation advanced"), "observation is printed: {log}");
    }

    // Tripwire: `cp` onto a running executable is ETXTBSY on Linux. Staging
    // beside the path and renaming over it is what lets the install succeed
    // while the coordinator is still the old inode; a direct overwrite is
    // how the happy path dies after the fold-test already passed.
    #[test]
    fn the_install_does_not_open_the_running_binary_for_write() {
        let shell = installed();
        let mut log = String::new();
        apply(&live_views(), &shell, &test_paths(), &drained_view(), &mut log).expect("a matching identity deploys");

        let calls = shell.calls();
        let overwrite = calls.iter().any(|line| line.starts_with("cp ") && line.ends_with(" /opt/bloomery"));
        assert!(!overwrite, "no cp targets the running path: {calls:?}");
        assert!(
            calls.iter().any(|line| line.starts_with("mv ") && line.ends_with(" /opt/bloomery")),
            "the running path is replaced by rename: {calls:?}"
        );
    }

    // Tripwire: a unit that came up `failed` is a broken restart, not a slow
    // observe tick. Calling that "not yet" would keep the operator waiting on
    // a coordinator that is not going to serve /view.
    #[test]
    fn a_failed_unit_is_failed_not_not_yet() {
        let shell = Fake::new(|line| match line {
            line if line.contains("ActiveState") => Run::ok("failed"),
            _ => Run::ok(""),
        });
        let mut log = String::new();

        let refusal = apply(&live_views(), &shell, &test_paths(), &drained_view(), &mut log)
            .expect_err("a failed unit is a failed restart")
            .to_string();

        assert!(refusal.contains("failed"), "the unit state is named: {refusal}");
        assert!(!refusal.contains("not yet"), "a failed unit is not a timeout: {refusal}");
    }

    // Tripwire: /view not answering yet while the unit is still active is the
    // boot window, not a crash. The timeout has to say "not yet" so an
    // operator does not treat a slow replay as a broken install.
    #[test]
    fn an_observation_timeout_says_not_yet_not_failed() {
        let shell = Fake::new(|line| match line {
            line if line.contains("ActiveState") => Run::ok("active"),
            line if line.contains("MainPID") => Run::ok("4321"),
            line if line.starts_with("readlink") => Run::ok("/opt/bloomery"),
            _ => Run::ok(""),
        });
        let views = Scripted::new(Err("connection refused".to_owned()), Ok(drained_view()));
        let mut log = String::new();

        let refusal = apply(&views, &shell, &test_paths(), &drained_view(), &mut log)
            .expect_err("a silent /view times out")
            .to_string();

        assert!(refusal.contains("not yet"), "the timeout is 'not yet': {refusal}");
        assert!(refusal.contains("not failed"), "the unit is distinguished from failed: {refusal}");
    }

    // Tripwire: a restart that came back as a different binary is the
    // unit launching BLOOMERY_BIN from a path we did not install. Treating
    // that as "not yet" would wait out the timeout on the old process.
    #[test]
    fn a_mismatched_executable_is_refused() {
        let shell = Fake::new(|line| match line {
            line if line.contains("ActiveState") => Run::ok("active"),
            line if line.contains("MainPID") => Run::ok("4321"),
            line if line.starts_with("readlink") && line.contains("/proc/") => Run::ok("/usr/wrong"),
            line if line.starts_with("readlink") => Run::ok("/opt/bloomery"),
            _ => Run::ok(""),
        });
        let mut log = String::new();

        let refusal = apply(&live_views(), &shell, &test_paths(), &drained_view(), &mut log)
            .expect_err("a wrong exe is refused")
            .to_string();

        assert!(refusal.contains("/usr/wrong"), "the observed exe is named: {refusal}");
        assert!(refusal.contains("/opt/bloomery"), "the expected exe is named: {refusal}");
    }

    // Tripwire: `--bin` is whatever the operator typed. Comparing that
    // spelling to `readlink -f /proc/$pid/exe` refuses a restart that did
    // land on the installed file whenever the flag was relative.
    #[test]
    fn a_relative_bin_flag_matches_the_canonical_process_image() {
        let mut paths = test_paths();
        paths.bin = PathBuf::from("bloomery");
        let shell = Fake::new(|line| match line {
            line if line.contains("ActiveState") => Run::ok("active"),
            line if line.contains("MainPID") => Run::ok("4321"),
            line if line.starts_with("readlink") && line.contains("/proc/") => Run::ok("/opt/bloomery"),
            line if line.starts_with("readlink") && line.contains("bloomery") => Run::ok("/opt/bloomery"),
            _ => Run::ok(""),
        });
        let mut log = String::new();

        apply(&live_views(), &shell, &paths, &drained_view(), &mut log).expect("canonical paths match");
        assert!(log.contains("process executable is /opt/bloomery"), "identity is the canonical path: {log}");
    }

    // Tripwire: a serving /view whose observed digest is not the pre-restart
    // one is replay still in flight, not a successful upgrade. Treating it as
    // advanced would print the check and return while the new process has not
    // folded the journal.
    #[test]
    fn a_stale_observed_digest_is_not_yet() {
        let mut empty = drained_view();
        empty.observed = DigestHex::from_bytes([0; 32]);
        let views = Scripted::new(Ok(empty), Ok(drained_view()));
        let mut log = String::new();

        let refusal = apply(&views, &installed(), &test_paths(), &drained_view(), &mut log)
            .expect_err("a pre-replay /view is not advanced")
            .to_string();

        assert!(refusal.contains("not yet"), "a stale observed is the boot window: {refusal}");
        assert!(!log.contains("observation advanced"), "the check is not printed early: {log}");
    }

    // Tripwire: /view can answer before replay finishes, with a default
    // observed digest. Calling that "advanced" would trust a coordinator
    // that has not yet folded the journal it was just installed to serve.
    #[test]
    fn observation_waits_for_the_pre_restart_digest() {
        let mut empty = drained_view();
        empty.observed = DigestHex::from_bytes([0; 32]);
        let views = Scripted::matching().first_live(empty);
        let mut paths = test_paths();
        paths.observe_timeout_millis = 5_000;
        let mut log = String::new();

        apply(&views, &installed(), &paths, &drained_view(), &mut log).expect("observation catches up");
        assert!(
            log.contains(&format!("observed={}", drained_view().observed)),
            "the pre-restart digest is what advanced: {log}"
        );
    }
}
