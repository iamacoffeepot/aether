//! `aether.render` cap. [`RenderCapability`] is the pumped, driver-thread
//! `aether.render` actor (ADR-0161): it owns the accumulators, the wgpu GPU
//! and surface, and the pending capture as plain state, dispatched through a
//! [`PumpedSlot`](aether_substrate::actor::native::PumpedSlot) on the chassis
//! driver thread rather than the worker pool. Frame recording, capture
//! readback, and present all run on the one thread that owns the surface, so
//! the largest cross-thread shared-state seam in the codebase collapses to
//! plain fields.
//!
//! The driver requests a frame by mailing [`Frame`] each redraw after the
//! advance chain settles; capture is a mail-driven state machine inside the
//! actor ([`Frame`] / [`PreSettled`] / [`Occluded`] complete it), so every
//! capture transition is a handler with trace brackets and a cost row. The
//! GPU boots lazily on the first frame (against a shared window cell on
//! desktop, or `offscreen_size` on the surfaceless harness path).
//!
//! The cap's drawing + texture mail kinds — and the three chassis-internal
//! driver kinds — live in [`kinds`] (ADR-0121): they ride the always-on
//! (marker-only `render`) region so a wasm guest sees the kind types for
//! typed addressing without the `render-runtime` GPU stack. The
//! capture-request and `FrameCheck` verification kinds stay in `aether-kinds`
//! (consumed upstream by `aether-mcp` and the substrate core), as do the
//! `QuadSpace` / `QuadScale` projection types the `aether.text` kinds share.
//!
//! The runtime decomposes along cohesion seams: `pipeline` (GPU bundle +
//! shared record helpers), `texture` (the texture registry), `quad` (the
//! quad-batch accumulator), `material` (the material-batch accumulator),
//! `surface` (the wgpu surface / offscreen boot), and `capture` (the
//! similarity-reference resolver).
//!
//! [`HeadlessRenderCapability`] is the chassis-without-GPU companion:
//! same `aether.render` mailbox, no-op `DrawTriangle` / `ViewProjection`
//! handlers (so desktop-designed components don't warn-storm),
//! `Err`-replying `CaptureFrame` / `CreateTexture` handlers. Headless chassis
//! composes it in place of [`RenderCapability`] (issue 603 Phase 2 § Resolved
//! Decision 5).

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// The cap's drawing + texture mail kinds (ADR-0121). Always-on (the
// `render` marker feature gates the whole module) so a wasm guest on the
// marker-only `render` feature sees the kind types.
pub mod kinds;
pub use kinds::*;

// Handler-signature kinds must be importable at file root because
// `#[actor]` emits `impl HandlesKind<K> for X {}` markers always-on
// (outside the `render-runtime` gate), against the identity. The drawing
// kinds come from the local `kinds` module (via the glob re-export
// above); `CaptureFrame` stays in `aether-kinds` (consumed by
// `aether-mcp`).
use aether_kinds::CaptureFrame;

// Auxiliary native-only types the chassis driver consumes alongside
// `RenderCapability`. The seams (`capture`, `pipeline`, `quad`, `texture`,
// `surface`, `config`) live under the `runtime` directory, covered by the one
// `mod runtime;` gate (`render-runtime`); their re-exports source through
// `runtime` so wasm components that opt into the marker-only `render` feature
// see only the identity ZST + Actor / HandlesKind impls, not these heavy
// GPU-bound types. `RenderCapabilityState` is the pumped runtime state the
// driver reads (`capture_deadline` / `triangles_rendered` /
// `capture_ready`) through `PumpedSlot::read_state`; the three chassis-internal
// driver kinds (`Frame` / `Occluded` / `PreSettled`) ride the always-on
// `kinds` module (re-exported above). The `WindowCell` is winit-typed and
// stays `desktop`-only.
#[cfg(feature = "desktop")]
pub use runtime::WindowCell;
#[cfg(feature = "runtime")]
pub use runtime::{
    RenderCapabilityState, RenderParams, RenderTuningConfig, RenderTuningConfigLayer, RenderTuningOverlay,
    WHITE_TEXTURE_ID,
};

// `#[actor]` sits on each capability struct (the struct-hosted ADR-0123
// form): it reads the cap's sibling runtime module off disk and emits the
// always-on addressing markers + handler inventory against the struct here.
// The state-bearing, GPU-bound behavior of each cap — its `#[runtime] impl
// NativeActor`, runtime state struct, the wgpu accumulator helpers, the
// `HubOutbound` — lives in a per-cap runtime module: `runtime` for
// [`RenderCapability`] (gated `render-runtime`) and `headless_runtime` for
// [`HeadlessRenderCapability`] (gated the default `runtime`). The
// `aether_substrate` ctx types each impl names (`NativeActor` / `NativeCtx`
// / … / `Manual` / `CaptureFrameResult`) are now sourced inside each runtime
// module beside the body, not here — only the handler-argument kinds the
// emitted markers lift verbatim must keep resolving at this file's root.
use aether_actor::actor;

// The pumped render runtime half — the wgpu-typed surface (state, ctx
// imports, record helpers, the mail-driven capture machine) — lives in
// `runtime/mod.rs`, gated once here on the `render-runtime` override
// (matching the `#[actor] impl`'s runtime gate).
#[cfg(feature = "runtime")]
mod runtime;

// The headless companion's runtime half lives in `headless_runtime.rs`,
// gated on the default `runtime` feature so a no-GPU headless build still
// compiles it.
#[cfg(feature = "runtime")]
mod headless_runtime;

/// `aether.render` cap **identity** (ADR-0122 identity/runtime split). A
/// ZST carrying only the addressing — `Addressable`, the per-handler
/// `HandlesKind` markers, and the name-inventory entry, all emitted
/// always-on by `#[actor]` so a wasm guest on the marker-only `render`
/// feature can `ctx.actor::<RenderCapability>().send(&triangle)` without
/// dragging the GPU stack.
///
/// The state-bearing runtime is the pumped, driver-thread `aether.render`
/// actor (ADR-0161): its [`RenderCapabilityState`] owns the accumulators, the
/// wgpu GPU + surface, and the pending capture as plain state, dispatched
/// through a [`PumpedSlot`](aether_substrate::actor::native::PumpedSlot) on
/// the chassis driver thread rather than the worker pool. It lives behind the
/// `render-runtime` gate in the `runtime` module, so a transport- or
/// marker-only build never names it nor pulls `aether_substrate`/wgpu through
/// this cap.
#[actor(singleton)]
pub struct RenderCapability;

/// `HeadlessRenderCapability` **identity** (ADR-0122 identity/runtime
/// split). The chassis-without-GPU companion to [`RenderCapability`],
/// claiming the same `aether.render` mailbox so desktop-designed
/// components loaded on headless can mail `DrawTriangle` / `aether.view_projection`
/// / `aether.render.capture_frame` against a known recipient —
/// `DrawTriangle` and `ViewProjection` no-op (the warn-storm sink-replacement role
/// pre-issue-603 Phase 2), `CaptureFrame` replies `Err` so MCP
/// `capture_frame` fails fast instead of timing out.
///
/// A ZST carrying only the addressing; the state-bearing runtime
/// (`HeadlessRenderCapabilityState`, holding the captured `HubOutbound`)
/// lives behind the default `runtime` gate in `headless_runtime` — no
/// `render-runtime` dep, so it compiles on a no-GPU headless build.
///
/// Headless chassis composes one of [`Self`] / [`RenderCapability`], never
/// both — the chassis builder rejects double-claiming a mailbox.
#[actor(singleton, headless_runtime)]
pub struct HeadlessRenderCapability;
