//! Published route contracts (ADR-0231 §4, issue #6836).
//!
//! A route publishes its actor's `(KindId, ReplyContract)` rows and its
//! fallback flag on its route record when it goes `Live`: a wasm trampoline
//! its guest's, republished on replace; an inline
//! child's alias its own type's, private children included; a native
//! capability its `#[actor]` surface. Each scenario reads the published
//! contract through the harness's `published_contract` door.
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a
//! hard panic there.

use std::fs;

use aether_actor::{ActorRef, Addressable, ChildOf, Instanced};
use aether_component::ComponentHostCapability;
use aether_data::{Kind, KindId, LoadName, ReplyContract};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, ReplaceComponent, ReplaceResult};
use aether_substrate::actor::native::{Dispatch, NativeActor};
use aether_test_fixtures_bundle::{ContractBase, InlineChild, InlineParent, InlineStatefulChild, InlineStatefulParent};
use aether_test_fixtures_kinds::{Bump, CountQuery, CountReport, InlineEcho, InlineProbe};

const BUNDLE: &str = "aether_test_fixtures_bundle";
const EXTENDED_EXPORT: &str = "test.contract.extended";

/// `rows` sorted by kind, the order a published contract holds them in.
fn sorted(mut rows: Vec<(KindId, ReplyContract)>) -> Vec<(KindId, ReplyContract)> {
    rows.sort_by_key(|(kind, _)| *kind);
    rows
}

fn harness() -> SubstrateHarness {
    SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot")
}

fn load_request(wasm: Vec<u8>) -> LoadComponent {
    LoadComponent { wasm, name: None, config: Vec::new(), export: None }
}

/// The `C` inline child keyed `name` beneath `parent`, once its alias is
/// live. The parent's `wire` stages its alias batch while the activation
/// hold is set; the owner's catch-up submits that batch before it promotes
/// the spawn, so the caller's already-returned load reply proves the batch
/// is queued, and the barrier proves the owner has applied it.
fn await_child<P, C>(harness: &SubstrateHarness, parent: ActorRef<P>, name: &str) -> ActorRef<C>
where
    P: Addressable,
    C: ChildOf<P> + Instanced,
{
    harness.await_registry_applied();
    harness
        .child::<P, C>(&parent, LoadName::new(name).expect("a valid instance key"))
        .unwrap_or_else(|error| panic!("inline child {name} must be live: {error}"))
}

/// A loaded trampoline publishes its guest's rows and fallback flag, and
/// neither its own framework arms nor its forwarding fallback. A replace
/// republishes the replacement's rows. Catches a trampoline that publishes its
/// own `#[actor]` contract or nothing, and a replace that never republishes.
/// A dropped instance's route no longer reads `Live`, which
/// `harness_drop_close` covers.
#[test]
fn a_loaded_component_publishes_its_guest_contract_through_replace() {
    let Some(wasm_path) = require_wasm(BUNDLE) else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let mut harness = harness();

    let (victim, path) = harness
        .load::<ContractBase>(load_request(wasm.clone()))
        .unwrap_or_else(|error| panic!("the base must load: {error}"));
    let base = sorted(vec![(Bump::ID, ReplyContract::None), (CountQuery::ID, ReplyContract::One(CountReport::ID))]);
    assert_eq!(harness.published_contract(victim.erase()), Some((base.clone(), false)));

    let replace = ReplaceComponent {
        target: path,
        wasm,
        drain_timeout_ms: None,
        config: Vec::new(),
        export: Some(EXTENDED_EXPORT.to_owned()),
    };
    let operation = HarnessOp::send_and_await_reply(&harness.actor_ref::<ComponentHostCapability>(), &replace);
    let replaced = harness.execute(vec![("replace", operation)]).expect("replace operation");
    if let ReplaceResult::Err { error } = replaced.reply::<ReplaceResult>("replace").expect("decode ReplaceResult") {
        panic!("a replace that only adds a row must succeed: {error}");
    }
    harness.execute(vec![("bump", HarnessOp::send_and_settle(&victim, &Bump))]).expect("bump the replaced actor");
    harness.await_registry_applied();
    let extended = (sorted([base, vec![(InlineProbe::ID, ReplyContract::None)]].concat()), false);
    assert_eq!(harness.published_contract(victim.erase()), Some(extended));
}

/// An inline child's alias publishes its own type's rows: an exported child
/// type from the module's exported groups, and a private one from its
/// private section. The stateful parent publishes only a fallback, so a
/// parent-row alias is visible, and the private child's rows exist nowhere
/// but that section. Catches an untagged host call, a map lookup that takes
/// the wrong entry, and a map that leaves out private children.
#[test]
fn an_inline_child_alias_publishes_its_own_type_contract() {
    let Some(wasm_path) = require_wasm(BUNDLE) else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let mut harness = harness();

    let (stateful_parent, _) = harness
        .load::<InlineStatefulParent>(load_request(wasm.clone()))
        .unwrap_or_else(|error| panic!("the stateful parent must load: {error}"));
    let stateful_child = await_child::<InlineStatefulParent, InlineStatefulChild>(&harness, stateful_parent, "widget");
    assert_eq!(harness.published_contract(stateful_parent.erase()), Some((Vec::new(), true)));
    assert_eq!(
        harness.published_contract(stateful_child.erase()),
        Some((
            sorted(vec![(Bump::ID, ReplyContract::None), (CountQuery::ID, ReplyContract::One(CountReport::ID))]),
            false
        )),
    );

    let (private_parent, _) = harness
        .load::<InlineParent>(load_request(wasm))
        .unwrap_or_else(|error| panic!("the private-child parent must load: {error}"));
    let private_child = await_child::<InlineParent, InlineChild>(&harness, private_parent, "widget");
    assert_eq!(
        harness.published_contract(private_child.erase()),
        Some((vec![(InlineProbe::ID, ReplyContract::One(InlineEcho::ID))], false)),
    );
}

/// A native capability booted with the chassis publishes its `#[actor]`
/// receive surface. Catches a boot path that leaves the route it claimed
/// publishing no rows.
#[test]
fn a_native_capability_publishes_its_actor_contract() {
    let harness = harness();
    let capabilities =
        <ComponentHostCapability as Dispatch<<ComponentHostCapability as NativeActor>::State>>::capabilities();
    let rows = sorted(capabilities.handlers.iter().map(|handler| (handler.id, handler.reply)).collect());
    assert!(!rows.is_empty(), "the component host declares handlers");

    assert_eq!(
        harness.published_contract(harness.actor_ref::<ComponentHostCapability>().erase()),
        Some((rows, capabilities.fallback.is_some())),
    );
}
