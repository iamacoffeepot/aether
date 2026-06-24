//! The `aether.render` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "render-native"` (the `mod runtime;`
//! declaration in the parent carries the gate, matching the `#[actor(…,
//! runtime_feature = "render-native")]` override on the impl), so a
//! transport-only or marker-only `render` build of the [`RenderCapability`]
//! identity never names these types nor pulls the wgpu-bound substrate
//! runtime through this cap. The substrate-typed imports + GPU-bound
//! helpers are gated once by this module rather than line-by-line; the
//! `#[actor] impl` reaches the state, ctx, and accumulator helpers through
//! the single `use runtime::*` glob in the parent.

// `Arc` is named here only by the state struct's field types; the parent
// `#[actor] impl` gets its own `Arc` from the shared `any(render-native,
// runtime)` import in `mod.rs`, so this stays a private import to avoid a
// redundant re-export. The substrate ctx types (`NativeActor` / `NativeCtx`
// / `NativeInitCtx` / `BootError` / `Manual` / `CaptureFrameResult`) the
// `#[actor] impl` names come from that same shared seam, not from here.
use std::sync::Arc;

pub use std::sync::atomic::{AtomicU64, Ordering};
pub use std::sync::{Mutex, OnceLock};

pub use aether_data::Kind;
pub use aether_substrate::capture::PendingCapture;
pub use aether_substrate::mail::helpers::resolve_bundle;
pub use aether_substrate::mail::mailer::Mailer;
pub use aether_substrate::mail::registry::Registry;
pub use aether_substrate::render::IDENTITY_VIEW_PROJ;

pub use super::config::RenderConfig;
pub use super::pipeline::RenderHandles;

// These seam items are `pub(super)` (visible in `render`), so the
// re-export back up to the parent `#[actor] impl` keeps that visibility —
// `pub use` would try to widen them to `pub` and fail (E0364/E0365).
// `pub(super)` of this module resolves to `render`, the exact scope the
// glob in `mod.rs` reaches them from.
pub(super) use super::capture::resolve_reference;
pub(super) use super::quad::QuadBatch;
pub(super) use super::texture::{
    StagedTexture, TextureRegistry, WHITE_TEXTURE_ID, expected_pixel_bytes,
};

/// `aether.render` runtime state (ADR-0066). Holds [`RenderHandles`] (the
/// driver-facing accumulator state plus GPU bundle) and the per-instance
/// [`RenderConfig`], plus the substrate registry + mailer captured at init
/// for the `capture_frame` resolve-bundle / push-pre-mails path. The
/// dispatcher holds this as the cap's state and routes envelopes through
/// the macro-emitted `Dispatch` impl; the addressing identity is the
/// distinct ZST [`super::RenderCapability`]. Driver glue fetches the
/// handle bundle via `DriverCtx::handle::<RenderHandles>()` (published in
/// `init`), not through this state. Living in this private module keeps it
/// `pub`-enough to satisfy the `NativeActor::State` interface without
/// exposing it as crate-public API.
pub struct RenderCapabilityState {
    pub(super) handles: RenderHandles,
    pub(super) config: RenderConfig,
    /// Substrate registry and mailer captured at init for the
    /// `capture_frame` resolve-bundle / push-pre-mails path. Both are
    /// Arc-shared with every other cap and the chassis loop.
    pub(super) registry: Arc<Registry>,
    pub(super) mailer: Arc<Mailer>,
}
