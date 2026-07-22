//! `aether.anthropic` — the anthropic provider vocabulary and its wasm guest
//! component. Two sibling text-completion request kinds —
//! `aether.anthropic.messages.send` (the official Messages API over HTTPS)
//! and `aether.anthropic.cli.send` (the local `claude` subprocess against
//! the user's subscription) — share identical input/output schemas, with
//! the routing chosen by the kind name.
//!
//! Per ADR-0159 the capability is a loadable guest component
//! ([`AnthropicComponent`]): a wasm actor that ports the pure request/response
//! logic and dispatches its I/O as mail — the Messages backend to `aether.http`
//! (ADR-0158), the `claude` CLI backend to `aether.process` (ADR-0157). It holds
//! the API key in its init-config ([`AnthropicComponentConfig`]) and owns no
//! socket. Provider access is opt-in: a substrate loads this component (boot
//! manifest or `load_component`); the default chassis composition carries it no
//! longer (issue #3893). The pure logic ported from the retired native cap
//! depends only on serde / the edge kinds, so the crate compiles unchanged to
//! `wasm32-unknown-unknown`; the `export!` FFI it emits is wasm32-only and inert
//! in the native rlib the host integration test links.

// The wire kinds carry the marker face; the guest component and its callers
// share this vocabulary crate (ADR-0066).
mod kinds;
pub use kinds::{AnthropicError, CliSend, CliSendResult, Message, MessagesSend, MessagesSendResult, Role};

// The ADR-0159 guest component: a wasm actor that keeps the wire kinds
// byte-identical and dispatches its I/O as mail to the `aether.http` and
// `aether.process` edge capabilities.
mod component;
pub use component::{AnthropicComponent, AnthropicComponentConfig, DEFAULT_CLI_BINARY};
aether_actor::export!(AnthropicComponent);
