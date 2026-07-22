//! Init-config and reply-correlation kinds for the guest `aether.anthropic`
//! component (ADR-0159).
//!
//! [`AnthropicComponentConfig`] is the ADR-0090 init-config the loader hands
//! `init`: the API key, the disable flag, the per-request timeout, and the
//! logical name the CLI backend runs through `aether.process.run`. Keys ride
//! init-config bytes — the ADR-0159 §5 recorded interim posture.
//!
//! [`RequestContext`] is the kind-typed context a request handler stashes with
//! `send_with_context` and the reply handler recovers with `take_context`
//! (ADR-0139), carrying the original caller back across the edge round-trip.

use serde::{Deserialize, Serialize};

use aether_actor::ReplyHandle;

/// Default per-request timeout when the config carries `0` — a long
/// completion can run tens of seconds. Matches the native cap's
/// `DEFAULT_TIMEOUT_MILLIS`.
pub const DEFAULT_TIMEOUT_MILLIS: u32 = 120_000;

/// Default logical name the CLI backend runs. The operator allowlists this
/// name on the `aether.process` cap; an allowlist that omits it yields the
/// graceful `CliNotFound` skip the kind already models (ADR-0159 §3).
pub const DEFAULT_CLI_BINARY: &str = "claude";

/// Init-config for the guest [`AnthropicComponent`](super::AnthropicComponent)
/// (ADR-0090 init-config bytes, ADR-0159 §5).
///
/// # Agent
/// Encode one of these to the component's `Config` shape and pass it as the
/// `config` bytes of the `aether.component.load` that instantiates the
/// component (or `load_component`'s `config_path`). Omitting config bytes
/// boots [`AnthropicComponentConfig::default()`] — no key, so
/// `aether.anthropic.messages.send` replies `Unauthorized` while
/// `aether.anthropic.cli.send` still routes through `aether.process`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.anthropic.config")]
pub struct AnthropicComponentConfig {
    /// Anthropic Messages API key placed on the `x-api-key` header of each
    /// `aether.http.fetch`. `None` (or `disabled`) leaves the Messages
    /// backend replying `Unauthorized`; the CLI backend never uses it.
    pub api_key: Option<String>,
    /// Disable the Messages backend even when a key is present — Messages
    /// requests reply `Unauthorized`, the CLI path still routes.
    pub disabled: bool,
    /// Per-request timeout in milliseconds for both backends. `0` selects
    /// [`DEFAULT_TIMEOUT_MILLIS`] for the Messages fetch and the
    /// `aether.process` cap's own default for the CLI run.
    pub timeout_millis: u32,
    /// Logical binary name the CLI backend runs through `aether.process.run`.
    /// Resolved against the process cap's operator allowlist, never a path.
    pub cli_binary: String,
}

impl Default for AnthropicComponentConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            disabled: false,
            timeout_millis: DEFAULT_TIMEOUT_MILLIS,
            cli_binary: String::from(DEFAULT_CLI_BINARY),
        }
    }
}

/// Which request kind a reply belongs to. A single reply handler per edge
/// (`on_fetch_result` / `on_run_result`) is enough because the two backends
/// reply with distinct kinds, but the context still records the path so the
/// handler assembles the matching `_result` kind.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendPath {
    Messages,
    Cli,
}

/// Reply-correlation context stashed on an outbound edge request and recovered
/// on its reply (ADR-0139). Carries the original caller's [`ReplyHandle`], the
/// caller-minted `request_id` echoed on both reply arms, the requested `model`
/// (the parse fallback + the `model_used` echo for the CLI path), and the
/// resolved `timeout_millis` (the elapsed a `TimedOut` CLI run reports).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.anthropic.request_context")]
pub struct RequestContext {
    pub reply: Option<ReplyHandle>,
    pub path: SendPath,
    pub request_id: u64,
    pub model: String,
    pub timeout_millis: u32,
}
