//! A dropped instance closes, and its name is spent (ADR-0241 §8, issue
//! #7066).
//!
//! `aether.component.drop` closes the instance's trampoline: the close runs
//! the guest's `unwire`, and its tail tombstones the name, retires its route
//! to `Dropped`, and sends each watcher a `MonitorNotice`. The scenario waits
//! on that notice, the signal production watchers act on, through a native
//! watcher composed beside the component host, and then checks that nothing
//! can take the name back.
//!
//! A guest that is never dropped closes the same way when its engine tears
//! down (ADR-0247 rule 5), which the last scenario reads off the boot
//! fixture's `unwire` marker.

use std::fs;

use aether_actor::{HeldReply, actor};
use aether_component::ComponentHostCapability;
use aether_data::{ErasedActorPath, Kind, LoadName};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent, LoadResult, MonitorNotice, Publish, PublishResult};
use aether_substrate::BootError;
use aether_substrate::MonitorHandle;
use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending};
use aether_substrate::testing::successor_wasm;
use aether_test_fixtures_bundle::Panel;
use aether_test_fixtures_kinds::{BootObserved, BootTornDown};

const BUNDLE: &str = "aether_test_fixtures_bundle";
/// An instanced export, so a load names its key.
const PANEL_EXPORT: &str = "test.ui.panel";

/// Monitor the component at `target`; the reply confirms the watch stands.
#[aether_data::kind(name = "test.drop_close.watch", no_serde)]
struct Watch {
    target: ErasedActorPath,
}

/// The path the watcher now monitors.
#[aether_data::kind(name = "test.drop_close.watching", partial_eq, no_serde)]
struct Watching {
    target: ErasedActorPath,
}

/// Answered once the watched component's `MonitorNotice` has arrived.
#[aether_data::kind(name = "test.drop_close.await_departure", copy, no_serde)]
struct AwaitDeparture;

#[aether_data::kind(name = "test.drop_close.departed", copy, partial_eq, no_serde)]
struct Departed {
    notified: bool,
}

impl HeldReply for Departed {
    fn unanswered() -> Self {
        Self { notified: false }
    }
}

/// Watches one component the way a capability watches its registrants, and
/// holds an `AwaitDeparture` until the component's notice arrives.
struct DepartureWatcher {
    watch: Option<MonitorHandle>,
    departed: bool,
    waiting: Option<Held<Departed>>,
}

#[actor(singleton, root)]
impl NativeActor for DepartureWatcher {
    const NAMESPACE: &'static str = "test.drop_close.watcher";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { watch: None, departed: false, waiting: None })
    }

    #[handler::request]
    fn on_watch(&mut self, ctx: &mut NativeCtx<'_>, watch: Watch) -> Watching {
        let proven = ctx.resolve_path(&watch.target).expect("the watched component is live");
        self.watch = Some(ctx.monitor(proven).expect("the watched component is monitorable"));
        Watching { target: watch.target }
    }

    #[handler::request]
    fn on_await_departure(&mut self, ctx: &mut NativeCtx<'_>, _await: AwaitDeparture) -> Pending<Departed> {
        let (pending, held) = ctx.hold::<Departed>();
        if self.departed {
            held.answer(ctx, &Departed { notified: true });
        } else {
            self.waiting = Some(held);
        }
        pending
    }

    #[handler::event]
    fn on_monitor_notice(&mut self, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        drop(self.watch.take());
        self.departed = true;
        if let Some(held) = self.waiting.take() {
            held.answer(ctx, &Departed { notified: true });
        }
    }
}

fn keyed_load(wasm: &[u8]) -> LoadComponent {
    LoadComponent {
        wasm: wasm.to_vec(),
        name: Some("victim".to_owned()),
        config: Vec::new(),
        export: Some(PANEL_EXPORT.to_owned()),
    }
}

/// Catches a drop that leaves a refillable `Live` slot: its name would stay
/// listed and published, a reload would answer with the live instance rather
/// than be refused as retired, a republish of its module would refill it, and a second drop at
/// its path would succeed.
#[test]
fn a_dropped_instance_closes_and_its_name_is_spent() {
    let Some(wasm_path) = require_wasm(BUNDLE) else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .with_actor::<DepartureWatcher>(())
        .build()
        .expect("boot");
    let host = harness.actor_ref::<ComponentHostCapability>();
    let watcher = harness.actor_ref::<DepartureWatcher>();

    harness.publish(wasm.clone()).unwrap_or_else(|error| panic!("the bundle publishes: {error}"));
    let victim_key = LoadName::new("victim").expect("a valid instance key");
    let victim = harness.spawn_keyed::<Panel>(&victim_key).unwrap_or_else(|error| panic!("the panel spawns: {error}"));
    let path = harness.actor_path(&victim);
    assert_eq!(path.as_str(), format!("{PANEL_EXPORT}:victim"));

    let drop = DropComponent { target: path.clone() };
    let closed = harness
        .execute(vec![
            ("watch", HarnessOp::send_and_await_reply(&watcher, &Watch { target: path.clone() })),
            ("drop", HarnessOp::send_and_await_reply(&host, &drop)),
            ("departed", HarnessOp::send_and_await_reply(&watcher, &AwaitDeparture)),
        ])
        .expect("drop and await the departure");
    assert_eq!(closed.reply::<Watching>("watch").expect("decode Watching"), Watching { target: path.clone() });
    if let DropResult::Err { error } = closed.reply::<DropResult>("drop").expect("decode DropResult") {
        panic!("the victim drops: {error}");
    }
    assert_eq!(closed.reply::<Departed>("departed").expect("decode Departed"), Departed { notified: true });

    let reload = harness.load_any(&keyed_load(&wasm)).expect_err("a reload of the dropped name is refused");
    assert!(reload.to_string().contains("SubnameRetired"), "the refusal names the retired name: {reload}");

    // The reload's module publish went through the registry owner after the
    // close tail's route retirement, so the owner has applied it.
    let listed = harness.list_components().expect("list components");
    assert!(!listed.contains(&path.to_string()), "the dropped instance is not listed: {listed:?}");
    assert_eq!(harness.published_contract(victim.erase()), None, "the dropped instance's route no longer reads `Live`");

    // A republish of the module moves every live instance and refills
    // nothing: the dropped name stays spent.
    let publish = Publish { code: successor_wasm(&wasm, 1).into(), configs: Vec::new() };
    let refused = harness
        .execute(vec![
            ("publish", HarnessOp::send_and_await_reply(&host, &publish)),
            ("drop-again", HarnessOp::send_and_await_reply(&host, &drop)),
        ])
        .expect("republish, then drop the dropped path");
    if let PublishResult::Err { error } = refused.reply::<PublishResult>("publish").expect("decode PublishResult") {
        panic!("a republish with no live instance of the dropped path still publishes: {error}");
    }
    let listed = harness.list_components().expect("list components");
    assert!(!listed.contains(&path.to_string()), "the republish refills no dropped name: {listed:?}");
    let DropResult::Err { error } = refused.reply::<DropResult>("drop-again").expect("decode DropResult") else {
        panic!("a second drop at a dropped path is refused");
    };
    assert!(error.contains(path.as_str()), "the refusal names the path: {error}");
}

/// Catches a drop the host forwards to an actor it did not load: a live
/// native actor's path proves at receipt, but the host holds no control proof
/// for it, so the drop is refused before anything is forwarded.
#[test]
fn a_drop_at_a_live_route_the_host_did_not_load_is_refused() {
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .with_actor::<DepartureWatcher>(())
        .build()
        .expect("boot");
    let host = harness.actor_ref::<ComponentHostCapability>();
    let target = ErasedActorPath::new("test.drop_close.watcher").expect("the watcher's namespace is an actor path");

    let refused = harness
        .execute(vec![("drop", HarnessOp::send_and_await_reply(&host, &DropComponent { target: target.clone() }))])
        .expect("drop the watcher");
    let DropResult::Err { error } = refused.reply::<DropResult>("drop").expect("decode DropResult") else {
        panic!("a drop at an actor the host did not load is refused");
    };
    assert!(
        error.contains("no live component to drop at") && error.contains(target.as_str()),
        "the refusal names the path: {error}"
    );
}

/// The boot fixture (`aether-test-fixtures-boot`): its boot actor mails
/// [`BootObserved`] from `wire` and [`BootTornDown`] from `unwire`.
const BOOT_FIXTURE: &str = "aether_test_fixtures_boot";

/// Catches a guest whose `unwire` export runs only on a drop addressed at
/// it: a guest nobody dropped still runs `unwire` when its engine tears down,
/// because every close of its trampoline releases it (ADR-0247 rule 5).
#[test]
fn engine_teardown_runs_the_unwire_of_a_guest_nobody_dropped() {
    let Some(wasm_path) = require_wasm(BOOT_FIXTURE) else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let host = harness.actor_ref::<ComponentHostCapability>();

    // Any load of the module stands its boot actor up beside the export.
    let load =
        LoadComponent { wasm, name: None, config: Vec::new(), export: Some("aether.test.boot.widget_a".to_owned()) };
    let loaded =
        harness.execute(vec![("load", HarnessOp::send_and_await_reply(&host, &load))]).expect("load the widget");
    if let LoadResult::Err { error } = loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        panic!("the widget loads: {error}");
    }
    harness.execute(vec![("settle", HarnessOp::advance(1))]).expect("settle the boot's wire");
    assert_eq!(
        harness.count_observed(BootObserved::NAME),
        1,
        "the boot wired; observed: {:?}",
        harness.observed_kinds()
    );
    assert_eq!(harness.count_observed(BootTornDown::NAME), 0, "a live boot has not run unwire");

    assert_eq!(
        harness.close_and_count_observed(BootTornDown::NAME),
        1,
        "engine teardown closes the boot, and its close runs the guest's unwire",
    );
}
