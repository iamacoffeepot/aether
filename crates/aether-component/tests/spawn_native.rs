//! Issue 7162: a `Spawn` naming a native namespace answers as it does for a
//! guest (ADR-0241 §9). A mail-spawnable native type (`Params = ()`, a
//! `Config` the engine's source stack resolves) is stood up at an absent
//! name and answers `Spawned` itself; a live name answers `Live` from the
//! instance there, which is not re-initialised; a retired name is spent; a
//! composed singleton answers `Live`. A spawn carrying config bytes, or of a
//! type whose `Params` is not `()`, is refused naming why.
//!
//! Every spawn goes through `Requester`, a native actor that sends the raw
//! `Spawn` to the component host and reports the reply beside its stamped
//! sender, so each scenario sees who answered, not only what.

use std::sync::mpsc::Sender;

use aether_actor::{ErasedActorRef, HeldReply, actor};
use aether_component::ComponentHostCapability;
use aether_data::ErasedActorPath;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{MonitorNotice, Spawn, SpawnResult};
use aether_substrate::BootError;
use aether_substrate::MonitorHandle;
use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending};

const BEACON: &str = "test.spawn_native.beacon";
const LIGHTHOUSE: &str = "test.spawn_native.lighthouse";
const TETHERED: &str = "test.spawn_native.tethered";

/// Ask the requester to send `spawn` to the component host.
#[aether_data::kind(name = "test.spawn_native.ask", no_serde)]
struct Ask {
    spawn: Spawn,
}

/// The host's answer to an `Ask`, beside the path of the actor that sent it.
#[aether_data::kind(name = "test.spawn_native.asked", no_serde)]
struct Asked {
    result: SpawnResult,
    sender: Option<ErasedActorPath>,
}

impl HeldReply for Asked {
    fn unanswered() -> Self {
        Self { result: SpawnResult::Err { error: "unanswered".to_owned() }, sender: None }
    }
}

/// The context an `Ask`'s spawn carries into its reply: the ask's held reply.
#[aether_data::kind(name = "test.spawn_native.ask_context")]
struct AskContext {
    held: Held<Asked>,
}

/// How many times the beacon the last spawn answered from has run `wire`.
#[aether_data::kind(name = "test.spawn_native.query", copy, default, no_serde)]
struct Query;

#[aether_data::kind(name = "test.spawn_native.report", copy, eq, no_serde)]
struct Report {
    wired: u32,
}

impl HeldReply for Report {
    fn unanswered() -> Self {
        Self { wired: 0 }
    }
}

/// Close the beacon the last spawn answered from, answered once it departs.
#[aether_data::kind(name = "test.spawn_native.retire_last", copy, default, no_serde)]
struct RetireLast;

#[aether_data::kind(name = "test.spawn_native.retire", copy, default, no_serde)]
struct Retire;

#[aether_data::kind(name = "test.spawn_native.retired", copy, eq, no_serde)]
struct Retired {
    departed: bool,
}

impl HeldReply for Retired {
    fn unanswered() -> Self {
        Self { departed: false }
    }
}

/// The rows the requester reaches a beacon through.
#[aether_actor::protocol]
trait BeaconRows {
    fn query(mail: Query) -> Report;
    fn retire(mail: Retire);
}

/// The mail-spawnable native type: instanced, rooted, `Params = ()`, and a
/// `Config` of `()`. It counts its `wire` runs, so a re-initialisation
/// shows.
struct Beacon {
    wired: u32,
}

#[actor(instanced, root)]
impl NativeActor for Beacon {
    const NAMESPACE: &'static str = BEACON;
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { wired: 0 })
    }

    fn wire(state: &mut Self, _ctx: &mut NativeCtx<'_>) {
        state.wired += 1;
    }

    #[handler::single]
    fn on_query(&mut self, _ctx: &mut NativeCtx<'_>, _query: Query) -> Report {
        Report { wired: self.wired }
    }

    #[handler::single]
    fn on_retire(&mut self, ctx: &mut NativeCtx<'_>, _retire: Retire) {
        ctx.shutdown();
    }
}

/// A native singleton the harness composes.
struct Lighthouse;

#[actor(singleton, root)]
impl NativeActor for Lighthouse {
    const NAMESPACE: &'static str = LIGHTHOUSE;
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_query(&mut self, _ctx: &mut NativeCtx<'_>, _query: Query) -> Report {
        Report { wired: 0 }
    }
}

/// A native type whose `Params` is construction wiring, so no spawn can
/// build it.
struct Tethered;

#[actor(instanced, root)]
impl NativeActor for Tethered {
    const NAMESPACE: &'static str = TETHERED;
    type Config = ();
    type Params = Sender<()>;

    fn init((): (), _tether: Sender<()>, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_query(&mut self, _ctx: &mut NativeCtx<'_>, _query: Query) -> Report {
        Report { wired: 0 }
    }
}

/// Sends raw `Spawn`s to the component host and reports each reply with its
/// stamped sender; keeps the last sender to query or retire it.
struct Requester {
    last: Option<ErasedActorRef>,
    watch: Option<MonitorHandle>,
    retiring: Option<Held<Retired>>,
}

#[actor(singleton, root, depends(ComponentHostCapability))]
impl NativeActor for Requester {
    const NAMESPACE: &'static str = "test.spawn_native.requester";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { last: None, watch: None, retiring: None })
    }

    #[handler::single]
    fn on_ask(&mut self, ctx: &mut NativeCtx<'_>, Ask { spawn }: Ask) -> Pending<Asked> {
        let (pending, held) = ctx.hold::<Asked>();
        let _ = ctx.send_with_context::<ComponentHostCapability>(&spawn, AskContext { held });
        pending
    }

    #[handler::single]
    fn on_spawn_result(&mut self, ctx: &mut NativeCtx<'_>, result: SpawnResult) {
        let AskContext { held } = ctx.take_context().expect("a spawn reply answers an ask");
        let sender = ctx.sender();
        if matches!(result, SpawnResult::Spawned { .. } | SpawnResult::Live { .. }) {
            self.last = sender;
        }
        let sender = sender.map(|sender| ctx.actor_path(sender));
        held.answer(ctx, &Asked { result, sender });
    }

    #[handler::single]
    fn on_query(&mut self, ctx: &mut NativeCtx<'_>, query: Query) -> Pending<Report> {
        let (pending, held) = ctx.hold::<Report>();
        let beacon = self.beacon(ctx);
        held.hand_off(ctx, beacon, &query);
        pending
    }

    #[handler::single]
    fn on_retire_last(&mut self, ctx: &mut NativeCtx<'_>, _retire: RetireLast) -> Pending<Retired> {
        let (pending, held) = ctx.hold::<Retired>();
        let beacon = self.beacon(ctx);
        self.watch = Some(ctx.monitor(beacon.erase()).expect("the beacon is monitorable"));
        self.retiring = Some(held);
        ctx.send_to(beacon, &Retire);
        pending
    }

    #[handler::single]
    fn on_monitor_notice(&mut self, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        drop(self.watch.take());
        if let Some(held) = self.retiring.take() {
            held.answer(ctx, &Retired { departed: true });
        }
    }
}

impl Requester {
    fn beacon(&self, ctx: &NativeCtx<'_, Self>) -> aether_actor::ProtocolRef<BeaconRows> {
        let last = self.last.expect("a spawn answered from a beacon first");
        ctx.cast::<BeaconRows>(last).expect("the last spawn answered from a live beacon")
    }
}

fn harness() -> SubstrateHarness {
    SubstrateHarness::builder()
        .with_component_host()
        .with_actor::<Lighthouse>(())
        .with_actor::<Requester>(())
        .size(64, 48)
        .build()
        .expect("boot")
}

fn spawn(namespace: &str, key: Option<&str>, config: Vec<u8>) -> Spawn {
    Spawn { namespace: namespace.to_owned(), key: key.map(str::to_owned), parent: None, config }
}

/// Send `spawn` to the component host through the requester and await the
/// answer.
fn ask(harness: &mut SubstrateHarness, spawn: Spawn) -> Asked {
    let requester = harness.actor_ref::<Requester>();
    harness
        .execute(vec![("ask", HarnessOp::send_and_await_reply(&requester, &Ask { spawn }))])
        .expect("ask the requester")
        .reply::<Asked>("ask")
        .expect("decode Asked")
}

#[test]
fn a_native_spawn_stands_up_an_absent_name_and_answers_a_live_one_from_itself() {
    // Catches: a native spawn the host answers for, whose stamped sender
    // would be the host rather than the instance; a spawn of a live name that
    // stands a second instance up or re-initialises the live one (its wire
    // count would move); and a live answer from another instance.
    let mut harness = harness();

    let first = ask(&mut harness, spawn(BEACON, Some("a"), Vec::new()));
    let again = ask(&mut harness, spawn(BEACON, Some("a"), Vec::new()));

    let SpawnResult::Spawned { path, .. } = first.result else {
        panic!("an absent name is stood up: {:?}", first.result);
    };
    assert_eq!(path.as_str(), format!("{BEACON}:a"));
    assert_eq!(first.sender.as_ref(), Some(&path), "the new instance answers the spawn itself");
    let SpawnResult::Live { path: live, .. } = again.result else {
        panic!("a live name answers with itself: {:?}", again.result);
    };
    assert_eq!(live, path, "the live answer names the same instance");
    assert_eq!(again.sender.as_ref(), Some(&path), "the live instance answers the spawn itself");
    let requester = harness.actor_ref::<Requester>();
    let wired = harness
        .execute(vec![("wired", HarnessOp::send_and_await_reply(&requester, &Query))])
        .expect("query the beacon")
        .reply::<Report>("wired")
        .expect("decode Report");
    assert_eq!(wired, Report { wired: 1 }, "the live instance was wired once and never re-initialised");
}

#[test]
fn a_native_spawn_of_a_retired_name_is_refused_as_spent() {
    // Catches: a tombstoned native name reused by a spawn, so a closed
    // instance comes back under its old name.
    let mut harness = harness();
    let first = ask(&mut harness, spawn(BEACON, Some("a"), Vec::new()));
    assert!(matches!(first.result, SpawnResult::Spawned { .. }), "the beacon spawns: {:?}", first.result);
    let requester = harness.actor_ref::<Requester>();
    let retired = harness
        .execute(vec![("retire", HarnessOp::send_and_await_reply(&requester, &RetireLast))])
        .expect("retire the beacon")
        .reply::<Retired>("retire")
        .expect("decode Retired");
    assert_eq!(retired, Retired { departed: true });

    let respawned = ask(&mut harness, spawn(BEACON, Some("a"), Vec::new()));

    let SpawnResult::Err { error } = respawned.result else {
        panic!("a retired name is spent: {:?}", respawned.result);
    };
    assert!(error.contains("SubnameRetired"), "the refusal names the retired name: {error}");
}

#[test]
fn a_native_spawn_of_a_composed_singleton_answers_live_from_it() {
    // Catches: a composed native singleton re-stood-up or refused by a
    // spawn, or answered for by the host rather than by itself.
    let mut harness = harness();

    let asked = ask(&mut harness, spawn(LIGHTHOUSE, None, Vec::new()));

    let SpawnResult::Live { path, .. } = asked.result else {
        panic!("a composed singleton answers with itself: {:?}", asked.result);
    };
    assert_eq!(path.as_str(), LIGHTHOUSE);
    assert_eq!(asked.sender.as_ref(), Some(&path), "the singleton answers the spawn itself");
}

#[test]
fn a_native_spawn_is_refused_config_bytes_and_a_type_with_wiring_params() {
    // Catches: config bytes decoded into a native type's knobs (ADR-0235: a
    // knob never rides a kind field), and a type built with no construction
    // wiring its `Params` needs.
    let mut harness = harness();

    let configured = ask(&mut harness, spawn(BEACON, Some("b"), vec![1]));
    let tethered = ask(&mut harness, spawn(TETHERED, Some("t"), Vec::new()));

    let SpawnResult::Err { error } = configured.result else {
        panic!("a native spawn carrying config bytes is refused: {:?}", configured.result);
    };
    assert!(error.contains("no config bytes"), "the refusal says why: {error}");
    let SpawnResult::Err { error } = tethered.result else {
        panic!("a type whose Params is not () is refused: {:?}", tethered.result);
    };
    assert!(error.contains(TETHERED) && error.contains("Params is not ()"), "the refusal says why: {error}");
}
