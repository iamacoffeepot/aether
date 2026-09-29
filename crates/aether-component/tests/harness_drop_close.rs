//! A dropped instance closes, and its name is spent (ADR-0241 §8, issue
//! #7066).
//!
//! `aether.component.drop` runs the guest's `unwire` and closes its
//! trampoline: the close tail tombstones the name, retires its route to
//! `Dropped`, and sends each watcher a `MonitorNotice`. The scenario waits on
//! that notice, the signal production watchers act on, through a native
//! watcher composed beside the component host, and then checks that nothing
//! can take the name back.

use std::fs;

use aether_actor::{HeldReply, actor};
use aether_component::ComponentHostCapability;
use aether_data::ErasedActorPath;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent, MonitorNotice, ReplaceComponent, ReplaceResult};
use aether_substrate::BootError;
use aether_substrate::MonitorHandle;
use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending};

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

fn keyed_load(wasm: &[u8]) -> LoadComponent {
    LoadComponent {
        wasm: wasm.to_vec(),
        name: Some("victim".to_owned()),
        config: Vec::new(),
        export: Some(PANEL_EXPORT.to_owned()),
    }
}

/// Catches a drop that leaves a refillable `Live` slot: its name would stay
/// listed and published, a reload would answer `SubnameInUse` rather than
/// retired, and a replace or a second drop at its path would succeed.
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

    let (victim, path) =
        harness.load_any(&keyed_load(&wasm)).unwrap_or_else(|error| panic!("the panel loads: {error}"));
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
    assert_eq!(harness.published_contract(victim), None, "the dropped instance's route no longer reads `Live`");

    let replace = ReplaceComponent {
        target: path.clone(),
        wasm,
        drain_timeout_ms: None,
        config: Vec::new(),
        export: Some(PANEL_EXPORT.to_owned()),
    };
    let refused = harness
        .execute(vec![
            ("replace", HarnessOp::send_and_await_reply(&host, &replace)),
            ("drop-again", HarnessOp::send_and_await_reply(&host, &drop)),
        ])
        .expect("replace and drop the dropped path");
    let ReplaceResult::Err { error } = refused.reply::<ReplaceResult>("replace").expect("decode ReplaceResult") else {
        panic!("a replace at a dropped path is refused");
    };
    assert!(error.contains(path.as_str()), "the refusal names the path: {error}");
    let DropResult::Err { error } = refused.reply::<DropResult>("drop-again").expect("decode DropResult") else {
        panic!("a second drop at a dropped path is refused");
    };
    assert!(error.contains(path.as_str()), "the refusal names the path: {error}");
}
