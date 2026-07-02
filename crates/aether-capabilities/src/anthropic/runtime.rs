//! The `aether.anthropic` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration in
//! the parent carries the gate), so a transport-only build of the
//! `AnthropicCapability` identity never names these types nor pulls
//! `aether_substrate`. The substrate-typed imports are gated once by this
//! module rather than line-by-line; the `#[actor] impl` reaches the state, ctx
//! types, gate/reply helpers, and reply assembly through the single
//! `use runtime::*` glob in the parent.

use super::kinds::{AnthropicError, CliSend, Message, MessagesSend, MessagesSendResult, Role};
use super::{
    AnthropicAdapter, AnthropicCapability, AnthropicConfig, CliSendResult,
    CombinedAnthropicAdapter, DisabledAnthropicAdapter, map_adapter_error,
};
use crate::shared::contentgen::adapter::{AdapterUsage, AnthropicRequest, AnthropicResponse};

pub use crate::shared::contentgen::task_queue::TaskQueue;
pub use std::sync::Arc;

use aether_actor::OutboundReply;
use aether_actor::runtime;
use aether_kinds::Usage;

pub use aether_actor::Manual;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
pub use aether_substrate::chassis::error::BootError;

/// Which send path a request rode. The generate handler threads it
/// into the worker closure to pick the blocking call + result kind.
#[derive(Copy, Clone)]
enum SendPath {
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
    adapter: Arc<dyn AnthropicAdapter>,
    tasks: TaskQueue,
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
    fn gate_model(
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
    fn reply_err(
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

#[runtime]
impl NativeActor for AnthropicCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// state-bearing struct holding the adapter + the rate-limit queue.
    type State = AnthropicCapabilityState;

    type Config = AnthropicConfig;

    /// ADR-0050 + ADR-0074 Phase 5 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.anthropic";

    /// Build the adapter from the resolved config and capture the
    /// mailer + own mailbox so the spawn-and-die helper can land
    /// loopback result mails. The adapter is built immediately so a
    /// key-absent boot still loads (replying Unauthorized) rather
    /// than warn-dropping.
    fn init(
        config: AnthropicConfig,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<AnthropicCapabilityState, BootError> {
        Ok(AnthropicCapabilityState {
            adapter: build_adapter(&config),
            tasks: TaskQueue::new(config.max_in_flight),
        })
    }

    /// Run a Messages-API completion off the dispatcher thread.
    ///
    /// # Agent
    /// Reply: `MessagesSendResult`. Validates `model` against the
    /// supported table synchronously (`UnknownModel` on a miss),
    /// then dispatches the blocking HTTPS call on an ephemeral
    /// thread; the reply lands when the call returns.
    #[handler::manual]
    fn on_messages_send(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Manual>,
        mail: MessagesSend,
    ) {
        let request_id = mail.request_id;
        if !state.gate_model(ctx, SendPath::Messages, request_id, &mail.model) {
            return;
        }
        let req = AnthropicRequest {
            prompt: flatten_prompt(&mail.messages),
            model: mail.model,
            system: mail.system,
            max_tokens: mail.max_tokens,
            temperature: mail.temperature,
        };
        let adapter = Arc::clone(&state.adapter);
        state.tasks.submit(ctx, move || {
            let result = adapter.messages_send(req);
            messages_reply(request_id, result)
        });
    }

    /// Run a `claude`-subprocess completion off the dispatcher
    /// thread.
    ///
    /// # Agent
    /// Reply: `CliSendResult`. Replies `Err { CliNotFound }` when
    /// `claude` isn't on PATH. The CLI uses the user's subscription,
    /// so it works even when `ANTHROPIC_API_KEY` is unset. The
    /// `claude` binary exposes no `--max-tokens` / `--temperature`
    /// flag, so setting either replies `Err { ParamNotSupported }`
    /// synchronously (no dispatch) rather than silently dropping it —
    /// route sampling knobs through `aether.anthropic.messages.send`.
    #[handler::manual]
    fn on_cli_send(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: CliSend) {
        // The `claude` CLI has no flag for either knob; reject when
        // set instead of silently dropping (the outcome to avoid —
        // `feedback_explicit_nulls_over_absent_fields`).
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
                reason: "the claude CLI has no flag for this; use aether.anthropic.messages.send"
                    .to_string(),
            };
            AnthropicCapabilityState::reply_err(ctx, SendPath::Cli, mail.request_id, error);
            return;
        }
        let request_id = mail.request_id;
        // CLI passes the model through to `claude` (no gate), so no
        // `gate_model` call here.
        let req = AnthropicRequest {
            prompt: flatten_prompt(&mail.messages),
            model: mail.model,
            system: mail.system,
            max_tokens: mail.max_tokens,
            temperature: mail.temperature,
        };
        let adapter = Arc::clone(&state.adapter);
        state.tasks.submit(ctx, move || {
            let result = adapter.cli_send(req);
            cli_reply(request_id, result)
        });
    }

    /// ADR-0093 completion for a finished Messages call: re-reply the
    /// worker's result to the original caller (drops the hold), then
    /// free the in-flight slot (draining the next pending request).
    #[handler(task)]
    fn on_messages_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<MessagesSendResult>,
    ) {
        done.resolve(ctx);
        state.tasks.on_complete(ctx);
    }

    /// ADR-0093 completion for a finished CLI call.
    #[handler(task)]
    fn on_cli_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<CliSendResult>,
    ) {
        done.resolve(ctx);
        state.tasks.on_complete(ctx);
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
        wall_clock_millis: u.wall_clock_millis,
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

#[cfg(all(test, feature = "runtime"))]
mod tests {
    use super::AnthropicCapabilityState;
    use crate::anthropic::{AnthropicAdapter, ClaudeCliAdapter, DisabledAnthropicAdapter};
    use crate::anthropic::{
        AnthropicCapability, AnthropicError, CliSend, CliSendResult, Message, MessagesSend,
        MessagesSendResult, Role,
    };
    use crate::shared::contentgen::adapter::{
        AnthropicRequest, AnthropicResponse, StubAnthropicAdapter,
    };
    use crate::test_chassis::{decode_session_reply, drive_task_completion, test_mailer_and_rx};
    use aether_data::{Kind, MailId, MailboxId, SessionToken, Source, SourceAddr, Uuid};
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::actor::native::ctx::NativeCtx;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::EgressEvent;
    use serde::de::DeserializeOwned;
    use std::sync::Arc;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    fn session_sender() -> Source {
        Source::to(SourceAddr::Session(SessionToken(Uuid::nil())))
    }

    fn user_msg(text: &str) -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: text.to_string(),
        }]
    }

    /// Thin alias over the shared `decode_session_reply` so call
    /// sites stay terse.
    fn decode_reply<K: Kind + DeserializeOwned>(rx: &Receiver<EgressEvent>) -> K {
        decode_session_reply(rx)
    }

    /// Adapter that records the prompt it saw and returns canned text.
    struct RecordingStub {
        inner: StubAnthropicAdapter,
    }

    impl AnthropicAdapter for RecordingStub {
        fn messages_send(&self, req: AnthropicRequest) -> Result<AnthropicResponse, String> {
            self.inner.messages_send(req)
        }
        fn cli_send(&self, req: AnthropicRequest) -> Result<AnthropicResponse, String> {
            self.inner.cli_send(req)
        }
        fn supported_models(&self) -> Vec<String> {
            vec!["claude-test".to_string()]
        }
    }

    /// Drive a stub Messages request end-to-end through the ADR-0093
    /// dispatch primitive: the cap submits to the `TaskQueue`, the real
    /// worker runs the stub call, pushes a completion wake, and the
    /// cap's `#[handler(task)]` re-replies the `Ok` to the caller.
    #[test]
    fn anthropic_stub_messages() {
        let (mailer, rx) = test_mailer_and_rx();
        let cap_mailbox = MailboxId(0);
        let mut state = AnthropicCapabilityState::from_parts(
            Arc::new(RecordingStub {
                inner: StubAnthropicAdapter::default(),
            }),
            4,
        );
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            cap_mailbox,
        ));
        let mut ctx =
            NativeCtx::new_dispatching(&transport, session_sender(), MailId::NONE, MailId::NONE);
        AnthropicCapability::on_messages_send(
            &mut state,
            &mut ctx,
            MessagesSend {
                request_id: 7,
                model: "claude-test".to_string(),
                messages: user_msg("hi"),
                max_tokens: Some(8),
                temperature: None,
                system: None,
            },
        );
        // The worker runs the stub call and pushes the completion wake;
        // route it through the cap's task handler.
        drive_task_completion::<AnthropicCapability>(&mut state, &transport, &rx);
        match decode_reply::<MessagesSendResult>(&rx) {
            MessagesSendResult::Ok {
                request_id, text, ..
            } => {
                assert_eq!(request_id, 7);
                assert_eq!(text, "stub completion");
            }
            other @ MessagesSendResult::Err { .. } => panic!("expected Ok, got {other:?}"),
        }
    }

    /// Unknown model errors synchronously, before any dispatch.
    #[test]
    fn anthropic_unknown_model_errors() {
        let (mailer, rx) = test_mailer_and_rx();
        let cap_mailbox = MailboxId(0);
        let mut state = AnthropicCapabilityState::from_parts(
            Arc::new(RecordingStub {
                inner: StubAnthropicAdapter::default(),
            }),
            4,
        );
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            cap_mailbox,
        ));
        let mut ctx =
            NativeCtx::new_dispatching(&transport, session_sender(), MailId::NONE, MailId::NONE);
        AnthropicCapability::on_messages_send(
            &mut state,
            &mut ctx,
            MessagesSend {
                request_id: 3,
                model: "claude-bogus".to_string(),
                messages: user_msg("hi"),
                max_tokens: None,
                temperature: None,
                system: None,
            },
        );
        match decode_reply::<MessagesSendResult>(&rx) {
            MessagesSendResult::Err {
                request_id,
                error: AnthropicError::UnknownModel { model, supported },
            } => {
                assert_eq!(request_id, 3);
                assert_eq!(model, "claude-bogus");
                assert!(supported.contains(&"claude-test".to_string()));
            }
            other => panic!("expected UnknownModel, got {other:?}"),
        }
        // No in-flight work was spawned — the synchronous error path
        // never touched the dispatch helper.
        assert_eq!(cap_in_flight(&state), 0);
    }

    /// Boot a cap against the recording stub and fire a `CliSend`
    /// carrying the given knobs at `on_cli_send`, returning the state so
    /// the caller can assert `test_in_flight()`. The reply lands on
    /// the `mailer`'s loopback rx (held separately by the caller).
    fn cli_send_with(
        mailer: &Arc<Mailer>,
        max_tokens: Option<u32>,
        temperature: Option<f32>,
    ) -> AnthropicCapabilityState {
        let cap_mailbox = MailboxId(0);
        let mut state = AnthropicCapabilityState::from_parts(
            Arc::new(RecordingStub {
                inner: StubAnthropicAdapter::default(),
            }),
            4,
        );
        let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(mailer), cap_mailbox));
        let mut ctx =
            NativeCtx::new_dispatching(&transport, session_sender(), MailId::NONE, MailId::NONE);
        AnthropicCapability::on_cli_send(
            &mut state,
            &mut ctx,
            CliSend {
                request_id: 11,
                model: "claude-test".to_string(),
                messages: user_msg("hi"),
                max_tokens,
                temperature,
                system: None,
            },
        );
        state
    }

    /// `on_cli_send` with `max_tokens` set replies
    /// `Err { ParamNotSupported }` synchronously and spawns no work —
    /// the `claude` CLI has no flag to honor it.
    #[test]
    fn anthropic_cli_rejects_max_tokens() {
        let (mailer, rx) = test_mailer_and_rx();
        let state = cli_send_with(&mailer, Some(256), None);
        match decode_reply::<CliSendResult>(&rx) {
            CliSendResult::Err {
                request_id,
                error: AnthropicError::ParamNotSupported { param, reason },
            } => {
                assert_eq!(request_id, 11);
                assert!(param.contains("max_tokens"), "param was {param:?}");
                assert!(reason.contains("messages.send"), "reason was {reason:?}");
            }
            other => panic!("expected ParamNotSupported, got {other:?}"),
        }
        // Synchronous error path never touched the dispatch helper.
        assert_eq!(cap_in_flight(&state), 0);
    }

    /// `on_cli_send` with `temperature` set replies
    /// `Err { ParamNotSupported }` synchronously and spawns no work.
    #[test]
    fn anthropic_cli_rejects_temperature() {
        let (mailer, rx) = test_mailer_and_rx();
        let state = cli_send_with(&mailer, None, Some(0.7));
        match decode_reply::<CliSendResult>(&rx) {
            CliSendResult::Err {
                request_id,
                error: AnthropicError::ParamNotSupported { param, .. },
            } => {
                assert_eq!(request_id, 11);
                assert!(param.contains("temperature"), "param was {param:?}");
            }
            other => panic!("expected ParamNotSupported, got {other:?}"),
        }
        assert_eq!(cap_in_flight(&state), 0);
    }

    /// A `CliSend` with both knobs `None` dispatches normally — the
    /// synchronous reject path is skipped and work is spawned.
    #[test]
    fn anthropic_cli_no_params_dispatches() {
        let (mailer, _rx) = test_mailer_and_rx();
        let state = cli_send_with(&mailer, None, None);
        assert_eq!(
            cap_in_flight(&state),
            1,
            "a param-free CliSend should dispatch one in-flight call"
        );
    }

    /// CLI send with a missing `claude` binary replies
    /// `Err { CliNotFound }` — a graceful skip, not a hang.
    #[test]
    fn anthropic_cli_skips_when_no_claude_on_path() {
        use crate::anthropic::error::UNAUTHORIZED_SENTINEL;
        // A disabled adapter routes CLI through the real subprocess
        // backend; pointing it at a missing binary exercises the
        // CliNotFound path without depending on the host's `claude`.
        struct MissingCliAdapter {
            cli: ClaudeCliAdapter,
        }
        impl AnthropicAdapter for MissingCliAdapter {
            fn messages_send(&self, _req: AnthropicRequest) -> Result<AnthropicResponse, String> {
                Err(UNAUTHORIZED_SENTINEL.to_string())
            }
            fn cli_send(&self, req: AnthropicRequest) -> Result<AnthropicResponse, String> {
                self.cli.cli_send(&req)
            }
        }

        let (mailer, rx) = test_mailer_and_rx();
        let cap_mailbox = MailboxId(0);
        let mut state = AnthropicCapabilityState::from_parts(
            Arc::new(MissingCliAdapter {
                cli: ClaudeCliAdapter::new(
                    "aether-nonexistent-claude-binary-xyzzy".to_string(),
                    Duration::from_secs(30),
                ),
            }),
            4,
        );
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            cap_mailbox,
        ));
        let mut ctx =
            NativeCtx::new_dispatching(&transport, session_sender(), MailId::NONE, MailId::NONE);
        AnthropicCapability::on_cli_send(
            &mut state,
            &mut ctx,
            CliSend {
                request_id: 5,
                model: "claude-test".to_string(),
                messages: user_msg("hi"),
                max_tokens: None,
                temperature: None,
                system: None,
            },
        );
        // The CLI backend runs on the real worker against a missing
        // binary, yielding CliNotFound; route the completion through the
        // cap's task handler.
        drive_task_completion::<AnthropicCapability>(&mut state, &transport, &rx);
        match decode_reply::<CliSendResult>(&rx) {
            CliSendResult::Err {
                request_id,
                error: AnthropicError::CliNotFound,
            } => {
                assert_eq!(request_id, 5);
            }
            other => panic!("expected CliNotFound, got {other:?}"),
        }
    }

    /// Disabled adapter replies `Unauthorized` to a Messages request.
    #[test]
    fn anthropic_disabled_messages_replies_unauthorized() {
        let (mailer, rx) = test_mailer_and_rx();
        let cap_mailbox = MailboxId(0);
        let mut state =
            AnthropicCapabilityState::from_parts(Arc::new(DisabledAnthropicAdapter::default()), 4);
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            cap_mailbox,
        ));
        let mut ctx =
            NativeCtx::new_dispatching(&transport, session_sender(), MailId::NONE, MailId::NONE);
        AnthropicCapability::on_messages_send(
            &mut state,
            &mut ctx,
            MessagesSend {
                request_id: 9,
                model: "claude-anything".to_string(),
                messages: user_msg("hi"),
                max_tokens: None,
                temperature: None,
                system: None,
            },
        );
        // Disabled adapter has an empty supported-models table, so the
        // model gate is skipped and the request dispatches; the worker
        // produces the Unauthorized result. Route the completion through
        // the cap's task handler.
        drive_task_completion::<AnthropicCapability>(&mut state, &transport, &rx);
        match decode_reply::<MessagesSendResult>(&rx) {
            MessagesSendResult::Err {
                request_id,
                error: AnthropicError::Unauthorized,
            } => assert_eq!(request_id, 9),
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    /// Real-API smoke. Hits the live Messages API with a tiny
    /// 5-`max_tokens` request — ignored by default so CI stays
    /// zero-cost; run with `ANTHROPIC_API_KEY` set.
    #[test]
    #[ignore = "needs ANTHROPIC_API_KEY"]
    fn anthropic_api_smoke() {
        use crate::anthropic::UreqAnthropicAdapter;
        use crate::shared::contentgen::adapter::AnthropicRequest;
        use std::env;
        // Test-only: the live-API smoke reads an external credential
        // (ANTHROPIC_API_KEY), not cap config; gated `#[ignore]`.
        #[allow(clippy::disallowed_methods)]
        let key = env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY set for smoke");
        let adapter = UreqAnthropicAdapter::new(key, Duration::from_secs(30));
        let resp = adapter
            .messages_send(&AnthropicRequest {
                model: "claude-haiku-4-5-20251001".to_string(),
                prompt: "say hi".to_string(),
                system: None,
                max_tokens: Some(5),
                temperature: None,
            })
            .expect("live messages request succeeds");
        assert!(!resp.text.is_empty());
    }

    // White-box accessor for the queue's in-flight count; the state's
    // `tasks` field is private, so tests read it through this shim.
    fn cap_in_flight(state: &AnthropicCapabilityState) -> usize {
        state.test_in_flight()
    }
}
