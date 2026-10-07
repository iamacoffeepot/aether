//! Issue 7535: a replace hook that can fail returns an error, and the engine
//! acts on what each hook did (ADR-0249 §1, §2, §4).
//!
//! - An `on_dehydrate` or a successor's `on_rehydrate` that returns an error
//!   refuses the republish, and the old guest keeps running with its state.
//! - A trap in the old guest's `on_dehydrate` aborts the engine: it is the
//!   live guest, and a guest that trapped runs no more code.
//! - The old guest's `on_rehydrate` returning an error after a refused
//!   republish closes the instance.
//! - An inline child the successor cannot rebuild refuses the republish.
//!
//! The fixtures are the hooks pair: `test.republish.hooks.parent`, whose
//! replace hooks do what its `HookFaultConfig` says, and its inline
//! `test.republish.hooks.counter`. v2's counter returns an error from
//! `on_rehydrate`. No test reads the counter after a refusal: the reinstated
//! parent's second `wire` spawns it again over the resident one, which is
//! issue 7536's to fix and assert.
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a
//! hard panic there.

use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};

use aether_actor::ProtocolRef;
use aether_component::ComponentHostCapability;
use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness, SubstrateHarnessError};
use aether_kinds::{LoadComponent, LoadResult};
use aether_substrate::testing::successor_wasm;
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, DEHYDRATE_REFUSAL, HELD_UNANSWERED_TAG, HeldRequest, HeldRequestResult,
    HookFaultConfig, HookOutcome, REHYDRATE_REFUSAL,
};

const PARENT: &str = "test.republish.hooks.parent";

/// The parent's rows this file sends: a silent `Bump`, `CountQuery ->
/// CountReport`, and `HeldRequest -> HeldRequestResult`, which it holds. Both
/// versions ship only as cdylib examples, so the test casts its `load_any`
/// reference to this instead of naming a type.
#[aether_actor::protocol]
trait HooksParent {
    fn bump(mail: Bump);
    fn count(mail: CountQuery) -> CountReport;
    fn request(mail: HeldRequest) -> HeldRequestResult;
}

/// The hooks pair's two versions, or `None` when they are not built.
struct Hooks {
    v1: Vec<u8>,
    v2: Vec<u8>,
}

fn hooks() -> Option<Hooks> {
    let read = |stem: &str| require_wasm(stem).map(|path| fs::read(path).expect("read fixture wasm"));
    Some(Hooks { v1: read("republish_hooks_v1")?, v2: read("republish_hooks_v2")? })
}

fn load_parent(wasm: &[u8], config: HookFaultConfig) -> LoadComponent {
    LoadComponent {
        wasm: wasm.to_vec(),
        name: None,
        config: config.encode_into_bytes(),
        export: Some(PARENT.to_owned()),
    }
}

/// Boot a harness, load v1's parent with `config`, and bump it three times.
fn parent_at_three(v1: &[u8], config: HookFaultConfig) -> (SubstrateHarness, ProtocolRef<HooksParent>) {
    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");
    let (parent, _) =
        harness.load_any(&load_parent(v1, config)).unwrap_or_else(|error| panic!("load {PARENT}: {error}"));
    let parent = harness.cast::<HooksParent>(parent).expect("the parent publishes Bump, CountQuery and HeldRequest");

    harness
        .execute(vec![
            ("bump_1", HarnessOp::send_and_settle(&parent, &Bump)),
            ("bump_2", HarnessOp::send_and_settle(&parent, &Bump)),
            ("bump_3", HarnessOp::send_and_settle(&parent, &Bump)),
        ])
        .expect("bump the parent");
    (harness, parent)
}

/// Republish `wasm` and answer the refusal it must end in.
fn refused(harness: &mut SubstrateHarness, wasm: Vec<u8>) -> String {
    match harness.publish(wasm) {
        Err(SubstrateHarnessError::Publish(refusal)) => refusal,
        Err(error) => panic!("the publish must answer: {error}"),
        Ok(_) => panic!("a republish whose hook failed was accepted"),
    }
}

fn count(harness: &mut SubstrateHarness, parent: ProtocolRef<HooksParent>) -> u32 {
    harness
        .execute(vec![("count", HarnessOp::send_and_await_reply(&parent, &CountQuery))])
        .expect("query the parent")
        .reply::<CountReport>("count")
        .expect("decode CountReport")
        .count
}

#[test]
fn an_on_dehydrate_that_returns_an_error_refuses_the_republish() {
    // Catches: the old guest's returned error treated as success, so the
    // guest is replaced by a successor that was handed no state and starts
    // from zero behind a publish that answered `Ok`.
    let Some(fixtures) = hooks() else {
        return;
    };
    let config = HookFaultConfig { dehydrate: HookOutcome::Refuses, ..HookFaultConfig::default() };
    let (mut harness, parent) = parent_at_three(&fixtures.v1, config);

    let refusal = refused(&mut harness, successor_wasm(&fixtures.v1, 1));

    assert!(refusal.contains("on_dehydrate failed"), "the refusal names the hook: {refusal}");
    assert!(refusal.contains(DEHYDRATE_REFUSAL), "the refusal carries the guest's message: {refusal}");
    assert_eq!(count(&mut harness, parent), 3, "the guest that refused keeps running with its count");
}

#[test]
fn a_trap_in_on_dehydrate_aborts_the_engine() {
    // Catches: the old guest's trap logged and the republish going on, which
    // answers `Ok` with no state carried, and leaves a guest whose store the
    // trap abandoned mid-hook to be reinstated and run again.
    let Some(fixtures) = hooks() else {
        return;
    };
    let config = HookFaultConfig { dehydrate: HookOutcome::Traps, ..HookFaultConfig::default() };
    let (mut harness, _parent) = parent_at_three(&fixtures.v1, config);

    let Err(SubstrateHarnessError::FatalAbort(reason)) = harness.publish(successor_wasm(&fixtures.v1, 1)) else {
        panic!("a republish whose old guest trapped in on_dehydrate must abort the engine");
    };

    assert!(reason.contains(PARENT), "the abort names the component: {reason}");
    assert!(reason.contains("on_dehydrate"), "the abort names the hook: {reason}");
    // A chassis that fatally aborted reports the abort at teardown rather
    // than waiting on its actors' closes.
    let teardown = catch_unwind(AssertUnwindSafe(|| drop(harness)));
    assert!(teardown.is_err(), "teardown of an aborted engine reports the abort");
}

#[test]
fn a_successors_on_rehydrate_that_returns_an_error_refuses_the_republish() {
    // Catches: the `on_rehydrate` export's return code ignored, so a
    // successor that refused the state it was handed is installed without it.
    let Some(fixtures) = hooks() else {
        return;
    };
    let config = HookFaultConfig { successor_rehydrate: HookOutcome::Refuses, ..HookFaultConfig::default() };
    let (mut harness, parent) = parent_at_three(&fixtures.v1, config);

    let refusal = refused(&mut harness, successor_wasm(&fixtures.v1, 1));

    assert!(refusal.contains("on_rehydrate failed"), "the refusal names the hook: {refusal}");
    assert!(refusal.contains(REHYDRATE_REFUSAL), "the refusal carries the guest's message: {refusal}");
    assert_eq!(count(&mut harness, parent), 3, "the reinstated guest takes its count back");
}

#[test]
fn an_old_guest_that_refuses_its_own_state_closes() {
    // Catches: a guest that refused the state it was handed back left live
    // and receiving mail without that state, with the reply it holds never
    // answered.
    let Some(fixtures) = hooks() else {
        return;
    };
    let config = HookFaultConfig {
        successor_rehydrate: HookOutcome::Refuses,
        reinstated_rehydrate: HookOutcome::Refuses,
        ..HookFaultConfig::default()
    };
    let (mut harness, parent) = parent_at_three(&fixtures.v1, config);
    // The parent's mailbox is FIFO, so the request reaches it ahead of the
    // host's prepare.
    let held = harness.send_deferred_to(&parent, &HeldRequest { tag: 7 }).expect("send the held request");

    let refusal = refused(&mut harness, successor_wasm(&fixtures.v1, 1));

    assert!(refusal.contains("on_rehydrate failed"), "the successor's refusal is reported: {refusal}");
    let answered = harness.await_deferred::<HeldRequestResult>(held).expect("the held reply");
    assert_eq!(answered.tag, HELD_UNANSWERED_TAG, "the closing instance answers the reply it held `unanswered`");

    // The close runs after the prepare that asked for it has answered, so a
    // load of the name is polled until the close's tombstone refuses it.
    let host = harness.actor_ref::<ComponentHostCapability>();
    let reload = load_parent(&fixtures.v1, config);
    let retired = |result: &LoadResult| matches!(result, LoadResult::Err { error } if error.contains("SubnameRetired"));
    harness
        .execute(vec![("reload", HarnessOp::poll_until(&host, &reload, retired))])
        .expect("a later load of the closed instance's name is refused as retired");
}

#[test]
fn a_child_that_cannot_be_rebuilt_refuses_the_republish() {
    // Catches: a child's rebuild failure logged and skipped, so the republish
    // answers `Ok` and the successor runs with a child missing.
    let Some(fixtures) = hooks() else {
        return;
    };
    let (mut harness, parent) = parent_at_three(&fixtures.v1, HookFaultConfig::default());

    let refusal = refused(&mut harness, fixtures.v2);

    assert!(refusal.contains("on_rehydrate failed"), "the refusal names the hook: {refusal}");
    assert!(refusal.contains("inline child `counter` was not rebuilt"), "the refusal names the child: {refusal}");
    assert!(refusal.contains(REHYDRATE_REFUSAL), "the refusal carries the child's message: {refusal}");
    assert_eq!(count(&mut harness, parent), 3, "the old parent keeps running with its count");
}
