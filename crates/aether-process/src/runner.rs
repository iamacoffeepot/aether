//! The one-shot spawn-and-capture loop (ADR-0157 §Dispatch). The single
//! reviewed implementation of the deadline / drain / reap discipline the
//! workspace previously hand-rolled per consumer.
//!
//! Extracted from `aether-anthropic/src/cli.rs` (stdin write + EOF, a
//! dedicated stdout-drain thread so a full pipe cannot stall the child,
//! a `try_wait` deadline poll, kill-and-reap on overrun) and
//! `xtask/src/transform.rs` (the reap-before-surface-error ordering — the
//! child is reaped on every path, including the wait-error path, so no
//! zombie is left behind). The group-reap escalation
//! (`setsid` at fork so a `killpg` takes down the whole process group,
//! SIGTERM → grace → SIGKILL) is lifted from
//! `aether-mcp/src/bin/aether-tunnel.rs`, so a grandchild holding a pipe
//! open cannot outlive the deadline.
//!
//! This module is the *general* loop — it owns a fully-configured
//! [`Command`] plus stdin bytes and a deadline, and returns a structured
//! [`RunOutcome`]. Allowlist resolution, environment construction, and
//! working-directory confinement are the caller's (the runtime half's)
//! job; keeping them out of here leaves the loop a pure, testable unit.

use std::io::{ErrorKind, Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How often the deadline loop polls `child.try_wait()`. Short enough
/// that a hung run is killed promptly after expiry without busy-waiting;
/// the value cli.rs proved.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long the group-reap waits between SIGTERM and the SIGKILL
/// escalation on a deadline overrun (the tunnel's grace window):
/// [`GROUP_TERM_GRACE_STEPS`] polls of [`GROUP_TERM_GRACE_STEP`].
const GROUP_TERM_GRACE_STEP: Duration = Duration::from_millis(20);
const GROUP_TERM_GRACE_STEPS: u32 = 50;

/// The structured result of running a [`Command`] to completion, timeout,
/// or failure. The runtime half maps this onto the wire
/// [`RunResult`](crate::RunResult): `Completed` → `Ok`, `TimedOut`
/// → `TimedOut`, and the two error arms → `ProcessError` variants.
#[derive(Debug)]
pub enum RunOutcome {
    /// The child reached exit. `exit_code` is `None` when it died by
    /// signal (`ExitStatus::code()` is `None`).
    Completed { exit_code: Option<i32>, stdout: Vec<u8>, stderr: Vec<u8> },
    /// The deadline fired; the child (and its process group) was killed
    /// and reaped. Carries the partial output drained before the kill.
    TimedOut { stdout: Vec<u8>, stderr: Vec<u8> },
    /// `Command::spawn` failed. `not_found` distinguishes the
    /// `ErrorKind::NotFound` path (the resolved path is not an executable
    /// file) from every other exec failure.
    SpawnFailed { not_found: bool, detail: String },
    /// The OS returned an error while waiting on the child. The child is
    /// reaped before this is surfaced.
    WaitFailed { detail: String },
}

/// Run `command` to completion under `timeout`, feeding `stdin` to the
/// child and capturing stdout + stderr.
///
/// The discipline, in one place:
/// - stdio is piped; `stdin` is written on its own thread and the pipe is
///   dropped (EOF) so a large payload cannot deadlock against a child
///   that has not started reading;
/// - stdout **and** stderr are drained on dedicated threads so neither a
///   full stdout nor a full stderr pipe can stall the child (and thus the
///   deadline poll);
/// - the loop polls `try_wait` every [`POLL_INTERVAL`] against the
///   deadline;
/// - on overrun the whole process group is killed
///   ([`group_kill_and_reap`]) and the reader threads are then joined for
///   the partial output (the group kill closes the pipes, so the joins
///   return rather than hanging on an orphaned grandchild — the reason
///   cli.rs could not join here);
/// - on a wait error the child is reaped before the error is surfaced
///   (the xtask ordering), so no zombie is left behind.
pub fn run_to_completion(mut command: Command, stdin: Vec<u8>, timeout: Duration) -> RunOutcome {
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    set_process_group(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return RunOutcome::SpawnFailed { not_found: e.kind() == ErrorKind::NotFound, detail: e.to_string() },
    };

    let writer = child.stdin.take().map(|mut sink| {
        // Infra thread inside the blocking dispatch worker — it touches no
        // mail and holds no chain; the settlement hold lives on the worker
        // itself.
        #[allow(clippy::disallowed_methods)]
        thread::spawn(move || {
            // A broken pipe (the child exited or does not read all of stdin)
            // is a normal outcome, not a failure — drop the sink to signal
            // EOF regardless.
            let _ = sink.write_all(&stdin);
        })
    });
    let stdout_reader = drain(child.stdout.take());
    let stderr_reader = drain(child.stderr.take());

    let started = Instant::now();
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break RunOutcome::Completed {
                    exit_code: status.code(),
                    stdout: join_drain(stdout_reader),
                    stderr: join_drain(stderr_reader),
                };
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    group_kill_and_reap(&mut child);
                    break RunOutcome::TimedOut {
                        stdout: join_drain(stdout_reader),
                        stderr: join_drain(stderr_reader),
                    };
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                // Reap before surfacing (the xtask discipline) so a wait
                // error never leaves a zombie.
                group_kill_and_reap(&mut child);
                drop(join_drain(stdout_reader));
                drop(join_drain(stderr_reader));
                break RunOutcome::WaitFailed { detail: e.to_string() };
            }
        }
    };

    // Join the stdin writer last: on every break above the child is either
    // exited or group-killed, so the write-end's read side is closed and
    // the writer thread has returned (it never outlives the deadline).
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    outcome
}

/// Spawn a thread that reads `stream` to EOF, returning its buffer. A
/// `None` stream (the pipe was not captured) yields an empty buffer.
fn drain<R: Read + Send + 'static>(stream: Option<R>) -> JoinHandle<Vec<u8>> {
    // Infra thread inside the blocking dispatch worker — no mail, no chain.
    #[allow(clippy::disallowed_methods)]
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stream) = stream {
            let _ = stream.read_to_end(&mut buf);
        }
        buf
    })
}

/// Join a drain thread, recovering its buffer (an empty buffer if the
/// thread panicked — a drain panic must not fail the run).
fn join_drain(handle: JoinHandle<Vec<u8>>) -> Vec<u8> {
    handle.join().unwrap_or_default()
}

/// Put the child in its own process group at fork so a deadline reap can
/// `killpg` the whole group. On unix this is `setsid(2)` in `pre_exec`;
/// on other platforms the child stands alone and a bare `kill` suffices,
/// so this is a no-op.
#[cfg(unix)]
fn set_process_group(command: &mut Command) {
    use std::io::Error;
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid(2)` is async-signal-safe and the only call made
    // between fork and exec. It moves the child into a fresh session +
    // process group so the reaper can `killpg` the group.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn set_process_group(_command: &mut Command) {}

/// Kill the child and reap it. On unix the whole process group is
/// signalled (SIGTERM, a grace window, then SIGKILL) so a grandchild the
/// permitted binary forked goes down with it; the child is its own group
/// leader (`setsid` at fork), so its pid is the group id. On other
/// platforms this is a bare kill + wait.
#[cfg(unix)]
fn group_kill_and_reap(child: &mut Child) {
    let Ok(pgid) = libc::pid_t::try_from(child.id()) else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    // SAFETY: a `killpg` against a group we created (the child is its own
    // group leader) is always safe; the result is ignored because the
    // child may already be gone.
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    for _ in 0..GROUP_TERM_GRACE_STEPS {
        if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
            let _ = child.wait();
            return;
        }
        thread::sleep(GROUP_TERM_GRACE_STEP);
    }
    // SAFETY: same as above — escalate to SIGKILL on the group.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(not(unix))]
fn group_kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(all(test, unix))]
mod tests {
    use super::{RunOutcome, run_to_completion};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{self, Command};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};
    use std::{env, fs};

    /// Monotonic suffix so concurrent test scripts never collide on one path.
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Write a `#!/bin/sh` script to a unique temp path, chmod +x, and
    /// return the path. The caller removes it.
    fn script(body: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = env::temp_dir();
        path.push(format!("aether-process-test-{}-{n}.sh", process::id()));
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write test script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod test script");
        path
    }

    /// A benign run reaches completion: `/bin/cat` echoes stdin to stdout
    /// and exits 0. Exercises the stdin-write-then-EOF path (cat blocks
    /// until EOF) and the completed capture.
    #[test]
    fn benign_run_captures_stdout_and_zero_exit() {
        let outcome = run_to_completion(Command::new("/bin/cat"), b"hello aether".to_vec(), Duration::from_secs(10));
        match outcome {
            RunOutcome::Completed { exit_code, stdout, stderr } => {
                assert_eq!(exit_code, Some(0));
                assert_eq!(stdout, b"hello aether");
                assert!(stderr.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// A non-zero exit is a *completed* run, not an error — the loop
    /// reports `Completed { exit_code: Some(non-zero) }`, leaving the
    /// judgment to the caller (ADR-0157: only inability to run/reap is an
    /// error).
    #[test]
    fn non_zero_exit_is_completed_not_error() {
        let path = script("exit 3");
        let outcome = run_to_completion(Command::new(&path), Vec::new(), Duration::from_secs(10));
        let _ = fs::remove_file(&path);
        match outcome {
            RunOutcome::Completed { exit_code, .. } => assert_eq!(exit_code, Some(3)),
            other => panic!("expected Completed with exit 3, got {other:?}"),
        }
    }

    /// A missing binary is `SpawnFailed { not_found: true }`, which the
    /// runtime half maps onto `ProcessError::BinaryNotFound`.
    #[test]
    fn missing_binary_is_spawn_failed_not_found() {
        let outcome =
            run_to_completion(Command::new("/nonexistent/aether-xyzzy-binary"), Vec::new(), Duration::from_secs(10));
        match outcome {
            RunOutcome::SpawnFailed { not_found, .. } => {
                assert!(not_found, "a missing path is the NotFound spawn path");
            }
            other => panic!("expected SpawnFailed not_found, got {other:?}"),
        }
    }

    /// A run that outlives its deadline is killed and reaped, and the
    /// partial stdout printed *before* the sleep is still captured — the
    /// group-reap improvement over cli.rs, which had to detach the reader
    /// (losing partial output) because it could not kill the group. The
    /// fast return also proves the child was reaped, not merely detached.
    #[test]
    fn timeout_kills_and_captures_partial_output() {
        // A comfortable 1s deadline so the shell reliably starts and prints
        // the line before the deadline fires, yet still far under the 30s
        // child sleep so the kill+reap is what ends the run.
        let path = script("echo partial-line\nsleep 30");
        let started = Instant::now();
        let outcome = run_to_completion(Command::new(&path), Vec::new(), Duration::from_secs(1));
        let elapsed = started.elapsed();
        let _ = fs::remove_file(&path);
        match outcome {
            RunOutcome::TimedOut { stdout, .. } => {
                assert_eq!(stdout, b"partial-line\n", "the line printed before the sleep is captured before the kill");
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }
        assert!(elapsed < Duration::from_secs(10), "the deadline + reap returned well under the 30s child sleep");
    }

    /// A permitted binary that forks a grandchild holding the stdout pipe
    /// open still returns at the deadline: the group kill (`killpg`) takes
    /// down the whole group so the reader-thread join sees EOF rather than
    /// blocking on the grandchild's open write-end. Without the group reap
    /// (a bare `child.kill()`) the surviving grandchild would keep the
    /// pipe open and the join would hang past the deadline.
    #[test]
    fn timeout_reaps_the_whole_process_group() {
        // The child backgrounds a grandchild that inherits stdout and
        // sleeps, then sleeps itself. Both outlive the deadline; the 800ms
        // window lets the grandchild reliably spawn (and inherit the pipe)
        // before the kill, so the group reap is genuinely exercised.
        let path = script("sleep 30 &\nsleep 30");
        let started = Instant::now();
        let outcome = run_to_completion(Command::new(&path), Vec::new(), Duration::from_millis(800));
        let elapsed = started.elapsed();
        let _ = fs::remove_file(&path);
        assert!(matches!(outcome, RunOutcome::TimedOut { .. }), "expected TimedOut, got {outcome:?}");
        assert!(
            elapsed < Duration::from_secs(10),
            "the group reap let the stdout-drain join return; a surviving grandchild would have hung it ({elapsed:?})",
        );
    }
}
