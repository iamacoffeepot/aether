//! The guest `aether.anthropic` component (ADR-0159).
//!
//! A wasm actor that keeps the four anthropic wire kinds byte-identical
//! (`aether.anthropic.messages.send` / `aether.anthropic.cli.send` and their
//! `_result` replies) and dispatches their I/O as mail to the native edge
//! capabilities: the Messages backend rides `aether.http.fetch` (ADR-0158), the
//! `claude` CLI backend rides `aether.process.run` (ADR-0157). The pure request
//! building, response parsing, and error mapping port unchanged from the native
//! cap ([`api`], [`error`]).
//!
//! # State machine (ADR-0139 `send_with_context` / `take_context`)
//!
//! Each request/reply flow is two handlers, the shape `aether.kit.mesh` runs:
//!
//! - The request handler (`on_messages_send`, `on_cli_send`) validates,
//!   builds the edge request, stashes a [`RequestContext`] carrying the
//!   original caller, and `send_with_context`s the edge request. A synchronous
//!   rejection (disabled / no key / unknown model / unsupported CLI knob)
//!   replies immediately and dispatches nothing.
//! - The reply handler (`on_fetch_result`, `on_run_result`) recovers the
//!   context with `take_context`, runs the pure parser + error mapping, and
//!   replies the provider `_result` kind to the original caller.
//!
//! The guest queues no pending work of its own — it submits each edge request
//! immediately and lets the edge's per-sender bound (ADR-0158) throttle,
//! keeping settlement engine-side and settlement-correct (ADR-0159 §4).

mod api;
mod config;
mod error;

pub use config::{AnthropicComponentConfig, DEFAULT_CLI_BINARY};
use config::{RequestContext, SendPath};

use aether_actor::{ActorInitError, Manual, OutboundReply, ReplyHandle, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_http::{Fetch, FetchResult, HttpCapability, HttpHeader, HttpMethod};
use aether_process::{ProcessCapability, Run, RunResult};

use aether_kinds::Usage;

use crate::kinds::{AnthropicError, CliSend, CliSendResult, Message, MessagesSend, MessagesSendResult, Role};

/// Official Messages API endpoint. Ported from the native backend.
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

/// Anthropic API version header value. Pinned per the public Messages API
/// contract; bump when the component is verified against a newer version.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Models the Messages backend accepts, validated before any dispatch — a miss
/// replies `UnknownModel` synchronously. The CLI backend passes the model
/// through to `claude` and does not gate. Ported from the native cap's
/// `SUPPORTED_MESSAGES_MODELS`; pinned to the 2026-05 lineup, bump as models
/// ship.
const SUPPORTED_MESSAGES_MODELS: &[&str] = &["claude-opus-4-7", "claude-sonnet-4-6", "claude-haiku-4-5-20251001"];

/// Guest `aether.anthropic` component. Holds only its resolved init-config; the
/// per-request state rides the ADR-0139 request contexts, never the actor.
pub struct AnthropicComponent {
    config: AnthropicComponentConfig,
}

/// `aether.anthropic` guest component.
///
/// # Agent
/// `load_component` this binary with an `AnthropicComponentConfig` (the API key
/// + timeout + CLI binary name), then send `aether.anthropic.messages.send`
/// (HTTPS via `aether.http`) or `aether.anthropic.cli.send` (the `claude`
/// subprocess via `aether.process`) to its loaded address and await the
/// matching `_result` reply. The Messages backend needs `aether.http` egress to
/// the Messages API host allowlisted; the CLI backend needs `claude`
/// allowlisted on `aether.process`.
#[actor]
impl WasmActor for AnthropicComponent {
    type Config = AnthropicComponentConfig;
    const NAMESPACE: &'static str = "aether.anthropic";

    fn init(config: AnthropicComponentConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { config })
    }

    /// Request a Messages-API completion over `aether.http.fetch`.
    ///
    /// # Agent
    /// Reply: `MessagesSendResult`. Replies `Err { Unauthorized }`
    /// synchronously when the component has no key (or is disabled), and
    /// `Err { UnknownModel }` when `model` is outside the supported table —
    /// neither dispatches a fetch. Otherwise submits the fetch immediately; the
    /// reply lands when the edge round-trip settles.
    #[handler::manual]
    fn on_messages_send(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: MessagesSend) {
        let reply = ctx.reply_target();
        let request_id = mail.request_id;

        if self.config.disabled || self.config.api_key.is_none() {
            Self::reply_messages(ctx, reply, request_id, Err(AnthropicError::Unauthorized));
            return;
        }
        if !SUPPORTED_MESSAGES_MODELS.contains(&mail.model.as_str()) {
            let error = AnthropicError::UnknownModel {
                model: mail.model,
                supported: SUPPORTED_MESSAGES_MODELS.iter().map(|m| (*m).to_string()).collect(),
            };
            Self::reply_messages(ctx, reply, request_id, Err(error));
            return;
        }

        let body = api::build_request_body(
            &mail.model,
            &flatten_prompt(&mail.messages),
            mail.system.as_deref(),
            mail.max_tokens,
            mail.temperature,
        );
        let body_bytes = match serde_json::to_vec(&body) {
            Ok(bytes) => bytes,
            Err(e) => {
                Self::reply_messages(
                    ctx,
                    reply,
                    request_id,
                    Err(AnthropicError::AdapterError(format!("encode request: {e}"))),
                );
                return;
            }
        };

        let fetch = Fetch {
            request_id,
            url: MESSAGES_URL.to_string(),
            method: HttpMethod::Post,
            headers: self.messages_headers(),
            body: body_bytes,
            timeout_ms: self.timeout_ms(),
        };
        let context = RequestContext {
            reply,
            path: SendPath::Messages,
            request_id,
            model: mail.model,
            timeout_millis: self.config.timeout_millis,
        };
        let _ = ctx.actor::<HttpCapability>().send_with_context(&fetch, &context);
    }

    /// Request a completion through the `claude` subprocess via
    /// `aether.process.run`.
    ///
    /// # Agent
    /// Reply: `CliSendResult`. The `claude` CLI exposes no `--max-tokens` /
    /// `--temperature` flag, so setting either replies
    /// `Err { ParamNotSupported }` synchronously (no run) — route sampling
    /// knobs through `aether.anthropic.messages.send`. The CLI uses the user's
    /// subscription, so it works with no API key; an allowlist that omits the
    /// CLI binary yields `Err { CliNotFound }`.
    #[handler::manual]
    fn on_cli_send(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: CliSend) {
        let reply = ctx.reply_target();
        let request_id = mail.request_id;

        let mut unsupported = Vec::new();
        if mail.max_tokens.is_some() {
            unsupported.push("max_tokens");
        }
        if mail.temperature.is_some() {
            unsupported.push("temperature");
        }
        if !unsupported.is_empty() {
            let error = AnthropicError::ParamNotSupported {
                param: unsupported.join(", "),
                reason: "the claude CLI has no flag for this; use aether.anthropic.messages.send".to_string(),
            };
            Self::reply_cli(ctx, reply, request_id, Err(error));
            return;
        }

        let mut args = vec!["--print".to_string(), "--model".to_string(), mail.model.clone()];
        if let Some(system) = &mail.system {
            args.push("--system-prompt".to_string());
            args.push(system.clone());
        }
        let run = Run {
            binary: self.config.cli_binary.clone(),
            args,
            env: Vec::new(),
            stdin: flatten_prompt(&mail.messages).into_bytes(),
            timeout_millis: self.config.timeout_millis,
        };
        let context = RequestContext {
            reply,
            path: SendPath::Cli,
            request_id,
            model: mail.model,
            timeout_millis: self.config.timeout_millis,
        };
        let _ = ctx.actor::<ProcessCapability>().send_with_context(&run, &context);
    }

    /// Recover the Messages request context and reply the parsed completion (or
    /// mapped error) to the original caller.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually.
    // The reply handlers reply from the recovered request context, not actor
    // state, so they read no `self`; the `&mut self` is the dispatch ABI.
    #[allow(clippy::unused_self)]
    #[handler::manual]
    fn on_fetch_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: FetchResult) {
        let Some(context) = ctx.take_context::<RequestContext>() else {
            return;
        };
        let outcome = match result {
            FetchResult::Ok { status, headers, body, .. } => {
                let text = String::from_utf8_lossy(&body);
                if (200..300).contains(&status) {
                    api::parse_messages_response(&text, &context.model)
                        .map(|parsed| (parsed.text, parsed.model_used, parsed.usage))
                        .map_err(AnthropicError::AdapterError)
                } else {
                    Err(error::status_to_error(status, error::retry_after_millis(&headers), &text))
                }
            }
            FetchResult::Err { error, .. } => Err(error::http_error_to_typed(error)),
        };
        Self::reply_messages(ctx, context.reply, context.request_id, outcome);
    }

    /// Recover the CLI request context and reply the completion (or mapped
    /// error) to the original caller, re-folding a non-zero `claude` exit —
    /// which arrives as `RunResult::Ok` per ADR-0157 — into a provider error.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually.
    #[allow(clippy::unused_self)]
    #[handler::manual]
    fn on_run_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: RunResult) {
        let Some(context) = ctx.take_context::<RequestContext>() else {
            return;
        };
        let outcome = match result {
            RunResult::Ok { exit_code, stdout, stderr } => {
                if exit_code == Some(0) {
                    let text = String::from_utf8_lossy(&stdout).trim().to_string();
                    Ok((text, context.model.clone(), cli_usage()))
                } else {
                    // ADR-0157 divergence: a non-zero exit is a completed run
                    // (`Ok`), not a process-cap failure, so the guest re-folds
                    // it into the provider taxonomy the native `cli.rs` did.
                    let code = exit_code.map_or_else(|| "signal".to_string(), |c| format!("code {c}"));
                    let stderr = String::from_utf8_lossy(&stderr);
                    Err(AnthropicError::AdapterError(format!(
                        "claude exited with {code}: {}",
                        error::snippet(stderr.trim())
                    )))
                }
            }
            RunResult::TimedOut { .. } => Err(AnthropicError::Timeout { elapsed_millis: context.timeout_millis }),
            RunResult::Err { error } => Err(error::process_error_to_typed(error)),
        };
        Self::reply_cli(ctx, context.reply, context.request_id, outcome);
    }
}

impl AnthropicComponent {
    /// The Messages-API request headers built from init-config: the `x-api-key`
    /// (present only in the enabled path this is reached from), the pinned
    /// `anthropic-version`, `content-type`, and a `user-agent`.
    fn messages_headers(&self) -> Vec<HttpHeader> {
        let mut headers = vec![
            HttpHeader { name: "anthropic-version".to_string(), value: ANTHROPIC_VERSION.to_string() },
            HttpHeader { name: "content-type".to_string(), value: "application/json".to_string() },
            HttpHeader {
                name: "user-agent".to_string(),
                value: concat!("aether/", env!("CARGO_PKG_VERSION")).to_string(),
            },
        ];
        if let Some(key) = &self.config.api_key {
            headers.push(HttpHeader { name: "x-api-key".to_string(), value: key.clone() });
        }
        headers
    }

    /// The per-request fetch timeout: the configured value, or `None` (the http
    /// cap's default) when the config carries `0`.
    fn timeout_ms(&self) -> Option<u32> {
        (self.config.timeout_millis > 0).then_some(self.config.timeout_millis)
    }

    /// Reply a `MessagesSendResult` to the original caller, if one awaits.
    fn reply_messages(
        ctx: &mut WasmCtx<'_, Manual>,
        reply: Option<ReplyHandle>,
        request_id: u64,
        outcome: Result<(String, String, Usage), AnthropicError>,
    ) {
        let Some(reply) = reply else {
            return;
        };
        let result = match outcome {
            Ok((text, model_used, usage)) => MessagesSendResult::Ok { request_id, text, model_used, usage },
            Err(error) => MessagesSendResult::Err { request_id, error },
        };
        ctx.reply_to(reply, &result);
    }

    /// Reply a `CliSendResult` to the original caller, if one awaits.
    fn reply_cli(
        ctx: &mut WasmCtx<'_, Manual>,
        reply: Option<ReplyHandle>,
        request_id: u64,
        outcome: Result<(String, String, Usage), AnthropicError>,
    ) {
        let Some(reply) = reply else {
            return;
        };
        let result = match outcome {
            Ok((text, model_used, usage)) => CliSendResult::Ok { request_id, text, model_used, usage },
            Err(error) => CliSendResult::Err { request_id, error },
        };
        ctx.reply_to(reply, &result);
    }
}

/// Flatten the conversation into a single prompt string. Ported from the native
/// cap — v1 doesn't model multi-turn API content, so it concatenates the
/// user/assistant turns into one prompt.
fn flatten_prompt(messages: &[Message]) -> String {
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

/// The `Usage` a CLI completion reports: no token counts (the subprocess
/// reports none) and no guest-side wall clock. Matches the native CLI backend's
/// zero-token accounting.
fn cli_usage() -> Usage {
    Usage { input_tokens: 0, output_tokens: 0, wall_clock_millis: 0, cost_micros: None }
}

#[cfg(test)]
mod tests {
    use super::flatten_prompt;
    use crate::kinds::{Message, Role};

    #[test]
    fn flatten_prompt_labels_speakers() {
        let messages = vec![
            Message { role: Role::User, content: "hi".to_string() },
            Message { role: Role::Assistant, content: "hello".to_string() },
        ];
        assert_eq!(flatten_prompt(&messages), "User: hi\nAssistant: hello");
    }
}
