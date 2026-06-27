//! `aether.text` cap (ADR-0105). Turns a TTF plus a string into the
//! textured quads the render surface draws — a CPU-only actor with no
//! GPU access that composes `aether.render`'s texture surface entirely by
//! mail.
//!
//! Two flows:
//!
//! - **`load_font`** mirrors `aether.audio.load_instrument` (ADR-0103):
//!   park the request keyed `(namespace, path)`, forward `aether.fs.read`,
//!   correlate on `aether.fs.read_result`, parse the font off the hot path
//!   in a `#[handler(task)]` arm, and register it under a session-scoped
//!   `font_id`. The reply is `load_font_result`.
//! - **`draw`** is fire-and-forget immediate mode: lay the string out with
//!   fontdue's horizontal metrics, rasterize any unseen glyph into the
//!   shelf-packed atlas, emit one `update_texture` per new glyph plus the
//!   `draw_textured_quads` batch — all to `aether.render` the same tick.
//!
//! The atlas texture is created lazily: the first `draw` sends
//! `create_texture` (the zeroed atlas) and correlates the
//! `create_texture_result` reply onto the cap's own mailbox. Until the id
//! lands the cap draws nothing; immediate mode resends every frame, so the
//! string appears the moment the texture exists.
//!
//! When the atlas fills, the cap resets it at the top of the next `draw`:
//! zeros the pixel buffer, clears the glyph cache, resets the shelf
//! cursor, and emits one full-rect `update_texture` to re-sync the GPU
//! side. Every glyph for that frame is then a cache miss and re-uploads
//! normally. The cost is at most one frame of missing overflow glyphs on
//! the saturating frame; the next frame fully recovers.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the decoded
// bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root because
// `#[actor]` emits `impl HandlesKind<K> for X {}` markers against the
// identity (always-on, outside the runtime gate). The `aether.text` mail
// kinds (ADR-0121) live in `kinds` and re-export here; `ReadResult` comes
// from the `aether.fs` cap, and `CreateTextureResult` from the render cap —
// both are replies the text cap receives.
use crate::fs::ReadResult;
use crate::render::CreateTextureResult;

// ADR-0121: the cap owns its mail kinds. Always-on + wasm-safe (only
// `aether-data` + `serde`), re-exported so callers address them as
// `aether_capabilities::text::DrawText`.
mod kinds;
pub use kinds::*;

/// `aether.text` cap **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`),
/// the per-handler `HandlesKind` markers, and the name-inventory entry, all
/// emitted always-on by `#[actor]`. The state-bearing runtime
/// (`TextCapabilityState`, which holds the `fontdue` font registry, the
/// glyph atlas, and the parked `load_font` requests) lives behind the one
/// `feature = "text-native"` gate, so a transport-only build never names
/// `TextCapabilityState` nor pulls `fontdue` / `aether_substrate` through
/// this cap.
#[actor(singleton)]
pub struct TextCapability;

// The struct-hosted `#[actor(singleton)]` above lifts this cap's identity
// (NAMESPACE + per-handler `HandlesKind` markers + the singleton
// name-inventory entry) from the `#[runtime] impl NativeActor` in
// `runtime.rs`, all emitted always-on against the ZST. The behaviour and
// state live in that runtime module, gated once by `feature = "text-native"`.
use aether_actor::actor;

// The runtime half — the whole `fontdue` / `aether_substrate`-typed surface
// (imports, `TextCapabilityState`, the `#[runtime] impl NativeActor`, the
// helper methods) — lives in `runtime.rs`, gated once here. The struct-hosted
// `#[actor(singleton)]` above reads this module off disk to lift the identity.
#[cfg(feature = "text-native")]
mod runtime;
