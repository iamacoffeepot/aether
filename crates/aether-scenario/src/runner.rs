//! `Runner` — walks a `Script` against a borrowed `TestBench`,
//! producing a `RunReport`. The bench is `&mut` because `advance`
//! and `capture` mutate state; the runner doesn't own the bench so
//! tests can interleave scenarios with their own bench operations.
//!
//! `SendMail` resolves kinds via `aether_kinds::descriptors::all()`
//! (inventory-collected at link time) and encodes YAML params through
//! `aether_codec::encode_schema` — same path the hub uses for
//! `mcp__aether-hub__send_mail`. The runner caches the descriptor
//! lookup once per script run so each step is an `O(1)` HashMap probe.

use std::collections::HashMap;
use std::fs;

use aether_codec::encode_schema;
use aether_data::{KindDescriptor, canonical::kind_id_from_parts};
use aether_kinds::{LoadComponent, LoadResult, descriptors};
use aether_substrate_bundle::{KindId, test_bench::TestBench};
use thiserror::Error;

use crate::report::{RunReport, StepReport, StepStatus};
use crate::script::{Check, Script, Step};
use crate::visual::{Image, decode_png, differs_from_background, not_all_black};

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("yaml parse failed: {0}")]
    Parse(String),
    #[error("test-bench boot failed: {0}")]
    Boot(String),
    #[error("test-bench operation failed: {0}")]
    TestBench(String),
}

/// One-shot helper: parse a YAML script, boot a fresh `TestBench`,
/// run the script, return the report. The bench is dropped before
/// the function returns. Use this for the common "spin up, execute,
/// observe" path the CLI and proc-macro consume; for finer control
/// (existing bench, custom size, multiple scripts in one bench)
/// drive `Runner::run` directly.
pub fn run_yaml_str(yaml: &str) -> Result<RunReport, RunnerError> {
    let script =
        crate::script::parse_script(yaml).map_err(|e| RunnerError::Parse(e.to_string()))?;
    let mut bench = TestBench::start().map_err(|e| RunnerError::Boot(e.to_string()))?;
    Ok(Runner::run(&mut bench, &script))
}

/// Stateless walker. The runner threads its own `last_capture`
/// through the step loop so consecutive `Assert` steps reuse the
/// most-recent capture without re-rendering.
pub struct Runner;

impl Runner {
    /// Run the script. Returns the populated report; bench errors
    /// surface as `StepStatus::Fail` strings rather than propagating,
    /// so the caller always gets a regular report shape to display.
    pub fn run(bench: &mut TestBench, script: &Script) -> RunReport {
        // Snapshot the descriptor list once and index by name —
        // every `SendMail` step probes this map, never re-iterates
        // the inventory.
        let descriptors_owned = descriptors::all();
        let descriptors_by_name: HashMap<&str, &KindDescriptor> = descriptors_owned
            .iter()
            .map(|d| (d.name.as_str(), d))
            .collect();

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

            let status = run_step(bench, &descriptors_by_name, step, &mut last_capture);

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

fn run_step(
    bench: &mut TestBench,
    descriptors_by_name: &HashMap<&str, &KindDescriptor>,
    step: &Step,
    last_capture: &mut Option<Image>,
) -> StepStatus {
    match step {
        Step::Advance { ticks } => match bench.advance(*ticks) {
            Ok(_) => StepStatus::Pass,
            Err(e) => StepStatus::Fail(format!("advance({ticks}) failed: {e}")),
        },
        Step::Capture => match bench.capture() {
            Ok(bytes) => match decode_png(&bytes) {
                Ok(img) => {
                    *last_capture = Some(img);
                    StepStatus::Pass
                }
                Err(e) => StepStatus::Fail(format!("capture decode failed: {e}")),
            },
            Err(e) => StepStatus::Fail(format!("capture failed: {e}")),
        },
        Step::Assert { check } => run_assert(bench, check, last_capture.as_ref()),
        Step::LoadComponent { path, name } => {
            let wasm = match fs::read(path) {
                Ok(bytes) => bytes,
                Err(e) => return StepStatus::Fail(format!("read wasm {path}: {e}")),
            };
            let mail = LoadComponent {
                wasm,
                name: name.clone(),
            };
            // Issue 603: ControlPlaneCapability dispatches asynchronously
            // on its own thread, so a `send_mail` here would race with
            // the next step. Awaiting `LoadResult` makes the script's
            // sequential semantics explicit (load completes before
            // anything that depends on the loaded component runs).
            match bench.send_and_await_reply::<LoadComponent, LoadResult>("aether.control", &mail) {
                Ok(LoadResult::Ok { .. }) => StepStatus::Pass,
                Ok(LoadResult::Err { error }) => {
                    StepStatus::Fail(format!("load_component failed: {error}"))
                }
                Err(e) => StepStatus::Fail(format!("load_component dispatch: {e}")),
            }
        }
        Step::SendMail {
            recipient,
            kind,
            params,
        } => match encode_send_mail(descriptors_by_name, kind, params) {
            Err(e) => StepStatus::Fail(e),
            Ok((kind_id, bytes)) => match bench.send_bytes(recipient, kind_id, bytes) {
                Ok(()) => StepStatus::Pass,
                Err(e) => StepStatus::Fail(format!("send_bytes to {recipient}: {e}")),
            },
        },
    }
}

/// Dispatch one `Assert` check against the current bench state.
/// Visual checks require a prior capture; mail checks always read
/// `TestBench::count_observed` and never touch `last_capture`.
fn run_assert(bench: &TestBench, check: &Check, last_capture: Option<&Image>) -> StepStatus {
    match check {
        Check::NotAllBlack => match last_capture {
            None => StepStatus::Fail(
                "assert with no prior capture: add a `capture` step first".to_owned(),
            ),
            Some(img) => match not_all_black(img) {
                Ok(()) => StepStatus::Pass,
                Err(reason) => StepStatus::Fail(reason),
            },
        },
        Check::DiffersFromBackground { tolerance } => match last_capture {
            None => StepStatus::Fail(
                "assert with no prior capture: add a `capture` step first".to_owned(),
            ),
            Some(img) => match differs_from_background(img, *tolerance) {
                Ok(()) => StepStatus::Pass,
                Err(reason) => StepStatus::Fail(reason),
            },
        },
        Check::MailObserved { name, min_count } => {
            let actual = bench.count_observed(name);
            if actual >= *min_count {
                StepStatus::Pass
            } else {
                StepStatus::Fail(format!(
                    "expected `{name}` observed at least {min_count} time(s), got {actual}; observed kinds so far: {:?}",
                    bench.observed_kinds()
                ))
            }
        }
        Check::MailNotObserved { name } => {
            let actual = bench.count_observed(name);
            if actual == 0 {
                StepStatus::Pass
            } else {
                StepStatus::Fail(format!(
                    "expected `{name}` not observed, but saw {actual} occurrence(s)"
                ))
            }
        }
    }
}

/// Resolve `kind` to a descriptor, convert YAML params to JSON, and
/// run them through the schema encoder. Returns `(kind_id, bytes)`
/// ready for `TestBench::send_bytes`.
fn encode_send_mail(
    descriptors_by_name: &HashMap<&str, &KindDescriptor>,
    kind: &str,
    params: &serde_yml::Value,
) -> Result<(KindId, Vec<u8>), String> {
    let desc = descriptors_by_name
        .get(kind)
        .ok_or_else(|| format!("unknown kind: {kind}"))?;
    // YAML and JSON values share the same value-tree shape for our
    // purposes (string-keyed maps, scalars, arrays). serde_yml::Value
    // implements Serialize, so re-serializing it through serde_json
    // gives a structurally-equivalent JSON value the schema encoder
    // accepts.
    let json: serde_json::Value =
        serde_json::to_value(params).map_err(|e| format!("yaml→json {kind}: {e}"))?;
    let bytes = encode_schema(&json, &desc.schema).map_err(|e| format!("encode {kind}: {e}"))?;
    let kind_id = KindId(kind_id_from_parts(&desc.name, &desc.schema));
    Ok((kind_id, bytes))
}

fn op_label(step: &Step) -> &'static str {
    match step {
        Step::Advance { .. } => "advance",
        Step::Capture => "capture",
        Step::Assert { .. } => "assert",
        Step::LoadComponent { .. } => "load_component",
        Step::SendMail { .. } => "send_mail",
    }
}

#[cfg(test)]
mod tests {
    use crate::script::{Check, Script, Step};

    fn assert_before_capture_script() -> Script {
        Script {
            name: "early assert".to_owned(),
            steps: vec![Step::Assert {
                check: Check::NotAllBlack,
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
        assert_eq!(super::op_label(&script.steps[0]), "assert");
    }

    #[test]
    fn op_labels_cover_every_variant() {
        let cases = [
            (Step::Advance { ticks: 1 }, "advance"),
            (Step::Capture, "capture"),
            (
                Step::Assert {
                    check: Check::NotAllBlack,
                },
                "assert",
            ),
            (
                Step::LoadComponent {
                    path: "/tmp/x.wasm".to_owned(),
                    name: None,
                },
                "load_component",
            ),
            (
                Step::SendMail {
                    recipient: "aether.control".to_owned(),
                    kind: "aether.control.drop_component".to_owned(),
                    params: serde_yml::Value::Null,
                },
                "send_mail",
            ),
        ];
        for (step, expected) in &cases {
            assert_eq!(super::op_label(step), *expected);
        }
    }

    /// The descriptor lookup is the load-bearing piece — verifying
    /// the substrate's inventory contains the kind we look up.
    /// Without this, `SendMail` for that kind would silently fail at
    /// run-time. (Equivalent of the old catalog's
    /// `default_catalog_covers_load_component` test.)
    #[test]
    fn descriptors_cover_load_component_and_io_write() {
        let descs = aether_kinds::descriptors::all();
        let names: Vec<&str> = descs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"aether.control.load_component"));
        assert!(names.contains(&"aether.io.write"));
    }
}
