//! The external programs the upgrade drives.
//!
//! `systemctl`, `cp`, and `readlink` sit behind one seam, following
//! the roll's `Shell` (and the broader xtask precedent of shelling to `git`
//! rather than linking an implementation). The seam is what lets the refusal
//! branches — a reshape that aborts replay, a store copy that dropped its WAL,
//! a supervisor that is not there — be exercised without a unit, a journal, or
//! a candidate binary. `launch` is the extra verb the fold-test needs: the
//! candidate is a server, so the upgrade has to start it, read `/view`, and
//! stop it, rather than wait for it to exit.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// What one captured invocation produced.
#[derive(Clone, Debug)]
pub struct Run {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// A child the upgrade started and still owns.
pub trait Session {
    /// `None` while the child is still running.
    fn try_wait(&mut self) -> Result<Option<Run>>;

    /// SIGTERM/kill, then collect what it printed.
    fn terminate(&mut self) -> Result<Run>;
}

/// What the upgrade asks of the host.
pub trait Shell {
    /// Run and capture, for anything the upgrade reads back.
    fn capture(&self, program: &str, args: &[&str]) -> Result<Run>;

    /// Run and capture with a narrow environment overlay.
    ///
    /// Each pair replaces that variable for the child and clears the rest, so
    /// the overlay is the only environment the child sees. Callers pass `PATH`
    /// and nothing else — that is how `--doctor` receives the service path
    /// without inheriting the operator shell or forwarding unrelated values.
    fn capture_with_env(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<Run>;

    /// Start a process the caller will poll and then stop.
    ///
    /// `stderr_log` receives the child's stderr. Piping it back through this
    /// process would stall a candidate whose boot log fills the pipe buffer
    /// before `/view` binds; a file has no such ceiling, and is what
    /// [`Session::try_wait`] reads when the child exits so a decode abort
    /// still names the error.
    fn launch(&self, program: &str, args: &[&str], env: &[(&str, &str)], stderr_log: &Path)
    -> Result<Box<dyn Session>>;

    /// Yield for a poll interval. A zero duration is a no-op so tests that
    /// set the timeouts to zero never sleep.
    fn pause(&self, millis: u64) {
        if millis > 0 {
            thread::sleep(Duration::from_millis(millis));
        }
    }
}

/// The real programs on this host.
pub struct Host;

impl Shell for Host {
    fn capture(&self, program: &str, args: &[&str]) -> Result<Run> {
        run_command(program, args, &[])
    }

    fn capture_with_env(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<Run> {
        run_command(program, args, env)
    }

    fn launch(
        &self,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        stderr_log: &Path,
    ) -> Result<Box<dyn Session>> {
        let stderr = fs::File::create(stderr_log)
            .with_context(|| format!("create fold-test stderr log {}", stderr_log.display()))?;
        let mut command = Command::new(program);
        command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr);
        for (key, value) in env {
            command.env(key, value);
        }
        let child = command.spawn().with_context(|| format!("launch {}", rendered(program, args)))?;
        Ok(Box::new(HostSession { child, stderr_log: stderr_log.to_path_buf() }))
    }
}

struct HostSession {
    child: Child,
    stderr_log: PathBuf,
}

impl HostSession {
    fn finish(&mut self, status: ExitStatus) -> Result<Run> {
        let stderr = fs::read(&self.stderr_log)
            .with_context(|| format!("read fold-test stderr log {}", self.stderr_log.display()))?;
        Ok(into_run(status, &[], &stderr))
    }
}

impl Session for HostSession {
    fn try_wait(&mut self) -> Result<Option<Run>> {
        match self.child.try_wait().context("poll launched process")? {
            None => Ok(None),
            Some(status) => Ok(Some(self.finish(status)?)),
        }
    }

    fn terminate(&mut self) -> Result<Run> {
        let _ = self.child.kill();
        let status = self.child.wait().context("wait for killed process")?;
        self.finish(status)
    }
}

/// Run, and fail with the program's own diagnosis when it exits non-zero.
pub fn checked(shell: &impl Shell, program: &str, args: &[&str]) -> Result<String> {
    let run = shell.capture(program, args)?;
    if !run.success {
        let reason = if run.stderr.is_empty() {
            &run.stdout
        } else {
            &run.stderr
        };
        bail!("{} failed: {reason}", rendered(program, args));
    }
    Ok(run.stdout)
}

/// One invocation as an operator would have typed it.
pub fn rendered(program: &str, args: &[&str]) -> String {
    format!("{program} {}", args.join(" "))
}

fn run_command(program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<Run> {
    let mut command = Command::new(program);
    command.args(args);
    if !env.is_empty() {
        command.env_clear();
        for (key, value) in env {
            command.env(key, value);
        }
    }
    let output = command.output().with_context(|| format!("spawn {}", rendered(program, args)))?;
    Ok(into_run(output.status, &output.stdout, &output.stderr))
}

fn into_run(status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> Run {
    Run {
        success: status.success(),
        stdout: String::from_utf8_lossy(stdout).trim().to_owned(),
        stderr: String::from_utf8_lossy(stderr).trim().to_owned(),
    }
}

#[cfg(test)]
pub(super) mod fake {
    use std::cell::RefCell;
    use std::path::Path;

    use anyhow::Result;

    use super::{Run, Session, Shell, rendered};

    impl Run {
        /// A clean exit carrying `stdout`.
        pub(in crate::bloom::upgrade) fn ok(stdout: &str) -> Self {
            Self { success: true, stdout: stdout.to_owned(), stderr: String::new() }
        }

        /// A non-zero exit carrying `stderr`.
        pub(in crate::bloom::upgrade) fn failed(stderr: &str) -> Self {
            Self { success: false, stdout: String::new(), stderr: stderr.to_owned() }
        }
    }

    /// A shell that answers from a closure over the rendered command line and
    /// records every invocation in order.
    pub(in crate::bloom::upgrade) struct Fake<'a> {
        reply: Box<dyn Fn(&str) -> Run + 'a>,
        calls: RefCell<Vec<String>>,
        overlays: RefCell<Vec<Vec<(String, String)>>>,
    }

    impl<'a> Fake<'a> {
        pub(in crate::bloom::upgrade) fn new(reply: impl Fn(&str) -> Run + 'a) -> Self {
            Self { reply: Box::new(reply), calls: RefCell::new(Vec::new()), overlays: RefCell::new(Vec::new()) }
        }

        pub(in crate::bloom::upgrade) fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        /// Overlays passed to [`Shell::capture_with_env`], for assertions only.
        pub(in crate::bloom::upgrade) fn overlays(&self) -> Vec<Vec<(String, String)>> {
            self.overlays.borrow().clone()
        }

        fn answer(&self, program: &str, args: &[&str]) -> Run {
            let line = rendered(program, args);
            let reply = (self.reply)(&line);
            self.calls.borrow_mut().push(line);
            reply
        }
    }

    impl Shell for Fake<'_> {
        fn capture(&self, program: &str, args: &[&str]) -> Result<Run> {
            Ok(self.answer(program, args))
        }

        fn capture_with_env(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<Run> {
            self.overlays
                .borrow_mut()
                .push(env.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect());
            self.capture(program, args)
        }

        fn launch(
            &self,
            program: &str,
            args: &[&str],
            _env: &[(&str, &str)],
            _stderr_log: &Path,
        ) -> Result<Box<dyn Session>> {
            let run = self.answer(program, args);
            Ok(Box::new(FakeSession { exit: (!run.success).then_some(run) }))
        }
    }

    struct FakeSession {
        exit: Option<Run>,
    }

    impl Session for FakeSession {
        fn try_wait(&mut self) -> Result<Option<Run>> {
            Ok(self.exit.clone())
        }

        fn terminate(&mut self) -> Result<Run> {
            Ok(self.exit.take().unwrap_or_else(|| Run::ok("")))
        }
    }
}

#[cfg(test)]
mod host_tests {
    use std::fs;
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{Host, Shell};
    use crate::bloom::upgrade::tests_support::unique_temp;

    fn scratch() -> PathBuf {
        let dir = unique_temp("aether-xtask-upgrade-host");
        fs::create_dir_all(&dir).expect("host-test scratch");
        dir
    }

    // Tripwire: a fold-test child writes its boot log to stderr. Piping that
    // back into this process and reading it only after exit fills the pipe
    // buffer (~64 KiB) and stalls the candidate before /view binds — the
    // fold-test then times out on a binary that would have served.
    #[test]
    fn a_launched_child_is_not_blocked_by_an_unread_pipe() {
        let dir = scratch();
        let sentinel = dir.join("done");
        let stderr_log = dir.join("fold.stderr");
        let script = format!("head -c 262144 /dev/zero >&2; echo done > {}", sentinel.display());
        let mut session = Host.launch("sh", &["-c", &script], &[], &stderr_log).expect("launch a chatty child");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !sentinel.exists() && Instant::now() < deadline {
            assert!(session.try_wait().expect("poll").is_none(), "the child is still writing");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(sentinel.exists(), "the child finished writing rather than blocking on a full pipe");
        let _ = session.terminate();
        let _ = fs::remove_dir_all(&dir);
    }

    // Tripwire: a reshape abort prints the decode error on stderr and exits.
    // Inheriting or discarding that stream would leave the refusal without
    // the error the operator has to see; the file is how try_wait still
    // names it after the child is gone.
    #[test]
    fn a_failed_child_surfaces_its_stderr() {
        let dir = scratch();
        let stderr_log = dir.join("fold.stderr");
        let mut session = Host
            .launch("sh", &["-c", "echo 'invalid bool/presence byte 11' >&2; exit 2"], &[], &stderr_log)
            .expect("launch a failing child");

        let deadline = Instant::now() + Duration::from_secs(5);
        let run = loop {
            if let Some(run) = session.try_wait().expect("poll") {
                break run;
            }
            assert!(Instant::now() < deadline, "the child did not exit");
            thread::sleep(Duration::from_millis(20));
        };
        assert!(!run.success, "a non-zero exit is a failed run");
        assert!(
            run.stderr.contains("invalid bool/presence byte 11"),
            "the decode error is what try_wait returns: {}",
            run.stderr
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // Tripwire: `--doctor` must see the service PATH, not the operator
    // shell's. Overlaying without clearing forwards HOME and the operator
    // PATH; the doctor then green-lights a host the unit cannot dispatch.
    #[test]
    fn capture_with_env_gives_the_child_only_the_overlay() {
        let run = Host
            .capture_with_env(
                "/bin/sh",
                &["-c", "printf %s \"$PATH\"; printf x%s \"$HOME\""],
                &[("PATH", "/only/service")],
            )
            .expect("overlay capture");
        assert!(run.success, "the child ran: {}", run.stderr);
        assert_eq!(run.stdout, "/only/servicex", "PATH is the overlay and unrelated values are not forwarded");
    }
}

#[cfg(test)]
mod fake_tests {
    use super::Run;
    use super::Shell;
    use super::fake::Fake;

    // Tripwire: the rendered call log is what every other test prints on
    // failure. Putting PATH in that line would dump the service path into
    // every assertion; overlays() is the only place the value is kept.
    #[test]
    fn fake_records_the_overlay_outside_the_call_log() {
        let fake = Fake::new(|_| Run::ok("ok"));
        fake.capture_with_env("/opt/candidate", &["--doctor"], &[("PATH", "/service/bin")]).expect("overlay capture");
        assert_eq!(fake.calls(), vec!["/opt/candidate --doctor".to_owned()]);
        assert_eq!(fake.overlays(), vec![vec![("PATH".to_owned(), "/service/bin".to_owned())]]);
    }
}
