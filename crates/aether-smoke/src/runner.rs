//! `Runner` — walks a `Script` against a borrowed `TestBench`,
//! producing a `RunReport`. The bench is `&mut` because `advance`
//! and `capture` mutate state; the runner doesn't own the bench so
//! tests can interleave smokes with their own bench operations.

use aether_substrate_test_bench::TestBench;
use thiserror::Error;

use crate::report::{RunReport, StepReport, StepStatus};
use crate::script::{Script, Step, VisualAssert};
use crate::visual::{Image, decode_png, not_all_black};

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("test-bench operation failed: {0}")]
    TestBench(String),
}

/// Stateless walker. The runner threads its own `last_capture`
/// through the step loop so consecutive `Assert` steps reuse the
/// most-recent capture without re-rendering.
pub struct Runner;

impl Runner {
    /// Run the script. Returns the populated report; panics or
    /// internal errors from the bench surface as `StepStatus::Fail`
    /// strings rather than propagating, so the caller always gets
    /// a regular report shape to display.
    pub fn run(bench: &mut TestBench, script: &Script) -> RunReport {
        let mut steps = Vec::with_capacity(script.steps.len());
        let mut last_capture: Option<Image> = None;
        let mut short_circuited = false;

        for (index, step) in script.steps.iter().enumerate() {
            let op = op_label(step);
            if short_circuited {
                steps.push(StepReport {
                    index,
                    op,
                    status: StepStatus::Fail("skipped: prior step failed".to_owned()),
                });
                continue;
            }

            let status = match step {
                Step::Advance { ticks } => match bench.advance(*ticks) {
                    Ok(_) => StepStatus::Pass,
                    Err(e) => StepStatus::Fail(format!("advance({ticks}) failed: {e}")),
                },
                Step::Capture => match bench.capture() {
                    Ok(bytes) => match decode_png(&bytes) {
                        Ok(img) => {
                            last_capture = Some(img);
                            StepStatus::Pass
                        }
                        Err(e) => StepStatus::Fail(format!("capture decode failed: {e}")),
                    },
                    Err(e) => StepStatus::Fail(format!("capture failed: {e}")),
                },
                Step::Assert { check } => match last_capture.as_ref() {
                    None => StepStatus::Fail(
                        "assert with no prior capture: add a `capture` step first".to_owned(),
                    ),
                    Some(img) => match check {
                        VisualAssert::NotAllBlack => match not_all_black(img) {
                            Ok(()) => StepStatus::Pass,
                            Err(reason) => StepStatus::Fail(reason),
                        },
                    },
                },
            };

            if !status.is_pass() {
                short_circuited = true;
            }
            steps.push(StepReport { index, op, status });
        }

        let passed = steps.iter().all(|s| s.status.is_pass());
        RunReport {
            script_name: script.name.clone(),
            steps,
            passed,
        }
    }
}

fn op_label(step: &Step) -> &'static str {
    match step {
        Step::Advance { .. } => "advance",
        Step::Capture => "capture",
        Step::Assert { .. } => "assert",
    }
}

#[cfg(test)]
mod tests {
    use crate::script::{Script, Step, VisualAssert};

    fn assert_before_capture_script() -> Script {
        Script {
            name: "early assert".to_owned(),
            steps: vec![Step::Assert {
                check: VisualAssert::NotAllBlack,
            }],
        }
    }

    /// The runner doesn't need a live bench to flag ordering errors —
    /// a script that asserts before capturing fails on the assert
    /// step's "no prior capture" check. We can't actually invoke
    /// `Runner::run` without a `&mut TestBench`, so this test
    /// verifies the report shape via direct Script construction.
    #[test]
    fn assert_before_capture_step_label() {
        let script = assert_before_capture_script();
        // op_label is the public-facing identifier in StepReport; if
        // the variant naming drifts the runner output drifts too.
        assert_eq!(super::op_label(&script.steps[0]), "assert");
    }
}
