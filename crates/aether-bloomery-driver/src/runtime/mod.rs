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
//! journal owner (reads, appends, and the watch), the component host (loads,
//! each under the unit's bundle name), bundle roots, and the providers of
//! program APIs. The core names a loaded bundle by its digest; the shell keeps
//! each root's proven reference, taken from its load reply's stamped sender
//! (ADR-0230 §3), keyed by that digest, and sends to it with the command's
//! ticket as the request context. Inbound [`Call`],
//! [`AwaitProcessed`], a bundle root's fetch-on-miss [`ReadArtifact`], and a
//! relayed [`ApiCall`] each hold a typed reply (ADR-0243), are fed to the
//! core, and keep the held ticket keyed by the returned [`CallerId`]; each
//! reply kind recovers its ticket from the request context and feeds the
//! matching core continuation. An actor close while the engine keeps running
//! answers every held ticket with its reply kind's `unanswered()` before the
//! state drops, and an engine teardown settles them silently (ADR-0243 §1).
//!
//! A program API call relays the same way as a fetch (ADR-0240 D6): the
//! invocation sends [`ApiCall`] to its bundle root, the root relays it here,
//! and the core maps `Http` to the http capability and `Workspace` to the
//! held workspace, refusing any other API; the provider's reply comes back
//! under an [`ApiTicket`] and answers the held call. The shell sends a
//! relayed workspace run over the unit's own journal as its `source`, written
//! once at `init` from the unit key (ADR-0240 D7).

mod bundles;
mod core;
mod perform;
mod programs;
mod reactors;
mod recovery;

pub use self::core::{
    ApiReply, ApiTicket, AppendTicket, ArtifactTicket, CallerId, ClosureTicket, Command, EVENTS_PAGE, EvaluateTicket,
    EventsTicket, InvokeTicket, LoadOutcome, LoadTicket, ProgramCore, StatusTicket, WarmTicket, WatchTicket,
};

use std::collections::{BTreeMap, HashMap};
use std::mem;

use aether_actor::{ActorPath, ActorRef, ErasedActorRef, ProtocolPath, runtime};
use aether_bloomery_journal::{JournalActor, MAX_READ_EVENTS};
use aether_bloomery_kinds::{
    ApiCall, ApiCallResult, AppendRecordsResult, ArtifactStorage, AwaitProcessed, Call, CallOutcome, ClosureLimit,
    Digest, Evaluated, Invoked, Processed, ReadArtifact, ReadArtifactResult, ReadClosureResult, ReadEventsResult,
    Status, UnitKey, Warmed, WatchHeadResult,
};
use aether_bloomery_workspace::WorkspaceCapability;
use aether_http::FetchResult;
use aether_kinds::LoadResult;
use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending};
use aether_substrate::chassis::error::BootError;

use crate::BundleDriver;

// Tripwire: the core reads the journal `EVENTS_PAGE` entries per page, and the
// journal owner refuses any page over `MAX_READ_EVENTS`, so the two limits
// must stay equal or every startup read fails with `Err`.
const _: () = assert!(EVENTS_PAGE == MAX_READ_EVENTS);

/// Composer-supplied construction input: the unit's key, the born journal
/// owner's reference, and the reference of the workspace the unit's programs
/// run through.
///
/// The journal reference is what the journal's own `spawn_actor(..).finish()`
/// returns, so holding it proves the journal was born (ADR-0230); a driver
/// cannot be built over a journal that does not exist. The driver sends
/// through both references, never by resolving a name.
pub struct DriverParams {
    /// The key of the unit this driver folds for. Every bundle root it loads
    /// is keyed by it, at `aether.bloomery.bundle.<hash>:<unit key>`: the
    /// bundle's content-addressed published name and this key (ADR-0240 D4).
    pub unit: UnitKey,
    /// The journal owner's proven reference, handed over at spawn.
    pub journal: ActorRef<JournalActor>,
    /// The workspace programs' `Workspace` calls reach (ADR-0240 D6).
    pub workspace: ActorRef<WorkspaceCapability>,
}

/// [`BundleDriver`] runtime state: the sans-io program core, the unit and
/// journal it folds for, the workspace its programs run through and the
/// source their runs read and stage through, the core's startup commands
/// until `wire` performs them, the held replies it owes, and each loaded
/// bundle's root.
pub struct BundleDriverState {
    core: ProgramCore,
    unit: UnitKey,
    journal: ActorRef<JournalActor>,
    workspace: ActorRef<WorkspaceCapability>,
    /// The unit's journal as the storage every relayed run names, written
    /// from the unit key: `aether.bloomery.journal:<unit key>`.
    source: ProtocolPath<ArtifactStorage>,
    startup: Vec<Command>,
    callers: HashMap<CallerId, Caller>,
    /// The digest each in-flight load was issued for, keyed by its ticket.
    loading: BTreeMap<LoadTicket, Digest>,
    /// Each loaded bundle's root, the stamped sender of its load reply.
    roots: HashMap<Digest, ErasedActorRef>,
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
        let DriverParams { unit, journal, workspace } = params;
        let (core, startup) = ProgramCore::start(limit);
        let source = ActorPath::<JournalActor>::instance(unit.as_load_name()).narrow::<ArtifactStorage>();
        Ok(BundleDriverState {
            core,
            source,
            unit,
            journal,
            workspace,
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
    #[handler::single]
    fn on_call(state: &mut Self::State, ctx: &mut NativeCtx<'_>, call: Call) -> Pending<CallOutcome> {
        let (pending, held) = ctx.hold::<CallOutcome>();
        let (caller, commands) = state.core.call(call);
        state.callers.insert(caller, Caller::Call(held));
        state.perform(ctx, commands);
        pending
    }

    #[handler::single]
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

    #[handler::single]
    fn on_read_events(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: ReadEventsResult) {
        let Some(ticket) = ctx.take_context::<EventsTicket>() else {
            return;
        };
        let commands = state.core.on_events(ticket, result);
        state.perform(ctx, commands);
    }

    #[handler::single]
    fn on_read_artifact(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: ReadArtifactResult) {
        let Some(ticket) = ctx.take_context::<ArtifactTicket>() else {
            return;
        };
        let commands = state.core.on_artifact(ticket, result);
        state.perform(ctx, commands);
    }

    /// Serves a bundle root's fetch-on-miss: the core answers it from its
    /// artifact cache or one shared journal read per digest, and the held
    /// reply carries the answer back to the root with the root's correlation.
    #[handler::single]
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
    #[handler::single]
    fn on_api_call(state: &mut Self::State, ctx: &mut NativeCtx<'_>, request: ApiCall) -> Pending<ApiCallResult> {
        let (pending, held) = ctx.hold::<ApiCallResult>();
        let (caller, commands) = state.core.call_api(request);
        state.callers.insert(caller, Caller::Api(held));
        state.perform(ctx, commands);
        pending
    }

    #[handler::single]
    fn on_fetch_result(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: FetchResult) {
        let Some(ticket) = ctx.take_context::<ApiTicket>() else {
            return;
        };
        let commands = state.core.on_api_reply(ticket, ApiReply::Fetch(result));
        state.perform(ctx, commands);
    }

    #[handler::single]
    fn on_workspace_run_result(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        result: aether_bloomery_workspace::RunResult,
    ) {
        let Some(ticket) = ctx.take_context::<ApiTicket>() else {
            return;
        };
        let commands = state.core.on_api_reply(ticket, ApiReply::Workspace(result));
        state.perform(ctx, commands);
    }

    #[handler::single]
    fn on_read_closure(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: ReadClosureResult) {
        let Some(ticket) = ctx.take_context::<ClosureTicket>() else {
            return;
        };
        let commands = state.core.on_closure(ticket, result);
        state.perform(ctx, commands);
    }

    #[handler::single]
    fn on_append_records(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: AppendRecordsResult) {
        let Some(ticket) = ctx.take_context::<AppendTicket>() else {
            return;
        };
        let commands = state.core.on_appended(ticket, result);
        state.perform(ctx, commands);
    }

    #[handler::single]
    fn on_load_result(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: LoadResult) {
        let Some(ticket) = ctx.take_context::<LoadTicket>() else {
            return;
        };
        // ADR-0230 §3: the loaded root sends its own load reply, so the
        // stamped sender is the reference the driver keeps for the digest.
        let bundle = state.loading.remove(&ticket);
        let outcome = match (result, ctx.sender(), bundle) {
            (LoadResult::Ok { .. }, Some(root), Some(bundle)) => {
                state.roots.insert(bundle, root);
                LoadOutcome::Loaded
            }
            (LoadResult::Ok { path, .. }, None, _) => {
                LoadOutcome::Failed { error: format!("load reply for {path} carried no sender") }
            }
            (LoadResult::Ok { path, .. }, Some(_), None) => {
                LoadOutcome::Failed { error: format!("load reply for {path} matched no issued load") }
            }
            (LoadResult::Err { error }, ..) => LoadOutcome::Failed { error },
        };
        let commands = state.core.on_loaded(ticket, outcome);
        state.perform(ctx, commands);
    }

    #[handler::single]
    fn on_invoked(state: &mut Self::State, ctx: &mut NativeCtx<'_>, invoked: Invoked) {
        let Some(ticket) = ctx.take_context::<InvokeTicket>() else {
            return;
        };
        let commands = state.core.on_invoked(ticket, invoked);
        state.perform(ctx, commands);
    }

    #[handler::single]
    fn on_watch_head_result(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: WatchHeadResult) {
        let Some(ticket) = ctx.take_context::<WatchTicket>() else {
            return;
        };
        let commands = state.core.on_watched(ticket, result);
        state.perform(ctx, commands);
    }

    #[handler::single]
    fn on_warmed(state: &mut Self::State, ctx: &mut NativeCtx<'_>, warmed: Warmed) {
        let Some(ticket) = ctx.take_context::<WarmTicket>() else {
            return;
        };
        let commands = state.core.on_warmed(ticket, warmed);
        state.perform(ctx, commands);
    }

    #[handler::single]
    fn on_evaluated(state: &mut Self::State, ctx: &mut NativeCtx<'_>, evaluated: Evaluated) {
        let Some(ticket) = ctx.take_context::<EvaluateTicket>() else {
            return;
        };
        let commands = state.core.on_evaluated(ticket, evaluated);
        state.perform(ctx, commands);
    }

    #[handler::single]
    fn on_status(state: &mut Self::State, ctx: &mut NativeCtx<'_>, status: Status) {
        let Some(ticket) = ctx.take_context::<StatusTicket>() else {
            return;
        };
        let commands = state.core.on_status(ticket, &status);
        state.perform(ctx, commands);
    }
}
