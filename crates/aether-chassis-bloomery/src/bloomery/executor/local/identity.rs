//! The files a dispatch leaves beside its evidence at spawn: the process
//! identity a later coordinator re-attaches to (issue #4999), and the worktree
//! `HEAD` the lane was standing in.
//!
//! A bare pid is not an identity: the kernel recycles them, and signalling a
//! recycled one kills a stranger. The identity is the pid plus the process
//! start time and boot identity this host can observe. Linux reads
//! `/proc/<pid>/stat` ticks-after-boot and `/proc/sys/kernel/random/boot_id`.
//! macOS reads `proc_pidinfo(PROC_PIDTBSDINFO)` start microseconds and
//! `kern.bootsessionuuid`. Re-attachment succeeds only when the pid is
//! observable *and* both start and boot match the record; a missing,
//! unreadable, or mismatched record is the same unowned run that existed
//! before this file, never a kill aimed at an unverified pid. `observe`
//! returning `None` is not proof the process has exited.

use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use std::mem::MaybeUninit;
#[cfg(target_os = "macos")]
use std::ptr::null_mut;
#[cfg(target_os = "macos")]
use std::str;

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
    /// Process start identity, compared as the raw integer. Linux: field 22 of
    /// `/proc/<pid>/stat` (clock ticks after boot). macOS: `pbi_start_tvsec *
    /// 1_000_000 + pbi_start_tvusec` (microseconds; usec must be `< 1_000_000`
    /// and the multiply must not overflow). Converting either unit is how two
    /// different processes would collide.
    pub starttime: u64,
    /// Boot identity. Linux: contents of `/proc/sys/kernel/random/boot_id`.
    /// macOS: `kern.bootsessionuuid` (read-only UUID string). A reboot recycles
    /// every pid, so a mismatched boot id is a mismatched identity.
    pub boot_id: String,
}

impl ProcessIdentity {
    /// Observe the live process at `pid`, or `None` when this host cannot
    /// confirm a live (non-zombie) identity.
    ///
    /// Linux reads `/proc/<pid>/stat` and the boot UUID. macOS reads
    /// `proc_pidinfo(PROC_PIDTBSDINFO)` and `kern.bootsessionuuid`. Other
    /// platforms return `None`.
    ///
    /// `None` is not proof the process has exited: a missing `/proc` entry, an
    /// unreadable `proc_pidinfo` or sysctl, a zombie, and an unsupported
    /// platform all yield `None`. A zombie is already dead — treating it as
    /// live would make a re-attached kill wait forever on a process whose only
    /// remaining holder is a `Child` in another process (or this one, in a test).
    #[must_use]
    pub fn observe(pid: u32) -> Option<Self> {
        observe_platform(pid)
    }

    /// Parse a `/proc/<pid>/stat` body together with a boot id. The pure core
    /// of [`observe`](Self::observe), so a test can drive the field walk
    /// without a live process. A zombie (`state == Z`) is `None`.
    #[cfg(any(target_os = "linux", test))]
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
/// the group, reparented to init. Group 0, group 1 (the `-1` broadcast
/// operand), and a pgid that does not fit a signed pid are refused before
/// `kill` runs.
pub fn terminate_pgid(pgid: u32) -> Result<(), LocalExecutorError> {
    signal_group(pgid, "TERM")?;
    if wait_until_pgid_gone(pgid) {
        return Ok(());
    }
    signal_group(pgid, "KILL")?;
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

/// Record the live identity of `pid` beside `evidence_dir`.
///
/// Observe uses this host's process identity (Linux `/proc`, macOS
/// `proc_pidinfo` and `kern.bootsessionuuid`). A miss or a write fault is
/// logged rather than failing the spawn: the child is already running, and a
/// missing record is the unowned run a restart already knows how to handle. A
/// miss is not classified: unreadable identity, a gone pid, and an unsupported
/// host all look the same.
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

/// Whether `observe` returned an identity for `pid`.
///
/// `false` is not proof of exit: an unreadable identity and an unsupported
/// host also fail `observe`. A zombie is gone; a missing `/proc` entry or
/// `proc_pidinfo` miss is not distinguished from that.
#[must_use]
pub fn pid_is_live(pid: u32) -> bool {
    ProcessIdentity::observe(pid).is_some()
}

#[cfg(target_os = "linux")]
fn read_boot_id() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|body| body.trim().to_owned())
        .filter(|id| !id.is_empty())
}

#[cfg(target_os = "linux")]
fn observe_platform(pid: u32) -> Option<ProcessIdentity> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    ProcessIdentity::from_stat(pid, &stat, &read_boot_id()?)
}

#[cfg(target_os = "macos")]
fn observe_platform(pid: u32) -> Option<ProcessIdentity> {
    let info = read_proc_bsdinfo(pid)?;
    from_bsdinfo(pid, &info, &read_boot_session_sysctl()?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn observe_platform(_pid: u32) -> Option<ProcessIdentity> {
    None
}

#[cfg(target_os = "macos")]
fn pid_as_proc_pid(pid: u32) -> Option<libc::c_int> {
    let pid = libc::c_int::try_from(pid).ok()?;
    (pid > 0).then_some(pid)
}

#[cfg(target_os = "macos")]
fn pidinfo_size_matches(got: libc::c_int, expected: libc::c_int) -> bool {
    got == expected
}

#[cfg(target_os = "macos")]
fn macos_starttime_micros(secs: u64, micros: u64) -> Option<u64> {
    if micros >= 1_000_000 {
        return None;
    }
    secs.checked_mul(1_000_000)?.checked_add(micros)
}

#[cfg(target_os = "macos")]
fn parse_boot_session_uuid(bytes: &[u8]) -> Option<String> {
    let (last, rest) = bytes.split_last()?;
    if *last != 0 || rest.is_empty() || rest.contains(&0) {
        return None;
    }
    let text = str::from_utf8(rest).ok()?;
    if text.chars().all(char::is_whitespace) {
        return None;
    }
    canonical_boot_session_uuid(text).then(|| text.to_owned())
}

#[cfg(target_os = "macos")]
fn canonical_boot_session_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, &byte)| match index {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => byte.is_ascii_hexdigit(),
    })
}

#[cfg(target_os = "macos")]
fn from_bsdinfo(requested_pid: u32, info: &libc::proc_bsdinfo, boot_bytes: &[u8]) -> Option<ProcessIdentity> {
    if info.pbi_pid != requested_pid || info.pbi_status == libc::SZOMB {
        return None;
    }
    let starttime = macos_starttime_micros(info.pbi_start_tvsec, info.pbi_start_tvusec)?;
    let boot_id = parse_boot_session_uuid(boot_bytes)?;
    Some(ProcessIdentity { pid: requested_pid, pgid: info.pbi_pgid, starttime, boot_id })
}

#[cfg(target_os = "macos")]
fn read_proc_bsdinfo(pid: u32) -> Option<libc::proc_bsdinfo> {
    let pid = pid_as_proc_pid(pid)?;
    let expected = libc::c_int::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    // SAFETY: `info` is a `MaybeUninit<proc_bsdinfo>` whose size is passed as
    // `buffersize`. `PROC_PIDTBSDINFO` writes that struct. We only
    // `assume_init` when the kernel returned exactly `expected` bytes, so every
    // field was written and no uninitialized byte is read.
    let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), expected) };
    if !pidinfo_size_matches(got, expected) {
        return None;
    }
    // SAFETY: `got == expected` means the kernel filled `size_of::<proc_bsdinfo>()`.
    Some(unsafe { info.assume_init() })
}

#[cfg(target_os = "macos")]
fn read_boot_session_sysctl() -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: `kern.bootsessionuuid` is a NUL-terminated C string. `oldp` /
    // `oldlenp` describe the 64-byte stack buffer. `newp` is null and `newlen`
    // is 0, so this is a read. The kernel writes at most `*oldlenp` bytes into
    // `buf` (which starts zeroed) and then stores the written length in `len`.
    // Only `buf[..written]` is parsed.
    let rc = unsafe {
        libc::sysctlbyname(c"kern.bootsessionuuid".as_ptr(), buf.as_mut_ptr().cast(), &raw mut len, null_mut(), 0)
    };
    if rc != 0 {
        return None;
    }
    Some(buf.get(..len)?.to_vec())
}

/// The `/proc/<pid>/stat` fields a re-attachment reads.
#[cfg(any(target_os = "linux", test))]
struct StatFields {
    state: char,
    pgid: u32,
    starttime: u64,
}

#[cfg(any(target_os = "linux", test))]
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

/// A process-group id `kill` can name as `-{pgid}`.
///
/// Group 0 is this process's group. Group 1 becomes the `-1` operand, which is
/// broadcast rather than a private lane group. A `u32` that does not fit `i32`
/// cannot be a signed kill target. All three are refused before `kill` runs.
pub(super) fn signed_pgid(pgid: u32) -> Result<i32, LocalExecutorError> {
    if pgid <= 1 {
        return Err(unterminated(format!("refusing to signal process group {pgid}")));
    }
    i32::try_from(pgid).map_err(|_| unterminated(format!("process group {pgid} does not fit a signed pid")))
}

/// `kill` argv that names process group `pgid` with POSIX `-s`.
///
/// Linux uses `-s SIGNAL -- -PGID` so the negative group is a separate operand
/// after options. macOS keeps `-s SIGNAL -PGID`, the form that already cancelled
/// groups there. `pgid` must already have passed [`signed_pgid`].
pub(super) fn kill_group_args(signal: &str, pgid: i32) -> Vec<String> {
    if cfg!(target_os = "linux") {
        vec!["-s".to_owned(), signal.to_owned(), "--".to_owned(), format!("-{pgid}")]
    } else {
        vec!["-s".to_owned(), signal.to_owned(), format!("-{pgid}")]
    }
}

fn signal_group(pgid: u32, signal: &str) -> Result<(), LocalExecutorError> {
    let output = Command::new("kill")
        .args(kill_group_args(signal, signed_pgid(pgid)?))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(LocalExecutorError::Io)?;
    // Non-zero is ESRCH when every member is already gone, and a usage/EPERM
    // fault when the group is still there. Only the former is delivery.
    if output.status.success() || !any_process_in_group(pgid) {
        return Ok(());
    }
    Err(unterminated(format!(
        "kill -s {signal} did not deliver to process group {pgid}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
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
    group_is_live(proc_group_is_live(pgid), || ps_group_is_live(pgid), || group_responds_to_signal_zero(pgid))
}

/// First probe that could observe wins. Later probes are closures so `ps` and
/// `kill -0` do not run once `/proc` answered. If every probe is unknown, the
/// group is live: a timeout-and-error is honest, a false "gone" is not.
fn group_is_live(
    proc: Option<bool>,
    ps: impl FnOnce() -> Option<bool>,
    signal_zero: impl FnOnce() -> Option<bool>,
) -> bool {
    proc.or_else(ps).or_else(signal_zero).unwrap_or(true)
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
            .args(kill_group_args("0", signed_pgid(pgid).ok()?))
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
    use std::fs;
    use std::io::{Error, ErrorKind};
    use std::iter::repeat_n;
    #[cfg(target_os = "macos")]
    use std::mem::zeroed;
    #[cfg(target_os = "macos")]
    use std::process::ExitStatus;
    #[cfg(unix)]
    use std::process::{Child, Command, Stdio};
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    use super::any_process_in_group;
    use super::{
        LocalExecutorError, ProcessIdentity, StatFields, group_is_live, kill_group_args, proc_listing_is_live,
        ps_listing_has_live_member, signal_zero_observation, terminate_pgid,
    };

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
            proc_listing_is_live(Err(Error::new(ErrorKind::NotFound, "no /proc")), 1),
            None,
            "a missing proc filesystem is not proof the group is gone",
        );
        let empty = tempfile::tempdir().expect("an empty stand-in for a readable /proc");
        assert_eq!(
            proc_listing_is_live(fs::read_dir(empty.path()), 1),
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
        assert_eq!(signal_zero_observation(Ok(false)), None, "a nonzero kill -0 is ESRCH or EPERM, not confirmed exit");
        assert_eq!(
            signal_zero_observation(Err(Error::new(ErrorKind::NotFound, "kill"))),
            None,
            "a kill that cannot be spawned is unknown, not an empty group",
        );
        assert_eq!(signal_zero_observation(Ok(true)), Some(true), "a successful kill -0 is a live group");
        assert!(
            group_is_live(None, || None, || signal_zero_observation(Ok(false))),
            "ambiguous kill -0 after failed proc and ps is live",
        );
        assert!(
            group_is_live(None, || None, || signal_zero_observation(Err(Error::new(ErrorKind::NotFound, "kill")))),
            "every probe unavailable is live, not an empty group",
        );
        assert!(group_is_live(None, || None, || None), "three unknown probes are live");
        assert!(group_is_live(None, || None, || Some(true)), "kill -0 success still confirms live");
        assert!(
            !group_is_live(None, || Some(false), || panic!("kill -0 must not run after ps observes")),
            "a successful empty ps listing is gone",
        );
        assert!(
            !group_is_live(
                Some(false),
                || panic!("ps must not run when /proc observed"),
                || panic!("kill -0 must not run when /proc observed"),
            ),
            "a successful empty proc scan is gone",
        );
    }

    #[test]
    fn group_kill_argv_is_platform_unambiguous() {
        // Construction pin for the chosen `-s` forms. Delivery is the live
        // child/grandchild tests and Linux CI, not this argv list.
        let term = kill_group_args("TERM", 42);
        let kill = kill_group_args("KILL", 42);
        let probe = kill_group_args("0", 42);
        #[cfg(target_os = "linux")]
        {
            assert_eq!(term, ["-s", "TERM", "--", "-42"]);
            assert_eq!(kill, ["-s", "KILL", "--", "-42"]);
            assert_eq!(probe, ["-s", "0", "--", "-42"]);
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert_eq!(term, ["-s", "TERM", "-42"]);
            assert_eq!(kill, ["-s", "KILL", "-42"]);
            assert_eq!(probe, ["-s", "0", "-42"]);
        }
    }

    #[test]
    fn terminate_pgid_refuses_unsafe_or_unrepresentable_group_targets() {
        // Tripwire: 0 is this process's group, 1 is the `-1` broadcast operand,
        // and a pgid that does not fit a signed pid cannot be named as `-{pgid}`.
        // Refused through terminate_pgid before `kill` runs — do not send `-1`.
        for pgid in [0, 1, u32::MAX] {
            match terminate_pgid(pgid) {
                Err(LocalExecutorError::Unterminated(detail)) => {
                    assert!(detail.contains(&pgid.to_string()), "{detail}");
                }
                other => panic!("process group {pgid} must be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_proc_observation_does_not_invoke_fallback_probes() {
        // Tripwire: eager Option::or evaluates ps and kill even after /proc
        // answered. The production helper must skip those closures.
        let mut ps_calls = 0;
        let mut kill_calls = 0;
        assert!(
            group_is_live(
                Some(true),
                || {
                    ps_calls += 1;
                    Some(false)
                },
                || {
                    kill_calls += 1;
                    Some(false)
                },
            ),
            "a live /proc reading is the answer",
        );
        assert_eq!(ps_calls, 0, "ps is not spawned when /proc observed");
        assert_eq!(kill_calls, 0, "kill -0 is not spawned when /proc observed");
    }

    #[cfg(unix)]
    fn spawn_group(program: &str, args: &[&str]) -> (Child, u32) {
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
        assert!(pgid > 1, "a spawned child must own a private group, not a broadcast target");
        (child, pgid)
    }

    #[cfg(unix)]
    struct GroupGuard(u32);

    #[cfg(unix)]
    impl Drop for GroupGuard {
        fn drop(&mut self) {
            let Ok(pid) = super::signed_pgid(self.0) else {
                return;
            };
            let _ = Command::new("kill")
                .args(kill_group_args("KILL", pid))
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
        assert!(head_gone_with_grandchild, "the head must exit leaving a TERM-ignoring grandchild in its group");
        terminate_pgid(pgid).expect("SIGKILL must finish a group that ignored SIGTERM");
        assert!(!any_process_in_group(pgid), "no member of the lane group survives teardown");
    }

    /// Owns one test child and reaps it on every path, including a panic before
    /// `observe` / `record_spawned` succeed. This case has no grandchildren, so
    /// `Child::kill` is enough — never a bare pid taken from an unowned table.
    #[cfg(target_os = "macos")]
    struct OwnedChild(Option<Child>);

    #[cfg(target_os = "macos")]
    impl OwnedChild {
        fn hold(child: Child) -> Self {
            Self(Some(child))
        }

        fn pid(&self) -> u32 {
            self.0.as_ref().expect("the guard still owns the child").id()
        }

        fn is_running(&mut self) -> bool {
            self.0.as_mut().is_some_and(|child| matches!(child.try_wait(), Ok(None)))
        }

        fn reap(&mut self) -> Option<ExitStatus> {
            let mut child = self.0.take()?;
            let _ = child.kill();
            child.wait().ok()
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.reap();
        }
    }

    #[cfg(target_os = "macos")]
    const SAMPLE_BOOT_SESSION_UUID: &str = "123e4567-e89b-12d3-a456-426614174000";

    #[cfg(target_os = "macos")]
    fn bsdinfo(pid: u32, status: u32, group_id: u32, secs: u64, micros: u64) -> libc::proc_bsdinfo {
        // SAFETY: `proc_bsdinfo` is a C POD of integers and byte arrays; the
        // zero bit pattern is valid for every field.
        let mut info: libc::proc_bsdinfo = unsafe { zeroed() };
        info.pbi_pid = pid;
        info.pbi_status = status;
        info.pbi_pgid = group_id;
        info.pbi_start_tvsec = secs;
        info.pbi_start_tvusec = micros;
        info
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_pid_as_proc_pid_rejects_zero_and_overflow() {
        assert_eq!(super::pid_as_proc_pid(1), Some(1));
        assert_eq!(super::pid_as_proc_pid(0), None, "pid 0 is not a user process");
        assert_eq!(
            super::pid_as_proc_pid(u32::MAX),
            None,
            "a pid that does not fit c_int cannot be passed to proc_pidinfo",
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_pidinfo_size_must_match_exactly() {
        let expected = 64;
        assert!(super::pidinfo_size_matches(expected, expected));
        assert!(!super::pidinfo_size_matches(expected - 1, expected), "a short kernel write is unreadable");
        assert!(!super::pidinfo_size_matches(-1, expected), "a negative proc_pidinfo return is unreadable");
        assert!(!super::pidinfo_size_matches(expected + 1, expected), "an oversize return is unreadable");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_starttime_micros_rejects_invalid_usec_and_overflow() {
        assert_eq!(super::macos_starttime_micros(1, 1), Some(1_000_001));
        assert_eq!(super::macos_starttime_micros(0, 0), Some(0));
        assert_eq!(super::macos_starttime_micros(1, 1_000_000), None, "usec must be strictly less than 1_000_000");
        assert_eq!(super::macos_starttime_micros(u64::MAX, 0), None, "secs * 1_000_000 must not overflow");
        assert_eq!(super::macos_starttime_micros(u64::MAX / 1_000_000 + 1, 0), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_boot_session_uuid_parser_rejects_malformed_bytes() {
        let valid = format!("{SAMPLE_BOOT_SESSION_UUID}\0");
        assert_eq!(super::parse_boot_session_uuid(valid.as_bytes()).as_deref(), Some(SAMPLE_BOOT_SESSION_UUID),);
        assert_eq!(super::parse_boot_session_uuid(b""), None, "empty is not NUL-terminated");
        assert_eq!(super::parse_boot_session_uuid(b"\0"), None, "empty body before NUL");
        assert_eq!(super::parse_boot_session_uuid(b"   \0"), None, "whitespace-only is not a UUID");
        assert_eq!(super::parse_boot_session_uuid(SAMPLE_BOOT_SESSION_UUID.as_bytes()), None, "missing trailing NUL");
        assert_eq!(super::parse_boot_session_uuid(b"abc\0def\0"), None, "embedded NUL");
        assert_eq!(super::parse_boot_session_uuid(&[0xff, 0]), None, "invalid UTF-8");
        assert_eq!(super::parse_boot_session_uuid(b"not-a-uuid-string-at-all-nope-nope-\0"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_from_bsdinfo_rejects_mismatch_zombie_and_bad_start() {
        let boot = format!("{SAMPLE_BOOT_SESSION_UUID}\0");
        let live = bsdinfo(7, libc::SRUN, 9, 1, 2);
        let identity = super::from_bsdinfo(7, &live, boot.as_bytes()).expect("a running non-leader is observable");
        assert_eq!(identity.pid, 7);
        assert_eq!(identity.pgid, 9, "observe records the kernel pgid, including a non-leader");
        assert_eq!(identity.starttime, 1_000_002);
        assert_eq!(identity.boot_id, SAMPLE_BOOT_SESSION_UUID);

        assert!(super::from_bsdinfo(8, &live, boot.as_bytes()).is_none(), "pbi_pid must equal the requested pid");
        assert!(
            super::from_bsdinfo(7, &bsdinfo(7, libc::SZOMB, 7, 1, 0), boot.as_bytes()).is_none(),
            "a zombie is gone"
        );
        assert!(super::from_bsdinfo(7, &bsdinfo(7, libc::SRUN, 7, 1, 1_000_000), boot.as_bytes()).is_none());
        assert!(super::from_bsdinfo(7, &bsdinfo(7, libc::SRUN, 7, u64::MAX, 0), boot.as_bytes()).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_spawn_identity_is_recorded_and_reattaches_without_proc() {
        // Tripwire: macOS must record pid, own pgid, start microseconds, and a
        // nonempty boot-session UUID without `/proc`, and re-attach only when
        // those match. The `Child` is under the guard before the first observe,
        // so a RED still reaps.
        let (spawned, _) = spawn_group("sleep", &["60"]);
        let mut child = OwnedChild::hold(spawned);
        let pid = child.pid();
        let evidence = tempfile::tempdir().expect("an evidence directory for the identity record");

        super::record_spawned(evidence.path(), pid);
        let recorded = ProcessIdentity::read(evidence.path())
            .expect("record_spawned writes an identity beside the evidence on macOS without /proc");
        assert_eq!(recorded.pid, pid, "the record names the spawned child");
        assert_eq!(recorded.pgid, pid, "process_group(0) makes the child its own group leader");
        assert!(!recorded.boot_id.is_empty(), "a macOS identity carries a nonempty boot-session token");

        let attached = recorded.attach().expect("the recorded identity re-attaches to the live child");
        assert_eq!(attached, recorded, "a second observation matches pid, pgid, starttime, and boot_id");
        assert_eq!(
            ProcessIdentity::observe(pid).expect("the child is still observable"),
            recorded,
            "observe agrees with the recorded identity",
        );

        let mut wrong_start = recorded.clone();
        wrong_start.starttime = wrong_start.starttime.wrapping_add(1);
        assert!(wrong_start.attach().is_none(), "a recycled pid with a different start time must not attach");
        assert!(child.is_running(), "a starttime mismatch must not kill the child");

        let mut wrong_boot = recorded;
        wrong_boot.boot_id.push_str("-tampered");
        assert!(wrong_boot.attach().is_none(), "a reboot-recycled pid must not attach");
        assert!(child.is_running(), "a boot_id mismatch must not kill the child");

        assert!(child.reap().is_some(), "the owned child wait succeeded");
        assert!(ProcessIdentity::observe(pid).is_none(), "a reaped pid is not a live identity");
    }
}
