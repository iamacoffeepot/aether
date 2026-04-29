//! Exercises the `scenario_dir!` proc-macro: scans
//! `tests/fixtures/scenarios` and emits one `#[test]` per `.yml`
//! file. Each generated test boots a fresh `TestBench`, runs the
//! script, and asserts the report passed. Because `run_yaml_str`
//! boots its own bench, every test gets fresh GPU state — no
//! cross-contamination.
//!
//! These tests also exercise the wgpu adapter on driverless runners.
//! `run_yaml_str` will return `Boot` errors there, which the
//! generated `expect("run scenario")` propagates as a test panic. To
//! keep CI green on Linux we rely on `mesa-vulkan-drivers` being
//! installed (CI workflow); on a developer box without Mesa, these
//! tests fail loudly rather than silently skip — that's the
//! agent-facing semantic the scenario runner wants.

aether_scenario::scenario_dir!("tests/fixtures/scenarios");
