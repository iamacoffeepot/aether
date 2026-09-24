//! Public `SubstrateHarness` placement coverage for issue #4535.
//!
//! The scenario uses only `HarnessOp` plus ordinary `LoadComponent` values to
//! build two component peer scopes. The fixture caller's real bare-type
//! `ctx.send::<R>(..)` proves the runtime parent selected during explicit
//! placement is what embedded resolution consumes. The refusal scenarios
//! pin the parent boundary: an address that does not resolve, and one that
//! resolves to a parent still `Starting`, both answer `Err` before any load.

use std::fs;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

use aether_actor::{ActorRef, Addressable, EMBEDDED_SCOPE, actor};
use aether_component::{ComponentHostCapability, WasmTrampoline};
use aether_data::{ActorPath, Kind, LoadName};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_substrate::BootError;
use aether_substrate::actor::native::spawn::Subname;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, SpawnOutcome, TaskDone};
use aether_test_fixtures_kinds::{Bump, TickObserved};

const PROBE_EXPORT: &str = "test.probe";
const CALLER_EXPORT: &str = "test.parent_peer.caller";
const TARGET_EXPORT: &str = "test.parent_peer.target";

fn load(
    harness: &mut SubstrateHarness,
    wasm: &[u8],
    label: &str,
    parent: Option<&str>,
    name: Option<&str>,
    export: &str,
) -> String {
    let component = LoadComponent {
        wasm: wasm.to_vec(),
        name: name.map(str::to_owned),
        config: Vec::new(),
        export: Some(export.to_owned()),
    };
    let host = harness.actor_ref::<ComponentHostCapability>();
    let operation = match parent {
        Some(parent) => HarnessOp::load_component_under(&host, parent, component),
        None => HarnessOp::send_and_await_reply(&host, &component),
    };
    let result = harness.execute(vec![(label, operation)]).expect("component load operation");

    match result.reply::<LoadResult>(label).expect("decode LoadResult") {
        LoadResult::Ok { path, .. } => path.to_string(),
        LoadResult::Err { error } => panic!("load {export} beneath {parent:?} failed: {error}"),
    }
}

/// The loaded trampoline keyed `name` beneath the trampoline `parent` — the
/// placement a load beneath a component parent produces.
fn nested_trampoline(
    harness: &SubstrateHarness,
    parent: ActorRef<WasmTrampoline>,
    name: &str,
) -> ActorRef<WasmTrampoline> {
    harness
        .child::<WasmTrampoline, WasmTrampoline>(&parent, LoadName::new(name).expect("a valid load name"))
        .unwrap_or_else(|error| panic!("the trampoline loaded as {name} is live: {error}"))
}

fn assert_child_identity(loaded: &str, parent: &str, subname: &str) {
    let expected = format!("{parent}/{EMBEDDED_SCOPE}:{subname}");
    assert_eq!(loaded, expected, "LoadResult must return the registry-canonical child path");
}

#[test]
fn explicit_and_nested_parents_scope_live_peer_delivery() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    let outer = load(&mut harness, &wasm, "outer", None, Some("outer"), PROBE_EXPORT);
    let outer_target = load(&mut harness, &wasm, "outer-target", Some(&outer), None, TARGET_EXPORT);
    let outer_caller = load(&mut harness, &wasm, "outer-caller", Some(&outer), None, CALLER_EXPORT);
    assert_child_identity(&outer_target, &outer, TARGET_EXPORT);
    assert_child_identity(&outer_caller, &outer, CALLER_EXPORT);

    let nested = load(&mut harness, &wasm, "nested", Some(&outer), Some("nested"), PROBE_EXPORT);
    assert_child_identity(&nested, &outer, "nested");
    let nested_target = load(&mut harness, &wasm, "nested-target", Some(&nested), None, TARGET_EXPORT);
    let nested_caller = load(&mut harness, &wasm, "nested-caller", Some(&nested), None, CALLER_EXPORT);
    assert_child_identity(&nested_target, &nested, TARGET_EXPORT);
    assert_child_identity(&nested_caller, &nested, CALLER_EXPORT);

    let host = harness.actor_ref::<ComponentHostCapability>();
    let outer = harness
        .child::<ComponentHostCapability, WasmTrampoline>(&host, LoadName::new("outer").expect("a valid load name"))
        .expect("the outer trampoline is live");
    let outer_caller = nested_trampoline(&harness, outer, CALLER_EXPORT);
    let nested_caller = nested_trampoline(&harness, nested_trampoline(&harness, outer, "nested"), CALLER_EXPORT);

    let baseline = harness.count_observed(TickObserved::NAME);
    harness
        .execute(vec![
            ("outer-peer", HarnessOp::send_and_settle(outer_caller.erase(), &Bump)),
            ("nested-peer", HarnessOp::send_and_settle(nested_caller.erase(), &Bump)),
        ])
        .expect("both parent-relative peer sends settle");
    assert_eq!(
        harness.count_observed(TickObserved::NAME) - baseline,
        2,
        "each caller must reach the target beneath its own runtime parent; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

#[test]
fn unresolved_explicit_parent_is_a_clean_load_error() {
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let host = harness.actor_ref::<ComponentHostCapability>();
    let result = harness
        .execute(vec![(
            "missing-parent",
            HarnessOp::load_component_under(
                &host,
                "aether.component/aether.embedded:missing",
                LoadComponent { wasm: Vec::new(), name: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("the component host replies to an unresolved parent");

    let LoadResult::Err { error } = result.reply::<LoadResult>("missing-parent").expect("decode LoadResult") else {
        panic!("an unresolved logical parent must not load a component");
    };
    assert!(error.contains("component parent"), "error identifies the parent boundary: {error}");
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
    staged: Option<ActorPath>,
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
    fn on_held_born(&mut self, _ctx: &mut NativeCtx<'_>, done: TaskDone<SpawnOutcome<HeldParent>, ()>) {
        if self.staged.as_ref() == Some(&done.output().canonical_name) {
            self.staged = None;
        }
        done.release_no_reply();
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
/// go on to load beneath an unborn parent.
#[test]
fn a_starting_parent_is_a_clean_load_error() {
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_workers(Some(4))
        .with_component_host()
        .with_actor::<Launcher>(())
        .build()
        .expect("boot");
    let _open = OpenOnDrop;
    let _hatch = harness.send_deferred(&harness.actor_ref::<Launcher>(), &HatchHeld);
    let (gate, changed) = &GATE;
    let entered = changed
        .wait_timeout_while(gate.lock().expect("gate lock"), Duration::from_secs(10), |state| !state.entered)
        .expect("gate lock");
    assert!(entered.0.entered, "the held parent reaches its wire hook");
    drop(entered);

    let parent = format!("{}/{}:held", Launcher::NAMESPACE, HeldParent::NAMESPACE);
    let host = harness.actor_ref::<ComponentHostCapability>();
    let result = harness
        .execute(vec![(
            "starting-parent",
            HarnessOp::load_component_under(
                &host,
                parent.as_str(),
                LoadComponent { wasm: Vec::new(), name: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("the component host replies to a starting parent");

    let LoadResult::Err { error } = result.reply::<LoadResult>("starting-parent").expect("decode LoadResult") else {
        panic!("a starting parent must not load a component");
    };
    assert!(error.contains("component parent"), "error identifies the parent boundary: {error}");
    assert!(error.contains("is not live"), "error names the parent's lifecycle: {error}");
}
