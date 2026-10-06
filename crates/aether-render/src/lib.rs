//! `aether.render` capability. [`RenderCapability`] runs on the chassis driver
//! thread through a [`PumpedSlot`](aether_substrate::actor::native::PumpedSlot)
//! instead of the worker pool, so it holds the accumulators, the wgpu device,
//! its window-keyed surfaces, and the pending capture as plain fields:
//! recording, capture readback, and present all happen on the one thread that
//! owns the surfaces (ADR-0161).
//!
//! The driver asks for a frame by mailing [`Frame`] on each redraw, once the
//! advance chain has settled. Capture is a mail-driven state machine inside
//! the actor ([`Frame`], [`PreSettled`], and [`Occluded`] complete it), so
//! every capture transition is a handler with trace brackets and a cost row.
//! Desktop surfaces attach explicitly by window path; the surfaceless harness
//! GPU boots lazily from `offscreen_size`.
//!
//! The drawing and texture kinds, plus the three chassis-internal driver
//! kinds, live in [`kinds`] and compile always-on, so a wasm guest gets the
//! kind types for typed addressing without the GPU stack behind the `runtime`
//! feature. That runtime half splits along cohesion seams: `pipeline`,
//! `texture`, `geometry`, `overlay`, `material`, `surface`, and `capture`. The
//! capture-request and `FrameCheck` kinds stay in `aether-kinds`, consumed
//! upstream by `aether-mcp` and the substrate core, as do the `QuadSpace` and
//! `QuadScale` projection types the `aether.text` kinds share.
//!
//! A chassis with no GPU composes no render actor at all, so a component that
//! depends on [`RenderCapability`] is refused where it would stand up.

#![forbid(unsafe_code)]
// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// The cap's drawing + texture mail kinds (ADR-0121). Always-on (the
// `render` marker feature gates the whole module) so a wasm guest on the
// marker-only `render` feature sees the kind types.
pub mod kinds;
pub use kinds::*;

// Auxiliary native-only types the chassis driver consumes alongside
// `RenderCapability`. The seams (`capture`, `pipeline`, `overlay`, `texture`,
// `surface`, `config`) live under the `runtime` directory, covered by the one
// `mod runtime;` gate (`render-runtime`); their re-exports source through
// `runtime` so wasm components that opt into the marker-only `render` feature
// see only the identity ZST + Actor / HandlesKind impls, not these heavy
// GPU-bound types. `RenderCapabilityState` is the pumped runtime state the
// driver reads (`capture_deadline` / `triangles_rendered` /
// `capture_ready`) through `PumpedSlot::read_state`; the three chassis-internal
// driver kinds (`Frame` / `Occluded` / `PreSettled`) ride the always-on
// `kinds` module (re-exported above).
#[cfg(feature = "runtime")]
pub use runtime::{
    DEFAULT_CLEAR_COLOR, DrawSet, DrawSetRegistry, DrawSetRows, GeometryRegistry, HeldDraw, InstancesRegistry,
    RealizedGeometry, RenderCapabilityState, RenderParams, RenderTuningConfig, RenderTuningConfigLayer,
    RenderTuningOverlay, StagedGeometry, StagedInstances, WHITE_TEXTURE_ID, apply_manifest_clear_color,
};

// What the desktop driver tells render about a window surface's present,
// on `attach_window` and `set_window_present`.
#[cfg(feature = "desktop")]
pub use runtime::SurfacePresent;

// `#[actor]` sits on each capability struct (the struct-hosted ADR-0123
// form): it reads the cap's runtime module off disk and emits the
// always-on addressing markers + handler inventory against the struct here.
// The state-bearing, GPU-bound behavior of each cap — its `#[runtime] impl
// NativeActor`, runtime state struct, the wgpu accumulator helpers, the
// `HubOutbound` — lives in the `runtime` module under the one `mod runtime;`
// gate. The `aether_substrate` ctx types the impl names (`NativeActor` /
// `NativeCtx` / … / `CaptureFrameResult`) are sourced inside that runtime
// module beside the body, not here — only the handler-argument kinds the
// emitted markers lift verbatim must keep resolving at this file's root.
use aether_actor::actor;

// The pumped render runtime half — the wgpu-typed surface (state, ctx
// imports, record helpers, the mail-driven capture machine) — lives in
// `runtime/mod.rs`, gated once here on the `render-runtime` override
// (matching the `#[actor] impl`'s runtime gate).
#[cfg(feature = "runtime")]
mod runtime;

/// `aether.render` cap **identity** (ADR-0122 identity/runtime split). A
/// ZST carrying only the addressing — `Addressable`, the per-handler
/// `HandlesKind` markers, and the name-inventory entry, all emitted
/// always-on by `#[actor]` so a wasm guest on the marker-only `render`
/// feature can `ctx.send::<RenderCapability>(&triangle)` without
/// dragging the GPU stack.
///
/// The state-bearing runtime is the pumped, driver-thread `aether.render`
/// actor (ADR-0161): its [`RenderCapabilityState`] owns the accumulators, the
/// wgpu GPU + window-keyed surfaces, and the pending capture as plain state,
/// dispatched through a [`PumpedSlot`](aether_substrate::actor::native::PumpedSlot)
/// on the chassis driver thread rather than the worker pool. It lives behind
/// the `render-runtime` gate in the `runtime` module, so a transport- or
/// marker-only build never names it nor pulls `aether_substrate`/wgpu through
/// this cap.
#[actor(singleton, root)]
pub struct RenderCapability;
