//! `aether-smoke` — declarative smoke-test runner for the test-bench
//! chassis (ADR-0067).
//!
//! A `Script` enumerates the steps to execute against a freshly-booted
//! `TestBench`: advance ticks, capture a frame, assert visual
//! properties. The `Runner` walks the steps and produces a `RunReport`
//! whose `passed` flag is the gate CI consumes.
//!
//! v1 vocabulary covers chassis-level smokes (does the bench boot?
//! does an empty capture round-trip?). Component-driving steps
//! (`load_component`, `send_mail`) land in a follow-up PR alongside
//! descriptor-based JSON encoding.

mod report;
mod runner;
mod script;
mod visual;

pub use report::{RunReport, StepReport, StepStatus};
pub use runner::{Runner, RunnerError};
pub use script::{Script, Step, VisualAssert, parse_script};
pub use visual::{Image, ImageError, decode_png, not_all_black};
