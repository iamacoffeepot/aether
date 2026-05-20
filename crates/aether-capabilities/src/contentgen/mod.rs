//! Shared infrastructure for the per-provider content-gen caps
//! (`aether.anthropic`, issue 1014; `aether.gemini`, issue 1015).
//!
//! ADR-0050 §2 settles the dispatch model both caps embed: cap-local
//! spawn-and-die with a per-cap concurrency bound. This module lands
//! that model once so neither provider cap re-derives the dispatch
//! loop, the `save://gen/` staging convention, or the stub-adapter
//! shapes:
//!
//! - [`InFlightDispatch`] — the actor-state half of the spawn-and-die
//!   model (in-flight counter + pending queue + `request_id`
//!   correlation map). The embedding cap calls `submit` /
//!   `on_reply_landed` from its handlers.
//! - [`stage_gen_output`] — write generated binary bytes to a fresh
//!   `save://gen/<uuid>.<ext>` and return the path the reply carries
//!   (binary outputs never ride the mail wire).
//! - [`adapter`] — the `AnthropicAdapter` / `GeminiAdapter` traits plus
//!   `StubAnthropicAdapter` / `StubGeminiAdapter` no-op impls so both
//!   caps land scaffolding + CI smokes before any network code exists.

pub mod adapter;
pub mod dispatch;
pub mod shared;
pub mod staging;

pub use adapter::{
    AdapterUsage, AnthropicAdapter, AnthropicRequest, AnthropicResponse, GeminiAdapter,
    GeminiArtifact, GeminiImageRequest, GeminiMusicRequest, GeminiResponse, StubAnthropicAdapter,
    StubGeminiAdapter,
};
pub use dispatch::{BlockingCall, DEFAULT_MAX_IN_FLIGHT, InFlightDispatch, LandedReply};
pub use staging::{GEN_PREFIX, gen_root, stage_gen_output};
