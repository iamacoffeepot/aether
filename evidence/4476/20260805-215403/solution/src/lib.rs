use aether_actor::{ActorInitError, Subname, WasmActor, WasmCtx, WasmInitCtx, actor};
use serde::{Deserialize, Serialize};

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "dogfood.resolve.setup")]
pub struct Setup {}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "dogfood.resolve.spawn_worker")]
pub struct SpawnWorker {}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "dogfood.resolve.probe_worker")]
pub struct ProbeWorker {
    pub value: u32,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "dogfood.resolve.work_request")]
pub struct WorkRequest {
    pub value: u32,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "dogfood.resolve.work_reply")]
pub struct WorkReply {
    pub value: u32,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "dogfood.resolve.accepted")]
pub struct Accepted {
    pub stage: u32,
}

pub struct Root;

#[actor(singleton)]
impl WasmActor for Root {
    const NAMESPACE: &'static str = "dogfood.resolve.root";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_setup(&mut self, ctx: &mut WasmCtx<'_>, _setup: Setup) -> Accepted {
        match ctx.spawn_child::<Root, Branch>(Subname::Named("branch"), &()) {
            Ok(_discarded_mailbox_id) => tracing::info!("spawned branch and discarded its mailbox id"),
            Err(error) => tracing::error!(?error, "failed to spawn branch"),
        }

        Accepted { stage: 1 }
    }

    #[handler::single]
    fn on_spawn_worker(&mut self, ctx: &mut WasmCtx<'_>, request: SpawnWorker) -> Accepted {
        ctx.resolve_actor::<Branch>("branch").send(&request);
        Accepted { stage: 2 }
    }

    #[handler::single]
    fn on_probe_worker(&mut self, ctx: &mut WasmCtx<'_>, request: ProbeWorker) -> Accepted {
        ctx.resolve_actor::<Branch>("branch").send(&request);
        Accepted { stage: 3 }
    }
}

pub struct Branch;

#[actor(instanced, child_of(Root))]
impl WasmActor for Branch {
    const NAMESPACE: &'static str = "dogfood.resolve.branch";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_spawn_worker(&mut self, ctx: &mut WasmCtx<'_>, _request: SpawnWorker) {
        match ctx.spawn_child::<Branch, Worker>(Subname::Named("alpha"), &()) {
            Ok(_deliberately_discarded_mailbox_id) => {
                tracing::info!("spawned worker alpha and deliberately discarded its mailbox id");
            }
            Err(error) => tracing::error!(?error, "failed to spawn worker alpha"),
        }
    }

    #[handler::single]
    fn on_probe_worker(&mut self, ctx: &mut WasmCtx<'_>, request: ProbeWorker) {
        ctx.resolve_actor::<Worker>("alpha").send(&WorkRequest { value: request.value });
        tracing::info!("resolved worker alpha by typed key and sent request");
    }

    #[handler::single]
    fn on_work_reply(&mut self, _ctx: &mut WasmCtx<'_>, reply: WorkReply) {
        tracing::info!(value = reply.value, "observed worker alpha reply");
    }
}

pub struct Worker;

#[actor(instanced, child_of(Branch))]
impl WasmActor for Worker {
    const NAMESPACE: &'static str = "dogfood.resolve.worker";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_work_request(&mut self, _ctx: &mut WasmCtx<'_>, request: WorkRequest) -> WorkReply {
        tracing::info!(value = request.value, "worker alpha received request");
        WorkReply { value: request.value + 1 }
    }
}

aether_actor::export!(default = Root, Branch, Worker);
