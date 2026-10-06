//! Issue 7496: a component watches an actor it holds a typed reference to
//! (ADR-0079 §8), driven through real loads, republishes, drops and mail.
//!
//! The fixtures are the republish watch family. `WatchLedger` watches each
//! provider that admits itself, by casting the admit's sender to a protocol,
//! and keeps the id `ctx.watch` returned under the admit's tag; its departure
//! handlers keep what they were handed and mail it to the harness observer.
//! `WatchDesk`
//! spawns an inline `WatchClerk` whose own `wire` watches another desk.
//! `republish_watch_v2` republishes them with a peer that traps in
//! `on_rehydrate` by config, and `republish_watch_reshaped` reshapes the
//! ledger's watch context kind.
//!
//! Two native actors are composed beside the component host. [`Provider`]
//! covers both protocols the ledger watches through, relays an admit to the
//! ledger so the ledger sees it as the sender, and shuts itself down when
//! told, so it closes whether or not the component host is being drained.
//! [`Watcher`] monitors one actor the way a capability does.
//!
//! A `WatchId` never leaves the actor that holds it, so no scenario sees
//! one. A fixture reports an id as its ordinal, the id's index among the
//! distinct ids that instance has been handed, so two ordinals from one
//! instance are equal exactly when the ids are. An instance a republish
//! installs has been handed none: its first departure reports
//! [`FIRST_HANDED`], and what shows that it kept its predecessor's id is the
//! tag, since the SDK stores a watch's context under the id and a departure
//! whose id has no context runs no handler.
//!
//! No scenario waits on a clock. A departure's notices are posted to the
//! watchers in the order they registered, and every scenario registers the
//! [`Watcher`] after the guest's watch. So once the [`Watcher`] has handled
//! its own notice, the guest's notice is already in the guest's inbox, and a
//! query sent after that is handled behind it. That is also how a scenario
//! shows that no handler ran. A scenario that holds a republish prepared
//! cannot await a reply, since every harness wait drains the pumped host; it
//! waits for the `ListComponents` the [`Watcher`] mails the host from its
//! notice to be queued there.
//!
//! No scenario here covers a ledger whose `wire` watched and then refused or
//! trapped. A registration that outlived such a birth would post its notice
//! to a mailbox whose guest holds no row for it, which runs nothing, so the
//! harness cannot tell a leak from a release. The cover is the substrate's
//! `dropping_a_guest_that_watched_leaves_both_monitor_indices_empty`, which
//! reads the registry's indices after the instance drops.
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`), and only
//! under `AETHER_ALLOW_WASM_SKIP=1`.

use std::fs;

use aether_actor::{ActorPath, ActorRef, HeldReply, ProtocolPath, actor};
use aether_component::ComponentHostCapability;
use aether_component::component::Prepared;
use aether_data::{ErasedActorPath, Kind, LoadName};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SendTarget, SubstrateHarness, SubstrateHarnessError};
use aether_kinds::{
    DropComponent, DropResult, ListComponents, ListComponentsResult, LoadComponent, MonitorNotice, Publish,
    PublishResult,
};
use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending};
use aether_substrate::{BootError, MonitorHandle};
use aether_test_fixtures_kinds::{
    WatchAdmit, WatchAdmitResult, WatchAudit, WatchClerkSpawn, WatchDeparture, WatchHeld, WatchHold, WatchLedgerConfig,
    WatchLedgerQuery, WatchLedgerReport, WatchNudge, WatchPeerAdmit, WatchPeerConfig, WatchProvider, WatchRelease,
    WatchThrough, WireOutcome,
};
use aether_test_fixtures_republish::{WatchClerk, WatchDesk, WatchLedger, WatchPeer};

/// The ordinal of the first watch id an instance is handed.
const FIRST_HANDED: u32 = 0;

/// The refusal a republish reports when the v2 peer traps in `on_rehydrate`.
const REHYDRATE_TRAP: &str = "on_rehydrate failed";

/// The ledger's rows a [`Provider`] sends: the admit it relays, and the hold.
#[aether_actor::protocol]
trait LedgerDoor {
    fn admit(mail: WatchAdmit) -> WatchAdmitResult;
    fn hold(mail: WatchHold);
}

/// Admit the provider to the ledger. The provider relays a [`WatchAdmit`]
/// and answers with the ledger's answer.
#[aether_data::kind(name = "test.guest_watch.admit", copy, no_serde)]
struct Admit {
    tag: u32,
    through: WatchThrough,
}

/// Have the ledger cast the provider and hold the reference unwatched.
#[aether_data::kind(name = "test.guest_watch.hold", copy, no_serde)]
struct Hold;

/// Tells the provider to shut itself down.
#[aether_data::kind(name = "test.guest_watch.shut_down", copy, no_serde)]
struct ShutDown;

/// The requester a relayed [`WatchAdmit`]'s answer goes back to.
#[aether_data::kind(name = "test.guest_watch.relayed")]
struct Relayed {
    held: Held<WatchAdmitResult>,
}

/// A native provider: it covers `WatchProvider` and `WatchAuditor`, admits
/// itself to a ledger when told, and closes when told.
struct Provider;

#[actor(singleton, root)]
impl NativeActor for Provider {
    const NAMESPACE: &'static str = "test.guest_watch.provider";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[handler::request]
    fn on_admit(&mut self, ctx: &mut NativeCtx<'_>, admit: Admit) -> Pending<WatchAdmitResult> {
        let (pending, held) = ctx.hold::<WatchAdmitResult>();
        let ledger = ctx.resolve(&ledger_door()).expect("the ledger is live");
        let relayed = WatchAdmit { tag: admit.tag, through: admit.through };
        let _ = ctx.send_to_with_context(ledger, &relayed, Relayed { held });

        pending
    }

    #[handler::response]
    fn on_admitted(&mut self, ctx: &mut NativeCtx<'_>, result: WatchAdmitResult, relayed: Relayed) {
        relayed.held.answer(ctx, &result);
    }

    #[handler::tell]
    fn on_hold(&mut self, ctx: &mut NativeCtx<'_>, _hold: Hold) {
        let ledger = ctx.resolve(&ledger_door()).expect("the ledger is live");
        ctx.send_to(ledger, &WatchHold);
    }

    #[handler::tell]
    fn on_shut_down(&mut self, ctx: &mut NativeCtx<'_>, _shut_down: ShutDown) {
        ctx.shutdown();
    }

    #[handler::tell]
    fn on_nudge(&mut self, _ctx: &mut NativeCtx<'_>, _nudge: WatchNudge) {}

    #[handler::tell]
    fn on_audit(&mut self, _ctx: &mut NativeCtx<'_>, _audit: WatchAudit) {}
}

/// Monitor the actor at `target`; the reply confirms the watch stands.
#[aether_data::kind(name = "test.guest_watch.watch", no_serde)]
struct Watch {
    target: ErasedActorPath,
}

#[aether_data::kind(name = "test.guest_watch.watching", copy, no_serde)]
struct Watching;

/// Answered once the watched actor's `MonitorNotice` has arrived.
#[aether_data::kind(name = "test.guest_watch.await_departure", copy, no_serde)]
struct AwaitDeparture;

#[aether_data::kind(name = "test.guest_watch.noticed", copy, partial_eq, no_serde)]
struct Noticed {
    notified: bool,
}

impl HeldReply for Noticed {
    fn unanswered() -> Self {
        Self { notified: false }
    }
}

/// Watches one actor at a time, the way a capability watches a registrant,
/// and holds an [`AwaitDeparture`] until that actor's notice arrives. Each
/// notice also mails the component host a `ListComponents`, the one signal
/// of the notice a scenario can wait for without draining a pumped host.
struct Watcher {
    watch: Option<MonitorHandle>,
    departed: bool,
    waiting: Option<Held<Noticed>>,
}

#[actor(singleton, root, depends(ComponentHostCapability))]
impl NativeActor for Watcher {
    const NAMESPACE: &'static str = "test.guest_watch.watcher";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { watch: None, departed: false, waiting: None })
    }

    #[handler::request]
    fn on_watch(&mut self, ctx: &mut NativeCtx<'_>, watch: Watch) -> Watching {
        let Watch { target } = watch;
        let proven = ctx.resolve_path(&target).expect("the watched actor is live");
        self.watch = Some(ctx.monitor(proven));
        self.departed = false;

        Watching
    }

    #[handler::request]
    fn on_await_departure(&mut self, ctx: &mut NativeCtx<'_>, _await: AwaitDeparture) -> Pending<Noticed> {
        let (pending, held) = ctx.hold::<Noticed>();
        if self.departed {
            held.answer(ctx, &Noticed { notified: true });
        } else {
            self.waiting = Some(held);
        }

        pending
    }

    #[handler::event]
    fn on_monitor_notice(&mut self, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        drop(self.watch.take());
        self.departed = true;
        ctx.send::<ComponentHostCapability>(&ListComponents {});
        if let Some(held) = self.waiting.take() {
            held.answer(ctx, &Noticed { notified: true });
        }
    }

    #[handler::response]
    fn on_listed(&mut self, _ctx: &mut NativeCtx<'_>, _listed: ListComponentsResult) {}
}

/// The watch family's three modules.
struct Family {
    v1: Vec<u8>,
    v2: Vec<u8>,
    reshaped: Vec<u8>,
}

fn family() -> Option<Family> {
    let read = |stem: &str| require_wasm(stem).map(|path| fs::read(path).expect("read fixture wasm"));

    Some(Family {
        v1: read("republish_watch_v1")?,
        v2: read("republish_watch_v2")?,
        reshaped: read("republish_watch_reshaped")?,
    })
}

fn pooled() -> SubstrateHarness {
    SubstrateHarness::builder()
        .with_component_host()
        .with_actor::<Provider>(())
        .with_actor::<Watcher>(())
        .size(64, 48)
        .build()
        .expect("boot")
}

/// The component host dispatches only while the harness drains it, so a
/// scenario can hold a republish prepared.
fn pumped() -> SubstrateHarness {
    SubstrateHarness::builder()
        .with_pumped_component_host()
        .with_actor::<Provider>(())
        .with_actor::<Watcher>(())
        .size(64, 48)
        .build()
        .expect("boot")
}

fn key(name: &str) -> LoadName {
    LoadName::new(name).expect("a valid instance key")
}

fn provider_path() -> ProtocolPath<WatchProvider> {
    ActorPath::<Provider>::root().narrow()
}

/// Where the ledger, a root singleton, stands once it is loaded.
fn ledger_door() -> ProtocolPath<LedgerDoor> {
    ActorPath::<WatchLedger>::root().narrow()
}

fn call<K: Kind + Clone + 'static, I, R: Kind>(
    harness: &mut SubstrateHarness,
    to: impl SendTarget<K, I>,
    mail: &K,
) -> R {
    harness
        .execute(vec![("call", HarnessOp::send_and_await_reply(to, mail))])
        .expect("the call is answered")
        .reply::<R>("call")
        .expect("decode the reply")
}

fn tell<K: Kind + Clone + 'static, I>(harness: &mut SubstrateHarness, to: impl SendTarget<K, I>, mail: &K) {
    harness.execute(vec![("tell", HarnessOp::send_and_settle(to, mail))]).expect("the tell settles");
}

/// Load the ledger from `wasm` with `config`.
fn load_ledger(
    harness: &mut SubstrateHarness,
    wasm: &[u8],
    config: &WatchLedgerConfig,
) -> Result<ActorRef<WatchLedger>, SubstrateHarnessError> {
    let load = LoadComponent { wasm: wasm.to_vec(), name: None, config: config.encode_into_bytes(), export: None };

    harness.load::<WatchLedger>(load)
}

/// A ledger whose `wire` watches nothing.
fn plain_ledger(harness: &mut SubstrateHarness, wasm: &[u8]) -> ActorRef<WatchLedger> {
    load_ledger(harness, wasm, &WatchLedgerConfig::default())
        .unwrap_or_else(|error| panic!("the ledger loads: {error}"))
}

/// A config whose `wire` watches the provider with `tag`, then does
/// `outcome`.
fn wire_watch(tag: u32, outcome: WireOutcome) -> WatchLedgerConfig {
    WatchLedgerConfig { target: Some(provider_path()), tag, outcome }
}

/// Load the watch peer from `wasm` with `config`. The reference is typed by
/// v1's `WatchPeer`, whose rows v2's peer keeps.
fn load_peer(harness: &mut SubstrateHarness, wasm: &[u8], config: WatchPeerConfig) -> ActorRef<WatchPeer> {
    let load = LoadComponent { wasm: wasm.to_vec(), name: None, config: config.encode_into_bytes(), export: None };

    harness.load::<WatchPeer>(load).unwrap_or_else(|error| panic!("the peer loads: {error}"))
}

fn load_desk(harness: &mut SubstrateHarness, wasm: &[u8], name: &str) -> ActorRef<WatchDesk> {
    let load = LoadComponent { wasm: wasm.to_vec(), name: Some(name.to_owned()), config: Vec::new(), export: None };

    harness.load::<WatchDesk>(load).unwrap_or_else(|error| panic!("desk {name} loads: {error}"))
}

/// The provider admits itself to the ledger, and the ledger's answer comes
/// back: the ordinal of the id its `watch` returned.
fn admit(harness: &mut SubstrateHarness, tag: u32, through: WatchThrough) -> u32 {
    let provider = harness.actor_ref::<Provider>();
    let answer: WatchAdmitResult = call(harness, &provider, &Admit { tag, through });

    watched(answer)
}

fn watched(answer: WatchAdmitResult) -> u32 {
    match answer {
        WatchAdmitResult::Ok { watch } => watch,
        WatchAdmitResult::Err { error } => panic!("the ledger watched nothing: {error}"),
    }
}

/// Register the native watcher on the actor at `target`. Called after the
/// guest's watch, so the watcher's notice is posted after the guest's.
fn watch_natively(harness: &mut SubstrateHarness, target: &ErasedActorPath) {
    let watcher = harness.actor_ref::<Watcher>();
    let _: Watching = call(harness, &watcher, &Watch { target: target.clone() });
}

/// Wait for the native watcher's notice.
fn await_notice(harness: &mut SubstrateHarness) {
    let watcher = harness.actor_ref::<Watcher>();
    let noticed: Noticed = call(harness, &watcher, &AwaitDeparture);

    assert_eq!(noticed, Noticed { notified: true }, "the native watcher was noticed");
}

/// Register the native watcher on the provider, close the provider, and wait
/// for the watcher's notice.
fn close_provider(harness: &mut SubstrateHarness) {
    watch_natively(harness, provider_path().as_erased());
    let provider = harness.actor_ref::<Provider>();
    tell(harness, &provider, &ShutDown);
    await_notice(harness);
}

/// Register the native watcher on the guest at `target`, drop the guest, and
/// wait for the watcher's notice.
fn drop_guest(harness: &mut SubstrateHarness, target: &ErasedActorPath) {
    watch_natively(harness, target);
    let host = harness.actor_ref::<ComponentHostCapability>();
    let dropped: DropResult = call(harness, &host, &DropComponent { target: target.clone() });
    if let DropResult::Err { error } = dropped {
        panic!("{target} drops: {error}");
    }
    await_notice(harness);
}

/// What the actor's departure handlers have run for, and what its `wire`
/// watched.
fn report<I>(harness: &mut SubstrateHarness, actor: impl SendTarget<WatchLedgerQuery, I>) -> WatchLedgerReport {
    call(harness, actor, &WatchLedgerQuery)
}

/// The departure the ledger's provider handler reports for a watch made
/// with `tag`, whose id has the ordinal `watch`.
fn provider_departure(tag: u32, watch: u32) -> WatchDeparture {
    WatchDeparture { through: WatchThrough::Provider, tag: Some(tag), watch, actor_is_sender: true }
}

fn publish(wasm: &[u8]) -> Publish {
    Publish { code: wasm.to_vec().into(), configs: Vec::new() }
}

/// Republish `wasm` and answer the host's refusal.
fn refused_republish(harness: &mut SubstrateHarness, wasm: &[u8]) -> String {
    match harness.publish(wasm.to_vec()) {
        Err(SubstrateHarnessError::Publish(error)) => error,
        other => panic!("the republish must be refused; got {other:?}"),
    }
}

fn refusal(result: PublishResult) -> String {
    match result {
        PublishResult::Err { error } => error,
        PublishResult::Ok { .. } => panic!("the republish was accepted"),
    }
}

/// Catches the reported leak, a provider that leaves without a word holding
/// its rows for good, and a notice delivered with no context.
#[test]
fn a_provider_that_closes_without_a_word_is_reported_once() {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pooled();
    let ledger = plain_ledger(&mut harness, &family.v1);
    let watch = admit(&mut harness, 7, WatchThrough::Provider);

    close_provider(&mut harness);

    assert_eq!(report(&mut harness, &ledger).handled, [provider_departure(7, watch)]);
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 1, "the handler ran once");
}

/// Catches a close that skips the monitor fan-out when the guest's `unwire`
/// faults.
#[test]
fn a_guest_provider_whose_unwire_traps_is_reported() {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pooled();
    let ledger = plain_ledger(&mut harness, &family.v1);
    let config = WatchPeerConfig { trap_on_rehydrate: false, trap_on_unwire: true };
    let peer = load_peer(&mut harness, &family.v1, config);
    tell(&mut harness, &peer, &WatchPeerAdmit { tag: 8 });

    let path = harness.actor_path(&peer);
    drop_guest(&mut harness, &path);

    let handled = report(&mut harness, &ledger).handled;
    let [departure] = handled.as_slice() else {
        panic!("the handler ran once: {handled:?}");
    };
    assert_eq!((departure.tag, departure.actor_is_sender), (Some(8), true));
}

/// Catches a host function that registers a watch on a closed target without
/// posting the notice a registration on a closed target owes.
#[test]
fn a_reference_watched_after_its_actor_closed_is_reported_once() {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pooled();
    let ledger = plain_ledger(&mut harness, &family.v1);
    let provider = harness.actor_ref::<Provider>();
    tell(&mut harness, &provider, &Hold);
    close_provider(&mut harness);
    assert_eq!(report(&mut harness, &ledger).handled, [], "nothing was watched when the provider closed");

    // The answer is sent when the handler that watched returns, and the
    // query is sent after it, so it is handled behind the notice that
    // `watch` posted.
    let answer: WatchAdmitResult = call(&mut harness, &ledger, &WatchHeld { tag: 4 });
    let watch = watched(answer);

    assert_eq!(report(&mut harness, &ledger).handled, [provider_departure(4, watch)]);
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 1, "the handler ran once");
}

/// Catches a repeated watch that makes a second watch, and one that keeps
/// the first context.
#[test]
fn two_admits_from_one_provider_are_one_watch_holding_the_later_context() {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pooled();
    let ledger = plain_ledger(&mut harness, &family.v1);

    let first = admit(&mut harness, 1, WatchThrough::Provider);
    let second = admit(&mut harness, 2, WatchThrough::Provider);
    assert_eq!(first, second, "a second watch of the same provider returns the standing id");

    close_provider(&mut harness);

    assert_eq!(report(&mut harness, &ledger).handled, [provider_departure(2, first)]);
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 1, "the handler ran once");
}

/// Catches a departure arm that stops after the first ended watch or hands
/// both to one handler, and a second registration for one actor, which would
/// post a second notice.
#[test]
fn one_provider_watched_through_two_protocols_runs_each_handler_once() {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pooled();
    let ledger = plain_ledger(&mut harness, &family.v1);

    let as_provider = admit(&mut harness, 5, WatchThrough::Provider);
    let as_auditor = admit(&mut harness, 0, WatchThrough::Auditor);
    assert_ne!(as_provider, as_auditor, "one actor watched through two types is two watches");

    close_provider(&mut harness);

    let auditor_departure =
        WatchDeparture { through: WatchThrough::Auditor, tag: None, watch: as_auditor, actor_is_sender: true };
    assert_eq!(report(&mut harness, &ledger).handled, [provider_departure(5, as_provider), auditor_departure]);
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 2, "each handler ran once");
}

/// Catches an `unwatch` that leaves the registration or the context behind.
#[test]
fn an_unwatched_provider_closes_unreported() {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pooled();
    let ledger = plain_ledger(&mut harness, &family.v1);
    let _ = admit(&mut harness, 3, WatchThrough::Provider);
    tell(&mut harness, &ledger, &WatchRelease { tag: 3 });

    close_provider(&mut harness);

    assert_eq!(report(&mut harness, &ledger).handled, [], "the ledger handled no notice");
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 0);
}

/// Catches an id reused for a new watch, and a released row found as
/// standing.
#[test]
fn a_watch_made_after_an_unwatch_takes_a_new_id() {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pooled();
    let ledger = plain_ledger(&mut harness, &family.v1);
    let first = admit(&mut harness, 1, WatchThrough::Provider);
    tell(&mut harness, &ledger, &WatchRelease { tag: 1 });

    let second = admit(&mut harness, 2, WatchThrough::Provider);
    assert_ne!(first, second, "a watch made after an unwatch is a new watch");

    close_provider(&mut harness);

    assert_eq!(report(&mut harness, &ledger).handled, [provider_departure(2, second)]);
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 1, "the handler ran once");
}

/// Catches rows left on the retired guest, contexts not restored when the
/// actor overrides neither republish hook, a successor's arm that cannot
/// pick the handler from the carried tag, and an event reference minted at
/// another position than the departed actor's.
#[test]
fn a_republished_ledger_reports_the_watch_its_predecessor_made() {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pooled();
    let ledger = plain_ledger(&mut harness, &family.v1);
    let _ = admit(&mut harness, 11, WatchThrough::Provider);

    harness.publish(family.v2.clone()).unwrap_or_else(|error| panic!("v2 republishes v1: {error}"));
    close_provider(&mut harness);

    assert_eq!(report(&mut harness, &ledger).handled, [provider_departure(11, FIRST_HANDED)]);
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 1, "the successor's handler ran once");
}

/// Catches a notice lost at the gate of a prepared slot, delivered to the
/// retired guest, or delivered to a successor that holds no row or no
/// context.
#[test]
fn a_provider_that_closes_while_the_ledger_is_prepared_is_reported_by_the_successor() {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pumped();
    let ledger = plain_ledger(&mut harness, &family.v1);
    let _ = admit(&mut harness, 12, WatchThrough::Provider);
    watch_natively(&mut harness, provider_path().as_erased());

    let host = harness.actor_ref::<ComponentHostCapability>();
    let republishing = harness.send_deferred(host, &publish(&family.v2));
    harness.step_component_host_through::<Prepared>(1).expect("the ledger answers its prepare");

    // The host has not run since the ledger answered, so no commit has been
    // sent. The watcher's notice is posted after the ledger's, and the
    // watcher mails the host from it, so once that mail is queued the
    // ledger's notice is at its gate.
    let provider = harness.actor_ref::<Provider>();
    let _ = harness.send_tracked(&provider, &ShutDown).expect("send the shutdown");
    harness.await_component_host_queued::<ListComponents>().expect("the native watcher was noticed");
    let republished = harness.await_deferred::<PublishResult>(republishing).expect("the republish answers");

    assert!(matches!(republished, PublishResult::Ok { .. }), "v2 republishes v1: {republished:?}");
    assert_eq!(report(&mut harness, &ledger).handled, [provider_departure(12, FIRST_HANDED)]);
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 1, "the successor's handler ran once");
}

/// Catches rows dropped with the losing candidate, and contexts not handed
/// back to the kept guest.
#[test]
fn an_aborted_republish_leaves_the_watch_with_the_kept_ledger() {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pooled();
    let ledger = plain_ledger(&mut harness, &family.v1);
    let _peer = load_peer(&mut harness, &family.v1, WatchPeerConfig { trap_on_rehydrate: true, trap_on_unwire: false });
    let watch = admit(&mut harness, 13, WatchThrough::Provider);

    let error = refused_republish(&mut harness, &family.v2);
    assert!(error.contains(REHYDRATE_TRAP), "the peer's refusal is reported: {error}");
    close_provider(&mut harness);

    assert_eq!(report(&mut harness, &ledger).handled, [provider_departure(13, watch)]);
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 1, "the kept ledger's handler ran once");
}

/// Catches gated notices released to the kept guest before its rows and
/// contexts are back.
#[test]
fn a_provider_that_closes_while_prepared_is_reported_after_the_abort() {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pumped();
    let ledger = plain_ledger(&mut harness, &family.v1);
    let _peer = load_peer(&mut harness, &family.v1, WatchPeerConfig { trap_on_rehydrate: true, trap_on_unwire: false });
    let watch = admit(&mut harness, 14, WatchThrough::Provider);
    watch_natively(&mut harness, provider_path().as_erased());

    let host = harness.actor_ref::<ComponentHostCapability>();
    let republishing = harness.send_deferred(host, &publish(&family.v2));
    harness.step_component_host_through::<Prepared>(1).expect("a member answers its prepare");

    // The host aborts only once both members have answered, and it has
    // dispatched one answer. The ledger's `Prepare` was sent before that
    // answer, so the notice posted now is behind it in the ledger's inbox
    // and ahead of the abort.
    let provider = harness.actor_ref::<Provider>();
    let _ = harness.send_tracked(&provider, &ShutDown).expect("send the shutdown");
    harness.await_component_host_queued::<ListComponents>().expect("the native watcher was noticed");
    let republished = harness.await_deferred::<PublishResult>(republishing).expect("the republish answers");

    let error = refusal(republished);
    assert!(error.contains(REHYDRATE_TRAP), "the peer's refusal is reported: {error}");
    assert_eq!(report(&mut harness, &ledger).handled, [provider_departure(14, watch)]);
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 1, "the kept ledger's handler ran once");
}

/// A ledger whose `wire` watches the provider has its republish aborted
/// `aborts` times, each of which runs its `wire` again, before the provider
/// closes.
///
/// Catches a reinstated guest's `wire` adding a watch, and the watch table
/// reaching the kept guest only after its `wire` ran.
fn a_wire_watch_stands_through_aborted_republishes(aborts: usize) {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pooled();
    let ledger = load_ledger(&mut harness, &family.v1, &wire_watch(15, WireOutcome::Succeeds))
        .unwrap_or_else(|error| panic!("the ledger loads: {error}"));
    let _peer = load_peer(&mut harness, &family.v1, WatchPeerConfig { trap_on_rehydrate: true, trap_on_unwire: false });
    let [watch] = report(&mut harness, &ledger).wired[..] else {
        panic!("the ledger's wire watched once");
    };

    for _ in 0..aborts {
        let error = refused_republish(&mut harness, &family.v2);
        assert!(error.contains(REHYDRATE_TRAP), "the peer's refusal is reported: {error}");
    }
    close_provider(&mut harness);

    let reported = report(&mut harness, &ledger);
    assert_eq!(reported.wired, vec![watch; aborts + 1], "each rerun of wire got the standing id");
    assert_eq!(reported.handled, [provider_departure(15, watch)]);
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 1, "the handler ran once");
}

#[test]
fn a_wire_watch_stands_through_one_aborted_republish() {
    a_wire_watch_stands_through_aborted_republishes(1);
}

#[test]
fn a_wire_watch_stands_through_four_aborted_republishes() {
    a_wire_watch_stands_through_aborted_republishes(4);
}

/// Catches a carried watch context reaching a module that cannot decode it.
#[test]
fn a_reshaped_watch_context_refuses_the_republish_and_the_kept_ledger_reports() {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pooled();
    let ledger = plain_ledger(&mut harness, &family.v1);
    let watch = admit(&mut harness, 16, WatchThrough::Provider);

    let error = refused_republish(&mut harness, &family.reshaped);
    assert!(
        error.contains(
            "replacement does not declare its carried context \
             aether.test_fixtures.republish_watch_note"
        ),
        "the refusal names the watch context kind: {error}",
    );
    close_provider(&mut harness);

    assert_eq!(report(&mut harness, &ledger).handled, [provider_departure(16, watch)]);
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 1, "the kept ledger's handler ran once");
}

/// Spawn a clerk keyed `name` beneath `desk` that watches the desk keyed
/// `target` with `tag`, and return it with the ordinal of the id its `wire`
/// got.
///
/// The clerk's `wire` ran while its alias had no route, so its watch waited.
/// The spawn's chain holds the alias publication and the turn of the desk's
/// trampoline that completes it, which is where the watch registers, so the
/// settled spawn has registered it.
fn spawn_clerk(
    harness: &mut SubstrateHarness,
    desk: ActorRef<WatchDesk>,
    name: &str,
    target: &str,
    tag: u32,
) -> (ActorRef<WatchClerk>, u32) {
    tell(harness, &desk, &WatchClerkSpawn { key: name.to_owned(), target: target.to_owned(), tag });
    let clerk = harness
        .child::<WatchDesk, WatchClerk>(&desk, key(name))
        .unwrap_or_else(|error| panic!("clerk {name} is live: {error}"));
    let [watch] = report(harness, &clerk).wired[..] else {
        panic!("clerk {name}'s wire watched once");
    };

    (clerk, watch)
}

/// Catches a child's watch registered under an alias with no route, whose
/// notice is dropped, and one delivered to the parent. Then, across a
/// republish of the desk: a child's context dropped from the composite
/// bundle, and a notice the membrane hands to the parent or to no one.
#[test]
fn a_clerk_that_watched_in_its_own_wire_reports_before_and_after_its_desks_republish() {
    let Some(family) = family() else {
        return;
    };
    let mut harness = pooled();
    let desk = load_desk(&mut harness, &family.v1, "a");
    let first_target = load_desk(&mut harness, &family.v1, "b");
    let second_target = load_desk(&mut harness, &family.v1, "c");

    let (first_clerk, first_watch) = spawn_clerk(&mut harness, desk, "one", "b", 21);
    let path = harness.actor_path(&first_target);
    drop_guest(&mut harness, &path);

    assert_eq!(report(&mut harness, &first_clerk).handled, [provider_departure(21, first_watch)]);
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 1, "the first clerk's handler ran once");

    let (second_clerk, _) = spawn_clerk(&mut harness, desk, "two", "c", 22);
    harness.publish(family.v2.clone()).unwrap_or_else(|error| panic!("v2 republishes v1: {error}"));
    let path = harness.actor_path(&second_target);
    drop_guest(&mut harness, &path);

    assert_eq!(report(&mut harness, &second_clerk).handled, [provider_departure(22, FIRST_HANDED)]);
    assert_eq!(harness.count_observed(WatchDeparture::NAME), 2, "the rebuilt clerk's handler ran once");
}
