//! Issue 6400: a guest's correlation counter carries across
//! `replace_component`, so a request still pending from before the swap
//! never shares its id with one the replacement sends (ADR-0139 §3 / §4).
//!
//! Issue 6409: a guest's reply table carries across `replace_component` with
//! its mailbox slot, so a reply handle parked before the swap still answers
//! its own requester, and the replacement never reissues its number
//! (ADR-0017 §4, amended).
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
const HOLDER: &str = "test.carry.holder";

/// Load the holder and the requester, send request 1, replace the `swapped`
/// export in place, send request 2, then release both parked replies.
/// Returns the harness to count matched replies on, or `None` when the
/// fixture wasm is not built.
fn release_across_swap(swapped: &str) -> Option<SubstrateHarness> {
    let wasm_path = require_wasm(FIXTURE_CRATE)?;

    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");

    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let mut load = |export: &str| {
        harness
            .load_any(&LoadComponent {
                wasm: wasm.clone(),
                name: None,
                config: Vec::new(),
                export: Some(export.to_owned()),
            })
            .unwrap_or_else(|error| panic!("load {export}: {error}"))
    };
    let (holder, holder_path) = load(HOLDER);
    let (requester, requester_path) = load(REQUESTER);
    let target = if swapped == HOLDER {
        holder_path
    } else {
        requester_path
    };

    let result = harness
        .execute(vec![
            ("request_1", HarnessOp::send_and_settle(requester, &RunCarriedRequest { tag: 1 })),
            (
                "swap",
                HarnessOp::send_and_await_reply(
                    &harness.actor_ref::<ComponentHostCapability>(),
                    &ReplaceComponent {
                        target,
                        wasm,
                        drain_timeout_ms: None,
                        config: Vec::new(),
                        export: Some(swapped.to_owned()),
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

    Some(harness)
}

#[test]
fn a_replaced_guest_never_reuses_a_pending_request_id() {
    // Catches: the replacement's correlation counter restarting at 1, so its
    // first request overwrites the rehydrated context of the still-pending
    // request 1 and the late reply to that old request takes it.
    let Some(harness) = release_across_swap(REQUESTER) else {
        return;
    };

    assert_eq!(
        harness.count_observed(CarriedReplyMatched::NAME),
        2,
        "both replies must recover their own request's context across the replace; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

#[test]
fn a_replaced_guest_answers_a_carried_reply_handle_to_its_own_requester() {
    // Catches: the replacement's reply table restarting at 0, so the request
    // arriving after the swap takes the carried handle's number — the carried
    // reply goes out with that request's correlation and the second reply
    // finds no entry and is dropped.
    let Some(harness) = release_across_swap(HOLDER) else {
        return;
    };

    assert_eq!(
        harness.count_observed(CarriedReplyMatched::NAME),
        2,
        "both parked replies must answer their own requester across the holder's replace; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}
