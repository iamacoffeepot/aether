//! The native bundle driver: the core's commands performed as mail.
//!
//! Native code spawns [`BundleDriver`] over a born journal owner, passing the
//! journal's reference in [`DriverParams`]. `init` builds the [`ProgramCore`] and
//! keeps its first commands; `wire` performs them once the mailbox is live.
//! Commands go to the journal owner (reads, appends, and the watch), the
//! component host (loads) and bundle roots. Inbound [`Call`]
//! and [`AwaitProcessed`] mail defers its reply, is fed to the core, and
//! appends the reply to a parked list tagged with its [`CallerId`]; each
//! reply kind recovers its ticket from the request context and feeds the
//! matching core continuation. Dropping the actor abandons every parked reply.

mod perform;
mod root;

pub use root::BundleRoot;

use std::mem;

use aether_actor::{ActorRef, Manual, actor};
use aether_bloomery_journal::{JournalActor, MAX_READ_EVENTS};
use aether_bloomery_kinds::{
    AppendRecordsResult, AwaitProcessed, Call, ClosureLimit, Evaluated, Invoked, ReadArtifactResult, ReadClosureResult,
    ReadEventsResult, Status, Warmed, WatchHeadResult,
};
use aether_kinds::LoadResult;
use aether_substrate::actor::native::{DeferredReply, NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate::{
    AppendTicket, ArtifactTicket, CallerId, ClosureTicket, Command, EVENTS_PAGE, EvaluateTicket, EventsTicket,
    InvokeTicket, LoadOutcome, LoadTicket, ProgramCore, StatusTicket, WarmTicket, WatchTicket,
};

// Tripwire: the core reads the journal `EVENTS_PAGE` entries per page, and the
// journal owner refuses any page over `MAX_READ_EVENTS`, so the two limits
// must stay equal or every startup read fails with `Err`.
const _: () = assert!(EVENTS_PAGE == MAX_READ_EVENTS);

/// Composer-supplied construction input: the born journal owner's reference.
///
/// The reference is what the journal's own `spawn_actor(..).finish()` returns,
/// so holding it proves the journal was born (ADR-0230); a driver cannot be
/// built over a journal that does not exist. The driver sends through it,
/// never by resolving a name.
pub struct DriverParams {
    /// The journal owner's proven reference, handed over at spawn.
    pub journal: ActorRef<JournalActor>,
}

/// Native bundle driver over the sans-io program core.
///
/// Answers `aether.bloomery.driver.call` with exactly one `CallOutcome` per
/// call, once the outcome is recorded, and `aether.bloomery.driver.await_processed`
/// with `Processed` once its bound is quiescent. It performs the core's commands
/// as mail to the journal owner (including the watch), the component host,
/// and bundle roots. One driver per engine; the type does not enforce it.
pub struct BundleDriver {
    core: ProgramCore,
    journal: ActorRef<JournalActor>,
    startup: Vec<Command>,
    callers: Vec<(CallerId, DeferredReply)>,
}

#[actor(instanced, root)]
impl NativeActor for BundleDriver {
    type Config = ClosureLimit;
    type Params = DriverParams;
    const NAMESPACE: &'static str = "aether.bloomery.driver";

    fn init(limit: ClosureLimit, params: DriverParams, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        let DriverParams { journal } = params;
        let (core, startup) = ProgramCore::start(limit);
        Ok(Self { core, journal, startup, callers: Vec::new() })
    }

    fn wire(&mut self, ctx: &mut NativeCtx<'_>) {
        let startup = mem::take(&mut self.startup);
        self.perform(ctx, startup);
    }

    #[handler::manual]
    fn on_call(&mut self, ctx: &mut NativeCtx<'_, aether_substrate::Erased, Manual>, call: Call) {
        let owed = ctx.defer_reply_to(ctx.reply_target());
        let (caller, commands) = self.core.call(call);
        self.callers.push((caller, owed));
        self.perform(ctx, commands);
    }

    #[handler::manual]
    fn on_await_processed(
        &mut self,
        ctx: &mut NativeCtx<'_, aether_substrate::Erased, Manual>,
        request: AwaitProcessed,
    ) {
        let owed = ctx.defer_reply_to(ctx.reply_target());
        let (caller, commands) = self.core.await_processed(request);
        self.callers.push((caller, owed));
        self.perform(ctx, commands);
    }

    #[handler::single]
    fn on_read_events(&mut self, ctx: &mut NativeCtx<'_>, result: ReadEventsResult) {
        let Some(ticket) = ctx.take_context::<EventsTicket>() else {
            return;
        };
        let commands = self.core.on_events(ticket, result);
        self.perform(ctx, commands);
    }

    #[handler::single]
    fn on_read_artifact(&mut self, ctx: &mut NativeCtx<'_>, result: ReadArtifactResult) {
        let Some(ticket) = ctx.take_context::<ArtifactTicket>() else {
            return;
        };
        let commands = self.core.on_artifact(ticket, result);
        self.perform(ctx, commands);
    }

    #[handler::single]
    fn on_read_closure(&mut self, ctx: &mut NativeCtx<'_>, result: ReadClosureResult) {
        let Some(ticket) = ctx.take_context::<ClosureTicket>() else {
            return;
        };
        let commands = self.core.on_closure(ticket, result);
        self.perform(ctx, commands);
    }

    #[handler::single]
    fn on_append_records(&mut self, ctx: &mut NativeCtx<'_>, result: AppendRecordsResult) {
        let Some(ticket) = ctx.take_context::<AppendTicket>() else {
            return;
        };
        let commands = self.core.on_appended(ticket, result);
        self.perform(ctx, commands);
    }

    #[handler::single]
    fn on_load_result(&mut self, ctx: &mut NativeCtx<'_>, result: LoadResult) {
        let Some(ticket) = ctx.take_context::<LoadTicket>() else {
            return;
        };
        let outcome = match result {
            LoadResult::Ok { mailbox_id, .. } => LoadOutcome::Loaded { root: mailbox_id },
            LoadResult::Err { error } => LoadOutcome::Failed { error },
        };
        let commands = self.core.on_loaded(ticket, outcome);
        self.perform(ctx, commands);
    }

    #[handler::single]
    fn on_invoked(&mut self, ctx: &mut NativeCtx<'_>, invoked: Invoked) {
        let Some(ticket) = ctx.take_context::<InvokeTicket>() else {
            return;
        };
        let commands = self.core.on_invoked(ticket, invoked);
        self.perform(ctx, commands);
    }

    #[handler::single]
    fn on_watch_head_result(&mut self, ctx: &mut NativeCtx<'_>, result: WatchHeadResult) {
        let Some(ticket) = ctx.take_context::<WatchTicket>() else {
            return;
        };
        let commands = self.core.on_watched(ticket, result);
        self.perform(ctx, commands);
    }

    #[handler::single]
    fn on_warmed(&mut self, ctx: &mut NativeCtx<'_>, warmed: Warmed) {
        let Some(ticket) = ctx.take_context::<WarmTicket>() else {
            return;
        };
        let commands = self.core.on_warmed(ticket, warmed);
        self.perform(ctx, commands);
    }

    #[handler::single]
    fn on_evaluated(&mut self, ctx: &mut NativeCtx<'_>, evaluated: Evaluated) {
        let Some(ticket) = ctx.take_context::<EvaluateTicket>() else {
            return;
        };
        let commands = self.core.on_evaluated(ticket, evaluated);
        self.perform(ctx, commands);
    }

    #[handler::single]
    fn on_status(&mut self, ctx: &mut NativeCtx<'_>, status: Status) {
        let Some(ticket) = ctx.take_context::<StatusTicket>() else {
            return;
        };
        let commands = self.core.on_status(ticket, &status);
        self.perform(ctx, commands);
    }
}

impl Drop for BundleDriver {
    fn drop(&mut self) {
        for (_, owed) in mem::take(&mut self.callers) {
            owed.abandon_for_actor_close();
        }
    }
}
