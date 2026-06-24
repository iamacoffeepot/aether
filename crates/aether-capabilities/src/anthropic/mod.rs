//! `aether.anthropic` cap (ADR-0050). One chassis-owned mailbox
//! exposing two sibling text-completion request kinds —
//! `aether.anthropic.messages.send` (HTTPS to the official Messages
//! API) and `aether.anthropic.cli.send` (the local `claude` subprocess
//! against the user's subscription) — with identical input/output
//! schemas, the routing chosen by the kind name.
//!
//! Long-tail calls (a multi-second Messages request, a `claude`
//! subprocess) ride the ADR-0093 hold-until-resolve dispatch: the
//! generate handler submits the blocking call to a
//! [`TaskQueue`], which hands it to
//! `ctx.dispatch_blocking` — the substrate spawns an ephemeral worker,
//! holds the chain open in its in-flight ledger, and routes the
//! completion to the cap's `#[handler(task)]` as a `TaskDone`. The cap
//! holds only the queue's slot count + pending queue (the per-cap
//! concurrency bound) in its lock-free actor state — no `Semaphore`, no
//! `Mutex`.
//!
//! The kind is the caller-stable contract; the `AnthropicAdapter` is
//! the vendor-compat layer (ADR-0050 §4). Production wires
//! [`CombinedAnthropicAdapter`] (the `ureq` Messages backend +
//! `claude` subprocess backend); a key-absent boot wires
//! [`DisabledAnthropicAdapter`] so the mailbox still loads and replies
//! `Err { Unauthorized }` rather than warn-dropping. CI smokes wire the
//! `StubAnthropicAdapter` from issue 1013.

mod api;
mod cli;
mod error;
mod kinds;
pub use kinds::{
    AnthropicError, CliSend, CliSendResult, Message, MessagesSend, MessagesSendResult, Role,
};

use std::time::Duration;

use crate::shared::contentgen::adapter::{AnthropicAdapter, AnthropicRequest, AnthropicResponse};

pub use api::UreqAnthropicAdapter;
pub use cli::ClaudeCliAdapter;
pub use config::{AnthropicConfig, AnthropicConfigLayer, AnthropicOverlay};

/// Default per-cap concurrency bound when `AETHER_ANTHROPIC_MAX_IN_FLIGHT`
/// is unset. Conservative — paid-endpoint throttling matters more than
/// throughput here.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 2;

/// Default per-request timeout when `AETHER_ANTHROPIC_TIMEOUT_MS` is
/// unset. A long completion can run tens of seconds.
pub const DEFAULT_TIMEOUT_MILLIS: u32 = 120_000;

/// Models the Messages-API backend accepts. The cap validates a
/// request's `model` against this before any dispatch; the CLI backend
/// passes the model through to `claude` and doesn't gate (the CLI
/// validates). Pinned to the 2026-05 model lineup; bump as new models
/// ship.
const SUPPORTED_MESSAGES_MODELS: &[&str] = &[
    "claude-opus-4-7",
    "claude-sonnet-4-6",
    "claude-haiku-4-5-20251001",
];

/// Adapter returned when `ANTHROPIC_API_KEY` is unset (or
/// `AETHER_ANTHROPIC_DISABLE=1`). Messages requests reply
/// `Err { Unauthorized }`; the CLI path still works (it uses the
/// user's subscription, not the API key) so it falls through to the
/// real subprocess backend.
#[derive(Default)]
pub struct DisabledAnthropicAdapter {
    cli: ClaudeCliAdapter,
}

impl DisabledAnthropicAdapter {
    /// Build the disabled adapter with the CLI backend wired to the
    /// cap's per-request `timeout`. The default impl uses
    /// `DEFAULT_TIMEOUT_MILLIS`; production threads `config.timeout`.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self {
            cli: ClaudeCliAdapter::new(String::from("claude"), timeout),
        }
    }
}

impl AnthropicAdapter for DisabledAnthropicAdapter {
    fn messages_send(&self, _req: AnthropicRequest) -> Result<AnthropicResponse, String> {
        // The cap maps this sentinel onto `AnthropicError::Unauthorized`.
        Err(error::UNAUTHORIZED_SENTINEL.to_string())
    }

    fn cli_send(&self, req: AnthropicRequest) -> Result<AnthropicResponse, String> {
        self.cli.cli_send(&req)
    }
}

/// Production adapter: the `ureq` Messages backend for `messages.send`
/// plus the `claude` subprocess backend for `cli.send`.
pub struct CombinedAnthropicAdapter {
    messages: UreqAnthropicAdapter,
    cli: ClaudeCliAdapter,
}

impl CombinedAnthropicAdapter {
    /// Build the combined adapter with a resolved API key + timeout. The
    /// `timeout` bounds both the Messages HTTPS call and the `claude`
    /// subprocess deadline.
    #[must_use]
    pub fn new(api_key: String, timeout: Duration) -> Self {
        Self {
            messages: UreqAnthropicAdapter::new(api_key, timeout),
            cli: ClaudeCliAdapter::new(String::from("claude"), timeout),
        }
    }
}

impl AnthropicAdapter for CombinedAnthropicAdapter {
    fn messages_send(&self, req: AnthropicRequest) -> Result<AnthropicResponse, String> {
        self.messages.messages_send(&req)
    }

    fn cli_send(&self, req: AnthropicRequest) -> Result<AnthropicResponse, String> {
        self.cli.cli_send(&req)
    }

    fn supported_models(&self) -> Vec<String> {
        SUPPORTED_MESSAGES_MODELS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }
}

mod config;

/// Convert an adapter error string into the typed `AnthropicError`.
/// Shared by both result paths.
fn map_adapter_error(raw: &str) -> AnthropicError {
    error::adapter_error_to_typed(raw)
}

/// `aether.anthropic` mailbox cap **identity** (ADR-0122 identity/runtime
/// split). A ZST carrying only the addressing — `Addressable`
/// (`NAMESPACE`, `Resolver`), the per-handler `HandlesKind` markers, and
/// the name-inventory entry, all emitted always-on by `#[actor]`. The
/// state-bearing runtime (`AnthropicCapabilityState`,
/// which holds the `aether_substrate`-typed adapter + task queue) lives
/// behind the one `feature = "runtime"` gate, so a transport-only build
/// never names it nor pulls `aether_substrate` through this cap.
//
// Handler-signature kinds (`MessagesSend` / `CliSend` / their results)
// resolve at file root through the `pub use kinds::{…}` re-export above —
// `#[actor]` emits the always-on `impl HandlesKind<K>` markers against the
// identity, outside the `feature = "runtime"` gate, so they reference these
// kinds from here.
pub struct AnthropicCapability;

// The `#[actor]` / `#[handler]` attribute path stays always-on (the macro
// divides what it emits). Everything that names an `aether_substrate` type —
// the handler/init ctx, the runtime state, the gate/reply helpers, the reply
// assembly — lives in the `runtime` module, gated once by `feature = "runtime"`;
// the `#[actor] impl` reaches all of it through the single `use runtime::*` glob.
use aether_actor::actor;

// The `runtime` module is this cap's private runtime-half namespace; the impl
// reaches all of it (state, ctx types, gate/reply helpers) through this single
// seam, so the glob is intentional rather than a dozen one-line imports.
#[cfg(feature = "runtime")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

#[cfg(feature = "runtime")]
mod runtime;

#[actor(singleton)]
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

#[cfg(all(test, feature = "runtime"))]
mod tests {
    use super::runtime::AnthropicCapabilityState;
    use super::{AnthropicAdapter, AnthropicConfig, ClaudeCliAdapter, DisabledAnthropicAdapter};
    use super::{
        AnthropicCapability, AnthropicError, CliSend, CliSendResult, Message, MessagesSend,
        MessagesSendResult, Role,
    };
    use crate::shared::contentgen::adapter::{
        AnthropicRequest, AnthropicResponse, StubAnthropicAdapter,
    };
    use crate::test_chassis::{
        TestChassis, decode_session_reply, drive_task_completion, fresh_substrate,
        test_mailer_and_rx,
    };
    use aether_actor::Addressable;
    use aether_data::{Kind, MailboxId, SessionToken, Source, SourceAddr, Uuid};
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::actor::native::ctx::NativeCtx;
    use aether_substrate::chassis::builder::Builder;
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

    /// Boot the cap against a default (key-absent) config and confirm
    /// the mailbox registers.
    #[test]
    fn capability_boots_and_registers_mailbox() {
        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<AnthropicCapability>(AnthropicConfig::default())
            .build_passive()
            .expect("anthropic capability boots");
        assert!(
            registry.lookup(AnthropicCapability::NAMESPACE).is_some(),
            "anthropic mailbox registered"
        );
        drop(chassis);
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
        let mut ctx = NativeCtx::new_dispatching(
            &transport,
            session_sender(),
            aether_data::MailId::NONE,
            aether_data::MailId::NONE,
        );
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
        let mut ctx = NativeCtx::new_dispatching(
            &transport,
            session_sender(),
            aether_data::MailId::NONE,
            aether_data::MailId::NONE,
        );
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
        let mut ctx = NativeCtx::new_dispatching(
            &transport,
            session_sender(),
            aether_data::MailId::NONE,
            aether_data::MailId::NONE,
        );
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
        // A disabled adapter routes CLI through the real subprocess
        // backend; pointing it at a missing binary exercises the
        // CliNotFound path without depending on the host's `claude`.
        struct MissingCliAdapter {
            cli: ClaudeCliAdapter,
        }
        impl AnthropicAdapter for MissingCliAdapter {
            fn messages_send(&self, _req: AnthropicRequest) -> Result<AnthropicResponse, String> {
                Err(super::error::UNAUTHORIZED_SENTINEL.to_string())
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
        let mut ctx = NativeCtx::new_dispatching(
            &transport,
            session_sender(),
            aether_data::MailId::NONE,
            aether_data::MailId::NONE,
        );
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
        let mut ctx = NativeCtx::new_dispatching(
            &transport,
            session_sender(),
            aether_data::MailId::NONE,
            aether_data::MailId::NONE,
        );
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
        use super::UreqAnthropicAdapter;
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
