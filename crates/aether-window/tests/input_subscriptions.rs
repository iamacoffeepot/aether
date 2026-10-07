//! ADR-0164 window-subscription round-trip via [`SubstrateHarness`]. Loads
//! `aether-test-fixtures`'s `probe` cdylib into a real chassis and exercises
//! selector-aware subscribe / unsubscribe plus the `aether.component.drop`
//! lifecycle's effect on the window subscriber set.
//!
//! Minimal composition (issue #3764): the component host (probe wasm) on
//! harness basics, which include the synthetic window runtime — no render,
//! no wgpu gate.
//! The probe wasm must be pre-built (`require_wasm` skips otherwise;
//! `AETHER_REQUIRE_RUNTIME=1` turns the skip into a hard failure).
//!
//! Targets the `Key` input stream, not `Tick`: issue 1490 moved `Tick`
//! onto `aether.lifecycle` because it is a frame-lifecycle stage, not a
//! window-originated interrupt. The probe subscribes `Key` for all windows
//! in `wire` and broadcasts a `key_observed` per dispatch; Tick-via-lifecycle
//! delivery is covered by the `substrate_harness` frame-loop scenarios.

use std::fs;
use std::mem;
use std::path::Path;

use aether_actor::{ActorPath, ActorRef, HeldReply, actor};
use aether_component::ComponentHostCapability;
use aether_data::{ErasedActorPath, Kind, LoadName};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent, MonitorNotice};
use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending};
use aether_substrate::{BootError, MonitorHandle};
use aether_test_fixtures_bundle::{KeyProbe, Probe};
use aether_test_fixtures_kinds::{KeyFocusObserved, KeyObserved, TakeKeyFocusAt, TextInputObserved, UnsubscribeKeys};
use aether_window::{Key, TextInput};
use aether_window::{WindowCapability, WindowInstance};

/// Arbitrary key code for the synthetic `Key` events these tests inject.
const KEY_CODE: u32 = 65;

/// The window the injected events claim to come from. The synthetic runtime
/// fans an injection out by selector without checking the window is live, so
/// no window is created for it.
fn test_window() -> ActorPath<WindowInstance> {
    WindowInstance::path(&test_window_name())
}

/// The name of [`test_window`], which a guest writes the typed path from.
fn test_window_name() -> LoadName {
    LoadName::new("main").expect("a valid window name")
}

fn boot_bench() -> SubstrateHarness {
    SubstrateHarness::builder().with_component_host().build().expect("boot")
}

/// Load the bundle's singleton `test.probe` export at its published name,
/// typed as `Probe`.
fn load_probe(harness: &mut SubstrateHarness, wasm_path: &Path) -> ActorRef<Probe> {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    harness
        .load::<Probe>(LoadComponent { wasm, name: None, config: Vec::new(), export: Some("test.probe".to_owned()) })
        .unwrap_or_else(|error| panic!("load_component(test.probe): {error}"))
}

/// Load the instanced root key subscriber `test.key_probe` under `key`: a
/// singleton is one per engine, so this is how one harness hosts several
/// independent subscribers.
fn load_key_probe(harness: &mut SubstrateHarness, wasm_path: &Path, key: &str) -> ActorRef<KeyProbe> {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let load = LoadComponent { wasm, name: Some(key.to_owned()), config: Vec::new(), export: None };
    harness.load::<KeyProbe>(load).unwrap_or_else(|error| panic!("load_component(test.key_probe:{key}): {error}"))
}

/// Inject `count` synthetic `Key` presses from one window. The synthetic
/// window actor fans each out to every matching subscriber; `execute` blocks on
/// settlement, so the `key_observed` broadcasts have landed by return.
fn send_keys(harness: &mut SubstrateHarness, count: usize) {
    let synthetic = harness.actor_ref::<WindowCapability>();
    let labels: Vec<String> = (0..count).map(|i| format!("key{i}")).collect();
    let steps: Vec<(&str, HarnessOp)> = labels
        .iter()
        .map(|label| {
            let key = Key { window: test_window(), code: KEY_CODE };
            (label.as_str(), HarnessOp::window_event(&synthetic, test_window(), &key))
        })
        .collect();
    harness.execute(steps).expect("key send sequence");
}

/// Have `probe` unsubscribe itself from `Key` on every window.
fn unsubscribe_keys(harness: &mut SubstrateHarness, probe: ActorRef<Probe>) {
    harness
        .execute(vec![("unsub", HarnessOp::send_and_settle(&probe, &UnsubscribeKeys))])
        .expect("unsubscribe sequence");
}

fn drop_component(harness: &mut SubstrateHarness, path: ErasedActorPath) {
    let result = harness
        .execute(vec![(
            "drop",
            HarnessOp::send_and_await_reply(
                &harness.actor_ref::<ComponentHostCapability>(),
                &DropComponent { target: path },
            ),
        )])
        .expect("drop sequence");
    match result.reply::<DropResult>("drop").expect("decode DropResult") {
        DropResult::Ok => {}
        DropResult::Err { error } => panic!("drop failed: {error}"),
    }
}

/// No probes loaded ⇒ no `Key` subscribers ⇒ an injected key event
/// fans out to no one. Confirms window fanout is gated on the
/// subscriber set rather than firing unconditionally.
#[test]
fn empty_subscribers_means_no_delivery() {
    if require_wasm("aether_test_fixtures_bundle").is_none() {
        return;
    }
    let mut harness = boot_bench();
    send_keys(&mut harness, 2);
    assert_eq!(
        harness.count_observed(KeyObserved::NAME),
        0,
        "no probe loaded but key_observed was broadcast; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

/// A subscribed probe receives fanned-out `TextInput`. The plausible bug
/// this guards: a window-originated kind that is published but not routed
/// to matching subscribers would silently disappear before the guest handler.
/// Injecting synthetic `TextInput` and observing the probe's re-broadcast
/// proves the generic window-event fan-out is wired.
#[test]
fn subscribed_component_receives_published_text_input() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = boot_bench();
    let _probe = load_probe(&mut harness, &wasm_path);
    let baseline = harness.count_observed(TextInputObserved::NAME);

    harness
        .execute(vec![(
            "text",
            HarnessOp::window_event(
                &harness.actor_ref::<WindowCapability>(),
                test_window(),
                &TextInput { window: test_window(), text: "hi".to_owned() },
            ),
        )])
        .expect("text send sequence");

    let delta = harness.count_observed(TextInputObserved::NAME) - baseline;
    assert_eq!(delta, 1, "expected 1 text_input_observed broadcast; observed kinds: {:?}", harness.observed_kinds());
}

/// One subscribed probe broadcasts once per injected key.
#[test]
fn subscribed_component_receives_published_keys() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = boot_bench();
    let _probe = load_probe(&mut harness, &wasm_path);
    let baseline = harness.count_observed(KeyObserved::NAME);

    send_keys(&mut harness, 3);
    let delta = harness.count_observed(KeyObserved::NAME) - baseline;
    assert_eq!(delta, 3, "expected 3 key_observed broadcasts; observed kinds: {:?}", harness.observed_kinds());
}

/// Two independently-loaded probes each subscribe their own mailbox
/// in `wire`; key fanout reaches both. 2 subscribers × 2 keys ⇒
/// 4 broadcasts.
#[test]
fn two_subscribers_each_receive_every_key() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = boot_bench();
    load_key_probe(&mut harness, &wasm_path, "a");
    load_key_probe(&mut harness, &wasm_path, "b");
    let baseline = harness.count_observed(KeyObserved::NAME);

    send_keys(&mut harness, 2);
    let delta = harness.count_observed(KeyObserved::NAME) - baseline;
    assert_eq!(
        delta,
        4,
        "2 subscribers × 2 keys should yield 4 broadcasts; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

/// The probe's own all-window unsubscribe removes it from the `Key`
/// subscriber set; subsequent key events stop producing broadcasts from
/// that probe.
#[test]
fn unsubscribe_stops_delivery() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = boot_bench();
    let probe = load_probe(&mut harness, &wasm_path);
    let baseline = harness.count_observed(KeyObserved::NAME);

    send_keys(&mut harness, 1);
    assert_eq!(
        harness.count_observed(KeyObserved::NAME) - baseline,
        1,
        "expected 1 broadcast in the pre-unsubscribe window; observed kinds: {:?}",
        harness.observed_kinds(),
    );
    let pre_unsub = harness.count_observed(KeyObserved::NAME);

    unsubscribe_keys(&mut harness, probe);
    send_keys(&mut harness, 2);
    assert_eq!(
        harness.count_observed(KeyObserved::NAME),
        pre_unsub,
        "key_observed climbed after unsubscribe; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

/// `aether.component.drop` clears the dropped mailbox from the window
/// subscriber set as a side effect of lifecycle teardown
/// (ADR-0164 + ADR-0038). Subsequent key events don't broadcast from the
/// dropped probe.
#[test]
fn drop_clears_subscriptions() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness = boot_bench();
    let probe = load_probe(&mut harness, &wasm_path);
    let probe_path = harness.actor_path(&probe);
    let baseline = harness.count_observed(KeyObserved::NAME);

    send_keys(&mut harness, 1);
    assert_eq!(
        harness.count_observed(KeyObserved::NAME) - baseline,
        1,
        "expected 1 broadcast in the pre-drop window; observed kinds: {:?}",
        harness.observed_kinds(),
    );
    let pre_drop = harness.count_observed(KeyObserved::NAME);

    drop_component(&mut harness, probe_path);
    send_keys(&mut harness, 2);
    assert_eq!(
        harness.count_observed(KeyObserved::NAME),
        pre_drop,
        "key_observed climbed after drop; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

/// Monitor the component at `target`.
#[aether_data::kind(name = "test.window.key_focus.watch", no_serde)]
struct Watch {
    target: ErasedActorPath,
}

/// Answered once the watched component's `MonitorNotice` has arrived.
#[aether_data::kind(name = "test.window.key_focus.await_departure", copy, no_serde)]
struct AwaitDeparture;

#[aether_data::kind(name = "test.window.key_focus.departed", copy, partial_eq, no_serde)]
struct Departed {
    notified: bool,
}

impl HeldReply for Departed {
    fn unanswered() -> Self {
        Self { notified: false }
    }
}

/// Watches one component as the window watches a key focus holder, and holds
/// an `AwaitDeparture` until the component's notice arrives. A drop's reply
/// precedes the departure it causes, which is posted to every watcher past
/// the dropping chain's settlement, so this is the signal a scenario waits on
/// before it reads what the departure changed.
struct DepartureWatcher {
    state: Departure,
}

/// Where a [`DepartureWatcher`] is in its one watch.
enum Departure {
    /// No `Watch` has arrived.
    Unwatched,
    /// The component is watched and nobody has asked after it.
    Watching(MonitorHandle),
    /// The component is watched and `held` is answered when it departs.
    Awaited { _monitor: MonitorHandle, held: Held<Departed> },
    /// The component's notice arrived.
    Departed,
}

#[actor(singleton, root)]
impl NativeActor for DepartureWatcher {
    const NAMESPACE: &'static str = "test.window.key_focus.watcher";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { state: Departure::Unwatched })
    }

    #[handler::tell]
    fn on_watch(&mut self, ctx: &mut NativeCtx<'_>, Watch { target }: Watch) {
        let proven = ctx.resolve_path(&target).expect("the watched component is live");
        self.state = Departure::Watching(ctx.monitor(proven));
    }

    #[handler::request]
    fn on_await_departure(&mut self, ctx: &mut NativeCtx<'_>, _await: AwaitDeparture) -> Pending<Departed> {
        let (pending, held) = ctx.hold::<Departed>();

        self.state = match mem::replace(&mut self.state, Departure::Departed) {
            Departure::Watching(monitor) => Departure::Awaited { _monitor: monitor, held },
            Departure::Departed => {
                held.answer(ctx, &Departed { notified: true });
                Departure::Departed
            }
            Departure::Unwatched => panic!("AwaitDeparture arrived before any Watch"),
            Departure::Awaited { .. } => panic!("a second AwaitDeparture arrived while one is held"),
        };
        pending
    }

    #[handler::event]
    fn on_monitor_notice(&mut self, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        if let Departure::Awaited { held, .. } = mem::replace(&mut self.state, Departure::Departed) {
            held.answer(ctx, &Departed { notified: true });
        }
    }
}

/// A guest takes key focus by the same plain mail a native actor sends: the
/// taker alone is sent the window's keys, it is told it gained key focus, and
/// its drop empties the slot. The plausible bugs: the take is not sendable
/// from a marker-only guest, a guest's handlers do not satisfy the take's
/// sender requirement so the engine refuses it, a notice does not reach a
/// guest, or a dropped component's slot is kept and silences the survivors.
#[test]
fn a_guest_takes_key_focus_and_its_drop_empties_the_slot() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let mut harness =
        SubstrateHarness::builder().with_component_host().with_actor::<DepartureWatcher>(()).build().expect("boot");
    let watcher = harness.actor_ref::<DepartureWatcher>();
    let taker = load_key_probe(&mut harness, &wasm_path, "a");
    load_key_probe(&mut harness, &wasm_path, "b");
    load_key_probe(&mut harness, &wasm_path, "c");
    let taker_path = harness.actor_path(&taker);

    harness
        .execute(vec![
            ("watch", HarnessOp::send_and_settle(&watcher, &Watch { target: taker_path.clone() })),
            ("take", HarnessOp::send_and_settle(&taker, &TakeKeyFocusAt { window: test_window_name() })),
        ])
        .expect("take sequence");
    assert_eq!(
        harness.count_observed(KeyFocusObserved::NAME),
        1,
        "the taker reports one gained notice; observed kinds: {:?}",
        harness.observed_kinds(),
    );

    let held = harness.count_observed(KeyObserved::NAME);
    send_keys(&mut harness, 1);
    assert_eq!(harness.count_observed(KeyObserved::NAME) - held, 1, "the holder alone is sent the key");

    drop_component(&mut harness, taker_path);
    let departed = harness
        .execute(vec![("departed", HarnessOp::send_and_await_reply(&watcher, &AwaitDeparture))])
        .expect("await the departure");
    assert_eq!(departed.reply::<Departed>("departed").expect("decode Departed"), Departed { notified: true });
    let emptied = harness.count_observed(KeyObserved::NAME);
    // A second code: the first is still down, and a key that is down keeps
    // routing by the record its press made, which names the dropped holder.
    let next = Key { window: test_window(), code: KEY_CODE + 1 };
    let window = harness.actor_ref::<WindowCapability>();
    harness.execute(vec![("next", HarnessOp::window_event(&window, test_window(), &next))]).expect("key send");
    assert_eq!(harness.count_observed(KeyObserved::NAME) - emptied, 2, "both survivors are sent the next key");
}
