//! The `aether.hub.mcp` tool provider actor.
//!
//! One `#[mcp::router]` implementation carrying the hub catalog. Each
//! `#[mcp::tool]` method builds a fleet request and defers to `FleetServer`;
//! each `#[mcp::reply]` mapping turns that peer's typed answer into the
//! tool's declared output. The router mints one hidden request kind per tool,
//! emits the dispatch and reply glue, and appends one `RegisterToolSelf` per
//! tool to `wire`, so the capability learns this catalog the same way any
//! provider's is learned — by mail, from a host-stamped source.
//!
//! There is no `#[http::router]` here. The stacked form is for an actor that
//! also owns routes; this one owns none, and the Model Context Protocol
//! endpoint is `McpServerCapability`'s own route, not a hub route.

// A tool method's parameter list and a reply mapping's return type are the
// authoring contract the router parses, not choices this module makes: the
// context and the decoded reply arrive owned, and a mapping declares
// `Result<Output, ToolError>` even where its own body cannot fail, because
// the macro binds it to a tool whose deferred sibling can.
#![allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)]

use aether_actor::{Manual, actor};
use aether_fleet::FleetServer;
use aether_kinds::{ListEngines, ListEnginesResult, SpawnEngineResult, TerminateEngine, TerminateEngineResult};
use aether_mcp as mcp;
use aether_mcp::RegisterToolResult;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use super::engines::{
    ListEnginesInput, ListEnginesOutput, SpawnSubstrateInput, SpawnSubstrateOutput, TerminateSubstrateInput,
    TerminateSubstrateOutput, list_output, spawn_output, spawn_request, terminate_output,
};

/// The hub's Model Context Protocol tool provider (ADR-0122 identity form,
/// impl-hosted).
///
/// Stateless: every tool's whole answer comes from the fleet reply it is
/// waiting on, and the deferral's correlation is carried by the capability's
/// own request-context table rather than by anything held here.
pub struct HubToolProvider;

#[mcp::router]
#[actor(singleton, root)]
impl NativeActor for HubToolProvider {
    type State = ();
    type Config = ();
    const NAMESPACE: &'static str = "aether.hub.mcp";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<(), BootError> {
        Ok(())
    }

    /// List the engines the hub supervises.
    #[mcp::tool(
        name = "list_engines",
        title = "List supervised engines",
        description = "List every engine the hub currently supervises, plus a bounded sidecar of the engines that \
                       recently left and why. A listed engine is live: the hub evicts one whose heartbeat crosses \
                       the miss limit.",
        read_only,
        idempotent,
        closed_world
    )]
    fn list_engines(
        _state: &mut (),
        context: mcp::Context<'_, NativeCtx<'_, Manual>>,
        _input: ListEnginesInput,
    ) -> mcp::Outcome<ListEnginesOutput> {
        context.defer(&ListEngines {}).to::<FleetServer>()
    }

    /// Fork a substrate from the hub's content-addressed binary store.
    #[mcp::tool(
        name = "spawn_substrate",
        title = "Spawn a substrate",
        description = "Fork a substrate binary resolved from the hub's content-addressed store and supervise it. \
                       Name an exact selector — a content hash, a name@version, or a name — or leave it null and \
                       let the chassis, caps, and target attribute query resolve one; an empty selection resolves \
                       the stored default. The hub assigns the engine's RPC port itself.",
        non_destructive,
        closed_world
    )]
    fn spawn_substrate(
        _state: &mut (),
        context: mcp::Context<'_, NativeCtx<'_, Manual>>,
        input: SpawnSubstrateInput,
    ) -> mcp::Outcome<SpawnSubstrateOutput> {
        context.defer(&spawn_request(input)).to::<FleetServer>()
    }

    /// Shut a supervised engine down.
    #[mcp::tool(
        name = "terminate_substrate",
        title = "Terminate a substrate",
        description = "Shut down one supervised engine by its identifier. The hub's proxy kills the child \
                       substrate and self-shuts-down; the engine then appears in list_engines.recently_died with \
                       a terminated reason. An unknown identifier is refused.",
        closed_world
    )]
    fn terminate_substrate(
        _state: &mut (),
        context: mcp::Context<'_, NativeCtx<'_, Manual>>,
        input: TerminateSubstrateInput,
    ) -> mcp::Outcome<TerminateSubstrateOutput> {
        context.defer(&TerminateEngine { engine_id: input.engine_id }).to::<FleetServer>()
    }

    #[mcp::reply(ListEnginesResult, tool = list_engines)]
    fn map_list_engines(
        _state: &mut (),
        _ctx: &mut NativeCtx<'_, Manual>,
        reply: ListEnginesResult,
    ) -> Result<ListEnginesOutput, mcp::ToolError> {
        Ok(list_output(reply))
    }

    #[mcp::reply(SpawnEngineResult, tool = spawn_substrate)]
    fn map_spawn_substrate(
        _state: &mut (),
        _ctx: &mut NativeCtx<'_, Manual>,
        reply: SpawnEngineResult,
    ) -> Result<SpawnSubstrateOutput, mcp::ToolError> {
        spawn_output(reply)
    }

    #[mcp::reply(TerminateEngineResult, tool = terminate_substrate)]
    fn map_terminate_substrate(
        _state: &mut (),
        _ctx: &mut NativeCtx<'_, Manual>,
        reply: TerminateEngineResult,
    ) -> Result<TerminateSubstrateOutput, mcp::ToolError> {
        terminate_output(reply)
    }

    /// The router leaves this slot free so an author can claim it when
    /// registration diagnostics matter. A hub that composed the endpoint but
    /// could not publish its catalog would otherwise answer `tools/list` with
    /// an empty catalog and no explanation anywhere.
    #[handler::single]
    fn on_register_tool_result(_state: &mut (), _ctx: &mut NativeCtx<'_>, result: RegisterToolResult) {
        match result {
            RegisterToolResult::Ok => {}
            RegisterToolResult::Err { error } => {
                tracing::error!(target: "aether_hub::mcp", "hub tool registration refused: {error}");
            }
        }
    }
}
