//! Issue 6400: a guest's correlation counter carries across
//! `replace_component`, so a request still pending from before the swap
//! never shares its id with one the replacement sends (ADR-0139 §3 / §4).
//!
//! Issue 6409: a guest's reply table carries across `replace_component` with
//! its mailbox slot, so a reply handle parked before the swap still answers
//! its own requester, and the replacement never reissues its number
//! (ADR-0017 §4, amended).
//!
//! Issue 6422: a guest's reply-lineage counter carries across
//! `replace_component` with its correlation counter, so a reply the
//! replacement sends never reuses the trace `MailId` of one its predecessor
//! already sent (ADR-0080 §1).
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a
//! hard panic there.

use std::fs;

use aether_actor::ErasedActorRef;
use aether_component::ComponentHostCapability;
use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::trace::{TraceEvent, TraceTail, TraceTailResult};
use aether_kinds::{LoadComponent, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_kinds::{CarriedReplyMatched, CarriedRequestResult, ReleaseCarried, RunCarriedRequest};

const FIXTURE_CRATE: &str = "aether_test_fixtures_bundle";
const REQUESTER: &str = "test.carry.requester";
const HOLDER: &str = "test.carry.holder";

/// Load the holder and the requester, send request 1, replace the `swapped`
/// export in place, send request 2, then release both parked replies. With
/// `release_before_swap`, the holder also releases between request 1 and the
/// swap, so the pre-swap instance answers the tag-1 handle itself. Returns
/// the harness to count matched replies on and the holder's reference, or
/// `None` when the fixture wasm is not built.
fn release_across_swap(swapped: &str, release_before_swap: bool) -> Option<(SubstrateHarness, ErasedActorRef)> {
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

    let mut steps = vec![("request_1", HarnessOp::send_and_settle(requester, &RunCarriedRequest { tag: 1 }))];
    if release_before_swap {
        steps.push(("release_before_swap", HarnessOp::send_and_settle(holder, &ReleaseCarried)));
    }
    steps.extend([
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
    ]);

    let result = harness.execute(steps).expect("carried-request sequence");
    match result.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    Some((harness, holder))
}

#[test]
fn a_replaced_guest_never_reuses_a_pending_request_id() {
    // Catches: the replacement's correlation counter restarting at 1, so its
    // first request overwrites the rehydrated context of the still-pending
    // request 1 and the late reply to that old request takes it.
    let Some((harness, _)) = release_across_swap(REQUESTER, false) else {
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
    let Some((harness, _)) = release_across_swap(HOLDER, false) else {
        return;
    };

    assert_eq!(
        harness.count_observed(CarriedReplyMatched::NAME),
        2,
        "both parked replies must answer their own requester across the holder's replace; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

#[test]
fn a_replaced_guest_never_reuses_a_reply_mail_id() {
    // Catches: the replacement's reply-lineage counter restarting at `1 << 63`,
    // so its first reply reuses the `MailId` of its predecessor's first reply
    // and the trace fold, which keys nodes by `MailId`, merges the two.
    let Some((mut harness, holder)) = release_across_swap(HOLDER, true) else {
        return;
    };

    let result = harness
        .execute(vec![(
            "tail",
            HarnessOp::send_and_await_reply(holder, &TraceTail { max: 0, since: None, root: None }),
        )])
        .expect("holder trace tail");
    let entries = match result.reply::<TraceTailResult>("tail").expect("decode TraceTailResult") {
        TraceTailResult::Ok { entries, .. } => entries,
        TraceTailResult::Err { error } => panic!("aether.trace.tail: {error}"),
    };
    let reply_ids: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry.event {
            TraceEvent::Sent { mail_id, kind, .. } if kind == CarriedRequestResult::ID => Some(mail_id),
            _ => None,
        })
        .collect();

    assert_eq!(reply_ids.len(), 2, "the holder sends one reply before the replace and one after: {reply_ids:?}");
    assert_ne!(reply_ids[0], reply_ids[1], "the replacement's reply reused its predecessor's reply MailId");
}
