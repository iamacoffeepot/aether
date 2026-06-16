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
//! [`TaskQueue`](crate::contentgen::TaskQueue), which hands it to
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

use std::time::Duration;

use crate::contentgen::adapter::{AnthropicAdapter, AnthropicRequest, AnthropicResponse};

use crate::config_env::DEFAULT_PROVIDER_MAX_IN_FLIGHT;
pub use api::UreqAnthropicAdapter;
pub use cli::ClaudeCliAdapter;
pub use config::{AnthropicConfig, AnthropicConfigLayer, AnthropicOverlay};

/// Default per-cap concurrency bound when `AETHER_ANTHROPIC_MAX_IN_FLIGHT`
/// is unset. Conservative — paid-endpoint throttling matters more than
/// throughput here.
pub const DEFAULT_MAX_IN_FLIGHT: usize = DEFAULT_PROVIDER_MAX_IN_FLIGHT;

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

mod config {
    use super::{DEFAULT_MAX_IN_FLIGHT, DEFAULT_TIMEOUT_MILLIS};
    // confique consumes these through `#[config(parse_env = …)]`; IntelliJ-Rust
    // doesn't trace macro-attr path args (Qodana FP), but rustc + clippy do.
    #[allow(unused_imports)]
    use crate::config_env::{parse_flag, parse_millis_strict, parse_provider_max_in_flight_strict};
    use std::time::Duration;

    /// Resolved configuration for the `aether.anthropic` cap. Chassis
    /// mains read env (`ANTHROPIC_API_KEY`, `AETHER_ANTHROPIC_DISABLE`,
    /// `AETHER_ANTHROPIC_MAX_IN_FLIGHT`, `AETHER_ANTHROPIC_TIMEOUT_MS`)
    /// into this and pass it to `with_actor::<AnthropicCapability>(cfg)`.
    /// Tests build it directly so they never read process env.
    ///
    /// ADR-0090 unit g (iamacoffeepot/aether#1264): the
    /// `#[derive(aether_substrate::Config)]` emits the env-shaped
    /// `AnthropicConfigLayer`, the clap-shaped `AnthropicOverlay`, the
    /// `FromArgvThenEnv` impl, and the inherent `from_env` shims under
    /// `feature = "native"`. The wasm-marker build carries only the
    /// domain struct.
    #[derive(Clone, Debug)]
    #[cfg_attr(feature = "native", derive(aether_substrate::Config))]
    #[cfg_attr(
        feature = "native",
        config(env_prefix = "AETHER_ANTHROPIC", cli_prefix = "anthropic")
    )]
    pub struct AnthropicConfig {
        /// The Messages-API key. `None` (or `disabled`) wires the
        /// `DisabledAnthropicAdapter` so Messages requests reply
        /// `Unauthorized` while the CLI path still works. `env`
        /// override pins the unprefixed `ANTHROPIC_API_KEY` key.
        #[cfg_attr(feature = "native", config(env = "ANTHROPIC_API_KEY"))]
        pub api_key: Option<String>,
        /// `AETHER_ANTHROPIC_DISABLE=1` forces the disabled adapter
        /// even when a key is present. `env` + `cli_long` overrides
        /// pin the historical wire shape (no `D` suffix on `DISABLE`).
        #[cfg_attr(
            feature = "native",
            config(
                env = "AETHER_ANTHROPIC_DISABLE",
                cli_long = "anthropic-disable",
                default = false,
                parse = parse_flag
            )
        )]
        pub disabled: bool,
        /// Per-cap concurrency bound (doubles as rate-limit throttling).
        #[cfg_attr(
            feature = "native",
            config(default = 2, parse = parse_provider_max_in_flight_strict)
        )]
        pub max_in_flight: usize,
        /// Per-request timeout for the Messages API. The derive's
        /// `ms_duration` hint + `layer_field = "timeout_ms"` pin the
        /// Layer / env / CLI shape to the pre-derive name
        /// (`AETHER_ANTHROPIC_TIMEOUT_MS`, `--anthropic-timeout-ms`).
        #[cfg_attr(
            feature = "native",
            config(
                default = 120_000,
                parse = parse_millis_strict::<DEFAULT_TIMEOUT_MILLIS>,
                ms_duration,
                layer_field = "timeout_ms"
            )
        )]
        pub timeout: Duration,
    }

    impl Default for AnthropicConfig {
        fn default() -> Self {
            Self {
                api_key: None,
                disabled: false,
                max_in_flight: DEFAULT_MAX_IN_FLIGHT,
                timeout: Duration::from_millis(u64::from(DEFAULT_TIMEOUT_MILLIS)),
            }
        }
    }

    #[cfg(all(test, feature = "native"))]
    mod tests {
        use super::{
            AnthropicConfig, AnthropicConfigLayer, DEFAULT_MAX_IN_FLIGHT, DEFAULT_TIMEOUT_MILLIS,
        };
        use confique::Config as _;
        use std::time::Duration;

        // ADR-0090: byte-identical to the prior hand-rolled reader,
        // exercised without touching process env (issue 464).

        #[test]
        fn anthropic_from_env_defaults_match() {
            // No `.env()` source: loads literal defaults only — env-free,
            // guards the layer defaults against the consts + default().
            let layer = AnthropicConfigLayer::builder()
                .load()
                .expect("defaults load");
            let default = AnthropicConfig::default();
            assert_eq!(layer.api_key, None);
            assert!(!layer.disabled);
            assert_eq!(layer.max_in_flight, DEFAULT_MAX_IN_FLIGHT);
            assert_eq!(layer.timeout_ms, DEFAULT_TIMEOUT_MILLIS);
            assert_eq!(
                Duration::from_millis(u64::from(layer.timeout_ms)),
                default.timeout
            );
        }
    }
}

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use aether_kinds::{CliSend, CliSendResult, MessagesSend, MessagesSendResult};

/// Convert an adapter error string into the typed `AnthropicError`.
/// Shared by both result paths.
fn map_adapter_error(raw: &str) -> aether_kinds::AnthropicError {
    error::adapter_error_to_typed(raw)
}

#[aether_actor::bridge(singleton)]
mod native {
    use std::sync::Arc;

    use super::{
        AnthropicAdapter, AnthropicConfig, AnthropicRequest, CliSend, CliSendResult,
        CombinedAnthropicAdapter, DisabledAnthropicAdapter, MessagesSend, MessagesSendResult,
        map_adapter_error,
    };
    use crate::contentgen::adapter::{AdapterUsage, AnthropicResponse};
    use crate::contentgen::task_queue::TaskQueue;
    use aether_actor::{Manual, OutboundReply, actor};
    use aether_kinds::{AnthropicError, Message, Role, Usage};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
    use aether_substrate::chassis::error::BootError;

    /// Which send path a request rode. The generate handler threads it
    /// into the worker closure to pick the blocking call + result kind.
    #[derive(Copy, Clone)]
    enum SendPath {
        Messages,
        Cli,
    }

    /// `aether.anthropic` mailbox cap. Owns the resolved adapter and the
    /// cap-level rate-limit queue over the ADR-0093 dispatch primitive.
    /// Single-threaded post-ADR-0038, so the queue state lives in plain
    /// fields with no lock.
    pub struct AnthropicCapability {
        adapter: Arc<dyn AnthropicAdapter>,
        tasks: TaskQueue,
    }

    #[cfg(test)]
    impl AnthropicCapability {
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

    fn build_adapter(config: &AnthropicConfig) -> Arc<dyn AnthropicAdapter> {
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

    impl AnthropicCapability {
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

    fn to_usage(u: AdapterUsage) -> Usage {
        Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            wall_clock_ms: u.wall_clock_ms,
            cost_micros: u.cost_micros,
        }
    }

    fn messages_reply(
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

    fn cli_reply(request_id: u64, result: Result<AnthropicResponse, String>) -> CliSendResult {
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

    #[actor]
    impl NativeActor for AnthropicCapability {
        type Config = AnthropicConfig;

        /// ADR-0050 + ADR-0074 Phase 5 chassis-owned mailbox.
        const NAMESPACE: &'static str = "aether.anthropic";

        /// Build the adapter from the resolved config and capture the
        /// mailer + own mailbox so the spawn-and-die helper can land
        /// loopback result mails. The adapter is built immediately so a
        /// key-absent boot still loads (replying Unauthorized) rather
        /// than warn-dropping.
        fn init(config: AnthropicConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self {
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
        fn on_messages_send(&mut self, ctx: &mut NativeCtx<'_, Manual>, mail: MessagesSend) {
            let request_id = mail.request_id;
            if !self.gate_model(ctx, SendPath::Messages, request_id, &mail.model) {
                return;
            }
            let req = AnthropicRequest {
                prompt: flatten_prompt(&mail.messages),
                model: mail.model,
                system: mail.system,
                max_tokens: mail.max_tokens,
                temperature: mail.temperature,
            };
            let adapter = Arc::clone(&self.adapter);
            self.tasks.submit(ctx, move || {
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
        fn on_cli_send(&mut self, ctx: &mut NativeCtx<'_, Manual>, mail: CliSend) {
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
                    reason:
                        "the claude CLI has no flag for this; use aether.anthropic.messages.send"
                            .to_string(),
                };
                Self::reply_err(ctx, SendPath::Cli, mail.request_id, error);
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
            let adapter = Arc::clone(&self.adapter);
            self.tasks.submit(ctx, move || {
                let result = adapter.cli_send(req);
                cli_reply(request_id, result)
            });
        }

        /// ADR-0093 completion for a finished Messages call: re-reply the
        /// worker's result to the original caller (drops the hold), then
        /// free the in-flight slot (draining the next pending request).
        #[handler(task)]
        fn on_messages_done(
            &mut self,
            ctx: &mut NativeCtx<'_>,
            done: TaskDone<MessagesSendResult>,
        ) {
            done.resolve(ctx);
            self.tasks.on_complete(ctx);
        }

        /// ADR-0093 completion for a finished CLI call.
        #[handler(task)]
        fn on_cli_done(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<CliSendResult>) {
            done.resolve(ctx);
            self.tasks.on_complete(ctx);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::{
            AnthropicAdapter, AnthropicConfig, ClaudeCliAdapter, DisabledAnthropicAdapter,
        };
        use super::AnthropicCapability;
        use crate::contentgen::adapter::{
            AnthropicRequest, AnthropicResponse, StubAnthropicAdapter,
        };
        use crate::test_chassis::{
            TestChassis, decode_session_reply, drive_task_completion, fresh_substrate,
            test_mailer_and_rx,
        };
        use aether_actor::Actor;
        use aether_data::{Kind, MailboxId, SessionToken, Source, SourceAddr, Uuid};
        use aether_kinds::{
            AnthropicError, CliSend, CliSendResult, Message, MessagesSend, MessagesSendResult, Role,
        };
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
            let mut cap = AnthropicCapability::from_parts(
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
            cap.on_messages_send(
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
            drive_task_completion(&mut cap, &transport, &rx);
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
            let mut cap = AnthropicCapability::from_parts(
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
            cap.on_messages_send(
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
            assert_eq!(cap_in_flight(&cap), 0);
        }

        /// Boot a cap against the recording stub and fire a `CliSend`
        /// carrying the given knobs at `on_cli_send`, returning the cap so
        /// the caller can assert `test_in_flight()`. The reply lands on
        /// the `mailer`'s loopback rx (held separately by the caller).
        fn cli_send_with(
            mailer: &Arc<Mailer>,
            max_tokens: Option<u32>,
            temperature: Option<f32>,
        ) -> AnthropicCapability {
            let cap_mailbox = MailboxId(0);
            let mut cap = AnthropicCapability::from_parts(
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
            cap.on_cli_send(
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
            cap
        }

        /// `on_cli_send` with `max_tokens` set replies
        /// `Err { ParamNotSupported }` synchronously and spawns no work —
        /// the `claude` CLI has no flag to honor it.
        #[test]
        fn anthropic_cli_rejects_max_tokens() {
            let (mailer, rx) = test_mailer_and_rx();
            let cap = cli_send_with(&mailer, Some(256), None);
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
            assert_eq!(cap_in_flight(&cap), 0);
        }

        /// `on_cli_send` with `temperature` set replies
        /// `Err { ParamNotSupported }` synchronously and spawns no work.
        #[test]
        fn anthropic_cli_rejects_temperature() {
            let (mailer, rx) = test_mailer_and_rx();
            let cap = cli_send_with(&mailer, None, Some(0.7));
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
            assert_eq!(cap_in_flight(&cap), 0);
        }

        /// A `CliSend` with both knobs `None` dispatches normally — the
        /// synchronous reject path is skipped and work is spawned.
        #[test]
        fn anthropic_cli_no_params_dispatches() {
            let (mailer, _rx) = test_mailer_and_rx();
            let cap = cli_send_with(&mailer, None, None);
            assert_eq!(
                cap_in_flight(&cap),
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
                fn messages_send(
                    &self,
                    _req: AnthropicRequest,
                ) -> Result<AnthropicResponse, String> {
                    Err(super::super::error::UNAUTHORIZED_SENTINEL.to_string())
                }
                fn cli_send(&self, req: AnthropicRequest) -> Result<AnthropicResponse, String> {
                    self.cli.cli_send(&req)
                }
            }

            let (mailer, rx) = test_mailer_and_rx();
            let cap_mailbox = MailboxId(0);
            let mut cap = AnthropicCapability::from_parts(
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
            cap.on_cli_send(
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
            drive_task_completion(&mut cap, &transport, &rx);
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
            let mut cap =
                AnthropicCapability::from_parts(Arc::new(DisabledAnthropicAdapter::default()), 4);
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
            cap.on_messages_send(
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
            drive_task_completion(&mut cap, &transport, &rx);
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
            use super::super::UreqAnthropicAdapter;
            use crate::contentgen::adapter::AnthropicRequest;
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

        // White-box accessor for the queue's in-flight count; the cap's
        // `tasks` field is private, so tests read it through this shim.
        fn cap_in_flight(cap: &AnthropicCapability) -> usize {
            cap.test_in_flight()
        }
    }
}
