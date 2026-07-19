//! Shared infrastructure for the per-provider content-gen caps
//! (`aether-anthropic`, issue 1014; `aether-gemini`, issue 1015).
//!
//! ADR-0050 §2 settles the dispatch model both caps embed: cap-local
//! spawn-and-die with a per-cap concurrency bound. This crate lands
//! that model once so neither provider cap re-derives the dispatch
//! loop, the root-relative `gen/` staging convention, or the stub-adapter
//! shapes:
//!
//! - [`TaskQueue`](aether_substrate::actor::native::TaskQueue) — the
//!   cap-level rate-limit + queue over the substrate's ADR-0093
//!   hold-until-resolve dispatch primitive
//!   (`NativeCtx::dispatch_blocking`). The embedding cap calls `submit`
//!   from its generate handlers and `on_complete` from its
//!   `#[handler(task)]` completion handlers; the framework owns the
//!   in-flight ledger (hold + reply target + worker spawn).
//! - [`stage_gen_output_under`] — write generated binary bytes to a fresh
//!   `gen/<uuid>.<ext>` below a caller-supplied root and return that relative
//!   path in the reply (binary outputs never ride the mail wire). The
//!   root is resolved once at chassis boot ([`ContentGenConfig`]) and
//!   threaded into the cap.
//! - [`adapter`] — the `AnthropicAdapter` / `GeminiAdapter` traits plus
//!   `StubAnthropicAdapter` / `StubGeminiAdapter` no-op impls so both
//!   caps land scaffolding + CI smokes before any network code exists.

// Always-on: the wasm-safe adapter traits + stub types and the
// `ContentGenConfig` domain struct carry the marker face.
pub mod adapter;
pub mod config;
// Runtime-only: the `gen/` staging path (names `uuid` + the `aether.fs`
// runtime `LocalFileAdapter`) and the shared `ureq` transport plumbing live
// behind the one `feature = "runtime"` gate, so a marker-only build never
// pulls the transport / substrate stack through this crate.
#[cfg(feature = "runtime")]
pub mod staging;
#[cfg(feature = "runtime")]
pub mod transport;

pub use adapter::{
    AdapterUsage, AnthropicAdapter, AnthropicRequest, AnthropicResponse, GeminiAdapter, GeminiArtifact,
    GeminiImageRequest, GeminiMusicRequest, GeminiResponse, StubAnthropicAdapter, StubGeminiAdapter,
};
pub use config::ContentGenConfig;
#[cfg(feature = "runtime")]
pub use config::{ContentGenConfigLayer, ContentGenOverlay};
#[cfg(feature = "runtime")]
pub use staging::{GEN_PREFIX, stage_gen_output_under};
