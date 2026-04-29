//! Exercises the `smoke_dir!` proc-macro: scans `tests/fixtures/smokes`
//! and emits one `#[test]` per `.yml` file. Each generated test boots
//! a fresh `TestBench`, runs the script, and asserts the report
//! passed. Because `run_yaml_str` boots its own bench, every test
//! gets fresh GPU state — no cross-contamination.
//!
//! These tests also exercise the wgpu adapter on driverless runners.
//! `run_yaml_str` will return `Boot` errors there, which the
//! generated `expect("run smoke")` propagates as a test panic. To
//! keep CI green on Linux we rely on `mesa-vulkan-drivers` being
//! installed (CI workflow); on a developer box without Mesa, these
//! tests fail loudly rather than silently skip — that's the
//! agent-facing semantic the smoke runner wants.

aether_smoke::smoke_dir!("tests/fixtures/smokes");
