//! `aether.gemini` media-generation provider (ADR-0050) — two request kinds,
//! `aether.gemini.nanobanana.generate` (image) and
//! `aether.gemini.lyria.generate` (music), with no text completion (the user
//! defaults to the Claude CLI per ADR-0050 §3).
//!
//! Per ADR-0159 the capability is a loadable wasm guest component
//! ([`GeminiComponent`], `component`): it reaches HTTPS through
//! `aether.http.fetch`, stages generated artifacts through `aether.fs.write`
//! (`gen/<uuid>.{png,wav}`), and reads reference images through
//! `aether.fs.read`. It holds the API key in its init-config
//! ([`GeminiComponentConfig`]) and owns no socket; each request/reply flow is
//! the ADR-0139 `send_with_context` / `take_context` two-handler shape. Its
//! egress is bounded per-sender at the `aether.http` edge (ADR-0158), so the
//! guest queues nothing itself. Provider access is opt-in: a substrate loads
//! this component (boot manifest or `load_component`); the default chassis
//! composition carries it no longer (issue #3893).
//!
//! The wire kinds (`kinds`) sit over the pure provider logic the component runs —
//! request-body construction (`body`), response parsing + per-model validation
//! (`nanobanana` / `lyria`), and the error taxonomy (`error`) — all wasm-safe,
//! so the crate compiles unchanged to `wasm32-unknown-unknown`. `export!` (in
//! `component`) emits the wasm32-only cdylib FFI, inert in the host rlib.

// The wire kinds carry the marker face; the guest component and its callers
// share this vocabulary crate (ADR-0066).
mod kinds;

// Pure provider logic (no I/O): the adapter-facing request DTO the body
// builder converts through, the request-body builders + base64 codec, the
// per-model validation tables + response parsers, and the error taxonomy.
mod adapter;
mod body;
mod error;
mod lyria;
mod nanobanana;

// ADR-0159 guest component: the `GeminiComponent` actor + its init-config kind +
// the request-context kinds, and `export!`.
mod component;

pub use component::{GeminiComponent, GeminiComponentConfig};
pub use kinds::*;

/// Default per-request timeout when the component's `timeout_millis` init-config
/// is unset. Media generation can run a couple minutes.
pub const DEFAULT_TIMEOUT_MILLIS: u32 = 180_000;
