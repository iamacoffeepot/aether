//! The inspect actor's runtime: each request holds its reply and runs as a
//! job keyed by a ticket, and every journal read and declarations query goes
//! out with that ticket as its request context.

use std::collections::HashMap;

use aether_actor::{ActorRef, ReplyMode, runtime};
use aether_bloomery_driver::BundleDriver;
use aether_bloomery_journal::JournalActor;
use aether_bloomery_kinds::{Declarations, DeclarationsResult, ReadArtifact, ReadArtifactResult, ReadEventsResult};
use aether_data::Digest;
use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending};
use aether_substrate::chassis::error::BootError;

use super::InspectActor;
use super::artifact::{self, ArtifactJob};
use super::events::{self, EventsJob};
use super::kinds::{InspectArtifact, InspectArtifactResult, InspectEvents, InspectEventsResult};

/// The context of one job's journal page read.
#[aether_data::kind(name = "aether.bloomery.inspect.ticket.events", copy, eq, no_serde)]
pub struct EventsTicket(u64);

/// The context of one job's artifact read: the job and the digest it asked
/// for, which the read verifies against.
#[aether_data::kind(name = "aether.bloomery.inspect.ticket.artifact", copy, eq, no_serde)]
pub struct ArtifactTicket {
    job: u64,
    digest: Digest,
}

/// The context of one job's declarations query.
#[aether_data::kind(name = "aether.bloomery.inspect.ticket.declarations", copy, eq, no_serde)]
pub struct DeclarationsTicket(u64);

/// Construction input from the mount seam: the born journal owner and bundle
/// driver the actor reads through.
pub struct InspectParams {
    /// The journal owner every read goes to.
    pub journal: ActorRef<JournalActor>,
    /// The bundle driver the declarations query goes to.
    pub driver: ActorRef<BundleDriver>,
}

/// [`InspectActor`] runtime state: the two references and every job in
/// flight with the reply it owes. Job ids come from one counter, so an id
/// names one job across both tables.
pub struct InspectState {
    journal: ActorRef<JournalActor>,
    driver: ActorRef<BundleDriver>,
    next: u64,
    artifacts: HashMap<u64, (Held<InspectArtifactResult>, ArtifactJob)>,
    events: HashMap<u64, (Held<InspectEventsResult>, EventsJob)>,
}

#[runtime]
impl NativeActor for InspectActor {
    type State = InspectState;

    type Config = ();
    type Params = InspectParams;
    const NAMESPACE: &'static str = "aether.bloomery.inspect";

    fn init((): (), params: InspectParams, _ctx: &mut NativeInitCtx<'_>) -> Result<InspectState, BootError> {
        let InspectParams { journal, driver } = params;
        Ok(InspectState { journal, driver, next: 0, artifacts: HashMap::new(), events: HashMap::new() })
    }

    #[handler::request]
    fn on_artifact(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        request: InspectArtifact,
    ) -> Pending<InspectArtifactResult> {
        let (pending, held) = ctx.hold::<InspectArtifactResult>();
        let job = state.mint();
        let (artifact, step) = ArtifactJob::start(Digest::from_bytes(request.digest), request.depth);
        state.artifacts.insert(job, (held, artifact));
        state.perform_artifact(ctx, job, step);
        pending
    }

    #[handler::request]
    fn on_events(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        request: InspectEvents,
    ) -> Pending<InspectEventsResult> {
        let (pending, held) = ctx.hold::<InspectEventsResult>();
        let job = state.mint();
        let (events, step) = EventsJob::start(request);
        state.events.insert(job, (held, events));
        state.perform_events(ctx, job, step);
        pending
    }

    #[handler::response]
    fn on_read_events(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        result: ReadEventsResult,
        ticket: EventsTicket,
    ) {
        let EventsTicket(job) = ticket;
        if let Some((_, events)) = state.events.get_mut(&job) {
            let step = events.on_page(result);
            state.perform_events(ctx, job, step);
        }
    }

    #[handler::response]
    fn on_read_artifact(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        result: ReadArtifactResult,
        ticket: ArtifactTicket,
    ) {
        let ArtifactTicket { job, digest } = ticket;
        if let Some((_, artifact)) = state.artifacts.get_mut(&job) {
            let step = artifact.on_read(digest, result);
            state.perform_artifact(ctx, job, step);
        }
    }

    #[handler::response]
    fn on_declarations(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        result: DeclarationsResult,
        ticket: DeclarationsTicket,
    ) {
        let DeclarationsTicket(job) = ticket;
        if let Some((_, artifact)) = state.artifacts.get_mut(&job) {
            let step = artifact.on_declarations(result);
            state.perform_artifact(ctx, job, step);
        } else if let Some((_, events)) = state.events.get_mut(&job) {
            let step = events.on_declarations(result);
            state.perform_events(ctx, job, step);
        }
    }
}

impl InspectState {
    /// A fresh job id.
    fn mint(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// Perform one `Artifact` job's step: send its reads or its query, or
    /// answer its held reply and drop the job.
    fn perform_artifact<M: ReplyMode, A, S>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, S, M>,
        job: u64,
        step: artifact::Step,
    ) {
        match step {
            artifact::Step::Read(digests) => {
                for digest in digests {
                    let _ = ctx.send_to_with_context(
                        self.journal,
                        &ReadArtifact { digest },
                        ArtifactTicket { job, digest },
                    );
                }
            }
            artifact::Step::Declarations => {
                let _ = ctx.send_to_with_context(self.driver, &Declarations, DeclarationsTicket(job));
            }
            artifact::Step::Answer(result) => {
                if let Some((held, _)) = self.artifacts.remove(&job) {
                    held.answer(ctx, &result);
                }
            }
            artifact::Step::Wait => {}
        }
    }

    /// Perform one `Events` job's step: send its page read or its query, or
    /// answer its held reply and drop the job.
    fn perform_events<M: ReplyMode, A, S>(&mut self, ctx: &mut NativeCtx<'_, A, S, M>, job: u64, step: events::Step) {
        match step {
            events::Step::Read(page) => {
                let _ = ctx.send_to_with_context(self.journal, &page, EventsTicket(job));
            }
            events::Step::Declarations => {
                let _ = ctx.send_to_with_context(self.driver, &Declarations, DeclarationsTicket(job));
            }
            events::Step::Answer(result) => {
                if let Some((held, _)) = self.events.remove(&job) {
                    held.answer(ctx, &result);
                }
            }
            events::Step::Wait => {}
        }
    }
}
