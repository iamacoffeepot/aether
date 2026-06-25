//! The `HeadlessRenderCapability` runtime half (ADR-0122 identity/runtime
//! split). Compiled under the default `feature = "runtime"` gate (the
//! `mod headless_runtime;` declaration in the parent carries it) — unlike
//! the GPU-bound [`super::RenderCapability`], the headless companion has no
//! `render-native` dep, so its runtime half must compile on a no-GPU
//! headless `runtime` build. The substrate-typed imports are gated once by
//! this module; the `#[actor] impl` reaches the state + ctx types through
//! the single `use headless_runtime::*` glob in the parent.

// `io` is named by the parent's `init` body (`io::Error::other`); `Arc` and
// `HubOutbound` only by the state struct's field. The substrate ctx types
// the `#[actor] impl` names (`NativeActor` / `NativeCtx` / `NativeInitCtx` /
// `BootError` / `Manual` / `CaptureFrameResult`) come from the shared
// `any(render-native, runtime)` seam in `mod.rs`, not from here, so a
// desktop build doesn't re-export them through two globs.
pub use std::io;

use std::sync::Arc;

use aether_substrate::mail::outbound::HubOutbound;

/// `HeadlessRenderCapability` runtime state. Holds only the [`HubOutbound`]
/// captured at init — the headless cap replies `Err` to the GPU-bound
/// kinds (`CaptureFrame` / `CreateTexture`) and no-ops the accumulator
/// kinds, so it needs no handles. The addressing identity is the distinct
/// ZST [`super::HeadlessRenderCapability`]. Living in this private module
/// keeps it `pub`-enough to satisfy the `NativeActor::State` interface
/// without exposing it as crate-public API.
pub struct HeadlessRenderCapabilityState {
    pub(super) outbound: Arc<HubOutbound>,
}
