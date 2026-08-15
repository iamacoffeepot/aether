//! The process identity a lane child leaves beside its evidence so a later
//! coordinator can re-attach to it (issue #4999).
//!
//! A bare pid is not an identity: the kernel recycles them, and signalling a
//! recycled one kills a stranger. The identity is the pid plus the process
//! start time from `/proc/<pid>/stat`, with the machine's boot id as a cheap
//! outer guard. Re-attachment succeeds only when the pid is live *and* both
//! match the record; a missing, unreadable, or mismatched record is the same
//! unowned run that existed before this file, never a kill aimed at an
//! unverified pid.

use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::error::LocalExecutorError;

/// The file a dispatch records its child's process identity in, inside its own
/// evidence directory — the sibling of the `slot` record, read back by boot
/// reconciliation.
pub const IDENTITY_RECORD: &str = "identity";

/// How long a re-attached kill waits for the process group to disappear after
/// each signal. SIGTERM is tried first; SIGKILL follows if the group is still
/// there when this budget elapses.
const GROUP_EXIT_BUDGET: Duration = Duration::from_secs(5);

/// The pid, process-group, start time, and boot id of one lane child.
///
/// Written at spawn, read at re-adoption. Not a journal type: it lives only as
/// a per-dispatch file under the scratch root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    /// The child's pid as `std::process::Child::id` reported it.
    pub pid: u32,
    /// The child's process-group id — equal to `pid` when the child was spawned
    /// with `process_group(0)`, and the target a re-attached kill signals.
    pub pgid: u32,
    /// Field 22 of `/proc/<pid>/stat`: clock ticks after boot. Compared as the
    /// raw integer; converting it is how two different processes would collide.
    pub starttime: u64,
    /// Contents of `/proc/sys/kernel/random/boot_id`. A reboot recycles every
    /// pid, so a mismatched boot id is a mismatched identity.
    pub boot_id: String,
}

impl ProcessIdentity {
    /// Observe the live process at `pid`, or `None` when `/proc` has no such
    /// process, the process is a zombie, or its stat line cannot be parsed.
    ///
    /// A zombie is already dead — it cannot write, and it cannot be signalled
    /// further. Treating it as live would make a re-attached kill wait forever
    /// on a process whose only remaining holder is a `Child` in another
    /// process (or this one, in a test).
    #[must_use]
    pub fn observe(pid: u32) -> Option<Self> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        Self::from_stat(pid, &stat, &read_boot_id()?)
    }

    /// Parse a `/proc/<pid>/stat` body together with a boot id. The pure core
    /// of [`observe`](Self::observe), so a test can drive the field walk
    /// without a live process. A zombie (`state == Z`) is `None`.
    #[must_use]
    pub fn from_stat(pid: u32, stat: &str, boot_id: &str) -> Option<Self> {
        let fields = StatFields::parse(stat)?;
        if fields.state == 'Z' {
            return None;
        }
        Some(Self { pid, pgid: fields.pgid, starttime: fields.starttime, boot_id: boot_id.trim().to_owned() })
    }

    /// Read the identity a dispatch recorded in `evidence_dir`, or `None` when
    /// the file is missing or unreadable as this shape.
    #[must_use]
    pub fn read(evidence_dir: &Path) -> Option<Self> {
        let body = fs::read_to_string(evidence_dir.join(IDENTITY_RECORD)).ok()?;
        serde_json::from_str(body.trim()).ok()
    }

    /// Persist this identity beside the dispatch's evidence. Best-effort at the
    /// call site: a record that cannot be written costs a restart its
    /// re-attachment, never the dispatch itself.
    pub fn write(&self, evidence_dir: &Path) -> Result<(), LocalExecutorError> {
        fs::create_dir_all(evidence_dir).map_err(LocalExecutorError::Io)?;
        let mut rendered =
            serde_json::to_string_pretty(self).map_err(|error| LocalExecutorError::Io(io::Error::other(error)))?;
        rendered.push('\n');
        fs::write(evidence_dir.join(IDENTITY_RECORD), rendered).map_err(LocalExecutorError::Io)
    }

    /// Whether `live` is the same process this record named: start time and
    /// boot id both match. The pid is the lookup key, not part of the match —
    /// a recycled pid is how two different processes share a number.
    #[must_use]
    pub fn matches(&self, live: &Self) -> bool {
        self.starttime == live.starttime && self.boot_id == live.boot_id
    }

    /// The live process at this record's pid, if it is still this process.
    #[must_use]
    pub fn attach(&self) -> Option<Self> {
        let live = Self::observe(self.pid)?;
        self.matches(&live).then_some(live)
    }

    /// Signal this process group and wait until no member remains.
    ///
    /// SIGTERM first, then SIGKILL if the group is still there. Success only
    /// after the group is observed gone — a signal that was sent is not
    /// evidence the child died.
    pub fn terminate_group(&self) -> Result<(), LocalExecutorError> {
        if self.pgid == 0 {
            return Err(unterminated("refusing to signal process group 0"));
        }
        signal_group(self.pgid, "-TERM")?;
        if wait_until_group_gone(self) {
            return Ok(());
        }
        signal_group(self.pgid, "-KILL")?;
        if wait_until_group_gone(self) {
            return Ok(());
        }
        Err(unterminated(format!("process group {} is still alive after SIGKILL", self.pgid)))
    }
}

/// Record the live identity of `pid` beside `evidence_dir`. A `/proc` miss or
/// a write fault is logged rather than failing the spawn: the child is already
/// running, and a missing record is the unowned run a restart already knows
/// how to handle.
pub fn record_spawned(evidence_dir: &Path, pid: u32) {
    let Some(identity) = ProcessIdentity::observe(pid) else {
        tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            pid,
            evidence = %evidence_dir.display(),
            "local executor backend: could not observe the spawned lane child's process identity; a restart will not re-attach to it",
        );
        return;
    };
    if let Err(error) = identity.write(evidence_dir) {
        tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            pid,
            evidence = %evidence_dir.display(),
            %error,
            "local executor backend: could not record the spawned lane child's process identity; a restart will not re-attach to it",
        );
    }
}

/// Whether `pid` currently names a running process. A missing `/proc` entry
/// or a zombie is not live — the latter is the child already having exited,
/// waiting only to be reaped.
#[must_use]
pub fn pid_is_live(pid: u32) -> bool {
    ProcessIdentity::observe(pid).is_some()
}

fn read_boot_id() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|body| body.trim().to_owned())
        .filter(|id| !id.is_empty())
}

/// The `/proc/<pid>/stat` fields a re-attachment reads.
struct StatFields {
    state: char,
    pgid: u32,
    starttime: u64,
}

impl StatFields {
    /// Walk a `/proc/<pid>/stat` line. `comm` is in parentheses and may contain
    /// spaces or further `)` characters, so the walk starts after the *last*
    /// `)` rather than splitting the line.
    ///
    /// After `comm`, the fields this needs sit at fixed offsets: `pgrp` is the
    /// third token (man-page field 5) and `starttime` is the twentieth
    /// (man-page field 22).
    fn parse(stat: &str) -> Option<Self> {
        let after_comm = stat.rsplit_once(')')?.1;
        let mut fields = after_comm.split_whitespace();
        let state = fields.next()?.chars().next()?;
        let _ppid = fields.next()?;
        let pgid = fields.next()?.parse().ok()?;
        for _ in 0..16 {
            fields.next()?;
        }
        let starttime = fields.next()?.parse().ok()?;
        Some(Self { state, pgid, starttime })
    }
}

fn signal_group(pgid: u32, signal: &str) -> Result<(), LocalExecutorError> {
    let status =
        Command::new("kill").args([signal, "--", &format!("-{pgid}")]).status().map_err(LocalExecutorError::Io)?;
    // `kill` exits non-zero when every member is already gone, which is the
    // success the waiter below is about to observe — not a reason to stop.
    let _ = status;
    Ok(())
}

fn wait_until_group_gone(identity: &ProcessIdentity) -> bool {
    let deadline = Instant::now() + GROUP_EXIT_BUDGET;
    loop {
        if !group_is_live(identity) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn group_is_live(identity: &ProcessIdentity) -> bool {
    if identity.attach().is_some() {
        return true;
    }
    any_process_in_group(identity.pgid)
}

fn any_process_in_group(pgid: u32) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|entry| {
        let Some(member) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            return false;
        };
        ProcessIdentity::observe(member).is_some_and(|live| live.pgid == pgid)
    })
}

fn unterminated(detail: impl Into<String>) -> LocalExecutorError {
    LocalExecutorError::Unterminated(detail.into())
}

#[cfg(test)]
mod tests {
    use super::{ProcessIdentity, StatFields};

    fn stat_line(comm: &str, pgid: u32, starttime: u64) -> String {
        stat_line_state('S', comm, pgid, starttime)
    }

    fn stat_line_state(state: char, comm: &str, pgid: u32, starttime: u64) -> String {
        // pid and comm, then 20 tokens after the closing paren so starttime
        // lands at man-page field 22. Values other than pgid/starttime are
        // unused padding.
        let mut fields = vec![state.to_string(), "1".to_owned(), pgid.to_string()];
        fields.extend(std::iter::repeat_n("0".to_owned(), 16));
        fields.push(starttime.to_string());
        format!("42 ({comm}) {}", fields.join(" "))
    }

    #[test]
    fn stat_parse_reads_pgrp_and_starttime_after_a_spaced_comm() {
        // Tripwire: comm is parenthesized and may contain spaces. A split on
        // whitespace would shift every later field and re-attach to a pid
        // whose start time we never actually compared.
        let Some(fields) = StatFields::parse(&stat_line("cargo xtask", 99, 1_234_567)) else {
            panic!("a well-formed stat line with a spaced comm must parse");
        };
        assert_eq!(fields.pgid, 99);
        assert_eq!(fields.starttime, 1_234_567);
    }

    #[test]
    fn stat_parse_uses_the_last_closing_paren() {
        // Tripwire: a comm that itself contains `)` (a real process name) must
        // not truncate the walk. Starting after the first `)` would parse
        // garbage as pgrp/starttime and either refuse a live child or, worse,
        // match a stranger.
        let Some(fields) = StatFields::parse(&stat_line("foo)bar", 7, 42)) else {
            panic!("a comm containing ')' must still parse from the last closing paren");
        };
        assert_eq!(fields.pgid, 7);
        assert_eq!(fields.starttime, 42);
    }

    #[test]
    fn identity_match_requires_starttime_and_boot_id() {
        // The plausible bug: treating a live pid as identity. A recycled pid
        // with a different start time, or the same pid after a reboot, is a
        // different process and must not match.
        let recorded = ProcessIdentity { pid: 10, pgid: 10, starttime: 100, boot_id: "boot-a".to_owned() };
        let same = ProcessIdentity { pid: 10, pgid: 10, starttime: 100, boot_id: "boot-a".to_owned() };
        let recycled = ProcessIdentity { pid: 10, pgid: 10, starttime: 200, boot_id: "boot-a".to_owned() };
        let rebooted = ProcessIdentity { pid: 10, pgid: 10, starttime: 100, boot_id: "boot-b".to_owned() };

        assert!(recorded.matches(&same), "the same start time on the same boot is this process");
        assert!(!recorded.matches(&recycled), "a recycled pid has a different start time");
        assert!(!recorded.matches(&rebooted), "a reboot recycles every pid");
    }

    #[test]
    fn a_zombie_stat_is_not_a_live_identity() {
        // Tripwire: after SIGKILL the child is a zombie until its original
        // parent reaps it. Counting that as live makes terminate_group wait
        // out the budget and report Unterminated for a process that is
        // already dead — which is exactly the test (and a restart whose
        // child died) that must report success.
        assert!(
            ProcessIdentity::from_stat(9, &stat_line_state('Z', "sleep", 9, 1), "boot").is_none(),
            "a zombie is gone, not a process this coordinator can still signal",
        );
        assert!(
            ProcessIdentity::from_stat(9, &stat_line("sleep", 9, 1), "boot").is_some(),
            "the same line with a running state still attaches",
        );
    }
}
