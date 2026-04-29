//! `RunReport` — what the runner returns after walking a script.
//! Each step gets a `StepReport`; the top-level `passed` flag is the
//! AND of every step's success. The CLI surfaces this as exit code,
//! Rust tests assert on `passed`, and human-readers eyeball the
//! per-step status to find the first failure.

/// Outcome of a single step. `Pass` carries no detail; `Fail` carries
/// a one-line reason fit for log output.
#[derive(Debug, Clone)]
pub enum StepStatus {
    Pass,
    Fail(String),
}

impl StepStatus {
    pub fn is_pass(&self) -> bool {
        matches!(self, StepStatus::Pass)
    }
}

/// Per-step summary. `index` is the 0-based position in the original
/// script — duplicating it here means a serialized report still
/// pinpoints the failed step without re-zipping with the source.
#[derive(Debug, Clone)]
pub struct StepReport {
    pub index: usize,
    pub op: &'static str,
    pub status: StepStatus,
}

/// Top-level run output. `passed` is the AND of every `StepReport`'s
/// status. A short-circuit failure (e.g. assert before capture) still
/// produces a fully-populated `steps` list — later steps are reported
/// as `Fail("skipped: prior step failed")` so the CLI's output stays
/// regular regardless of where the script broke.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub script_name: String,
    pub steps: Vec<StepReport>,
    pub passed: bool,
}

impl RunReport {
    /// Build an empty report for a script that hasn't started running
    /// (used when boot fails before any step executes).
    pub fn boot_failed(script_name: String, error: String) -> Self {
        Self {
            script_name,
            steps: vec![StepReport {
                index: 0,
                op: "boot",
                status: StepStatus::Fail(error),
            }],
            passed: false,
        }
    }

    /// First failing step's status, if any. Useful for terse CLI
    /// output: "script X failed at step N: <reason>".
    pub fn first_failure(&self) -> Option<&StepReport> {
        self.steps.iter().find(|s| !s.status.is_pass())
    }
}
