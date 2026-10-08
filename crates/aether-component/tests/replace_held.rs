//! Issue 6983: a guest's held reply survives `replace_component` of the guest
//! that holds it, and a replace that would strand one is refused
//! (ADR-0243 §6).
//!
//! Each scenario has a held requester send one detached `HeldRequest` to a
//! held actor, republishes that actor's module while the reply is owed —
//! every loaded actor of the module moves together (ADR-0241 §7) — then
//! releases it. The held reply lands on the detached request's chain, which no harness
//! step joins, so each scenario polls the requester's match count as its
//! barrier before it counts `HeldReplyMatched` reports.
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a
//! hard panic there.

use std::fs;

use aether_actor::Addressable;
use aether_component::ComponentHostCapability;
use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{ExecutionError, HarnessOp, SendTarget, SubstrateHarness};
use aether_kinds::{LoadComponent, Publish, PublishResult};
use aether_substrate::testing::successor_wasm;
use aether_test_fixtures_bundle::{HeldForgetter, HeldKeeper, HeldRequester, ReplyHolder as CarryReplyHolder};
use aether_test_fixtures_kinds::{
    CountQuery, CountReport, HELD_TARGET_FORGETTER, HELD_TARGET_KEEPER, HELD_TARGET_RELAY, HeldReplyMatched,
    ReleaseCarried, ReleaseHeld, RunHeldRequest,
};
use aether_test_fixtures_republish::ReplyHolder;

const FIXTURE_CRATE: &str = "aether_test_fixtures_bundle";
const HOLDER: &str = "test.carry.holder";
const RELAY: &str = "test.held.relay";
const KEEPER: &str = "test.held.keeper";
const FORGETTER: &str = "test.held.forgetter";
const REQUESTER: &str = "test.held.requester";

/// The `test.republish.carry.held_requester` rows this file sends: a silent
/// `RunHeldRequest` and `CountQuery -> CountReport`. It ships only as a
/// cdylib example, so the test casts its `load_any` reference to this
/// instead of naming a type.
#[aether_actor::protocol]
trait CarriedHeldRequester {
    fn run(mail: RunHeldRequest);
    fn count(mail: CountQuery) -> CountReport;
}

/// The held actor a scenario sends its request to, and how its held reply
/// is released.
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

/// Load the bundle's held actors and the requester, send one held request
/// to `holder`, republish the bundle with its own code under a new hash —
/// every loaded actor moves together (ADR-0241 §7) — release the held reply,
/// and wait for the requester to match it. Returns the harness to count
/// reports on and the swap's result, or `None` when the fixture wasm is not
/// built.
fn replace_while_held(holder: Holder) -> Option<(SubstrateHarness, PublishResult)> {
    let wasm = fs::read(require_wasm(FIXTURE_CRATE)?).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");

    let component = |wasm: &[u8]| LoadComponent { wasm: wasm.to_vec(), name: None, config: Vec::new(), export: None };
    let reply_holder =
        harness.load::<CarryReplyHolder>(component(&wasm)).unwrap_or_else(|error| panic!("load {HOLDER}: {error}"));
    let _ = harness
        .load_any(&LoadComponent { wasm: wasm.clone(), name: None, config: Vec::new(), export: Some(RELAY.to_owned()) })
        .unwrap_or_else(|error| panic!("load {RELAY}: {error}"));
    let keeper = harness.load::<HeldKeeper>(component(&wasm)).unwrap_or_else(|error| panic!("load {KEEPER}: {error}"));
    let forgetter =
        harness.load::<HeldForgetter>(component(&wasm)).unwrap_or_else(|error| panic!("load {FORGETTER}: {error}"));
    let requester =
        harness.load::<HeldRequester>(component(&wasm)).unwrap_or_else(|error| panic!("load {REQUESTER}: {error}"));

    let release = match holder {
        Holder::Relay => HarnessOp::send_and_settle(&reply_holder, &ReleaseCarried),
        Holder::Keeper => HarnessOp::send_and_settle(&keeper, &ReleaseHeld),
        Holder::Forgetter => HarnessOp::send_and_settle(&forgetter, &ReleaseHeld),
    };
    let publish = Publish { code: successor_wasm(&wasm, 1).into(), configs: Vec::new() };
    let run = RunHeldRequest { tag: 1, target: holder.target() };

    let swap = run_across_swap(&mut harness, &requester, run, &publish, release)
        .unwrap_or_else(|error| panic!("held-reply sequence for {}: {error}", holder.export()));
    Some((harness, swap))
}

/// Send `run` to `requester`, republish with `publish`, run `release`, and
/// wait for the requester to match the held reply: the held reply lands on
/// the detached request's chain, which no harness step joins, so the
/// requester's match count is the barrier. Generic over the requester's
/// reference shape: the bundle's `HeldRequester` sends through an
/// `&ActorRef<R>`, and the cdylib-only carry requester through a
/// `&ProtocolRef<P>` cast.
fn run_across_swap<'r, T, I1, I2>(
    harness: &mut SubstrateHarness,
    requester: &'r T,
    run: RunHeldRequest,
    publish: &Publish,
    release: HarnessOp,
) -> Result<PublishResult, ExecutionError>
where
    &'r T: SendTarget<RunHeldRequest, I1> + SendTarget<CountQuery, I2>,
{
    let steps = vec![
        ("request", HarnessOp::send_and_settle(requester, &run)),
        ("swap", HarnessOp::send_and_await_reply(&harness.actor_ref::<ComponentHostCapability>(), publish)),
        ("release", release),
        ("matched", HarnessOp::poll_until(requester, &CountQuery, |report: &CountReport| report.count >= 1)),
    ];
    Ok(harness.execute(steps)?.reply::<PublishResult>("swap").expect("decode PublishResult"))
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
    let Some((harness, swap)) = replace_while_held(Holder::Relay) else {
        return;
    };

    assert!(matches!(swap, PublishResult::Ok { .. }), "publish: {swap:?}");
    assert_one_match(&harness);
}

#[test]
fn a_held_reply_in_saved_state_answers_after_replace() {
    // Catches: the dehydrate encoder refusing a `Held`, or the rehydrate
    // decode ctx not granting its claim, so the keeper's saved reply is
    // lost across the replace.
    let Some((harness, swap)) = replace_while_held(Holder::Keeper) else {
        return;
    };

    assert!(matches!(swap, PublishResult::Ok { .. }), "publish: {swap:?}");
    assert_one_match(&harness);
}

#[test]
fn an_unsaved_held_reply_refuses_the_replace_and_the_old_guest_answers() {
    // Catches: the host ignoring the error `on_dehydrate` returned and
    // swapping the forgetter out with its reply unsaved, or the rollback
    // losing the reply table, so the reinstated guest's answer never arrives.
    let Some((harness, swap)) = replace_while_held(Holder::Forgetter) else {
        return;
    };

    match swap {
        PublishResult::Err { error } => assert!(
            error.contains("a held reply is live and was not saved"),
            "the refusal must name the unsaved held reply: {error}",
        ),
        PublishResult::Ok { .. } => panic!("a replace that strands a live held reply was accepted"),
    }
    assert_one_match(&harness);
}

#[test]
fn a_replacement_that_changed_a_held_reply_kind_is_refused() {
    // Catches: `Ticket` hashing dropping the reply id, so the reshaped relay
    // context shares v1's `KindId`, the replace is accepted, and the
    // successor's take of the carried context decodes a `Held` of the wrong
    // reply kind.
    let (Some(v1_path), Some(v2_path)) = (require_wasm("republish_carry_v1"), require_wasm("republish_carry_v2"))
    else {
        return;
    };
    let v1 = fs::read(v1_path).expect("read republish_carry_v1");
    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");
    let reply_holder = harness
        .load::<ReplyHolder>(LoadComponent { wasm: v1.clone(), name: None, config: Vec::new(), export: None })
        .unwrap_or_else(|error| panic!("load {}: {error}", ReplyHolder::NAMESPACE));

    let mut load = |export: &str| {
        harness
            .load_any(&LoadComponent {
                wasm: v1.clone(),
                name: None,
                config: Vec::new(),
                export: Some(export.to_owned()),
            })
            .unwrap_or_else(|error| panic!("load {export}: {error}"))
    };
    let _ = load("test.republish.carry.held_relay");
    let (requester, _) = load("test.republish.carry.held_requester");
    let requester = harness
        .cast::<CarriedHeldRequester>(requester)
        .expect("the held requester publishes RunHeldRequest and CountQuery");

    let publish = Publish { code: fs::read(v2_path).expect("read republish_carry_v2").into(), configs: Vec::new() };
    let release = HarnessOp::send_and_settle(&reply_holder, &ReleaseCarried);
    let swap = run_across_swap(
        &mut harness,
        &requester,
        RunHeldRequest { tag: 1, target: HELD_TARGET_RELAY },
        &publish,
        release,
    )
    .unwrap_or_else(|error| panic!("held-reply sequence for the reshaped relay: {error}"));

    match swap {
        PublishResult::Err { error } => assert!(
            error.contains(
                "replacement does not declare its carried context \
                 aether.test_fixtures.republish_held_relay_context"
            ),
            "the refusal must name the carried held relay context: {error}",
        ),
        PublishResult::Ok { .. } => panic!("a replacement that changed a carried held reply kind was accepted"),
    }
    assert_one_match(&harness);
}
