//! Publish admission on load (ADR-0241 §3/§4, issue #6865).
//!
//! A load publishes its module through the registry owner before anything
//! spawns, and admission refuses a module that exports a namespace a native
//! actor linked into the engine publishes. The squatter below is such an
//! actor, taking the bundle's `test.probe` export. It lives in its own test
//! binary because its link-time name entry makes every bundle load in the
//! same binary refuse.

use std::fs;

use aether_actor::actor;
use aether_component::ComponentHostCapability;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_substrate::BootError;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_test_fixtures_kinds::Bump;

const SQUATTED_EXPORT: &str = "test.probe";

/// A native actor linked into the test binary but never composed: linking
/// alone publishes its namespace natively.
struct Squatter {
    bumps: u32,
}

#[actor(singleton, root)]
impl NativeActor for Squatter {
    const NAMESPACE: &'static str = SQUATTED_EXPORT;
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { bumps: 0 })
    }

    /// Never sent here: an actor declares at least one handler.
    #[handler::single]
    fn on_bump(&mut self, _ctx: &mut NativeCtx<'_>, _bump: Bump) {
        self.bumps += 1;
    }
}

/// Catches admission not wired to the load, or its refusal not reaching the
/// reply: the bundle would load and a guest would stand at the
/// requested name, beside a native namespace it shares.
#[test]
fn a_module_exporting_a_native_namespace_is_refused_before_it_spawns() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let host = harness.actor_ref::<ComponentHostCapability>();

    let load = LoadComponent { wasm, name: None, config: Vec::new(), export: Some(SQUATTED_EXPORT.to_owned()) };
    let result = harness
        .execute(vec![("load", HarnessOp::send_and_await_reply(&host, &load))])
        .expect("the component host replies to the load");

    let LoadResult::Err { error } = result.reply::<LoadResult>("load").expect("decode LoadResult") else {
        panic!("a module exporting a native namespace must not load");
    };
    assert!(
        error.starts_with(&format!("module publish refused: {SQUATTED_EXPORT} is published by a native actor")),
        "the refusal names the native namespace and the rule: {error}",
    );
    let listed = harness.list_components().expect("list components");
    assert!(!listed.iter().any(|name| name == SQUATTED_EXPORT), "a refused publish spawns no guest: {listed:?}");
}
