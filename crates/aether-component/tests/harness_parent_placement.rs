//! Public `SubstrateHarness` placement coverage for issue #4535.
//!
//! The refusal scenarios pin the parent boundary: a spawn beneath an address
//! that does not resolve, and beneath one that resolves to a parent still
//! `Starting`, both answer `Err` before any guest is staged. Root placement is pinned the same way (ADR-0241
//! §5): a root load of a type whose only declared placement is `child_of(P)`
//! answers `Err` naming it, before the module publishes or its route is
//! staged. `harness_guest_addresses` covers a placement beneath a live
//! parent.

use std::fs;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

use aether_actor::{Addressable, actor};
use aether_component::ComponentHostCapability;
use aether_data::ErasedActorPath;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult, Spawn, SpawnResult};
use aether_substrate::BootError;
use aether_substrate::actor::native::spawn::Subname;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, SpawnOutcome, TaskDone};

const CHILD_ONLY_EXPORT: &str = "test.matrix.child";
const PANEL_EXPORT: &str = "test.ui.panel";

/// Publish the bundle fixture into `harness`, or `None` when its wasm is not
/// built.
fn published_bundle(harness: &mut SubstrateHarness) -> Option<()> {
    let wasm = fs::read(require_wasm("aether_test_fixtures_bundle")?).expect("read fixture wasm");
    harness.publish(wasm).expect("publish the bundle");
    Some(())
}

/// Send a raw `Spawn` of the published child-only type beneath `parent`,
/// which the typed spawn cannot name: it is unresolved, or not yet `Live`.
fn spawn_under(harness: &mut SubstrateHarness, label: &str, parent: &str) -> SpawnResult {
    let spawn = Spawn {
        namespace: CHILD_ONLY_EXPORT.to_owned(),
        key: Some("k".to_owned()),
        parent: Some(ErasedActorPath::new(parent).expect("a valid parent path")),
        config: Vec::new(),
    };
    let host = harness.actor_ref::<ComponentHostCapability>();
    let result =
        harness.execute(vec![(label, HarnessOp::send_and_await_reply(&host, &spawn))]).expect("component spawn");

    result.reply::<SpawnResult>(label).expect("decode SpawnResult")
}

fn load_result(
    harness: &mut SubstrateHarness,
    wasm: &[u8],
    label: &str,
    name: Option<&str>,
    export: &str,
) -> LoadResult {
    let component = LoadComponent {
        wasm: wasm.to_vec(),
        name: name.map(str::to_owned),
        config: Vec::new(),
        export: Some(export.to_owned()),
    };
    let host = harness.actor_ref::<ComponentHostCapability>();
    let result =
        harness.execute(vec![(label, HarnessOp::send_and_await_reply(&host, &component))]).expect("component load");

    result.reply::<LoadResult>(label).expect("decode LoadResult")
}

/// Catches a root load that ignores the selected type's lineage (the
/// child-only type would load at the root) and a refusal that comes after
/// staging (a route left behind would be listed).
#[test]
fn a_root_load_of_a_child_only_type_is_refused_before_staging() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    let LoadResult::Err { error } = load_result(&mut harness, &wasm, "child-only", Some("stray"), CHILD_ONLY_EXPORT)
    else {
        panic!("a type declaring only child_of(..) must not load at the root");
    };
    assert!(error.contains(CHILD_ONLY_EXPORT), "the refusal names the type: {error}");
    let listed = harness.list_components().expect("list components");
    assert!(!listed.contains(&format!("{CHILD_ONLY_EXPORT}:stray")), "the refused load left no route: {listed:?}");

    let LoadResult::Ok { .. } = load_result(&mut harness, &wasm, "root", Some("stray"), PANEL_EXPORT) else {
        panic!("a root type loads at the root");
    };
}

#[test]
fn unresolved_explicit_parent_is_a_clean_spawn_error() {
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    if published_bundle(&mut harness).is_none() {
        return;
    }

    let SpawnResult::Err { error } = spawn_under(&mut harness, "missing-parent", "test.missing") else {
        panic!("an unresolved logical parent must not spawn a component");
    };
    assert!(error.contains("spawn parent"), "error identifies the parent boundary: {error}");
    assert!(error.contains("missing"), "error retains the unresolved address: {error}");
}

/// Where [`HeldParent::wire`] stands: `entered` once the hook runs, and the
/// hook returns only after the test sets `open`. Until then the held parent's
/// route stays `Starting`.
struct Gate {
    entered: bool,
    open: bool,
}

static GATE: (Mutex<Gate>, Condvar) = (Mutex::new(Gate { entered: false, open: false }), Condvar::new());

/// Opens the gate when dropped, so a failed assertion still lets the held
/// birth finish and the harness shut down.
struct OpenOnDrop;

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        let (gate, changed) = &GATE;
        gate.lock().unwrap_or_else(PoisonError::into_inner).open = true;
        changed.notify_all();
    }
}

#[aether_data::kind(name = "test.parent_placement.hatch_held", copy, no_serde)]
struct HatchHeld;

/// Stages the held parent from a handler, as any actor births a child, and
/// keeps its receipt's name until the birth is decided.
struct Launcher {
    staged: Option<ErasedActorPath>,
}

#[actor(singleton, root)]
impl NativeActor for Launcher {
    const NAMESPACE: &'static str = "test.parent_placement.launcher";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { staged: None })
    }

    #[handler::single]
    fn on_hatch(&mut self, ctx: &mut NativeCtx<'_>, _hatch: HatchHeld) {
        let receipt =
            ctx.spawn_child::<HeldParent>(Subname::Named("held"), (), ()).stage().expect("the held parent stages");
        self.staged = Some(receipt.canonical_name);
    }

    #[handler(task)]
    fn on_held_born(&mut self, _ctx: &mut NativeCtx<'_>, done: TaskDone<SpawnOutcome<HeldParent>>) {
        if self.staged.as_ref() == Some(&done.into_output().canonical_name) {
            self.staged = None;
        }
    }
}

/// A child whose `wire` hook parks on [`GATE`], holding its route `Starting`.
struct HeldParent {
    hatches: u32,
}

#[actor(instanced, child_of(Launcher))]
impl NativeActor for HeldParent {
    const NAMESPACE: &'static str = "test.parent_placement.held";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { hatches: 0 })
    }

    fn wire(_state: &mut Self, _ctx: &mut NativeCtx<'_>) {
        let (gate, changed) = &GATE;
        gate.lock().expect("gate lock").entered = true;
        changed.notify_all();
        drop(changed.wait_while(gate.lock().expect("gate lock"), |state| !state.open).expect("gate lock"));
    }

    /// Never sent here: an actor declares at least one handler.
    #[handler::single]
    fn on_hatch(&mut self, _ctx: &mut NativeCtx<'_>, _hatch: HatchHeld) {
        self.hatches += 1;
    }
}

/// Catches staging a child beneath a parent that has not reached `Live`: the
/// parent's address resolves while it is `Starting`, and only proving the
/// route (ADR-0230 §1) refuses it, so a host that stopped at resolution would
/// go on to spawn beneath an unborn parent.
#[test]
fn a_starting_parent_is_a_clean_spawn_error() {
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_workers(Some(4))
        .with_component_host()
        .with_actor::<Launcher>(())
        .build()
        .expect("boot");
    if published_bundle(&mut harness).is_none() {
        return;
    }
    let _open = OpenOnDrop;
    let _hatch = harness.send_deferred(harness.actor_ref::<Launcher>(), &HatchHeld);
    let (gate, changed) = &GATE;
    let entered = changed
        .wait_timeout_while(gate.lock().expect("gate lock"), Duration::from_secs(10), |state| !state.entered)
        .expect("gate lock");
    assert!(entered.0.entered, "the held parent reaches its wire hook");
    drop(entered);

    let parent = format!("{}/{}:held", Launcher::NAMESPACE, HeldParent::NAMESPACE);
    let SpawnResult::Err { error } = spawn_under(&mut harness, "starting-parent", &parent) else {
        panic!("a starting parent must not spawn a component");
    };
    assert!(error.contains("spawn parent"), "error identifies the parent boundary: {error}");
    assert!(error.contains(&parent), "error retains the parent's address: {error}");
    assert!(error.contains("is not live"), "error names the parent's lifecycle: {error}");
}
