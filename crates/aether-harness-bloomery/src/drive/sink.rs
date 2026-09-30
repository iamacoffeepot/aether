//! The one reply sink every harness request names as its reply target.
//!
//! Each typed handler forwards the reply it receives, with the correlation it
//! arrived under, over an `mpsc` observer channel to the harness. Nothing is
//! parked in a shared cell: the channel is the only way a reply leaves the
//! actor.

use std::sync::mpsc;

use aether_bloomery_kinds::{
    CallOutcome, DeclarationsResult, MoveHeadResult, Processed, PublishResult, WatchHeadResult,
};
use aether_bloomery_workspace::{ImportResult, RunResult};
use aether_chassis_bloomery::inspect::{InspectArtifactResult, InspectEventsResult};
use aether_kinds::{ListComponentsResult, LoadResult, SpawnResult};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

/// One reply the sink received, tagged by the request kind it answers.
#[derive(Debug)]
pub enum Reply {
    /// The bundle driver's answer to a `Call`, boxed: an outcome carries a
    /// whole recorded `Transition` or `Fault`, and the other replies are small.
    Call(Box<CallOutcome>),
    /// The journal owner's answer to a `MoveHead`.
    MoveHead(MoveHeadResult),
    /// The journal owner's answer to a `Publish`.
    Publish(PublishResult),
    /// The bundle driver's answer to an `AwaitProcessed`.
    Processed(Processed),
    /// The bundle driver's answer to a `Declarations`.
    Declarations(DeclarationsResult),
    /// The component host's answer to a `LoadComponent`, boxed: it carries
    /// the component's whole receive surface.
    Load(Box<LoadResult>),
    /// The component host's answer to a code `Publish`.
    PublishCode(aether_kinds::PublishResult),
    /// A spawned or live instance's answer to a `Spawn`, or the component
    /// host's refusal, boxed: it carries the instance's receive surface.
    Spawn(Box<SpawnResult>),
    /// The component host's answer to a `ListComponents`.
    List(ListComponentsResult),
    /// The journal owner's answer to a `WatchHead`.
    Watch(WatchHeadResult),
    /// The workspace's answer to an `Import`.
    Import(ImportResult),
    /// The workspace's answer to a `Run`, boxed: an outcome carries every
    /// step's record.
    Run(Box<RunResult>),
    /// The inspect actor's answer to an `InspectArtifact`.
    InspectArtifact(InspectArtifactResult),
    /// The inspect actor's answer to an `InspectEvents`.
    InspectEvents(InspectEventsResult),
}

/// A reply and the correlation the harness minted for the request it answers.
pub type Arrival = (u64, Reply);

/// The harness's reply sink, spawned once per harness at the chassis root.
pub struct ReplySink {
    arrivals: mpsc::Sender<Arrival>,
}

impl ReplySink {
    /// Forward one reply to the harness. A harness that has already dropped
    /// its receiver is tearing down, so there is no one left to tell.
    fn forward<A>(&self, ctx: &NativeCtx<'_, A>, reply: Reply) {
        let _ = self.arrivals.send((ctx.reply_target().correlation_id, reply));
    }
}

#[aether_actor::actor(instanced, root)]
impl NativeActor for ReplySink {
    type Config = ();
    type Params = mpsc::Sender<Arrival>;
    const NAMESPACE: &'static str = "aether.harness.bloomery.reply_sink";

    fn init((): (), arrivals: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { arrivals })
    }

    #[aether_actor::handler::response]
    fn on_call_outcome(&mut self, ctx: &mut NativeCtx<'_>, outcome: CallOutcome) {
        self.forward(ctx, Reply::Call(Box::new(outcome)));
    }

    #[aether_actor::handler::response]
    fn on_move_head_result(&mut self, ctx: &mut NativeCtx<'_>, result: MoveHeadResult) {
        self.forward(ctx, Reply::MoveHead(result));
    }

    #[aether_actor::handler::response]
    fn on_publish_result(&mut self, ctx: &mut NativeCtx<'_>, result: PublishResult) {
        self.forward(ctx, Reply::Publish(result));
    }

    #[aether_actor::handler::response]
    fn on_processed(&mut self, ctx: &mut NativeCtx<'_>, processed: Processed) {
        self.forward(ctx, Reply::Processed(processed));
    }

    #[aether_actor::handler::response]
    fn on_declarations_result(&mut self, ctx: &mut NativeCtx<'_>, result: DeclarationsResult) {
        self.forward(ctx, Reply::Declarations(result));
    }

    #[aether_actor::handler::response]
    fn on_load_result(&mut self, ctx: &mut NativeCtx<'_>, result: LoadResult) {
        self.forward(ctx, Reply::Load(Box::new(result)));
    }

    #[aether_actor::handler::response]
    fn on_publish_code_result(&mut self, ctx: &mut NativeCtx<'_>, result: aether_kinds::PublishResult) {
        self.forward(ctx, Reply::PublishCode(result));
    }

    #[aether_actor::handler::response]
    fn on_spawn_result(&mut self, ctx: &mut NativeCtx<'_>, result: SpawnResult) {
        self.forward(ctx, Reply::Spawn(Box::new(result)));
    }

    #[aether_actor::handler::response]
    fn on_list_components_result(&mut self, ctx: &mut NativeCtx<'_>, result: ListComponentsResult) {
        self.forward(ctx, Reply::List(result));
    }

    #[aether_actor::handler::response]
    fn on_watch_head_result(&mut self, ctx: &mut NativeCtx<'_>, result: WatchHeadResult) {
        self.forward(ctx, Reply::Watch(result));
    }

    #[aether_actor::handler::response]
    fn on_import_result(&mut self, ctx: &mut NativeCtx<'_>, result: ImportResult) {
        self.forward(ctx, Reply::Import(result));
    }

    #[aether_actor::handler::response]
    fn on_run_result(&mut self, ctx: &mut NativeCtx<'_>, result: RunResult) {
        self.forward(ctx, Reply::Run(Box::new(result)));
    }

    #[aether_actor::handler::response]
    fn on_inspect_artifact_result(&mut self, ctx: &mut NativeCtx<'_>, result: InspectArtifactResult) {
        self.forward(ctx, Reply::InspectArtifact(result));
    }

    #[aether_actor::handler::response]
    fn on_inspect_events_result(&mut self, ctx: &mut NativeCtx<'_>, result: InspectEventsResult) {
        self.forward(ctx, Reply::InspectEvents(result));
    }
}
