//! Issue 6983: a guest's held reply survives `replace_component` of the guest
//! that holds it, and a replace that would strand one is refused
//! (ADR-0243 §6).
//!
//! Each scenario has `test.held.requester` send one detached `HeldRequest` to
//! a held actor, replaces that actor while the reply is owed, then releases
//! it. The held reply lands on the detached request's chain, which no harness
//! step joins, so each scenario polls the requester's match count as its
//! barrier before it counts `HeldReplyMatched` reports.
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
use aether_test_fixtures_kinds::{
    CountQuery, CountReport, HELD_TARGET_FORGETTER, HELD_TARGET_KEEPER, HELD_TARGET_RELAY, HeldReplyMatched,
    ReleaseCarried, ReleaseHeld, RunHeldRequest,
};

const FIXTURE_CRATE: &str = "aether_test_fixtures_bundle";
const RESHAPED_CRATE: &str = "aether_test_fixtures_carry_reshaped";
const HOLDER: &str = "test.carry.holder";
const RELAY: &str = "test.held.relay";
const KEEPER: &str = "test.held.keeper";
const FORGETTER: &str = "test.held.forgetter";
const REQUESTER: &str = "test.held.requester";
const RESHAPED_RELAY: &str = "test.held.reshaped_relay";

/// The held actor a scenario replaces, and how its held reply is released.
#[derive(Clone, Copy)]
enum Holder {
    /// `test.held.relay`, released through the correlation-carry holder.
    Relay,
    /// `test.held.keeper`, released directly.
    Keeper,
    /// `test.held.forgetter`, released directly.
    Forgetter,
}

impl Holder {
    const fn export(self) -> &'static str {
        match self {
            Self::Relay => RELAY,
            Self::Keeper => KEEPER,
            Self::Forgetter => FORGETTER,
        }
    }

    const fn target(self) -> u32 {
        match self {
            Self::Relay => HELD_TARGET_RELAY,
            Self::Keeper => HELD_TARGET_KEEPER,
            Self::Forgetter => HELD_TARGET_FORGETTER,
        }
    }
}

/// Load the held actors and the requester, send one held request to
/// `holder`, replace `holder` in place with the `export` of the module
/// `replacement_crate` builds, release the held reply, and wait for the
/// requester to match it. Returns the harness to count reports on and the
/// swap's result, or `None` when a fixture wasm is not built.
fn replace_while_held(
    holder: Holder,
    replacement_crate: &str,
    export: &str,
) -> Option<(SubstrateHarness, ReplaceResult)> {
    let wasm = fs::read(require_wasm(FIXTURE_CRATE)?).expect("read fixture wasm");
    let replacement = fs::read(require_wasm(replacement_crate)?).expect("read replacement wasm");

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
    let (reply_holder, _) = load(HOLDER);
    let (_, relay_path) = load(RELAY);
    let (keeper, keeper_path) = load(KEEPER);
    let (forgetter, forgetter_path) = load(FORGETTER);
    let (requester, _) = load(REQUESTER);

    let (target, release) = match holder {
        Holder::Relay => (relay_path, HarnessOp::send_and_settle(reply_holder, &ReleaseCarried)),
        Holder::Keeper => (keeper_path, HarnessOp::send_and_settle(keeper, &ReleaseHeld)),
        Holder::Forgetter => (forgetter_path, HarnessOp::send_and_settle(forgetter, &ReleaseHeld)),
    };

    let steps = vec![
        ("request", HarnessOp::send_and_settle(requester, &RunHeldRequest { tag: 1, target: holder.target() })),
        (
            "swap",
            HarnessOp::send_and_await_reply(
                &harness.actor_ref::<ComponentHostCapability>(),
                &ReplaceComponent {
                    target,
                    wasm: replacement,
                    drain_timeout_ms: None,
                    config: Vec::new(),
                    export: Some(export.to_owned()),
                },
            ),
        ),
        ("release", release),
        ("matched", HarnessOp::poll_until(requester, &CountQuery, |report: &CountReport| report.count >= 1)),
    ];

    let result =
        harness.execute(steps).unwrap_or_else(|error| panic!("held-reply sequence for {}: {error}", holder.export()));
    let swap = result.reply::<ReplaceResult>("swap").expect("decode ReplaceResult");

    Some((harness, swap))
}

fn assert_one_match(harness: &SubstrateHarness) {
    assert_eq!(
        harness.count_observed(HeldReplyMatched::NAME),
        1,
        "the held reply must reach its requester exactly once; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

#[test]
fn a_held_reply_in_a_carried_context_answers_after_replace() {
    // Catches: the successor failing to claim a ticket restored with the
    // carried request context, or the host losing the held reply-table slot
    // on replace, so the relay's answer never reaches the requester.
    let Some((harness, swap)) = replace_while_held(Holder::Relay, FIXTURE_CRATE, RELAY) else {
        return;
    };

    assert!(matches!(swap, ReplaceResult::Ok { .. }), "replace_component: {swap:?}");
    assert_one_match(&harness);
}

#[test]
fn a_held_reply_in_saved_state_answers_after_replace() {
    // Catches: the dehydrate encoder refusing a `Held`, or the rehydrate
    // decode ctx not granting its claim, so the keeper's saved reply is
    // lost across the replace.
    let Some((harness, swap)) = replace_while_held(Holder::Keeper, FIXTURE_CRATE, KEEPER) else {
        return;
    };

    assert!(matches!(swap, ReplaceResult::Ok { .. }), "replace_component: {swap:?}");
    assert_one_match(&harness);
}

#[test]
fn an_unsaved_held_reply_refuses_the_replace_and_the_old_guest_answers() {
    // Catches: the host ignoring `DEHYDRATE_HELD_UNSAVED` and swapping the
    // forgetter out with its reply unsaved, or the rollback losing the reply
    // table, so the reinstated guest's answer never arrives.
    let Some((harness, swap)) = replace_while_held(Holder::Forgetter, FIXTURE_CRATE, FORGETTER) else {
        return;
    };

    match swap {
        ReplaceResult::Err { error } => assert!(
            error.contains("a held reply is live and was not saved"),
            "the refusal must name the unsaved held reply: {error}",
        ),
        ReplaceResult::Ok { .. } => panic!("a replace that strands a live held reply was accepted"),
    }
    assert_one_match(&harness);
}

#[test]
fn a_replacement_that_changed_a_held_reply_kind_is_refused() {
    // Catches: `Ticket` hashing dropping the reply id, so the reshaped relay
    // context shares the bundle's `KindId`, the replace is accepted, and the
    // successor's take of the carried context decodes a `Held` of the wrong
    // reply kind.
    let Some((harness, swap)) = replace_while_held(Holder::Relay, RESHAPED_CRATE, RESHAPED_RELAY) else {
        return;
    };

    match swap {
        ReplaceResult::Err { error } => assert!(
            error.contains(
                "replacement does not declare its carried request context aether.test_fixtures.held_relay_context"
            ),
            "the refusal must name the carried held relay context: {error}",
        ),
        ReplaceResult::Ok { .. } => panic!("a replacement that changed a carried held reply kind was accepted"),
    }
    assert_one_match(&harness);
}
