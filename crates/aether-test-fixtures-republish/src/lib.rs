//! Actors every version of a republish fixture family shares (issue 7109).
//!
//! Each module version is an example cdylib of this crate and lists these
//! actors in its own `export!`, so a republish of one version by the other
//! keeps every namespace its predecessor exported (ADR-0241 §4). This lib has
//! no `export!` of its own.
//!
//! [`ProbeGate`] is the one exception: only `republish_group_v2` exports it.
//! `v1`'s gate has no `GateProbe` row, and several scenarios assert that
//! absence, so `v1` keeps its own gate in its example. `ProbeGate` lives here
//! only so a test can name the successor's type (issue 7143).
//!
//! The watch family (issue 7496) is [`WatchLedger`], [`WatchPeer`], and
//! [`WatchDesk`] with its inline [`WatchClerk`]: guests that watch other
//! actors with `ctx.watch` (ADR-0079 §8). `republish_watch_v1` and
//! `republish_watch_v2` export them; `v2` swaps the peer for one that traps in
//! `on_rehydrate`. `republish_watch_reshaped` names nothing in this crate, so
//! that [`WatchNote`], the ledger's context kind, is not a kind its module
//! declares.

use std::collections::BTreeMap;
use std::mem;

use aether_actor::{
    ActorInitError, ActorPath, Anyone, Departed, Erased, Held, NoContext, OutboundReply, Pending, PriorState,
    ProtocolRef, ReplyHandle, Subname, Unchecked, WasmActor, WasmCtx, WasmDropCtx, WasmInitCtx, WatchId, WireCtx,
    actor,
};
use aether_data::LoadName;
use aether_test_fixtures_kinds::{
    CarriedRequest, CarriedRequestResult, CountQuery, CountReport, GateConfig, GateProbe, GateQuery, GateQueryResult,
    HeldRequest, HeldRequestResult, HookOutcome, ReleaseCarried, SubstrateHarnessObserver, WIRE_REFUSAL, WatchAdmit,
    WatchAdmitResult, WatchAuditor, WatchClerkSpawn, WatchDeparture, WatchHeld, WatchHold, WatchLedgerConfig,
    WatchLedgerQuery, WatchLedgerReport, WatchNudge, WatchPeerAdmit, WatchPeerConfig, WatchProvider, WatchRelease,
    WatchThrough, WireCountQuery,
};

/// The reply handles `ReplyHolder` has parked, with their tags, carried
/// across a replace of the holder.
#[aether_data::kind(name = "aether.test_fixtures.republish_parked_replies", no_serde)]
struct ParkedReplies {
    handles: Vec<ReplyHandle>,
    tags: Vec<u32>,
}

/// Parks each [`CarriedRequest`]'s reply handle with its tag until
/// [`ReleaseCarried`], then answers them all in arrival order. The carry
/// family's requesters and held relays send to it by type, and it keeps its
/// parked handles across a replace.
pub struct ReplyHolder {
    parked: Vec<(ReplyHandle, u32)>,
}

#[actor(root)]
impl WasmActor for ReplyHolder {
    const NAMESPACE: &'static str = "test.republish.carry.holder";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ReplyHolder { parked: Vec::new() })
    }

    #[handler::unchecked(reason = "test: parks the reply target for a later release")]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>, request: CarriedRequest) {
        if let Some(handle) = ctx.reply_target() {
            self.parked.push((handle, request.tag));
        }
    }

    #[handler::unchecked(reason = "test: answers parked requests from another handler")]
    fn on_release(&mut self, ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>, _release: ReleaseCarried) {
        for (handle, tag) in self.parked.drain(..) {
            ctx.reply_to(handle, &CarriedRequestResult { tag });
        }
    }

    /// Saves copies, so a guest reinstated by an aborted republish still
    /// holds its parked handles (ADR-0241 §7).
    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) -> Result<(), ActorInitError> {
        let (handles, tags) = self.parked.iter().copied().unzip();
        ctx.save_state_kind::<ParkedReplies>(0, &ParkedReplies { handles, tags })
    }

    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) -> Result<(), ActorInitError> {
        if let Some(saved) = prior.decode_kind::<ParkedReplies>() {
            self.parked = saved.handles.into_iter().zip(saved.tags).collect();
        }
        Ok(())
    }
}

/// What `Keeper` moves out of itself in `on_dehydrate`: its first held
/// reply with that reply's tag, and its request count.
#[aether_data::kind(name = "aether.test_fixtures.republish_kept_state")]
struct KeptState {
    held: Option<Held<HeldRequestResult>>,
    tag: u32,
    kept: u32,
}

/// Holds each [`HeldRequest`]'s reply until [`ReleaseCarried`], then answers
/// every one it holds. The first request's reply is `held`; any later one is
/// a `stray`. It answers [`CountQuery`] with the number of requests it took.
///
/// Its `on_dehydrate` moves `held`, its tag and the count out of the actor
/// into saved state, so a reinstated guest that did not get that state back
/// has lost them (issue 7125). It leaves every stray live and unsaved, so a
/// keeper holding a second request refuses its dehydrate as held-unsaved.
pub struct Keeper {
    held: Option<Held<HeldRequestResult>>,
    tag: u32,
    stray: Vec<(Held<HeldRequestResult>, u32)>,
    kept: u32,
}

#[actor(root)]
impl WasmActor for Keeper {
    const NAMESPACE: &'static str = "test.republish.keep.keeper";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Keeper { held: None, tag: 0, stray: Vec::new(), kept: 0 })
    }

    #[handler::request]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_>, request: HeldRequest) -> Pending<HeldRequestResult> {
        let (pending, held) = ctx.hold::<HeldRequestResult>();
        if self.held.is_none() {
            self.held = Some(held);
            self.tag = request.tag;
        } else {
            self.stray.push((held, request.tag));
        }
        self.kept += 1;
        pending
    }

    #[handler::request]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.kept }
    }

    #[handler::tell]
    fn on_release(&mut self, ctx: &mut WasmCtx<'_>, _release: ReleaseCarried) {
        if let Some(held) = self.held.take() {
            held.answer(ctx, &HeldRequestResult { tag: self.tag });
        }
        for (held, tag) in self.stray.drain(..) {
            held.answer(ctx, &HeldRequestResult { tag });
        }
    }

    /// Moves the saved fields out rather than copying them, and leaves every
    /// stray behind.
    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) -> Result<(), ActorInitError> {
        let state =
            KeptState { held: self.held.take(), tag: mem::take(&mut self.tag), kept: mem::take(&mut self.kept) };
        ctx.save_state_kind(0, &state)
    }

    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) -> Result<(), ActorInitError> {
        if let Some(KeptState { held, tag, kept }) = prior.decode_kind::<KeptState>() {
            self.held = held;
            self.tag = tag;
            self.kept = kept;
        }
        Ok(())
    }
}

/// The `test.republish.gate` row `v2` adds over `v1` (issue 7109): a
/// `GateProbe` handler that records each probe's `seq` in arrival order,
/// answered by `GateQuery`. It counts each run of `wire` and answers
/// `WireCountQuery` with the count (issue 7086). It sends nothing from its
/// lifecycle hooks, so every probe it records is one a test sent — a test
/// that casts or loads it by this type is typed by the successor whose rows
/// its probes are written for.
pub struct ProbeGate {
    seqs: Vec<u32>,
    /// How many times `wire` has run on this instance.
    wired: u32,
}

#[actor(instanced, root)]
impl WasmActor for ProbeGate {
    type Config = GateConfig;
    const NAMESPACE: &'static str = "test.republish.gate";

    fn init(_config: GateConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ProbeGate { seqs: Vec::new(), wired: 0 })
    }

    #[handler::tell]
    fn on_probe(&mut self, _ctx: &mut WasmCtx<'_>, probe: GateProbe) {
        self.seqs.push(probe.seq);
    }

    /// Count each run of the hook, without sending anything.
    fn wire(&mut self, _ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        self.wired += 1;
        Ok(())
    }

    #[handler::request]
    fn on_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: GateQuery) -> GateQueryResult {
        GateQueryResult { seqs: self.seqs.clone() }
    }

    /// The number of times this instance has been wired.
    #[handler::request]
    fn on_wired(&mut self, _ctx: &mut WasmCtx<'_>, _query: WireCountQuery) -> CountReport {
        CountReport { count: self.wired }
    }
}

/// The context [`WatchLedger`] and [`WatchClerk`] store with each watch that
/// has something to note: the tag the watch was made with.
#[aether_data::kind(name = "aether.test_fixtures.republish_watch_note", no_serde)]
pub struct WatchNote {
    tag: u32,
}

/// The distinct watch ids one fixture instance has been handed, by `watch`
/// or by a departure event, in the order it first saw them.
///
/// A `WatchId` never leaves the actor that holds it, so a fixture reports an
/// id as its ordinal here. Two ordinals from one instance are equal exactly
/// when the ids are.
#[derive(Default)]
struct HandedWatches(Vec<WatchId>);

impl HandedWatches {
    /// The ordinal of `watch`: its index here, added at the end when this
    /// instance has not been handed it before.
    fn ordinal(&mut self, watch: WatchId) -> u32 {
        let index = self.0.iter().position(|handed| *handed == watch).unwrap_or_else(|| {
            self.0.push(watch);
            self.0.len() - 1
        });

        u32::try_from(index).expect("a fixture is handed few watches")
    }
}

/// Watches the providers that admit themselves (issue 7496, ADR-0079 §8).
///
/// - A [`WatchAdmit`] casts its sender to the protocol the admit names and
///   watches it: through [`WatchProvider`] with a [`WatchNote`] carrying the
///   admit's tag, or through [`WatchAuditor`] with [`NoContext`]. It keeps
///   the id `watch` returned under the admit's tag and answers with the id's
///   ordinal.
/// - A [`WatchHold`] casts its sender and keeps the reference unwatched; a
///   [`WatchHeld`] watches it later.
/// - A [`WatchRelease`] ends the watch kept under its tag.
/// - With a `target` in its config, `wire` resolves the path and watches the
///   provider there, then succeeds, refuses, or traps as the config says.
///
/// Each departure handler mails the harness observer a [`WatchDeparture`]
/// and keeps it for a [`WatchLedgerQuery`].
///
/// It has no saved state and neither republish hook: what a republish
/// carries of its watches is the host's and the SDK's doing alone. So its
/// successor keeps no id under any tag and has been handed none. Nor does a
/// republish run `wire` on the instance it installs (ADR-0114, amendment of
/// 2026-07-08), so a successor's `wired` list stays empty.
pub struct WatchLedger {
    config: WatchLedgerConfig,
    /// The provider the last [`WatchHold`] came from.
    held: Option<ProtocolRef<WatchProvider>>,
    /// The id the watch made for each tag returned.
    by_tag: BTreeMap<u32, WatchId>,
    handed: HandedWatches,
    /// The ordinal of the id each run of `wire` got.
    wired: Vec<u32>,
    handled: Vec<WatchDeparture>,
}

impl WatchLedger {
    /// Keep `watch` under `tag` and answer with its ordinal.
    fn admitted(&mut self, tag: u32, watch: WatchId) -> WatchAdmitResult {
        self.by_tag.insert(tag, watch);

        WatchAdmitResult::Ok { watch: self.handed.ordinal(watch) }
    }
}

#[actor(root, depends(SubstrateHarnessObserver))]
impl WasmActor for WatchLedger {
    type Config = WatchLedgerConfig;
    const NAMESPACE: &'static str = "test.republish.watch.ledger";

    fn init(config: WatchLedgerConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WatchLedger {
            config,
            held: None,
            by_tag: BTreeMap::new(),
            handed: HandedWatches::default(),
            wired: Vec::new(),
            handled: Vec::new(),
        })
    }

    /// Watch the config's target, then succeed, refuse, or trap as
    /// configured. A guest reinstated after an aborted republish runs this
    /// again, and its watch finds the one already standing.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        if let Some(target) = &self.config.target {
            let provider = ctx.resolve(target).map_err(|error| ActorInitError::new(error.to_string()))?;
            let watch = ctx.watch(provider, WatchNote { tag: self.config.tag });
            self.wired.push(self.handed.ordinal(watch));
        }

        match self.config.outcome {
            HookOutcome::Succeeds => Ok(()),
            HookOutcome::Refuses => Err(ActorInitError::new(WIRE_REFUSAL)),
            HookOutcome::Traps => panic!("the fixture was told to trap in wire"),
        }
    }

    #[handler::request]
    fn on_admit(&mut self, ctx: &mut WasmCtx<'_>, admit: WatchAdmit) -> WatchAdmitResult {
        let Some(sender) = ctx.sender() else {
            return not_watched("the admit has no sender");
        };
        let watch = match admit.through {
            WatchThrough::Provider => {
                ctx.cast::<WatchProvider>(sender).map(|provider| ctx.watch(provider, WatchNote { tag: admit.tag }))
            }
            WatchThrough::Auditor => ctx.cast::<WatchAuditor>(sender).map(|auditor| ctx.watch(auditor, NoContext)),
        };

        let Some(watch) = watch else {
            return not_watched("the sender does not cover the protocol the admit names");
        };

        self.admitted(admit.tag, watch)
    }

    #[handler::tell]
    fn on_hold(&mut self, ctx: &mut WasmCtx<'_>, _hold: WatchHold) {
        self.held = ctx.sender().and_then(|sender| ctx.cast::<WatchProvider>(sender));
    }

    #[handler::request]
    fn on_held(&mut self, ctx: &mut WasmCtx<'_>, held: WatchHeld) -> WatchAdmitResult {
        let Some(provider) = self.held else {
            return not_watched("no reference is held");
        };

        let watch = ctx.watch(provider, WatchNote { tag: held.tag });

        self.admitted(held.tag, watch)
    }

    #[handler::tell]
    fn on_release(&mut self, ctx: &mut WasmCtx<'_>, release: WatchRelease) {
        if let Some(watch) = self.by_tag.remove(&release.tag) {
            ctx.unwatch(watch);
        }
    }

    #[handler::request]
    fn on_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: WatchLedgerQuery) -> WatchLedgerReport {
        WatchLedgerReport { wired: self.wired.clone(), handled: self.handled.clone() }
    }

    #[handler::event]
    fn on_provider_gone(&mut self, ctx: &mut WasmCtx<'_>, event: Departed<WatchProvider>, note: WatchNote) {
        let departure = WatchDeparture {
            through: WatchThrough::Provider,
            tag: Some(note.tag),
            watch: self.handed.ordinal(event.watch),
            actor_is_sender: ctx.sender() == Some(event.actor.erase()),
        };

        ctx.send::<SubstrateHarnessObserver>(&departure);
        self.handled.push(departure);
    }

    #[handler::event]
    fn on_auditor_gone(&mut self, ctx: &mut WasmCtx<'_>, event: Departed<WatchAuditor>) {
        let departure = WatchDeparture {
            through: WatchThrough::Auditor,
            tag: None,
            watch: self.handed.ordinal(event.watch),
            actor_is_sender: ctx.sender() == Some(event.actor.erase()),
        };

        ctx.send::<SubstrateHarnessObserver>(&departure);
        self.handled.push(departure);
    }
}

fn not_watched(reason: &str) -> WatchAdmitResult {
    WatchAdmitResult::Err { error: reason.to_owned() }
}

/// A guest provider: it covers [`WatchProvider`], and a [`WatchPeerAdmit`]
/// makes it admit itself to the [`WatchLedger`]. With
/// `WatchPeerConfig::trap_on_unwire` set its `unwire` traps, so its close
/// goes on past a faulting hook.
///
/// It carries the number of admits it has sent across a republish, so its
/// successor is handed saved state and runs `on_rehydrate`.
/// `republish_watch_v2` exports its own peer at this namespace, which traps
/// there when its config says so.
pub struct WatchPeer {
    trap_on_unwire: bool,
    admits: u32,
}

#[actor(root, depends(WatchLedger))]
impl WasmActor for WatchPeer {
    type Config = WatchPeerConfig;
    const NAMESPACE: &'static str = "test.republish.watch.peer";

    type State = CountReport;

    fn init(config: WatchPeerConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WatchPeer { trap_on_unwire: config.trap_on_unwire, admits: 0 })
    }

    fn dehydrate(&self) -> CountReport {
        CountReport { count: self.admits }
    }

    fn rehydrate(&mut self, CountReport { count }: CountReport) {
        self.admits = count;
    }

    fn unwire(&mut self, _ctx: &mut WasmCtx<'_>) {
        assert!(!self.trap_on_unwire, "the fixture was told to trap in unwire");
    }

    #[handler::tell]
    fn on_admit(&mut self, ctx: &mut WasmCtx<'_>, admit: WatchPeerAdmit) {
        ctx.send::<WatchLedger>(&WatchAdmit { tag: admit.tag, through: WatchThrough::Provider });
        self.admits += 1;
    }

    #[handler::response]
    fn on_admitted(&mut self, _ctx: &mut WasmCtx<'_>, _result: WatchAdmitResult) {}

    #[handler::tell]
    fn on_nudge(&mut self, _ctx: &mut WasmCtx<'_>, _nudge: WatchNudge) {}
}

/// The config a [`WatchDesk`] hands each [`WatchClerk`] it spawns: the desk
/// the clerk watches, and the tag its watch's context carries. A config must
/// have a default, and a path has none, so the default names no desk.
#[aether_data::kind(name = "aether.test_fixtures.watch.clerk.config", default, no_serde)]
pub struct WatchClerkConfig {
    target: Option<ActorPath<WatchDesk>>,
    tag: u32,
}

/// Spawns a [`WatchClerk`] as an inline child for each [`WatchClerkSpawn`],
/// and is itself what a clerk watches: a clerk's target is another desk,
/// named by its key.
pub struct WatchDesk;

#[actor(instanced, root, spawns(WatchClerk))]
impl WasmActor for WatchDesk {
    const NAMESPACE: &'static str = "test.republish.watch.desk";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WatchDesk)
    }

    /// Spawn the clerk. A key that is no valid name or a refused spawn is a
    /// misuse of the fixture, and traps.
    #[handler::tell]
    fn on_spawn(&mut self, ctx: &mut WasmCtx<'_>, spawn: WatchClerkSpawn) {
        let WatchClerkSpawn { key, target, tag } = spawn;
        let target = LoadName::new(&target).expect("the fixture is told a valid desk key");
        let config = WatchClerkConfig { target: Some(ActorPath::<WatchDesk>::instance(&target)), tag };

        ctx.spawn_inline::<WatchClerk>(Subname::Named(&key), &config).map(drop).expect("the clerk spawns");
    }
}

/// A [`WatchDesk`]'s inline child. Its own `wire` resolves the desk its
/// config names and watches it with a [`WatchNote`] carrying the config's
/// tag, while the clerk's alias has no route yet. Its departure handler
/// mails the harness observer a [`WatchDeparture`] and keeps it for a
/// [`WatchLedgerQuery`].
pub struct WatchClerk {
    config: WatchClerkConfig,
    handed: HandedWatches,
    /// The ordinal of the id each run of `wire` got.
    wired: Vec<u32>,
    handled: Vec<WatchDeparture>,
}

#[actor(instanced, child_of(WatchDesk), depends(SubstrateHarnessObserver))]
impl WasmActor for WatchClerk {
    type Config = WatchClerkConfig;
    const NAMESPACE: &'static str = "test.republish.watch.clerk";

    fn init(config: WatchClerkConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WatchClerk { config, handed: HandedWatches::default(), wired: Vec::new(), handled: Vec::new() })
    }

    /// Watch the config's desk, failing the spawn when it cannot be resolved.
    /// A republish rebuilds a clerk through `init` and `on_rehydrate` and
    /// never runs its `wire` (ADR-0114, amendment of 2026-07-08), so only a
    /// fresh spawn reaches this hook.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        if let Some(target) = &self.config.target {
            let desk = ctx.resolve(target).map_err(|error| ActorInitError::new(error.to_string()))?;
            let watch = ctx.watch(desk, WatchNote { tag: self.config.tag });
            self.wired.push(self.handed.ordinal(watch));
        }

        Ok(())
    }

    #[handler::request]
    fn on_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: WatchLedgerQuery) -> WatchLedgerReport {
        WatchLedgerReport { wired: self.wired.clone(), handled: self.handled.clone() }
    }

    #[handler::event]
    fn on_desk_gone(&mut self, ctx: &mut WasmCtx<'_>, event: Departed<WatchDesk>, note: WatchNote) {
        let departure = WatchDeparture {
            through: WatchThrough::Provider,
            tag: Some(note.tag),
            watch: self.handed.ordinal(event.watch),
            actor_is_sender: ctx.sender() == Some(event.actor.erase()),
        };

        ctx.send::<SubstrateHarnessObserver>(&departure);
        self.handled.push(departure);
    }
}
