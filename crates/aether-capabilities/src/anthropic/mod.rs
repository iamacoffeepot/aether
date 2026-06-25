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
//! [`TaskQueue`](crate::shared::contentgen::TaskQueue), which hands it to
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
#[actor(singleton)]
pub struct AnthropicCapability;

// The `#[actor]` / `#[handler]` attribute path stays always-on (the macro
// divides what it emits). Everything that names an `aether_substrate` type —
// the handler/init ctx, the runtime state, the gate/reply helpers, the reply
// assembly — lives in the `runtime` module, gated once by `feature = "runtime"`;
// the `#[actor] impl` reaches all of it through the single `use runtime::*` glob.
use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;
