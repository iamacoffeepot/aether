//! Issue 7154: `Spawn` asks for an instance of a published type to exist
//! (ADR-0241 §9), and the name it would take decides the answer: an absent
//! name stands the instance up, a live one answers with itself and is not
//! re-initialised, a dropped one is spent (§8), a namespace no module
//! publishes is refused, and a native namespace is refused as composed by its
//! chassis or parent. A `parent` places the instance at `parent/NS:key`.
//!
//! The group fixture's `test.republish.gate` is instanced and counts each run
//! of its `wire`. The bundle's `test.matrix.child` declares `child_of` the
//! bundle's `test.matrix.parent`. Skipped when the fixture wasm hasn't been
//! built (`require_wasm`); CI pre-builds it and sets
//! `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a hard panic there.

use std::fs;

use aether_actor::{HandlesKind, HeldReply, actor};
use aether_component::ComponentHostCapability;
use aether_data::{Blob, ErasedActorPath, Kind};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{
    DropComponent, DropResult, LoadComponent, MonitorNotice, Publish, PublishResult, Spawn, SpawnResult,
};
use aether_substrate::BootError;
use aether_substrate::MonitorHandle;
use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending};
use aether_test_fixtures_kinds::{CountReport, WireCountQuery};

const GATE: &str = "test.republish.gate";
const OBSERVER_EXPORT: &str = "test.source_observer";
const MATRIX_PARENT_EXPORT: &str = "test.matrix.parent";
const MATRIX_CHILD_EXPORT: &str = "test.matrix.child";

/// Monitor the component at `target`; the reply confirms the watch stands.
#[aether_data::kind(name = "test.spawn_door.watch", no_serde)]
struct Watch {
    target: ErasedActorPath,
}

/// The path the watcher now monitors.
#[aether_data::kind(name = "test.spawn_door.watching", no_serde)]
struct Watching {
    target: ErasedActorPath,
}

/// Answered once the watched component's `MonitorNotice` has arrived.
#[aether_data::kind(name = "test.spawn_door.await_departure", copy, no_serde)]
struct AwaitDeparture;

#[aether_data::kind(name = "test.spawn_door.departed", copy, partial_eq, no_serde)]
struct Departed {
    notified: bool,
}

impl HeldReply for Departed {
    fn unanswered() -> Self {
        Self { notified: false }
    }
}

/// Watches one component, and holds an `AwaitDeparture` until the
/// component's close notice arrives, so a scenario waits on the close tail
/// that tombstones the name rather than on a clock.
struct DepartureWatcher {
    watch: Option<MonitorHandle>,
    departed: bool,
    waiting: Option<Held<Departed>>,
}

#[actor(singleton, root)]
impl NativeActor for DepartureWatcher {
    const NAMESPACE: &'static str = "test.spawn_door.watcher";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { watch: None, departed: false, waiting: None })
    }

    #[handler::single]
    fn on_watch(&mut self, ctx: &mut NativeCtx<'_>, watch: Watch) -> Watching {
        let proven = ctx.resolve_path(&watch.target).expect("the watched component is live");
        self.watch = Some(ctx.monitor(proven).expect("the watched component is monitorable"));
        Watching { target: watch.target }
    }

    #[handler::single]
    fn on_await_departure(&mut self, ctx: &mut NativeCtx<'_>, _await: AwaitDeparture) -> Pending<Departed> {
        let (pending, held) = ctx.hold::<Departed>();
        if self.departed {
            held.answer(ctx, &Departed { notified: true });
        } else {
            self.waiting = Some(held);
        }
        pending
    }

    #[handler::single]
    fn on_monitor_notice(&mut self, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        drop(self.watch.take());
        self.departed = true;
        if let Some(held) = self.waiting.take() {
            held.answer(ctx, &Departed { notified: true });
        }
    }
}

fn read(stem: &str) -> Option<Vec<u8>> {
    require_wasm(stem).map(|path| fs::read(path).expect("read fixture wasm"))
}

fn harness() -> SubstrateHarness {
    SubstrateHarness::builder()
        .with_component_host()
        .with_actor::<DepartureWatcher>(())
        .size(64, 48)
        .build()
        .expect("boot")
}

/// Send `mail` to the component host and await its reply.
fn host_call<K: Kind + Clone + 'static, R: Kind>(harness: &mut SubstrateHarness, mail: &K) -> R
where
    ComponentHostCapability: HandlesKind<K>,
{
    let host = harness.actor_ref::<ComponentHostCapability>();
    harness
        .execute(vec![("call", HarnessOp::send_and_await_reply(&host, mail))])
        .expect("component host call")
        .reply::<R>("call")
        .expect("decode the host's reply")
}

fn publish(harness: &mut SubstrateHarness, wasm: &[u8]) {
    let published: PublishResult =
        host_call(harness, &Publish { code: Blob::from(wasm.to_vec()), configs: Vec::new() });
    if let PublishResult::Err { error } = published {
        panic!("the publish was refused: {error}");
    }
}

fn spawn_gate(harness: &mut SubstrateHarness, key: &str) -> SpawnResult {
    let spawn = Spawn { namespace: GATE.to_owned(), key: Some(key.to_owned()), parent: None, config: Vec::new() };
    host_call(harness, &spawn)
}

#[test]
fn a_spawn_stands_up_an_absent_name_and_answers_a_live_one_with_itself() {
    // Catches: a spawn of a live name that stands a second instance up or
    // re-initialises the live one (its wire count would move), or a live
    // answer that names another instance; and a live instance that the host,
    // not the instance, answers for, whose stamped sender would be the host.
    let Some(wasm) = read("republish_group_v1") else {
        return;
    };
    let mut harness = harness();
    publish(&mut harness, &wasm);

    let first = spawn_gate(&mut harness, "a");
    let again = spawn_gate(&mut harness, "a");

    let SpawnResult::Spawned { path, .. } = first else {
        panic!("an absent name is stood up: {first:?}");
    };
    assert_eq!(path.as_str(), format!("{GATE}:a"));
    let SpawnResult::Live { path: live, .. } = again else {
        panic!("a live name answers with itself: {again:?}");
    };
    assert_eq!(live, path, "the live answer names the same instance");
    let load = LoadComponent { wasm, name: Some("a".to_owned()), config: Vec::new(), export: Some(GATE.to_owned()) };
    let (gate, loaded) = harness.load_any(&load).unwrap_or_else(|error| panic!("a load of a live name: {error}"));
    assert_eq!(loaded, path, "a load of the live name answers with the live instance");
    let wired = harness
        .execute(vec![("wired", HarnessOp::send_and_await_reply(gate, &WireCountQuery))])
        .expect("query the gate")
        .reply::<CountReport>("wired")
        .expect("decode CountReport");
    assert_eq!(wired.count, 1, "the live instance was wired once and never re-initialised");
}

#[test]
fn a_spawn_of_a_dropped_name_is_refused_as_spent() {
    // Catches: a tombstoned name reused by a spawn, so a dropped instance
    // comes back under its old name.
    let Some(wasm) = read("republish_group_v1") else {
        return;
    };
    let mut harness = harness();
    publish(&mut harness, &wasm);
    let SpawnResult::Spawned { path, .. } = spawn_gate(&mut harness, "a") else {
        panic!("the gate spawns");
    };
    let host = harness.actor_ref::<ComponentHostCapability>();
    let watcher = harness.actor_ref::<DepartureWatcher>();
    let closed = harness
        .execute(vec![
            ("watch", HarnessOp::send_and_await_reply(&watcher, &Watch { target: path.clone() })),
            ("drop", HarnessOp::send_and_await_reply(&host, &DropComponent { target: path })),
            ("departed", HarnessOp::send_and_await_reply(&watcher, &AwaitDeparture)),
        ])
        .expect("drop the gate and await its departure");
    assert!(matches!(closed.reply::<DropResult>("drop").expect("decode DropResult"), DropResult::Ok));
    assert_eq!(closed.reply::<Departed>("departed").expect("decode Departed"), Departed { notified: true });

    let respawned = spawn_gate(&mut harness, "a");

    let SpawnResult::Err { error } = respawned else {
        panic!("a dropped name is spent: {respawned:?}");
    };
    assert!(error.contains("SubnameRetired"), "the refusal names the retired name: {error}");
}

#[test]
fn a_spawn_of_an_unpublished_namespace_is_refused() {
    // Catches: a spawn that stands a guest up from code nobody published.
    let mut harness = harness();

    let spawn = Spawn { namespace: GATE.to_owned(), key: Some("a".to_owned()), parent: None, config: Vec::new() };
    let refused: SpawnResult = host_call(&mut harness, &spawn);

    let SpawnResult::Err { error } = refused else {
        panic!("an unpublished namespace is refused: {refused:?}");
    };
    assert!(error.contains(GATE), "the refusal names the namespace: {error}");
}

#[test]
fn a_spawn_of_a_native_namespace_is_refused_as_composed() {
    // Catches: a native namespace answered as unpublished (telling the caller
    // to publish code the binary links), or a composed singleton answered
    // `Live`.
    let mut harness = harness();

    let spawn =
        Spawn { namespace: DepartureWatcher::NAMESPACE.to_owned(), key: None, parent: None, config: Vec::new() };
    let refused: SpawnResult = host_call(&mut harness, &spawn);

    let SpawnResult::Err { error } = refused else {
        panic!("a native namespace is refused: {refused:?}");
    };
    assert!(error.contains(DepartureWatcher::NAMESPACE), "the refusal names the namespace: {error}");
    assert!(error.contains("composed"), "the refusal says native types are composed: {error}");
}

#[test]
fn a_spawn_beneath_a_parent_lands_at_its_path() {
    // Catches: a parent ignored by the spawn, placing the child at the root,
    // or its path built from anything but the parent's canonical path.
    let Some(wasm) = read("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = harness();
    let load = |export: &str| LoadComponent {
        wasm: wasm.clone(),
        name: None,
        config: Vec::new(),
        export: Some(export.to_owned()),
    };
    harness.load_any(&load(OBSERVER_EXPORT)).unwrap_or_else(|error| panic!("load the observer: {error}"));
    let (_, parent) =
        harness.load_any(&load(MATRIX_PARENT_EXPORT)).unwrap_or_else(|error| panic!("load the parent: {error}"));

    let spawn = Spawn {
        namespace: MATRIX_CHILD_EXPORT.to_owned(),
        key: Some("k".to_owned()),
        parent: Some(parent),
        config: Vec::new(),
    };
    let spawned: SpawnResult = host_call(&mut harness, &spawn);

    let SpawnResult::Spawned { path, .. } = spawned else {
        panic!("the child spawns beneath its declared parent: {spawned:?}");
    };
    assert_eq!(path.as_str(), format!("{MATRIX_PARENT_EXPORT}/{MATRIX_CHILD_EXPORT}:k"));
}
