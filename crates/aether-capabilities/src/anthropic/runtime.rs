//! The `aether.anthropic` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration in
//! the parent carries the gate), so a transport-only build of the
//! `AnthropicCapability` identity never names these types nor pulls
//! `aether_substrate`. The substrate-typed imports are gated once by this
//! module rather than line-by-line; the `#[actor] impl` reaches the state, ctx
//! types, gate/reply helpers, and reply assembly through the single
//! `use runtime::*` glob in the parent.

use super::kinds::{AnthropicError, Message, MessagesSendResult, Role};
use super::{
    AnthropicAdapter, AnthropicConfig, CliSendResult, CombinedAnthropicAdapter,
    DisabledAnthropicAdapter, map_adapter_error,
};
use crate::shared::contentgen::adapter::{AdapterUsage, AnthropicResponse};

pub use crate::shared::contentgen::task_queue::TaskQueue;
pub use std::sync::Arc;

use aether_actor::OutboundReply;
use aether_kinds::Usage;

pub use aether_actor::Manual;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
pub use aether_substrate::chassis::error::BootError;

/// Which send path a request rode. The generate handler threads it
/// into the worker closure to pick the blocking call + result kind.
#[derive(Copy, Clone)]
pub enum SendPath {
    Messages,
    Cli,
}

/// `aether.anthropic` runtime state (ADR-0050). Owns the resolved adapter
/// and the cap-level rate-limit queue over the ADR-0093 dispatch
/// primitive. Single-threaded post-ADR-0038, so the queue state lives in
/// plain fields with no lock. The dispatcher holds this as the cap's state
/// and routes envelopes through the macro-emitted `Dispatch` impl; the
/// addressing identity is the distinct ZST
/// [`AnthropicCapability`](super::AnthropicCapability). Living in this
/// private module keeps it `pub`-enough to satisfy the `NativeActor::State`
/// interface without exposing it as crate-public API.
pub struct AnthropicCapabilityState {
    pub(super) adapter: Arc<dyn AnthropicAdapter>,
    pub(super) tasks: TaskQueue,
}

#[cfg(test)]
impl AnthropicCapabilityState {
    /// Test-only constructor. Production boots through
    /// `Builder::with_actor::<AnthropicCapability>(config)`; tests
    /// hand in a stub adapter directly.
    pub(crate) fn from_parts(adapter: Arc<dyn AnthropicAdapter>, max_in_flight: usize) -> Self {
        Self {
            adapter,
            tasks: TaskQueue::new(max_in_flight),
        }
    }

    /// White-box accessor for tests asserting the queue's in-flight
    /// counter (e.g. that a synchronous validation error never spawned
    /// work).
    pub(crate) fn test_in_flight(&self) -> usize {
        self.tasks.in_flight()
    }
}

impl AnthropicCapabilityState {
    /// Gate a request's model before any dispatch. Returns `false`
    /// (after replying `UnknownModel`) when the Messages path's
    /// supported-model table rejects it; `true` to proceed. Empty
    /// `supported` = accept-any (disabled / CLI passthrough); the CLI
    /// path always passes through.
    pub fn gate_model(
        &self,
        ctx: &mut NativeCtx<'_, Manual>,
        path: SendPath,
        request_id: u64,
        model: &str,
    ) -> bool {
        let supported = self.adapter.supported_models();
        let gate = matches!(path, SendPath::Messages) && !supported.is_empty();
        if gate && !supported.iter().any(|m| m == model) {
            let err = AnthropicError::UnknownModel {
                model: model.to_string(),
                supported,
            };
            Self::reply_err(ctx, path, request_id, err);
            return false;
        }
        true
    }

    /// Reply an `Err` synchronously (model validation failure)
    /// before any dispatch.
    pub fn reply_err(
        ctx: &mut NativeCtx<'_, Manual>,
        path: SendPath,
        request_id: u64,
        error: AnthropicError,
    ) {
        match path {
            SendPath::Messages => {
                OutboundReply::reply(ctx, &MessagesSendResult::Err { request_id, error });
            }
            SendPath::Cli => {
                OutboundReply::reply(ctx, &CliSendResult::Err { request_id, error });
            }
        }
    }
}

pub fn build_adapter(config: &AnthropicConfig) -> Arc<dyn AnthropicAdapter> {
    if config.disabled {
        tracing::info!(
            target: "aether_capabilities::anthropic",
            "anthropic adapter disabled — messages reply Unauthorized; cli still routes",
        );
        return Arc::new(DisabledAnthropicAdapter::new(config.timeout));
    }
    config.api_key.as_ref().map_or_else(
        || {
            tracing::info!(
                target: "aether_capabilities::anthropic",
                "ANTHROPIC_API_KEY unset — messages reply Unauthorized; cli still routes",
            );
            Arc::new(DisabledAnthropicAdapter::new(config.timeout)) as Arc<dyn AnthropicAdapter>
        },
        |key| {
            tracing::info!(
                target: "aether_capabilities::anthropic",
                "anthropic adapter configured (messages + cli)",
            );
            Arc::new(CombinedAnthropicAdapter::new(key.clone(), config.timeout))
                as Arc<dyn AnthropicAdapter>
        },
    )
}

/// Flatten the conversation into a single prompt string. v1 doesn't
/// model multi-turn API content; it concatenates the user/assistant
/// turns so the adapter sees one prompt.
pub fn flatten_prompt(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| {
            let speaker = match m.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
            };
            format!("{speaker}: {}", m.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn to_usage(u: AdapterUsage) -> Usage {
    Usage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        wall_clock_ms: u.wall_clock_ms,
        cost_micros: u.cost_micros,
    }
}

pub fn messages_reply(
    request_id: u64,
    result: Result<AnthropicResponse, String>,
) -> MessagesSendResult {
    match result {
        Ok(resp) => MessagesSendResult::Ok {
            request_id,
            text: resp.text,
            model_used: resp.model_used,
            usage: to_usage(resp.usage),
        },
        Err(raw) => MessagesSendResult::Err {
            request_id,
            error: map_adapter_error(&raw),
        },
    }
}

pub fn cli_reply(request_id: u64, result: Result<AnthropicResponse, String>) -> CliSendResult {
    match result {
        Ok(resp) => CliSendResult::Ok {
            request_id,
            text: resp.text,
            model_used: resp.model_used,
            usage: to_usage(resp.usage),
        },
        Err(raw) => CliSendResult::Err {
            request_id,
            error: map_adapter_error(&raw),
        },
    }
}
