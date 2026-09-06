//! The files a dispatch leaves beside its evidence at spawn: the process
//! identity a later coordinator re-attaches to (issue #4999), and the worktree
//! `HEAD` the lane was standing in.
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
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::error::LocalExecutorError;

/// The file a dispatch records its child's process identity in, inside its own
/// evidence directory — the sibling of the `slot` record, read back by boot
/// reconciliation.
pub const IDENTITY_RECORD: &str = "identity";

/// The file a dispatch records the worktree `HEAD` it started on, inside its
/// own evidence directory — the sibling of [`IDENTITY_RECORD`].
pub const CHECKOUT_HEAD_RECORD: &str = "checkout-head";

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
        write_json_record(evidence_dir, IDENTITY_RECORD, self)
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
        terminate_pgid(self.pgid)
    }
}

/// Signal process group `pgid` and wait until no member remains.
///
/// The live-child teardown path names the group by the head pid
/// (`process_group(0)` at spawn), even when that head is already a zombie and
/// cannot be observed. A pid-only kill would leave harness grandchildren in
/// the group, reparented to init.
pub fn terminate_pgid(pgid: u32) -> Result<(), LocalExecutorError> {
    if pgid == 0 {
        return Err(unterminated("refusing to signal process group 0"));
    }
    signal_group(pgid, "-TERM")?;
    if wait_until_pgid_gone(pgid) {
        return Ok(());
    }
    signal_group(pgid, "-KILL")?;
    if wait_until_pgid_gone(pgid) {
        return Ok(());
    }
    Err(unterminated(format!("process group {pgid} is still alive after SIGKILL")))
}

/// The worktree `HEAD` a dispatch started on, recorded beside its evidence so
/// an observer can name the tree without reconstructing slot occupancy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutHead {
    /// `git rev-parse HEAD` of the worktree after it was materialized, trimmed.
    pub head: String,
}

impl CheckoutHead {
    /// Read the `HEAD` a dispatch recorded in `evidence_dir`, or `None` when
    /// the file is missing or unreadable as this shape.
    #[cfg(test)]
    #[must_use]
    pub fn read(evidence_dir: &Path) -> Option<Self> {
        let body = fs::read_to_string(evidence_dir.join(CHECKOUT_HEAD_RECORD)).ok()?;
        serde_json::from_str(body.trim()).ok()
    }

    /// Persist this `HEAD` beside the dispatch's evidence.
    pub fn write(&self, evidence_dir: &Path) -> Result<(), LocalExecutorError> {
        write_json_record(evidence_dir, CHECKOUT_HEAD_RECORD, self)
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

/// Record the worktree `HEAD` the lane started on beside `evidence_dir`. A
/// write fault is logged rather than failing the spawn: the child is already
/// running, and a missing record is the same gap a restart already knows how
/// to handle.
pub fn record_checkout_head(evidence_dir: &Path, head: &str) {
    let recorded = CheckoutHead { head: head.to_owned() };
    if let Err(error) = recorded.write(evidence_dir) {
        tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            evidence = %evidence_dir.display(),
            head,
            %error,
            "local executor backend: could not record the worktree HEAD; a later observer cannot name the tree this lane started on",
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
    // No `--`: BSD `kill` (macOS) treats it as a pid, and `-{pgid}` is already numeric.
    let status = Command::new("kill")
        .args([signal, &format!("-{pgid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(LocalExecutorError::Io)?;
    // `kill` exits non-zero when every member is already gone, which is the
    // success the waiter below is about to observe — not a reason to stop.
    let _ = status;
    Ok(())
}

fn wait_until_pgid_gone(pgid: u32) -> bool {
    let deadline = Instant::now() + GROUP_EXIT_BUDGET;
    loop {
        if !any_process_in_group(pgid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Whether any live (non-zombie) process is in `pgid`.
///
/// `/proc` is the precise reading where it exists. A missing `/proc` is not an
/// empty group — that skipped SIGKILL on macOS — so the next probes are `ps`
/// (state-aware) and `kill -0` on the group. A probe that cannot observe is
/// unknown, not gone: unknown is live so cancellation escalates instead of
/// returning success into an unbounded `child.wait`.
fn any_process_in_group(pgid: u32) -> bool {
    group_is_live(proc_group_is_live(pgid), ps_group_is_live(pgid), group_responds_to_signal_zero(pgid))
}

/// First probe that could observe wins. If every probe is unknown, the group
/// is live: a timeout-and-error is honest, a false "gone" is not.
fn group_is_live(proc: Option<bool>, ps: Option<bool>, signal_zero: Option<bool>) -> bool {
    proc.or(ps).or(signal_zero).unwrap_or(true)
}

fn proc_group_is_live(pgid: u32) -> Option<bool> {
    proc_listing_is_live(fs::read_dir("/proc"), pgid)
}

/// `None` means the listing could not be read — not that the group is empty.
fn proc_listing_is_live(listing: io::Result<fs::ReadDir>, pgid: u32) -> Option<bool> {
    let entries = listing.ok()?;
    Some(entries.flatten().any(|entry| {
        let Some(member) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            return false;
        };
        ProcessIdentity::observe(member).is_some_and(|live| live.pgid == pgid)
    }))
}

fn ps_group_is_live(pgid: u32) -> Option<bool> {
    let output = Command::new("ps")
        .args(["-A", "-o", "pgid=", "-o", "state="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(ps_listing_has_live_member(&String::from_utf8_lossy(&output.stdout), pgid))
}

/// Whether `listing` from `ps -A -o pgid= -o state=` contains a non-zombie
/// member of `pgid`. Zombies are already dead: counting them as live waits out
/// the SIGKILL budget and reports [`LocalExecutorError::Unterminated`] for a
/// child this process is about to reap.
fn ps_listing_has_live_member(listing: &str, pgid: u32) -> bool {
    listing.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let Some(member_pgid) = fields.next().and_then(|field| field.parse::<u32>().ok()) else {
            return false;
        };
        let Some(state) = fields.next() else {
            return false;
        };
        member_pgid == pgid && !state.starts_with('Z')
    })
}

fn group_responds_to_signal_zero(pgid: u32) -> Option<bool> {
    signal_zero_observation(
        Command::new("kill")
            .args(["-0", &format!("-{pgid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success()),
    )
}

/// `kill -0` can confirm a live group. A spawn fault or nonzero exit cannot
/// confirm exit: ESRCH and EPERM are the same command status, and a missing
/// `kill` is not an empty group.
fn signal_zero_observation(result: io::Result<bool>) -> Option<bool> {
    result.ok().filter(|&alive| alive)
}

fn unterminated(detail: impl Into<String>) -> LocalExecutorError {
    LocalExecutorError::Unterminated(detail.into())
}

/// Pretty-JSON + trailing newline, the shape every evidence-dir record this
/// module writes.
fn write_json_record(evidence_dir: &Path, name: &str, value: &impl Serialize) -> Result<(), LocalExecutorError> {
    fs::create_dir_all(evidence_dir).map_err(LocalExecutorError::Io)?;
    let mut rendered =
        serde_json::to_string_pretty(value).map_err(|error| LocalExecutorError::Io(io::Error::other(error)))?;
    rendered.push('\n');
    fs::write(evidence_dir.join(name), rendered).map_err(LocalExecutorError::Io)
}

#[cfg(test)]
mod tests {
    use std::iter::repeat_n;
    #[cfg(unix)]
    use std::process::{Command, Stdio};
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    use super::{
        ProcessIdentity, StatFields, group_is_live, proc_listing_is_live, ps_listing_has_live_member,
        signal_zero_observation,
    };
    #[cfg(unix)]
    use super::{any_process_in_group, terminate_pgid};

    fn stat_line(comm: &str, pgid: u32, starttime: u64) -> String {
        stat_line_state('S', comm, pgid, starttime)
    }

    fn stat_line_state(state: char, comm: &str, pgid: u32, starttime: u64) -> String {
        // pid and comm, then 20 tokens after the closing paren so starttime
        // lands at man-page field 22. Values other than pgid/starttime are
        // unused padding.
        let mut fields = vec![state.to_string(), "1".to_owned(), pgid.to_string()];
        fields.extend(repeat_n("0".to_owned(), 16));
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

    #[test]
    fn a_failed_proc_listing_is_unknown_not_an_empty_group() {
        // Tripwire: the macOS bug was mapping read_dir("/proc") failure to "no
        // members", which skipped SIGKILL. An IO error must be unknown so
        // any_process_in_group falls through; a readable empty table is gone.
        assert_eq!(
            proc_listing_is_live(Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no /proc")), 1),
            None,
            "a missing proc filesystem is not proof the group is gone",
        );
        let empty = tempfile::tempdir().expect("an empty stand-in for a readable /proc");
        assert_eq!(
            proc_listing_is_live(std::fs::read_dir(empty.path()), 1),
            Some(false),
            "a readable proc table with no members is an empty group",
        );
    }

    #[test]
    fn ps_listing_treats_zombies_as_gone_and_live_members_as_present() {
        // Tripwire: the portable fallback must not treat a zombie as live (that
        // waits out the SIGKILL budget and reports Unterminated for a child
        // this process is about to reap) and must not miss a live member in the
        // group (that skips SIGKILL on a host without /proc).
        let listing = "\
    10 S
    10 Z+
    99 R
";
        assert!(ps_listing_has_live_member(listing, 10), "a running member keeps the group live");
        assert!(
            !ps_listing_has_live_member("    10 Z+\n    99 R\n", 10),
            "a zombie is gone, even when other groups still have runners",
        );
        assert!(!ps_listing_has_live_member(listing, 7), "a live process in another group is not this group");
        assert!(!ps_listing_has_live_member("", 10), "an empty table is an empty group");
        assert!(!ps_listing_has_live_member("not a ps line\n", 10), "garbage lines do not invent members");
    }

    #[test]
    fn unavailable_probes_are_live_not_gone() {
        // Tripwire: if ps cannot run and kill -0 cannot run or returns EPERM,
        // mapping that to "no members" is the same false exit as a missing
        // /proc. Injected unknown probes must stay live so terminate_pgid
        // escalates and times out rather than succeeding into child.wait.
        assert_eq!(
            signal_zero_observation(Ok(false)),
            None,
            "a nonzero kill -0 is ESRCH or EPERM, not confirmed exit",
        );
        assert_eq!(
            signal_zero_observation(Err(std::io::Error::new(std::io::ErrorKind::NotFound, "kill"))),
            None,
            "a kill that cannot be spawned is unknown, not an empty group",
        );
        assert_eq!(signal_zero_observation(Ok(true)), Some(true), "a successful kill -0 is a live group");
        assert!(
            group_is_live(None, None, signal_zero_observation(Ok(false))),
            "ambiguous kill -0 after failed proc and ps is live",
        );
        assert!(
            group_is_live(
                None,
                None,
                signal_zero_observation(Err(std::io::Error::new(std::io::ErrorKind::NotFound, "kill"))),
            ),
            "every probe unavailable is live, not an empty group",
        );
        assert!(group_is_live(None, None, None), "three unknown probes are live");
        assert!(group_is_live(None, None, Some(true)), "kill -0 success still confirms live");
        assert!(!group_is_live(None, Some(false), None), "a successful empty ps listing is gone");
        assert!(!group_is_live(Some(false), None, None), "a successful empty proc scan is gone");
    }

    #[cfg(unix)]
    fn spawn_group(program: &str, args: &[&str]) -> (std::process::Child, u32) {
        use std::os::unix::process::CommandExt as _;
        let child = Command::new(program)
            .args(args)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the test child forks");
        let pgid = child.id();
        (child, pgid)
    }

    #[cfg(unix)]
    struct GroupGuard(u32);

    #[cfg(unix)]
    impl Drop for GroupGuard {
        fn drop(&mut self) {
            let _ = Command::new("kill")
                .args(["-KILL", &format!("-{}", self.0)])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_live_process_group_is_observed_without_proc() {
        // Tripwire: read_dir("/proc") fails on macOS. Treating that IO error as
        // "no members" makes wait_until_pgid_gone return immediately, skip
        // SIGKILL, and leave the child for an unbounded wait().
        let (mut child, pgid) = spawn_group("sleep", &["60"]);
        let _guard = GroupGuard(pgid);
        assert!(any_process_in_group(pgid), "a just-spawned group must still look live when /proc is missing");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn terminate_pgid_does_not_treat_a_reaped_or_zombie_child_as_live() {
        // Tripwire: kill -0 still succeeds for a zombie. After SIGTERM the
        // child is dead but unreaped; counting that as live waits out the
        // SIGKILL budget and returns Unterminated for a group that is gone.
        let (mut child, pgid) = spawn_group("sleep", &["60"]);
        let _guard = GroupGuard(pgid);
        terminate_pgid(pgid).expect("a TERM-honoring group is gone, including as a zombie");
        child.wait().expect("the head is reaped");
        assert!(!any_process_in_group(pgid), "no live member remains after a successful terminate");
    }

    #[cfg(unix)]
    #[test]
    fn terminate_pgid_sigkills_a_term_ignoring_grandchild() {
        // Tripwire: a missing /proc used to report the group gone after SIGTERM,
        // skip SIGKILL, and leave a TERM-ignoring grandchild reparented to init.
        let (mut child, pgid) = spawn_group("sh", &["-c", "trap '' TERM; sleep 60 & exit 0"]);
        let _guard = GroupGuard(pgid);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut head_gone_with_grandchild = false;
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() && any_process_in_group(pgid) {
                head_gone_with_grandchild = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            head_gone_with_grandchild,
            "the head must exit leaving a TERM-ignoring grandchild in its group",
        );
        terminate_pgid(pgid).expect("SIGKILL must finish a group that ignored SIGTERM");
        assert!(!any_process_in_group(pgid), "no member of the lane group survives teardown");
    }
}
