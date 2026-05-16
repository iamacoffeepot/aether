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
//! `aether_codec::encode_schema` — the same path the hub uses
//! for `mcp__aether-hub__send_mail`. Adding a new kind to the
//! substrate makes it sendable from a scenario script automatically.
//!
//! ## Status (issue 821)
//!
//! The YAML authoring surface — `scenario_dir!`, `parse_script`,
//! `run_yaml_str`, the `aether-scenario` bin — retired pre-adoption.
//! `test_helpers` and `visual` relocated into
//! `aether_substrate_bundle::test_bench`; this crate re-exports them
//! for the existing consumer surface and keeps the `Script` / `Step`
//! / `Check` / `Runner` Rust vocabulary that the four consumer test
//! files still drive. PR 2 of issue 821 will inline that vocabulary
//! at the call sites and retire this crate.

mod report;
mod runner;
mod script;
pub mod test_helpers;

pub use aether_substrate_bundle::test_bench::visual::{
    Image, ImageError, decode_png, differs_from_background, not_all_black,
};
pub use report::{RunReport, StepReport, StepStatus};
pub use runner::{Runner, RunnerError};
pub use script::{Check, Script, Step};
