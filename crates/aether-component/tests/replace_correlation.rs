//! Issue 6400: a guest's correlation counter carries across
//! `replace_component`, so a request still pending from before the swap
//! never shares its id with one the replacement sends (ADR-0139 §3 / §4).
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a
//! hard panic there.

use std::fs;

use aether_component::ComponentHostCapability;
use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_kinds::{CarriedReplyMatched, ReleaseCarried, RunCarriedRequest};

const FIXTURE_CRATE: &str = "aether_test_fixtures_bundle";
const REQUESTER: &str = "test.carry.requester";

#[test]
fn a_replaced_guest_never_reuses_a_pending_request_id() {
    // Catches: the replacement's correlation counter restarting at 1, so its
    // first request overwrites the rehydrated context of the still-pending
    // request 1 and the late reply to that old request takes it.
    let Some(wasm_path) = require_wasm(FIXTURE_CRATE) else {
        return;
    };

    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");

    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let (holder, _) = harness
        .load_any(&LoadComponent {
            wasm: wasm.clone(),
            name: None,
            config: Vec::new(),
            export: Some("test.carry.holder".to_owned()),
        })
        .unwrap_or_else(|error| panic!("load holder: {error}"));
    let (requester, path) = harness
        .load_any(&LoadComponent {
            wasm: wasm.clone(),
            name: None,
            config: Vec::new(),
            export: Some(REQUESTER.to_owned()),
        })
        .unwrap_or_else(|error| panic!("load requester: {error}"));

    let result = harness
        .execute(vec![
            ("request_1", HarnessOp::send_and_settle(requester, &RunCarriedRequest { tag: 1 })),
            (
                "swap",
                HarnessOp::send_and_await_reply(
                    &harness.actor_ref::<ComponentHostCapability>(),
                    &ReplaceComponent {
                        target: path,
                        wasm,
                        drain_timeout_ms: None,
                        config: Vec::new(),
                        export: Some(REQUESTER.to_owned()),
                    },
                ),
            ),
            ("request_2", HarnessOp::send_and_settle(requester, &RunCarriedRequest { tag: 2 })),
            ("release", HarnessOp::send_and_settle(holder, &ReleaseCarried)),
        ])
        .expect("carried-request sequence");
    match result.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    assert_eq!(
        harness.count_observed(CarriedReplyMatched::NAME),
        2,
        "both replies must recover their own request's context across the replace; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}
