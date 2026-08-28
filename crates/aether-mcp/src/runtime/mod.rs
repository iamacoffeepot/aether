//! The `aether.mcp.server` runtime half (ADR-0122 identity/runtime split).
//!
//! Compiled only under `feature = "runtime"`, so a provider that takes this
//! crate at the marker-and-vocabulary tier never names these types nor pulls
//! `aether_substrate` through. The substrate-typed imports are gated once here
//! rather than line by line.
//!
//! The capability owns no socket. Its endpoint is one
//! `#[http::route(any, "/mcp")]` claimed from `wire` through
//! `HttpServerCapability`, so binding, body ceilings, and connection limits
//! belong to that capability's configuration. What lives here is everything the
//! protocol layer itself decides: who may call, how much it will parse, how
//! long a provider may take, and when a result becomes an address.
//!
//! The concern submodules split along the shape of one request:
//! [`transport`] decides what may become a protocol message, [`admission`]
//! decides whether the server will take it now, [`registry`] decides what a
//! name resolves to, [`request`] carries it to a provider and projects the
//! answer, and [`response_resources`] holds what did not fit inline.

// `#[handler]` methods take their decoded payload by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the decoded bytes so
// callers can't see references.
#![allow(clippy::needless_pass_by_value)]

pub mod admission;
pub mod registry;
pub mod request;
pub mod response_resources;
mod state;
pub mod transport;

#[cfg(test)]
mod tests;

pub use state::McpServerState;

use std::sync::Arc;
use std::time::Instant;

use aether_actor::{Addressable, Manual, runtime};
use aether_http as http;
use aether_http::kinds::HttpServerResponse;
use aether_kinds::MonitorNotice;
use aether_kinds::trace::Settled;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate::kinds::SERVER_NAMESPACE;
use crate::kinds::{
    ReadResourceResult, RegisterResourceProviderResult, RegisterResourceProviderSelf, RegisterToolResult,
    RegisterToolSelf, RequestDeadlineElapsed, ToolInvocationResult,
};
use crate::{McpServerCapability, McpServerConfiguration};

use admission::DeadlineTimer;

// Tripwire: the actor's namespace is stated twice by necessity — the identity
// harvest and the route macro both need a literal here, and `SERVER_NAMESPACE`
// is what a provider addresses and what every kind name in `kinds.rs` is
// derived from. A drift between the two would give the capability an address no
// documented constant names, and nothing else would notice.
const _: () = assert!(same_text(<McpServerCapability as Addressable>::NAMESPACE, SERVER_NAMESPACE));

/// Compile-time string equality, which `==` is not in a const context.
const fn same_text(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// ADR-0155 §3 fail-fast for a composed-but-disabled server. The capability
/// still claims its mailbox and its route, so "linked but not enabled" is a
/// first-class diagnosable state rather than mail warn-dropping at an address
/// nothing serves.
fn disabled_registration() -> String {
    "aether.mcp.server is composed but disabled on this chassis (enabled = false); \
     no tools or resource providers can be registered"
        .to_string()
}

#[http::router]
#[runtime]
impl NativeActor for McpServerCapability {
    type State = McpServerState;

    type Config = McpServerConfiguration;

    // A literal, because the identity harvest lifts this expression verbatim
    // into `Addressable` and the route macro needs it before name resolution
    // exists. The tripwire below holds it equal to the public constant.
    const NAMESPACE: &'static str = "aether.mcp.server";

    fn init(config: McpServerConfiguration, ctx: &mut NativeInitCtx<'_>) -> Result<McpServerState, BootError> {
        let enabled = config.enabled;
        let mailer = ctx.mailer();
        let self_id = ctx.self_id();
        // One monotonic base for every deadline and every stored-response
        // lifetime. A wall clock would let a clock adjustment expire a live
        // tool call or a live address for a reason no log could explain.
        let mut state = McpServerState::new(config, Arc::clone(&mailer), self_id, Instant::now());

        if enabled {
            state.timer = Some(DeadlineTimer::start(mailer, self_id, state.epoch));
            tracing::info!(target: "aether_mcp::server", "model context protocol endpoint claiming POST /mcp");
        } else {
            tracing::info!(
                target: "aether_mcp::server",
                "model context protocol server composed disabled (enabled = false)",
            );
        }
        Ok(state)
    }

    /// The route registration `#[http::router]` appends its `POST /mcp`
    /// `RegisterRouteSelf` send to.
    ///
    /// Written out rather than left to the macro's synthesized form, because
    /// the synthesized one copies this impl's state parameter into a body that
    /// never reads it; stating the hook here keeps the registration visible at
    /// the place a reader looks for it.
    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {}

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        // Dropping the timer signals and joins its thread; taking it here
        // rather than leaving it to the state's own drop keeps the join inside
        // the teardown hook where a stall is attributable.
        state.timer = None;
    }

    /// The whole Model Context Protocol endpoint: one route, every method.
    ///
    /// `any` rather than `Post`, so a `GET` or `DELETE` reaches this handler and
    /// is answered `405` by the protocol's own rule. Registering POST alone
    /// would let those fall through to whatever else claims `/`, and a client
    /// probing for the optional event stream would learn nothing.
    ///
    /// # Agent
    /// `POST /mcp` with a JSON-RPC 2.0 body, `Content-Type: application/json`,
    /// an `Accept` listing both `application/json` and `text/event-stream`, a
    /// bearer token, and — after `initialize` — `MCP-Protocol-Version`.
    #[http::route(any, "/mcp")]
    fn on_mcp(state: &mut McpServerState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        if !state.config.enabled {
            return http::Outcome::Reply(HttpServerResponse {
                status: 503,
                headers: Vec::new(),
                body: disabled_registration().into_bytes(),
            });
        }
        request::serve(state, ctx)
    }

    /// Claim a tool name for the *sending* actor, resolved from the inbound
    /// envelope's host-stamped `Source`.
    ///
    /// # Agent
    /// `RegisterToolSelf`, sent from a provider's `wire` hook — the authoring
    /// macro appends one per declared tool. An external session has no local
    /// mailbox and is answered `Err`.
    #[handler::single]
    fn on_register_tool_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: RegisterToolSelf,
    ) -> RegisterToolResult {
        if !state.config.enabled {
            return RegisterToolResult::Err { error: disabled_registration() };
        }
        let Some(registrant) = ctx.source_mailbox() else {
            return RegisterToolResult::Err {
                error: "aether.mcp.server.register_tool_self requires a local sender; the registrant \
                        is resolved from the host-stamped source and cannot be named in the payload"
                    .to_string(),
            };
        };

        // The capability registry is cloned out first so the predicate owns
        // its own handle: the registry borrow would otherwise still be alive
        // across the mutable registry call it is passed into.
        let capabilities = Arc::clone(state.mailer.capability_registry());
        let accepts = move |mailbox, kind| capabilities.accepts(mailbox, kind);

        let result = state.tools.register(registrant, &payload, &accepts);
        if matches!(result, RegisterToolResult::Ok) {
            state.watch(ctx, registrant);
        }
        result
    }

    /// Claim a URI prefix for the *sending* actor.
    ///
    /// # Agent
    /// `RegisterResourceProviderSelf { prefix, descriptors }`, sent from
    /// `wire`. Prefix claims are exclusive and `aether://mcp/response/` is
    /// reserved.
    #[handler::single]
    fn on_register_resource_provider_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: RegisterResourceProviderSelf,
    ) -> RegisterResourceProviderResult {
        if !state.config.enabled {
            return RegisterResourceProviderResult::Err { error: disabled_registration() };
        }
        let Some(registrant) = ctx.source_mailbox() else {
            return RegisterResourceProviderResult::Err {
                error: "aether.mcp.server.register_resource_provider_self requires a local sender".to_string(),
            };
        };

        let result = state.resources.register(registrant, &payload);
        if matches!(result, RegisterResourceProviderResult::Ok) {
            state.watch(ctx, registrant);
        }
        result
    }

    /// A provider's answer to a dispatched tool call.
    ///
    /// Correlated by the reply's own `in_reply_to`, which is the correlation
    /// ordinary Aether reply routing already echoes — never by the client's
    /// JSON-RPC identifier, which two concurrent POSTs may legally share.
    #[handler::manual]
    fn on_tool_invocation_result(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Manual>,
        result: ToolInvocationResult,
    ) {
        let Some(correlation) = ctx.in_reply_to() else {
            tracing::debug!(target: "aether_mcp::server", "tool result with no correlation dropped");
            return;
        };
        request::project_tool_result(state, correlation.0, &result);
    }

    /// A resource provider's answer to a dispatched read.
    #[handler::manual]
    fn on_read_resource_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, result: ReadResourceResult) {
        let Some(correlation) = ctx.in_reply_to() else {
            tracing::debug!(target: "aether_mcp::server", "resource result with no correlation dropped");
            return;
        };
        request::project_resource_result(state, correlation.0, &result);
    }

    /// A dispatched chain drained without the provider ever replying.
    ///
    /// The dispatch is a detached root precisely so this is distinguishable
    /// from a reply: an inherited send would fold into the request's own chain
    /// and this notice would never arrive on its own.
    #[handler::single]
    fn on_settled(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, settled: Settled) {
        request::settle_without_reply(state, settled.root.correlation_id);
    }

    /// One armed deadline came due.
    #[handler::single]
    fn on_request_deadline_elapsed(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, elapsed: RequestDeadlineElapsed) {
        request::expire(state, elapsed.correlation_id, elapsed.generation);
    }

    /// A registration holder departed: release its claims.
    ///
    /// Descriptors survive with no live member. A catalog that shrank when a
    /// holder departed would contradict the `listChanged: false` this server
    /// advertises, and an ordinary actor replacement would look to a client
    /// like the tool being withdrawn.
    #[handler::single]
    fn on_monitor_notice(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, notice: MonitorNotice) {
        state.monitors.remove(&notice.target);
        state.tools.purge(notice.target);
        state.resources.purge(notice.target);
    }
}
