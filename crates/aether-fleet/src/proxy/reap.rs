//! Group-scoped termination of the substrate child a proxy owns.
//!
//! A proxy that forked its substrate is the only owner of that process, so
//! every teardown path — `Drop`, and the failed-init rollback in
//! [`FleetProxy::init`](super::FleetProxy) — has to leave nothing running.
//! A bare `Child::kill` cannot promise that: the substrate is forked into
//! its own process group (`process_group(0)` at the spawn site in the
//! engines cap), and anything *it* forks joins that group rather than
//! dying with the pid this proxy recorded.
//!
//! So the teardown signals the **group**, and gives it a chance to exit on
//! its own terms first: SIGTERM the group, poll out a bounded grace
//! window, then SIGKILL whatever is left and reap. The same escalation the
//! tunnel supervisor and the `aether.process` one-shot runner already run
//! over their own children.
//!
//! Native-unix owns the group signalling; elsewhere a child stands alone
//! and the bare kill is already the whole story.

use std::process::Child;

/// Total grace a SIGTERM'd substrate group gets to exit on its own before
/// the SIGKILL escalation. A substrate's shutdown is a socket close and a
/// scheduler drain, not a flush of user data, so this is a courtesy
/// window rather than a deadline anyone is expected to use in full.
const GROUP_TERM_GRACE_MILLIS: u64 = 1_000;

/// How often the grace window re-checks whether the group has exited.
/// Short enough that the common case — an immediate exit — costs one
/// poll rather than the whole window.
const GROUP_TERM_POLL_MILLIS: u64 = 20;

/// Stop the substrate `child` and reap it, leaving no process and no
/// zombie behind.
///
/// On unix the whole process group is signalled, so a subprocess the
/// substrate forked goes down with it: SIGTERM, then a bounded grace
/// window, then SIGKILL. The child is its own group leader (the engines
/// cap forks it with `process_group(0)`), so its pid *is* the group id.
/// A pid that will not convert to a `pid_t` — which no real pid does —
/// falls back to the direct kill rather than signalling a guessed group.
#[cfg(unix)]
pub fn terminate_child_group(child: &mut Child) {
    match libc::pid_t::try_from(child.id()) {
        Ok(pgid) => {
            signal_group(pgid, libc::SIGTERM);
            if !exited_within_grace(child) {
                signal_group(pgid, libc::SIGKILL);
            }
        }
        Err(_) => {
            let _ = child.kill();
        }
    }
    // Reap on every branch, including the one where the group was already
    // gone before the first signal: an unwaited child is a zombie the hub
    // keeps for its whole lifetime.
    let _ = child.wait();
}

/// Deliver `signal` to the process group led by `pgid`.
///
/// The result is deliberately dropped: the only failure that can arise
/// here is `ESRCH` — the group exited between the poll and the signal —
/// which is the outcome the caller wanted anyway.
#[cfg(unix)]
fn signal_group(pgid: libc::pid_t, signal: libc::c_int) {
    // SAFETY: `killpg(2)` takes two integers and returns one; the group is
    // one this process created by forking its leader, so the call cannot
    // reach a group we do not own.
    unsafe {
        libc::killpg(pgid, signal);
    }
}

/// Poll `child` across the grace window, reporting whether it exited
/// inside it. A wait error counts as exited: the pid is unobservable, so
/// there is nothing further a SIGKILL could accomplish.
#[cfg(unix)]
fn exited_within_grace(child: &mut Child) -> bool {
    use std::thread::sleep;
    use std::time::Duration;

    let polls = GROUP_TERM_GRACE_MILLIS / GROUP_TERM_POLL_MILLIS;
    (0..polls).any(|_| {
        if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
            return true;
        }
        sleep(Duration::from_millis(GROUP_TERM_POLL_MILLIS));
        false
    })
}

/// Non-unix fallback: no process groups to signal, so the child is the
/// whole subtree this proxy can reach.
#[cfg(not(unix))]
pub fn terminate_child_group(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(all(test, unix))]
mod tests {
    use super::terminate_child_group;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    /// How long a sleeper in the fixture lives if nothing signals it —
    /// far past any honest teardown, so a reap that fails to signal shows
    /// up as a blocked `wait` rather than a passing test.
    const FIXTURE_SLEEP_SECS: u32 = 30;

    /// Fork `body` under `/bin/sh` in its own process group, the way the
    /// engines cap forks a substrate, so the reap has a real group to
    /// signal.
    fn spawn_grouped(body: &str) -> Child {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(body).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        command.process_group(0);
        command.spawn().expect("test setup: /bin/sh is present on every unix host")
    }

    /// Whether any process still belongs to the group led by `pgid`.
    /// Signal `0` performs the permission and existence checks without
    /// delivering anything, so this observes the group without disturbing
    /// it — and it reads live kernel state rather than restating what the
    /// code under test just did.
    fn group_is_populated(pgid: libc::pid_t) -> bool {
        // SAFETY: `killpg(2)` with signal 0 delivers nothing; it only
        // reports whether the group exists and is signallable.
        unsafe { libc::killpg(pgid, 0) == 0 }
    }

    /// Tripwire: the reap signals `killpg(child.id())`, and nothing else
    /// in the path re-derives that group id — it is correct only while
    /// the forked child is its own group leader. If the engines cap's
    /// spawn site ever loses its `process_group(0)`, the child inherits
    /// the hub's group and this same `killpg` would signal the hub
    /// itself. The failure that pins is not a leaked process but a
    /// self-inflicted kill, so assert the leadership rather than trusting
    /// the spawn call to keep saying it.
    #[test]
    fn a_grouped_child_leads_the_group_the_reap_signals() {
        let mut child = spawn_grouped(&format!("sleep {FIXTURE_SLEEP_SECS}"));
        let leader = libc::pid_t::try_from(child.id()).expect("test setup: a real pid converts to pid_t");
        // SAFETY: `getpgid(2)` reads the process group of a pid this
        // process forked and still owns.
        let observed_group = unsafe { libc::getpgid(leader) };
        assert_eq!(observed_group, leader, "process_group(0) must make the child its own group leader");
        terminate_child_group(&mut child);
    }

    /// The escalation takes down a *grandchild* the substrate forked, not
    /// just the recorded pid. The fixture backgrounds a sleeper and then
    /// sleeps itself, so the leader and a second group member are both
    /// live when the reap runs; afterwards no member of the group answers
    /// a signal-0 probe. A bare `Child::kill` passes the leader half of
    /// this and leaves the backgrounded sleeper orphaned — which is the
    /// leak the group escalation exists to close.
    #[test]
    fn terminating_the_group_takes_down_a_grandchild_too() {
        let mut child = spawn_grouped(&format!("sleep {FIXTURE_SLEEP_SECS} &\nsleep {FIXTURE_SLEEP_SECS}"));
        let pgid = libc::pid_t::try_from(child.id()).expect("test setup: a real pid converts to pid_t");

        // Let the shell reach its `&` before signalling, so the group
        // genuinely has two members and the test cannot pass vacuously.
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until && !group_is_populated(pgid) {
            sleep(Duration::from_millis(20));
        }
        assert!(group_is_populated(pgid), "test setup: the forked group must be live before the reap");

        let started = Instant::now();
        terminate_child_group(&mut child);
        assert!(
            started.elapsed() < Duration::from_secs(u64::from(FIXTURE_SLEEP_SECS)),
            "the reap must signal rather than wait the sleeper out",
        );
        assert!(!group_is_populated(pgid), "no member of the substrate's group may survive the reap");
    }
}
