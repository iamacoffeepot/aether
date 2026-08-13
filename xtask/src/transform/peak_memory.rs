//! How much memory a lane's run actually needed, when the host can say (#4912).
//!
//! The lane concurrency ceiling is a memory question — several `-j8` builds
//! coexist in one host's RAM, several `-j32` ones do not — and it has been
//! calibrated from spike estimates rather than from production laps. So a run
//! reports what it peaked at, beside the sccache counters it already reports:
//! `Maximum resident set size`, as `/usr/bin/time -v` reads it out of the
//! wait-status rusage.
//!
//! That reading covers the child **and every descendant it waited for**, which is
//! what makes it the right number here: the lane's own process is trivial, and
//! what fills the machine is the cargo it spawns and the rustc processes under
//! that.
//!
//! It is host tooling, exactly like sccache. GNU time is the Linux fleet host's;
//! a BSD `/usr/bin/time` does not take `-v` at all, and a container may carry no
//! `time` binary. Any of those is a host that cannot measure this, and the lane
//! then runs its command unwrapped and stamps no reading — never a zero, which
//! would read as a run that used no memory, and never a failed lane, which would
//! trade a measurement for the work it was measuring.

use std::cell::Cell;
use std::process::{Command, Stdio};

/// The wrapper, at its absolute path rather than through `PATH`: `time` is a
/// shell builtin in most shells, and the builtin takes no `-v`.
const TIME: &str = "/usr/bin/time";

/// The flag that turns the one-line summary into the verbose report the
/// `Maximum resident set size` line lives in.
const VERBOSE: &str = "-v";

/// The line the verbose report opens with. Its position is where the wrapper's
/// output starts and the wrapped command's own stderr ends.
const REPORT_MARKER: &str = "Command being timed:";

/// The report line this reads. GNU time labels it `(kbytes)` and takes it from
/// `ru_maxrss`, which the kernel reports in kibibytes.
const MAXIMUM_RESIDENT: &str = "Maximum resident set size";

/// The key lane evidence carries the reading under.
const EVIDENCE_KEY: &str = "peak_resident_bytes";

/// The host's peak-memory wrapper, held across one lane run.
pub(super) struct PeakMemory {
    /// Whether this host's `/usr/bin/time` produced a report the reader
    /// understands. False leaves every command unwrapped.
    available: bool,
    /// The largest reading any run under this wrapper reported. A lane runs
    /// several commands (seven verify members, each possibly preceded by a
    /// pre-build), and what the concurrency model needs is the high-water mark
    /// one lane reached, not the last member to finish.
    observed: Cell<Option<u64>>,
}

/// The host's wrapper, or an unavailable one when it has none.
///
/// The probe is a real wrapped run: a host answers it with a report this reader
/// parses, or it is a host that cannot measure this. Probing by asking whether
/// the file exists would pass on macOS, where `/usr/bin/time` is BSD time and
/// refuses `-v` outright — every lane command would then fail to spawn its work.
pub(super) fn detect() -> PeakMemory {
    let probed = Command::new(TIME)
        .args([VERBOSE, "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .ok()
        .filter(|probed| probed.status.success())
        .is_some_and(|probed| split_report(&probed.stderr).1.is_some());
    PeakMemory { available: probed, observed: Cell::new(None) }
}

impl PeakMemory {
    /// A [`Command`] running `program` under the wrapper, or `program` itself on
    /// a host without one. Arguments are appended by the caller either way, since
    /// the wrapper takes the whole command line after its own flags.
    pub(super) fn command(&self, program: &str) -> Command {
        if !self.available {
            return Command::new(program);
        }
        let mut wrapped = Command::new(TIME);
        wrapped.args([VERBOSE, program]);
        wrapped
    }

    /// Record what a wrapped run reported and hand back its `stderr` with the
    /// wrapper's own report removed.
    ///
    /// The removal is not cosmetic. A member's stderr becomes the log a `Refine`
    /// re-entry is handed, and the distiller falls back to the log's *tail* when
    /// it recognizes no diagnostic in it — so a report left on the end would
    /// replace the failure a reader needs with twenty lines about page faults.
    pub(super) fn take_report(&self, mut stderr: Vec<u8>) -> Vec<u8> {
        let (body_len, peak) = {
            let (body, peak) = split_report(&stderr);
            (body.len(), peak)
        };
        self.record(peak);
        stderr.truncate(body_len);
        stderr
    }

    /// Record what a wrapped run reported, for a caller whose stderr is not kept
    /// — the model lanes, which read theirs only to explain a failed exit.
    pub(super) fn observe(&self, stderr: &[u8]) {
        self.record(split_report(stderr).1);
    }

    /// The largest reading this run's commands reported, in bytes, or `None` on a
    /// host that could not measure one.
    pub(super) fn peak_resident_bytes(&self) -> Option<u64> {
        self.observed.get()
    }

    /// Keep `peak` when it is the largest seen so far.
    fn record(&self, peak: Option<u64>) {
        if let Some(peak) = peak {
            self.observed.set(Some(self.observed.get().map_or(peak, |seen| seen.max(peak))));
        }
    }
}

/// Stamp what a model lane's run peaked at onto its evidence envelope.
///
/// Presence-driven like the sccache counters beside it: a host that cannot
/// measure this stamps no key, so a reader sees "unmeasured" rather than a zero
/// that claims a run which allocated nothing.
pub(super) fn stamp(evidence: &mut serde_json::Value, peak_resident_bytes: Option<u64>) {
    if let Some(bytes) = peak_resident_bytes
        && let Some(object) = evidence.as_object_mut()
    {
        object.insert(EVIDENCE_KEY.to_owned(), serde_json::json!(bytes));
    }
}

/// Split a wrapped run's `stderr` into the command's own output and the peak the
/// wrapper reported for it.
///
/// The whole reading is optional and its absence is ordinary: an unwrapped run
/// (no wrapper on this host), a wrapper whose report shape this does not
/// recognize, or output truncated before the report all leave the stderr whole
/// and report nothing.
fn split_report(stderr: &[u8]) -> (&[u8], Option<u64>) {
    let Some(start) = report_start(stderr) else {
        return (stderr, None);
    };
    (&stderr[..start], parse_maximum_resident_bytes(&String::from_utf8_lossy(&stderr[start..])))
}

/// The byte offset the wrapper's report begins at — the start of the line
/// carrying [`REPORT_MARKER`].
///
/// The **last** such line, because the wrapped command is free to have printed
/// the same text itself; the report is what comes after everything the command
/// wrote.
fn report_start(stderr: &[u8]) -> Option<usize> {
    let marker = stderr.windows(REPORT_MARKER.len()).rposition(|window| window == REPORT_MARKER.as_bytes())?;
    Some(stderr[..marker].iter().rposition(|byte| *byte == b'\n').map_or(0, |newline| newline + 1))
}

/// The `Maximum resident set size` line's value, converted from the kibibytes
/// GNU time prints to bytes so the evidence carries one unambiguous unit.
fn parse_maximum_resident_bytes(report: &str) -> Option<u64> {
    report
        .lines()
        .find(|line| line.trim_start().starts_with(MAXIMUM_RESIDENT))
        .and_then(|line| line.rsplit(':').next())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|kibibytes| kibibytes.saturating_mul(1024))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{PeakMemory, split_report, stamp};

    /// A wrapper in the stated availability state, without probing the host — the
    /// tests state both hosts, and a real probe would make which one they get
    /// depend on which machine is running them.
    fn wrapper(available: bool) -> PeakMemory {
        PeakMemory { available, observed: Cell::new(None) }
    }

    /// The verbose report GNU time appends to a wrapped command's stderr,
    /// trimmed to the lines around the one this reads.
    const REPORT: &str = "\tCommand being timed: \"cargo clippy\"\n\
                          \tUser time (seconds): 41.20\n\
                          \tMaximum resident set size (kbytes): 5242880\n\
                          \tExit status: 0\n";

    #[test]
    fn a_host_without_the_wrapper_runs_the_command_itself() {
        // Tripwire: the reading is worth nothing next to the work being
        // measured. `/usr/bin/time` is GNU time on the fleet host and BSD time
        // on a developer's mac, where `-v` is not a flag at all — wrapping
        // unconditionally would turn every verify member on that host into a
        // usage error, which is a lane that fails for a reason unrelated to the
        // candidate.
        assert_eq!(wrapper(false).command("cargo").get_program(), "cargo", "an unmeasurable host runs its own command");

        let wrapped = wrapper(true).command("cargo");
        assert_eq!(wrapped.get_program(), "/usr/bin/time");
        assert_eq!(
            wrapped.get_args().collect::<Vec<_>>(),
            ["-v", "cargo"],
            "the wrapper takes the command line after its own flags, so the caller's args still append",
        );
    }

    #[test]
    fn the_report_is_read_off_the_end_of_the_commands_own_stderr() {
        let peak = wrapper(true);
        let body = peak.take_report(format!("error[E0308]: mismatched types\n{REPORT}").into_bytes());

        assert_eq!(
            String::from_utf8_lossy(&body),
            "error[E0308]: mismatched types\n",
            "the member's log keeps its diagnostics and not the wrapper's report, which would displace the \
             failure tail a Refine re-entry is handed",
        );
        assert_eq!(peak.peak_resident_bytes(), Some(5_242_880 * 1024), "kibibytes are carried as bytes");
    }

    #[test]
    fn unwrapped_or_unrecognized_output_is_left_whole_and_reports_nothing() {
        // Tripwire: the split is driven by text the wrapper prints, so anything
        // else must pass through untouched. A reader that guessed would eat the
        // tail of a log whose failure is the only thing in it — and a zero
        // stamped for an unmeasurable host would read as a run that allocated
        // nothing, which is the opposite conclusion from the true one.
        let peak = wrapper(true);
        let unwrapped = b"error: could not compile `aether-data`\n".to_vec();

        assert_eq!(peak.take_report(unwrapped.clone()), unwrapped, "a run with no report keeps its whole stderr");
        assert_eq!(peak.peak_resident_bytes(), None, "and reports no reading rather than zero");

        assert_eq!(
            split_report(b"\tCommand being timed: \"cargo doc\"\n\tUser time (seconds): 2.10\n").1,
            None,
            "a report carrying no resident-size line is not a reading",
        );
    }

    #[test]
    fn a_lane_reports_the_high_water_mark_of_its_commands() {
        // The lane runs seven members; the concurrency model needs the most any
        // one of them held at once, not whichever finished last.
        let peak = wrapper(true);
        peak.observe(REPORT.as_bytes());
        peak.observe(b"\tCommand being timed: \"cargo fmt\"\n\tMaximum resident set size (kbytes): 12000\n");

        assert_eq!(peak.peak_resident_bytes(), Some(5_242_880 * 1024), "the largest member's peak stands");
    }

    #[test]
    fn an_unmeasured_run_stamps_no_evidence_key() {
        let mut absent = serde_json::json!({ "command": "construct.implement" });
        stamp(&mut absent, None);
        assert!(absent.get("peak_resident_bytes").is_none(), "no reading means no key at all");

        let mut present = serde_json::json!({ "command": "construct.implement" });
        stamp(&mut present, Some(5_368_709_120));
        assert_eq!(present["peak_resident_bytes"], 5_368_709_120_u64);
    }
}
