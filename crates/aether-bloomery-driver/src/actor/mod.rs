//! The native bundle driver: the core's commands performed as mail.
//!
//! Native code spawns [`BundleDriver`] over a born journal owner, passing the
//! journal's id in [`DriverParams`]. `init` builds the [`ProgramCore`] and
//! keeps its first commands; `wire` performs them once the mailbox is live.
//! Inbound [`Call`] mail defers its reply, is fed to the core, and parks the
//! reply under its [`CallerId`]; each reply kind recovers its ticket from the
//! request context and feeds the matching core continuation. Dropping the
//! actor abandons every parked reply.

mod perform;
mod root;

pub use root::ProgramBundleRoot;

use std::collections::HashMap;
use std::mem;

use aether_actor::{Manual, actor};
use aether_bloomery_journal::MAX_READ_EVENTS;
use aether_bloomery_kinds::{
    AppendRecordsResult, Call, ClosureLimit, Invoked, ReadArtifactResult, ReadClosureResult, ReadEventsResult,
};
use aether_data::MailboxId;
use aether_kinds::LoadResult;
use aether_substrate::actor::native::{DeferredReply, NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate::{
    AppendTicket, ArtifactTicket, CallerId, ClosureTicket, Command, EVENTS_PAGE, EventsTicket, InvokeTicket,
    LoadOutcome, LoadTicket, ProgramCore,
};

// Tripwire: the core reads the journal `EVENTS_PAGE` entries per page, and the
// journal owner refuses any page over `MAX_READ_EVENTS`, so the two limits
// must stay equal or every startup read fails with `Err`.
const _: () = assert!(EVENTS_PAGE == MAX_READ_EVENTS);

/// Composer-supplied construction input: the born journal owner's id.
///
/// The id comes from the journal's own `spawn_actor(..).finish()`, so holding
/// it proves the journal was born; a driver cannot be built over a journal
/// that does not exist. The driver addresses it through `actor_at`, never by
/// resolving a name.
pub struct DriverParams {
    /// The journal owner's mailbox id, handed over at spawn.
    pub journal: MailboxId,
}

/// Native bundle driver over the sans-io program core.
///
/// Answers `aether.bloomery.driver.call` with exactly one `CallOutcome` per
/// call, once the outcome is recorded. One driver per engine; the type does
/// not enforce it.
pub struct BundleDriver {
    core: ProgramCore,
    journal: MailboxId,
    startup: Vec<Command>,
    callers: HashMap<CallerId, DeferredReply>,
}

#[actor(instanced, root)]
impl NativeActor for BundleDriver {
    type Config = ClosureLimit;
    type Params = DriverParams;
    const NAMESPACE: &'static str = "aether.bloomery.driver";

    fn init(limit: ClosureLimit, params: DriverParams, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        let DriverParams { journal } = params;
        let (core, startup) = ProgramCore::start(limit);
        Ok(Self { core, journal, startup, callers: HashMap::new() })
    }

    fn wire(&mut self, ctx: &mut NativeCtx<'_>) {
        let startup = mem::take(&mut self.startup);
        self.perform(ctx, startup);
    }

    #[handler::manual]
    fn on_call(&mut self, ctx: &mut NativeCtx<'_, Manual>, call: Call) {
        let owed = ctx.defer_reply_to(ctx.reply_target());
        let (caller, commands) = self.core.call(call);
        assert!(self.callers.insert(caller, owed).is_none(), "the core mints each CallerId once");
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
}

impl Drop for BundleDriver {
    fn drop(&mut self) {
        for (_, owed) in mem::take(&mut self.callers) {
            owed.abandon_for_actor_close();
        }
    }
}
