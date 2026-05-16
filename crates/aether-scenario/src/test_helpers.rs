//! Per-component scenario test helpers. The chassis-side helpers
//! (`require_runtime`, `init_save_sandbox`, `test_namespace_roots`,
//! `write_fixture`, `has_wgpu_adapter`, `locate_component_wasm`)
//! live in `aether_substrate_bundle::test_bench::test_helpers` and
//! are re-exported here for the existing consumer surface (issue
//! 821).
//!
//! The two helpers that bind to the scenario vocabulary —
//! `run_or_panic` (wraps `Runner::run`) and `tick_to` (builds a
//! `Step::SendMail`) — stay in this crate because they reference
//! `Script` / `Step` directly. Both retire in PR 2 of issue 821
//! along with the Script vocabulary itself.

use aether_substrate_bundle::test_bench::TestBench;

pub use aether_substrate_bundle::test_bench::test_helpers::{
    has_wgpu_adapter, init_save_sandbox, locate_component_wasm, require_runtime,
    test_namespace_roots, write_fixture,
};

use crate::{Runner, Script, Step};

/// Build a `SendMail` step that fires a direct `aether.tick` to
/// `mailbox` so the next `Capture` frame sees fresh render-sink
/// emissions.
///
/// Background: `TestBench::capture` runs its frame with
/// `dispatch_tick=false` (capture is a state snapshot, not a tick
/// advance). The render sink's vert buffer is consumed-and-replaced
/// every frame, so a component that only emits geometry on `on_tick`
/// will paint nothing during the capture frame even though the
/// previous `Advance` ticked it. Pushing `aether.tick` to the
/// component's mailbox right before `Capture` queues a tick that
/// drains alongside the capture request, populating the buffer
/// before the offscreen render reads it.
pub fn tick_to(mailbox: &str) -> Step {
    Step::SendMail {
        recipient: mailbox.to_owned(),
        kind: "aether.tick".to_owned(),
        params: serde_yml::Value::Null,
    }
}

/// Run the script and panic with a structured failure report if any
/// step did not pass. Collapses the
/// `let report = Runner::run(...); assert!(report.passed, ...)` pair
/// every consumer file repeats per `Script`.
///
/// The panic message includes the script name (so a multi-script
/// test file identifies which script regressed) and the per-step
/// debug dump.
pub fn run_or_panic(bench: &mut TestBench, script: &Script) {
    let report = Runner::run(bench, script);
    assert!(
        report.passed,
        "{} failed:\n{:#?}",
        script.name, report.steps,
    );
}
