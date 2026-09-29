//! Issue 7015: a guest that holds a reply and is dropped before answering
//! sends its requester the `unanswered` value it registered when it held
//! (ADR-0243 §6), including after a replace carried the hold to a successor
//! and after a refused replace reinstated the guest.
//!
//! Each scenario has `test.held.requester` send one detached `HeldRequest` to
//! a held actor, optionally replaces that actor while the reply is owed, then
//! drops it. The unanswered reply lands on the detached request's chain,
//! which no harness step joins, but the drop's handler queues it at the
//! requester before it answers the drop. A `CountQuery` sent after the drop's
//! reply therefore reaches the requester behind it, and its answer is the
//! barrier each scenario reads before it counts the requester's reports.
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a
//! hard panic there.

use std::fs;

use aether_component::ComponentHostCapability;
use aether_data::{ErasedActorPath, Kind};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_kinds::{
    CountQuery, CountReport, HELD_TARGET_FORGETTER, HELD_TARGET_KEEPER, HeldReplyMatched, HeldReplyUnanswered,
    RunHeldRequest,
};

const FIXTURE_CRATE: &str = "aether_test_fixtures_bundle";
const HOLDER: &str = "test.carry.holder";
const RELAY: &str = "test.held.relay";
const KEEPER: &str = "test.held.keeper";
const FORGETTER: &str = "test.held.forgetter";
const REQUESTER: &str = "test.held.requester";

/// What happens to the holder between the request and its drop.
#[derive(Clone, Copy)]
enum BeforeDrop {
    /// Nothing: the guest that held is the one dropped.
    Nothing,
    /// A replace of the keeper, which saves its held reply, so the successor
    /// is dropped with the hold it carried.
    Replaced,
    /// A replace of the forgetter, refused because it saves nothing, so the
    /// reinstated guest is dropped.
    Reinstated,
}

/// Load the held actors and the requester, send one held request to the
/// keeper (or, for [`BeforeDrop::Reinstated`], the forgetter), run `before`,
/// drop the holder, and ask the requester how many replies it received,
/// which must be one. Returns the harness to count reports on and the
/// replace's result, if one ran, or `None` when the fixture wasm is not
/// built.
fn drop_while_held(before: BeforeDrop) -> Option<(SubstrateHarness, Option<ReplaceResult>)> {
    let wasm = fs::read(require_wasm(FIXTURE_CRATE)?).expect("read fixture wasm");

    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");
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
    load(HOLDER);
    load(RELAY);
    let (_, keeper_path) = load(KEEPER);
    let (_, forgetter_path) = load(FORGETTER);
    let (requester, _) = load(REQUESTER);

    let (export, path, target) = match before {
        BeforeDrop::Nothing | BeforeDrop::Replaced => (KEEPER, keeper_path, HELD_TARGET_KEEPER),
        BeforeDrop::Reinstated => (FORGETTER, forgetter_path, HELD_TARGET_FORGETTER),
    };
    let host = harness.actor_ref::<ComponentHostCapability>();
    let replace = |target: ErasedActorPath| ReplaceComponent {
        target,
        wasm: wasm.clone(),
        drain_timeout_ms: None,
        config: Vec::new(),
        export: Some(export.to_owned()),
    };

    let mut steps = vec![("request", HarnessOp::send_and_settle(requester, &RunHeldRequest { tag: 1, target }))];
    if !matches!(before, BeforeDrop::Nothing) {
        steps.push(("replace", HarnessOp::send_and_await_reply(&host, &replace(path.clone()))));
    }
    steps.extend([
        ("drop", HarnessOp::send_and_await_reply(&host, &DropComponent { target: path })),
        ("replied", HarnessOp::send_and_await_reply(requester, &CountQuery)),
    ]);

    let result = harness.execute(steps).unwrap_or_else(|error| panic!("drop while held from {export}: {error}"));
    if let DropResult::Err { error } = result.reply::<DropResult>("drop").expect("decode DropResult") {
        panic!("the holder drops: {error}");
    }
    let replied = result.reply::<CountReport>("replied").expect("decode CountReport");
    assert_eq!(replied.count, 1, "the requester has its one reply by the time the drop answers");
    let replaced =
        (!matches!(before, BeforeDrop::Nothing)).then(|| result.reply::<ReplaceResult>("replace").expect("decode"));

    Some((harness, replaced))
}

fn assert_one_unanswered(harness: &SubstrateHarness) {
    assert_eq!(
        harness.count_observed(HeldReplyUnanswered::NAME),
        1,
        "the requester must receive the registered unanswered reply exactly once; observed kinds: {:?}",
        harness.observed_kinds(),
    );
    assert_eq!(harness.count_observed(HeldReplyMatched::NAME), 0, "the holder never answered");
}

#[test]
fn dropping_a_holder_answers_unanswered() {
    // Catches: the unload releasing the held slot's hold without sending the
    // registered reply, so the requester's handler never runs and its
    // context stays stranded; or sending it twice.
    let Some((harness, _)) = drop_while_held(BeforeDrop::Nothing) else {
        return;
    };

    assert_one_unanswered(&harness);
}

#[test]
fn a_replaced_holder_answers_unanswered_on_drop() {
    // Catches: a replace losing the registration the held slot carried, so
    // the successor that restored the hold from saved state is dropped with
    // nothing to send its requester.
    let Some((harness, replaced)) = drop_while_held(BeforeDrop::Replaced) else {
        return;
    };

    assert!(matches!(replaced, Some(ReplaceResult::Ok { .. })), "replace_component: {replaced:?}");
    assert_one_unanswered(&harness);
}

#[test]
fn a_reinstated_holder_answers_unanswered_on_drop() {
    // Catches: an aborted replace dropping the registration as the reply
    // table moves back to the reinstated guest, so its drop sends nothing.
    let Some((harness, replaced)) = drop_while_held(BeforeDrop::Reinstated) else {
        return;
    };

    assert!(matches!(replaced, Some(ReplaceResult::Err { .. })), "replace_component: {replaced:?}");
    assert_one_unanswered(&harness);
}
