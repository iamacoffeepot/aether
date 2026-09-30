//! The `aether.bloomery.driver` runtime half (ADR-0122 split), compiled only
//! under `feature = "runtime"`: the sans-io driver core and the native shell
//! that performs its commands as mail.
//!
//! The core holds both roles of the driver: startup recovery, `Call`
//! handling, the per-digest program pipeline (section check, closure read,
//! invoke) over one load per digest serving both roles, and `Transition` /
//! `Fault` recording, plus the reactor half (journal following, membership,
//! activation, live delivery, and reaction records). It owns every ADR-0226
//! program decision (decisions 3,
//! 4, and 9 for programs, and 11 for `Call`) and every reactor routing
//! decision (decisions 5-9 for reactors, and 10-11 for `WatchHead` and
//! `AwaitProcessed`) as a state machine over the journal folds.
//!
//! The core is sans-io: calls and typed replies go in, [`Command`]s come
//! out, and the core itself performs no mail, threads, or clock reads. The
//! native actor that sends and receives its mail stores each command's ticket
//! as the request context, takes it back from the reply, and routes the reply
//! to the matching [`ProgramCore`] method. A reply whose ticket the core is
//! not waiting on returns no commands.
//!
//! Journal bytes are the only source of fold state: after a `Committed`
//! append the core reads its own records back before its next decision,
//! and at most one fenced [`AppendRecords`](aether_bloomery_kinds::AppendRecords)
//! is ever in flight.
//!
//! Native code spawns [`BundleDriver`] over a born journal owner, passing the
//! unit's key, the journal's reference, and the unit's workspace reference in
//! [`DriverParams`]. `init` builds the [`ProgramCore`] and keeps its first
//! commands; `wire` performs them once the mailbox is live. Commands go to the
//! journal owner (reads, appends, and the watch), the component host (a load
//! publishes the bundle's code, then spawns its root at the bound
//! `aether.bloomery.bundle.<hash>` namespace under the unit key), bundle
//! roots, and the providers of program APIs. The core names a loaded bundle
//! by its digest; the shell casts its spawn reply's stamped sender (ADR-0230
//! §3) once per declared role, to [`ProgramRoot`](aether_bloomery_kinds::ProgramRoot)
//! and [`ReactorRoot`](aether_bloomery_kinds::ReactorRoot), keeps the typed
//! references keyed by that digest, and sends through them with the command's
//! ticket as the request context. A root that does not publish a role its
//! bundle declares fails the load (ADR-0240 D4). A spawn that
//! finds the root already live adopts it (ADR-0226 D9), and the core resumes
//! a reactor root from the cursor it reports. Inbound [`Call`],
//! [`AwaitProcessed`], a bundle root's fetch-on-miss [`ReadArtifact`], and a
//! relayed [`ApiCall`] each hold a typed reply (ADR-0243), are fed to the
//! core, and keep the held ticket keyed by the returned [`CallerId`]; each
//! reply kind recovers its ticket from the request context and feeds the
//! matching core continuation. An actor close while the engine keeps running
//! answers every held ticket with its reply kind's `unanswered()` before the
//! state drops, and an engine teardown settles them silently (ADR-0243 §1).
//!
//! Timers run on the shell's clock (ADR-0245). The core asks for one tick
//! at a time with [`Command::ArmTick`], and only while a `clock.until`
//! request is armed. The shell offloads the wait to a worker that sleeps one
//! tick period and holds no settlement chain, since an armed timer waits on
//! time rather than on any caller's work; its completion reads the injected
//! clock and feeds [`ProgramCore::tick`]. The clock is the journal's own, so
//! the two read one time.
//!
//! A program API call relays the same way as a fetch (ADR-0240 D6): the
//! invocation sends [`ApiCall`] to its bundle root, the root relays it here,
//! and the core maps `Http` to the http capability and `Workspace` to the
//! held workspace, refusing any other API; the provider's reply comes back
//! under an [`ApiTicket`] and answers the held call. The shell sends a
//! relayed workspace run over the unit's own journal as its `source`, written
//! once at `init` from the unit key (ADR-0240 D7).

mod bundles;
mod clock;
mod core;
mod perform;
mod programs;
mod reactors;
mod recovery;
mod root;

pub use self::core::{
    ApiReply, ApiTicket, AppendTicket, ArtifactTicket, ArtifactsTicket, CallerId, ClosureTicket, Command, EVENTS_PAGE,
    EvaluateTicket, EventsTicket, InvokeTicket, LoadOutcome, LoadTicket, ProgramCore, RootRoles, StatusTicket,
    WarmTicket, WatchTicket,
};

use std::collections::{BTreeMap, HashMap};
use std::mem;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aether_actor::{ActorPath, ActorRef, ProtocolPath, ReplyMode, runtime};
use aether_bloomery_journal::{Clock, JournalActor, MAX_READ_EVENTS};
use aether_bloomery_kinds::{
    ApiCall, ApiCallResult, AppendRecordsResult, ArtifactStorage, AwaitProcessed, BUNDLE_NAMESPACE, Call, CallOutcome,
    ClosureLimit, Digest, Evaluated, Invoked, Processed, ReadArtifact, ReadArtifactResult, ReadArtifactsResult,
    ReadClosureResult, ReadEventsResult, Status, UnitKey, Warmed, WatchHeadResult,
};
use aether_bloomery_workspace::WorkspaceCapability;
use aether_http::FetchResult;
use aether_kinds::{PublishResult, SpawnResult};
use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending, TaskDone};
use aether_substrate::chassis::error::BootError;

use self::root::BundleRoot;
use crate::BundleDriver;

// Tripwire: the core reads the journal `EVENTS_PAGE` entries per page, and the
// journal owner refuses any page over `MAX_READ_EVENTS`, so the two limits
// must stay equal or every startup read fails with `Err`.
const _: () = assert!(EVENTS_PAGE == MAX_READ_EVENTS);

/// Composer-supplied construction input: the unit's key, the born journal
/// owner's reference, the reference of the workspace the unit's programs
/// run through, and the clock and tick period its timers fire on.
///
/// The journal reference is what the journal's own `spawn_actor(..).finish()`
/// returns, so holding it proves the journal was born (ADR-0230); a driver
/// cannot be built over a journal that does not exist. The driver sends
/// through both references, never by resolving a name.
pub struct DriverParams {
    /// The key of the unit this driver folds for. Every bundle root it
    /// spawns or adopts is keyed by it, at
    /// `aether.bloomery.bundle.<hash>:<unit key>`: the bundle's
    /// content-addressed published name and this key (ADR-0240 D4).
    pub unit: UnitKey,
    /// The journal owner's proven reference, handed over at spawn.
    pub journal: ActorRef<JournalActor>,
    /// The workspace programs' `Workspace` calls reach (ADR-0240 D6).
    pub workspace: ActorRef<WorkspaceCapability>,
    /// The clock timers fire against: the one the journal stamps entries
    /// with, so a due time and the stamp of its firing read one time
    /// (ADR-0245).
    pub clock: Arc<dyn Clock + Send + Sync>,
    /// How long one tick waits before it reads the clock. A timer fires
    /// within about one tick after its due time while the engine is healthy.
    pub tick: Duration,
}

/// The output of one tick's wait: the period elapsed and the clock is due a
/// read.
pub struct TickElapsed;

/// [`BundleDriver`] runtime state: the sans-io program core, the unit and
/// journal it folds for, the workspace its programs run through and the
/// source their runs read and stage through, the core's startup commands
/// until `wire` performs them, the held replies it owes, and each loaded or
/// adopted bundle's root.
pub struct BundleDriverState {
    core: ProgramCore,
    unit: UnitKey,
    journal: ActorRef<JournalActor>,
    workspace: ActorRef<WorkspaceCapability>,
    /// The clock each tick reads, and the period a tick waits.
    clock: Arc<dyn Clock + Send + Sync>,
    tick: Duration,
    /// The unit's journal as the storage every relayed run names, written
    /// from the unit key: `aether.bloomery.journal:<unit key>`.
    source: ProtocolPath<ArtifactStorage>,
    startup: Vec<Command>,
    callers: HashMap<CallerId, Caller>,
    /// The digest each in-flight load was issued for and the roles its root
    /// is cast to, keyed by its ticket, from its publish until its spawn
    /// answers.
    loading: BTreeMap<LoadTicket, (Digest, RootRoles)>,
    /// Each loaded or adopted bundle's root, cast from the stamped sender of
    /// its spawn reply once per declared role.
    roots: HashMap<Digest, BundleRoot>,
}

/// One held reply the driver owes, typed by the inbound kind that armed it.
/// Every [`CallerId`] comes from one core counter, so it is unique across
/// the variants.
enum Caller {
    /// A [`Call`]'s exactly-once outcome.
    Call(Held<CallOutcome>),
    /// An [`AwaitProcessed`] barrier's processed head.
    Processed(Held<Processed>),
    /// A bundle root's fetch-on-miss [`ReadArtifact`].
    Fetched(Held<ReadArtifactResult>),
    /// A bundle root's relayed [`ApiCall`].
    Api(Held<ApiCallResult>),
}

#[runtime]
impl NativeActor for BundleDriver {
    type State = BundleDriverState;

    type Config = ClosureLimit;
    type Params = DriverParams;
    const NAMESPACE: &'static str = "aether.bloomery.driver";

    fn init(
        limit: ClosureLimit,
        params: DriverParams,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<BundleDriverState, BootError> {
        let DriverParams { unit, journal, workspace, clock, tick } = params;
        let (core, startup) = ProgramCore::start(limit);
        let source = ActorPath::<JournalActor>::instance(unit.as_load_name()).narrow::<ArtifactStorage>();
        Ok(BundleDriverState {
            core,
            source,
            unit,
            journal,
            workspace,
            clock,
            tick,
            startup,
            callers: HashMap::new(),
            loading: BTreeMap::new(),
            roots: HashMap::new(),
        })
    }

    fn wire(state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        let startup = mem::take(&mut state.startup);
        state.perform(ctx, startup);
    }

    /// The held ticket is stored before the commands run, because the core
    /// may answer within this same dispatch (an already recorded outcome).
    #[handler::request]
    fn on_call(state: &mut Self::State, ctx: &mut NativeCtx<'_>, call: Call) -> Pending<CallOutcome> {
        let (pending, held) = ctx.hold::<CallOutcome>();
        let (caller, commands) = state.core.call(call);
        state.callers.insert(caller, Caller::Call(held));
        state.perform(ctx, commands);
        pending
    }

    #[handler::request]
    fn on_await_processed(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        request: AwaitProcessed,
    ) -> Pending<Processed> {
        let (pending, held) = ctx.hold::<Processed>();
        let (caller, commands) = state.core.await_processed(request);
        state.callers.insert(caller, Caller::Processed(held));
        state.perform(ctx, commands);
        pending
    }

    #[handler::response]
    fn on_read_events(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        result: ReadEventsResult,
        ticket: EventsTicket,
    ) {
        let commands = state.core.on_events(ticket, result);
        state.perform(ctx, commands);
    }

    #[handler::response]
    fn on_read_artifact(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        result: ReadArtifactResult,
        ticket: ArtifactTicket,
    ) {
        let commands = state.core.on_artifact(ticket, result);
        state.perform(ctx, commands);
    }

    #[handler::response]
    fn on_read_artifacts(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        result: ReadArtifactsResult,
        ticket: ArtifactsTicket,
    ) {
        let commands = state.core.on_artifacts(ticket, result);
        state.perform(ctx, commands);
    }

    /// Serves a bundle root's fetch-on-miss: the core answers it from its
    /// artifact cache or one shared journal read per digest, and the held
    /// reply carries the answer back to the root with the root's correlation.
    #[handler::request]
    fn on_fetch_artifact(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        request: ReadArtifact,
    ) -> Pending<ReadArtifactResult> {
        let (pending, held) = ctx.hold::<ReadArtifactResult>();
        let (caller, commands) = state.core.fetch_artifact(request);
        state.callers.insert(caller, Caller::Fetched(held));
        state.perform(ctx, commands);
        pending
    }

    /// Serves a bundle root's relayed program API call: the core sends it to
    /// the API's provider or refuses it, and the held reply carries the
    /// answer back to the root with the root's correlation.
    #[handler::request]
    fn on_api_call(state: &mut Self::State, ctx: &mut NativeCtx<'_>, request: ApiCall) -> Pending<ApiCallResult> {
        let (pending, held) = ctx.hold::<ApiCallResult>();
        let (caller, commands) = state.core.call_api(request);
        state.callers.insert(caller, Caller::Api(held));
        state.perform(ctx, commands);
        pending
    }

    #[handler::response]
    fn on_fetch_result(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: FetchResult, ticket: ApiTicket) {
        let commands = state.core.on_api_reply(ticket, ApiReply::Fetch(result));
        state.perform(ctx, commands);
    }

    #[handler::response]
    fn on_workspace_run_result(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        result: aether_bloomery_workspace::RunResult,
        ticket: ApiTicket,
    ) {
        let commands = state.core.on_api_reply(ticket, ApiReply::Workspace(result));
        state.perform(ctx, commands);
    }

    #[handler::response]
    fn on_read_closure(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        result: ReadClosureResult,
        ticket: ClosureTicket,
    ) {
        let commands = state.core.on_closure(ticket, result);
        state.perform(ctx, commands);
    }

    #[handler::response]
    fn on_append_records(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        result: AppendRecordsResult,
        ticket: AppendTicket,
    ) {
        let commands = state.core.on_appended(ticket, result);
        state.perform(ctx, commands);
    }

    /// The bundle's code is published: spawn its root at the bound bundle
    /// namespace under the unit key, with the same ticket, or finish the
    /// load failed when the publish was refused or bound no bundle root.
    #[handler::response]
    fn on_publish_result(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: PublishResult, ticket: LoadTicket) {
        let namespace = match result {
            PublishResult::Ok { types } => types
                .into_iter()
                .map(|published| published.namespace)
                .find(|namespace| bundle_root(namespace))
                .ok_or_else(|| format!("the published bundle binds no {BUNDLE_NAMESPACE} root")),
            PublishResult::Err { error } => Err(error),
        };
        match namespace {
            Ok(namespace) => state.spawn_root(ctx, namespace, ticket),
            Err(error) => {
                state.loading.remove(&ticket);
                let commands = state.core.on_loaded(ticket, LoadOutcome::Failed { error });
                state.perform(ctx, commands);
            }
        }
    }

    /// The bundle's root answered its spawn. ADR-0230 §3: the root sends its
    /// own spawn reply, so the stamped sender is the root, and the driver
    /// casts it once per declared role and keeps the typed references for
    /// the digest (ADR-0231 §4). A root the spawn stood up is `Loaded`; one
    /// the engine already held live under the unit key is `Adopted`
    /// (ADR-0226 D9). A root that does not publish a declared role fails the
    /// load, so the core never addresses it in that role.
    #[handler::response]
    fn on_spawn_result(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: SpawnResult, ticket: LoadTicket) {
        let load = state.loading.remove(&ticket);
        let outcome = match (result, ctx.sender(), load) {
            (SpawnResult::Err { error }, ..) => LoadOutcome::Failed { error },
            (SpawnResult::Spawned { path, .. } | SpawnResult::Live { path, .. }, None, _) => {
                LoadOutcome::Failed { error: format!("spawn reply for {path} carried no sender") }
            }
            (SpawnResult::Spawned { path, .. } | SpawnResult::Live { path, .. }, Some(_), None) => {
                LoadOutcome::Failed { error: format!("spawn reply for {path} matched no issued load") }
            }
            (SpawnResult::Spawned { path, .. }, Some(sender), Some((bundle, roles))) => {
                state.keep_root(BundleRoot::cast(ctx, sender, roles, &path), bundle, LoadOutcome::Loaded)
            }
            (SpawnResult::Live { path, .. }, Some(sender), Some((bundle, roles))) => {
                state.keep_root(BundleRoot::cast(ctx, sender, roles, &path), bundle, LoadOutcome::Adopted)
            }
        };
        let commands = state.core.on_loaded(ticket, outcome);
        state.perform(ctx, commands);
    }

    #[handler::response]
    fn on_invoked(state: &mut Self::State, ctx: &mut NativeCtx<'_>, invoked: Invoked, ticket: InvokeTicket) {
        let commands = state.core.on_invoked(ticket, invoked);
        state.perform(ctx, commands);
    }

    #[handler::response]
    fn on_watch_head_result(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        result: WatchHeadResult,
        ticket: WatchTicket,
    ) {
        let commands = state.core.on_watched(ticket, result);
        state.perform(ctx, commands);
    }

    #[handler::response]
    fn on_warmed(state: &mut Self::State, ctx: &mut NativeCtx<'_>, warmed: Warmed, ticket: WarmTicket) {
        let commands = state.core.on_warmed(ticket, warmed);
        state.perform(ctx, commands);
    }

    #[handler::response]
    fn on_evaluated(state: &mut Self::State, ctx: &mut NativeCtx<'_>, evaluated: Evaluated, ticket: EvaluateTicket) {
        let commands = state.core.on_evaluated(ticket, evaluated);
        state.perform(ctx, commands);
    }

    #[handler::response]
    fn on_status(state: &mut Self::State, ctx: &mut NativeCtx<'_>, status: Status, ticket: StatusTicket) {
        let commands = state.core.on_status(ticket, &status);
        state.perform(ctx, commands);
    }

    /// One tick's wait elapsed: read the clock, fire every due timer, and
    /// perform what follows, a next tick included while a timer stays armed
    /// (ADR-0245). The wait owes no one a reply.
    #[handler(task)]
    fn on_tick_elapsed(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _done: &TaskDone<TickElapsed>) {
        let commands = state.core.tick(state.clock.now_millis());
        state.perform(ctx, commands);
    }
}

impl BundleDriverState {
    /// Wait one tick period on a worker, then complete into
    /// `on_tick_elapsed`.
    ///
    /// The wait holds no settlement chain: an armed timer waits on time, not
    /// on the work of whichever chain armed it, so a chain that sets a
    /// timer settles without waiting it out.
    pub(crate) fn arm_tick<M: ReplyMode, A>(&self, ctx: &mut NativeCtx<'_, A, M>) {
        let period = self.tick;
        let reply_to = ctx.reply_target();
        let _ = ctx.dispatch_blocking_resumed_with(None, reply_to, (), move || {
            thread::sleep(period);
            TickElapsed
        });
    }

    /// Keep `bundle`'s cast root and report `kept`, or fail the load with the
    /// role the root refused.
    fn keep_root(&mut self, root: Result<BundleRoot, String>, bundle: Digest, kept: LoadOutcome) -> LoadOutcome {
        match root {
            Ok(root) => {
                self.roots.insert(bundle, root);
                kept
            }
            Err(error) => LoadOutcome::Failed { error },
        }
    }
}

/// Whether `namespace` is a bundle's root type as published: the bundle
/// namespace qualified by the module's lowercase-hex hash (ADR-0241 §3). A
/// bundle's other exports, such as `aether.bloomery.bundle.invocation`, are
/// qualified the same way under their own declared names and do not match.
fn bundle_root(namespace: &str) -> bool {
    namespace
        .strip_prefix(BUNDLE_NAMESPACE)
        .and_then(|qualified| qualified.strip_prefix('.'))
        .is_some_and(|hash| !hash.is_empty() && hash.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
}
