//! The report: step cells from the drained samples, the session cell, and the
//! process-level readings the session cell carries.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use aether_harness_substrate::perf::harness::summarize;
use aether_harness_substrate::perf::report::{SessionCell, StepCell, TrialReport};

use crate::knobs::Knobs;

/// What the run measured, besides the step samples.
pub struct Measured {
    /// The window from the first open to the last rest.
    pub elapsed: Duration,
    /// How many sessions rested `Completed`.
    pub rested: usize,
    /// The journal root's growth over the window, in bytes.
    pub journal_bytes: u64,
}

/// The trial report for one (sessions × shape) cell.
pub fn trial(knobs: &Knobs, samples: BTreeMap<String, Vec<u64>>, measured: &Measured) -> TrialReport {
    let sessions = usize::try_from(knobs.sessions).unwrap_or(usize::MAX);
    let shape = knobs.shape();
    let steps = samples
        .into_iter()
        .map(|(step, samples)| {
            let stats = summarize(samples);
            StepCell {
                sessions,
                shape: shape.clone(),
                step,
                p50: stats.p50,
                p90: stats.p90,
                p99: stats.p99,
                max: stats.max,
                n: stats.n,
            }
        })
        .collect();
    let secs = measured.elapsed.as_secs_f64();
    let session = SessionCell {
        sessions,
        shape,
        sessions_per_sec: (measured.rested == sessions && secs > 0.0).then(|| f64::from(knobs.sessions) / secs),
        journal_bytes_per_session: measured.journal_bytes / u64::from(knobs.sessions),
        peak_rss_bytes: peak_rss_bytes(),
    };
    TrialReport::from_bloomery(steps, vec![session], git_sha())
}

/// The total size of every file under `root`, walked with an explicit stack.
pub fn journal_bytes(root: &Path) -> io::Result<u64> {
    let mut total = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                total += metadata.len();
            }
        }
    }
    Ok(total)
}

/// The process's peak resident set, from `VmHWM` in `/proc/self/status`, or
/// `None` where that file is unreadable.
fn peak_rss_bytes() -> Option<u64> {
    fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|value| value.trim().strip_suffix("kB"))
        .and_then(|kibibytes| kibibytes.trim().parse::<u64>().ok())
        .and_then(|kibibytes| kibibytes.checked_mul(1024))
}

// Dev/perf tooling: optional CI-provided git-sha override, as perf-trial reads
// it — not a capability, no config layer in scope.
#[allow(clippy::disallowed_methods)] // aether-suppression-request: the optional AETHER_PERF_GIT_SHA stamp, read as perf-trial reads it (owner sign-off in #7281)
fn git_sha() -> Option<String> {
    if let Ok(sha) = env::var("AETHER_PERF_GIT_SHA")
        && !sha.is_empty()
    {
        return Some(sha);
    }
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|sha| sha.trim().to_owned())
        .filter(|sha| !sha.is_empty())
}
