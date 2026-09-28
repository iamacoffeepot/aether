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
//! [`AwaitProcessed`], and a bundle root's fetch-on-miss [`ReadArtifact`]
//! mail defers its reply, is fed to the core, and parks the reply keyed by
//! its [`CallerId`]; each reply kind recovers its ticket from the request
//! context and feeds the matching core continuation. Dropping the actor's
//! state abandons every parked reply.
//!
//! A program API call relays the same way as a fetch (ADR-0240 D6): the
//! invocation sends [`ApiCall`] to its bundle root, the root relays it here,
//! and the core maps `Http` to the http capability and `Workspace` to the
//! held workspace, refusing any other API; the provider's reply comes back
//! under an [`ApiTicket`] and answers the parked call.

mod bundles;
mod core;
mod perform;
mod programs;
mod reactors;
mod recovery;

pub use self::core::{
    ApiTicket, AppendTicket, ArtifactTicket, CallerId, ClosureTicket, Command, EVENTS_PAGE, EvaluateTicket,
    EventsTicket, InvokeTicket, LoadOutcome, LoadTicket, ProgramCore, StatusTicket, WarmTicket, WatchTicket,
};

use std::collections::{BTreeMap, HashMap};
use std::mem;

use aether_actor::{ActorRef, ErasedActorRef, Manual, runtime};
use aether_bloomery_journal::{JournalActor, MAX_READ_EVENTS};
use aether_bloomery_kinds::{
    ApiCall, AppendRecordsResult, AwaitProcessed, Call, ClosureLimit, Digest, Evaluated, Invoked, ReadArtifact,
    ReadArtifactResult, ReadClosureResult, ReadEventsResult, Status, UnitKey, Warmed, WatchHeadResult,
};
use aether_bloomery_workspace::WorkspaceCapability;
use aether_data::Kind;
use aether_http::FetchResult;
use aether_kinds::LoadResult;
use aether_substrate::actor::native::{DeferredReply, NativeActor, NativeCtx, NativeInitCtx};
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
    /// is named [`UnitBundle::name`] of this key and the bundle's digest
    /// (ADR-0240 D4).
    ///
    /// [`UnitBundle::name`]: aether_bloomery_kinds::UnitBundle::name
    pub unit: UnitKey,
    /// The journal owner's proven reference, handed over at spawn.
    pub journal: ActorRef<JournalActor>,
    /// The workspace programs' `Workspace` calls reach (ADR-0240 D6).
    pub workspace: ActorRef<WorkspaceCapability>,
}

/// [`BundleDriver`] runtime state: the sans-io program core, the unit and
/// journal it folds for, the workspace its programs run through, the core's
/// startup commands until `wire` performs them, the parked replies it owes,
/// and each loaded bundle's root.
pub struct BundleDriverState {
    core: ProgramCore,
    unit: UnitKey,
    journal: ActorRef<JournalActor>,
    workspace: ActorRef<WorkspaceCapability>,
    startup: Vec<Command>,
    callers: HashMap<CallerId, DeferredReply>,
    /// The digest each in-flight load was issued for, keyed by its ticket.
    loading: BTreeMap<LoadTicket, Digest>,
    /// Each loaded bundle's root, the stamped sender of its load reply.
    roots: HashMap<Digest, ErasedActorRef>,
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
        Ok(BundleDriverState {
            core,
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

    #[handler::manual]
    fn on_call(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Manual>, call: Call) {
        let owed = ctx.defer_reply_to(ctx.reply_target());
        let (caller, commands) = state.core.call(call);
        state.callers.insert(caller, owed);
        state.perform(ctx, commands);
    }

    #[handler::manual]
    fn on_await_processed(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Manual>, request: AwaitProcessed) {
        let owed = ctx.defer_reply_to(ctx.reply_target());
        let (caller, commands) = state.core.await_processed(request);
        state.callers.insert(caller, owed);
        state.perform(ctx, commands);
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
    /// artifact cache or one shared journal read per digest, and the parked
    /// reply carries the answer back to the root with the root's correlation.
    #[handler::manual]
    fn on_fetch_artifact(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Manual>, request: ReadArtifact) {
        let owed = ctx.defer_reply_to(ctx.reply_target());
        let (caller, commands) = state.core.fetch_artifact(request);
        state.callers.insert(caller, owed);
        state.perform(ctx, commands);
    }

    /// Serves a bundle root's relayed program API call: the core sends it to
    /// the API's provider or refuses it, and the parked reply carries the
    /// answer back to the root with the root's correlation.
    #[handler::manual]
    fn on_api_call(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Manual>, request: ApiCall) {
        let owed = ctx.defer_reply_to(ctx.reply_target());
        let (caller, commands) = state.core.call_api(request);
        state.callers.insert(caller, owed);
        state.perform(ctx, commands);
    }

    #[handler::single]
    #[expect(clippy::needless_pass_by_value, reason = "a handler takes its kind by value; the reply relays as bytes")]
    fn on_fetch_result(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: FetchResult) {
        let Some(ticket) = ctx.take_context::<ApiTicket>() else {
            return;
        };
        let commands = state.core.on_api_reply(ticket, FetchResult::ID, result.encode_into_bytes());
        state.perform(ctx, commands);
    }

    #[handler::single]
    #[expect(clippy::needless_pass_by_value, reason = "a handler takes its kind by value; the reply relays as bytes")]
    fn on_workspace_run_result(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        result: aether_bloomery_workspace::RunResult,
    ) {
        let Some(ticket) = ctx.take_context::<ApiTicket>() else {
            return;
        };
        let commands =
            state.core.on_api_reply(ticket, aether_bloomery_workspace::RunResult::ID, result.encode_into_bytes());
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

impl Drop for BundleDriverState {
    fn drop(&mut self) {
        for owed in mem::take(&mut self.callers).into_values() {
            owed.abandon_for_actor_close();
        }
    }
}
