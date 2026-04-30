//! `aether-scenario` — declarative scenario runner for the
//! test-bench chassis (ADR-0067).
//!
//! A `Script` enumerates the steps to execute against a freshly-booted
//! `TestBench`: advance ticks, capture a frame, assert visual
//! properties, drive components via `LoadComponent` / `SendMail`. The
//! `Runner` walks the steps and produces a `RunReport` whose `passed`
//! flag is the gate CI consumes.
//!
//! `SendMail` consults `aether_kinds::descriptors::all()` to look up
//! the kind by name, then encodes YAML params into wire bytes through
//! `aether_params_codec::encode_schema` — the same path the hub uses
//! for `mcp__aether-hub__send_mail`. Adding a new kind to the
//! substrate makes it sendable from a scenario script automatically.

mod report;
mod runner;
mod script;
pub mod test_helpers;
mod visual;

pub use aether_scenario_macros::scenario_dir;
pub use report::{RunReport, StepReport, StepStatus};
pub use runner::{Runner, RunnerError, run_yaml_str};
pub use script::{Check, Script, Step, parse_script};
pub use visual::{Image, ImageError, decode_png, differs_from_background, not_all_black};
