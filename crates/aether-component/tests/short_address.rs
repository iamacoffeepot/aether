//! ADR-0241 §5: a loaded guest is named by its own published namespace, and
//! the component host is no address parent. It declares no instanced child,
//! so `aether.component/:NAME` (ADR-0166 §5/§6) is refused as an illegal
//! segment rather than expanded to a position nothing is born at.
//!
//! Driven through a real load and the host's own address resolution, the
//! `DescribeComponent` read.

use std::fs;

use aether_actor::Addressable;
use aether_component::ComponentHostCapability;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DescribeComponent, DescribeComponentResult, LoadComponent};

const PROBE_EXPORT: &str = "test.probe";

fn describe(harness: &mut SubstrateHarness, name: &str) -> DescribeComponentResult {
    let host = harness.actor_ref::<ComponentHostCapability>();
    let described = harness
        .execute(vec![(
            "describe",
            HarnessOp::send_and_await_reply(&host, &DescribeComponent { name: name.to_owned() }),
        )])
        .expect("describe sequence");
    described.reply::<DescribeComponentResult>("describe").expect("decode DescribeComponentResult")
}

/// Catches a guest still born beneath the component host (its short path
/// would resolve to it, and its published name would not), and a host
/// placement re-added by hand: the hole would expand and the refusal would
/// be a no-live-mailbox one, not the illegal segment.
#[test]
fn a_guest_is_reached_by_its_published_name_not_the_host_short_path() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    harness
        .load_any(&LoadComponent { wasm, name: None, config: Vec::new(), export: Some(PROBE_EXPORT.to_owned()) })
        .unwrap_or_else(|error| panic!("load {PROBE_EXPORT}: {error}"));

    assert!(matches!(describe(&mut harness, PROBE_EXPORT), DescribeComponentResult::Ok { .. }));
    let short = format!("{}/:{PROBE_EXPORT}", ComponentHostCapability::NAMESPACE);
    let DescribeComponentResult::Err { error } = describe(&mut harness, &short) else {
        panic!("{short} reaches no guest");
    };
    let illegal = format!("segment `:{PROBE_EXPORT}` is not legal beneath `{}`", ComponentHostCapability::NAMESPACE);
    assert!(error.contains(&illegal), "{short} is refused as an illegal segment: {error}");
}
