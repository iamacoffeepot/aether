//! `aether.anthropic` — the anthropic provider vocabulary and its two
//! implementations. Two sibling text-completion request kinds —
//! `aether.anthropic.messages.send` (the official Messages API over HTTPS)
//! and `aether.anthropic.cli.send` (the local `claude` subprocess against
//! the user's subscription) — share identical input/output schemas, with
//! the routing chosen by the kind name.
//!
//! Two implementations of those kinds live here (ADR-0159):
//!
//! - The **guest component** ([`AnthropicComponent`]) is the default,
//!   runtime-less build: a wasm actor that ports the pure request/response
//!   logic and dispatches its I/O as mail — the Messages backend to
//!   `aether.http` (ADR-0158), the `claude` CLI backend to `aether.process`
//!   (ADR-0157). This is what `load_component` instantiates.
//! - The **native cap** ([`AnthropicCapability`], behind `feature =
//!   "runtime"`) is the chassis-owned mailbox retained until the component
//!   reaches parity. Long-tail calls ride the ADR-0093 hold-until-resolve
//!   dispatch: the generate handler submits the blocking call to a
//!   `TaskQueue` (`aether_substrate::actor::native::TaskQueue`), the
//!   substrate spawns an ephemeral worker and holds the chain open, and the
//!   completion routes to the cap's `#[handler(task)]`. Production wires the
//!   `CombinedAnthropicAdapter` (the `ureq` Messages backend + `claude`
//!   subprocess backend); a key-absent boot wires the
//!   `DisabledAnthropicAdapter` so Messages requests reply `Unauthorized`
//!   while the CLI path still routes.

// Always-on: the mail kinds + the `AnthropicConfig` domain struct carry the
// marker face. The handler-signature kinds resolve at file root because
// `#[actor]` emits `impl HandlesKind<K>` markers against the identity.
mod config;
mod kinds;
pub use config::AnthropicConfig;
pub use kinds::{AnthropicError, CliSend, CliSendResult, Message, MessagesSend, MessagesSendResult, Role};

// The ADR-0159 guest component: a wasm actor that keeps the wire kinds
// byte-identical and dispatches its I/O as mail to the `aether.http` and
// `aether.process` edge capabilities. Always-on and wasm-safe (the pure logic
// ported from the native backend depends only on serde / the edge kinds), so
// the default (runtime-less) build of this crate is the loadable component; the
// `export!` FFI it emits is wasm32-only and inert in the native `runtime`
// build, where it coexists with the native cap below (distinct types, one
// shared `NAMESPACE`; only the native identity carries the name-inventory
// entry, so there is no runtime collision).
mod component;
pub use component::{AnthropicComponent, AnthropicComponentConfig, DEFAULT_CLI_BINARY};
aether_actor::export!(AnthropicComponent);

// Runtime-only: the `Config`-derive layer/overlay + the adapter machinery (the
// `ureq` Messages backend `api`, the `claude` subprocess backend `cli`, the
// error taxonomy `error`, and the `AnthropicAdapter` impls below) live behind
// the one `feature = "runtime"` gate, so a marker-only build never names them
// nor pulls the transport / substrate stack through.
#[cfg(feature = "runtime")]
pub use config::{AnthropicConfigLayer, AnthropicOverlay};

#[cfg(feature = "runtime")]
mod api;
#[cfg(feature = "runtime")]
mod cli;
#[cfg(feature = "runtime")]
mod error;

#[cfg(feature = "runtime")]
use std::time::Duration;

#[cfg(feature = "runtime")]
use aether_contentgen::adapter::{AnthropicAdapter, AnthropicRequest, AnthropicResponse};

#[cfg(feature = "runtime")]
pub use api::UreqAnthropicAdapter;
#[cfg(feature = "runtime")]
use cli::ClaudeCliAdapter;

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
#[cfg(feature = "runtime")]
const SUPPORTED_MESSAGES_MODELS: &[&str] = &["claude-opus-4-7", "claude-sonnet-4-6", "claude-haiku-4-5-20251001"];

/// Adapter returned when `ANTHROPIC_API_KEY` is unset (or
/// `AETHER_ANTHROPIC_DISABLE=1`). Messages requests reply
/// `Err { Unauthorized }`; the CLI path still works (it uses the
/// user's subscription, not the API key) so it falls through to the
/// real subprocess backend.
#[cfg(feature = "runtime")]
#[derive(Default)]
pub struct DisabledAnthropicAdapter {
    cli: ClaudeCliAdapter,
}

#[cfg(feature = "runtime")]
impl DisabledAnthropicAdapter {
    /// Build the disabled adapter with the CLI backend wired to the
    /// cap's per-request `timeout`. The default impl uses
    /// `DEFAULT_TIMEOUT_MILLIS`; production threads `config.timeout`.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self { cli: ClaudeCliAdapter::new(String::from("claude"), timeout) }
    }
}

#[cfg(feature = "runtime")]
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
#[cfg(feature = "runtime")]
pub struct CombinedAnthropicAdapter {
    messages: UreqAnthropicAdapter,
    cli: ClaudeCliAdapter,
}

#[cfg(feature = "runtime")]
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

#[cfg(feature = "runtime")]
impl AnthropicAdapter for CombinedAnthropicAdapter {
    fn messages_send(&self, req: AnthropicRequest) -> Result<AnthropicResponse, String> {
        self.messages.messages_send(&req)
    }

    fn cli_send(&self, req: AnthropicRequest) -> Result<AnthropicResponse, String> {
        self.cli.cli_send(&req)
    }

    fn supported_models(&self) -> Vec<String> {
        SUPPORTED_MESSAGES_MODELS.iter().map(|s| (*s).to_string()).collect()
    }
}

/// Convert an adapter error string into the typed `AnthropicError`.
/// Shared by both result paths.
#[cfg(feature = "runtime")]
use error::adapter_error_to_typed as map_adapter_error;

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
