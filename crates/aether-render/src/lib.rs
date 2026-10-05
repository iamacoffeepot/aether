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

// SPIKE-ONLY (branch `spike/mesh-draw-path`).
pub mod spike_kinds;
pub use spike_kinds::*;

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
    DEFAULT_CLEAR_COLOR, GeometryRegistry, RealizedGeometry, RenderCapabilityState, RenderParams, RenderTuningConfig,
    RenderTuningConfigLayer, RenderTuningOverlay, StagedGeometry, WHITE_TEXTURE_ID, apply_manifest_clear_color,
};

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

/// SPIKE-ONLY instrumentation (branch `spike/mesh-draw-path`, never on
/// `main`): wall-clock accumulators the mesh-draw-path spike reads to
/// separate the render actor's CPU work from its wait on the GPU.
#[cfg(feature = "runtime")]
pub mod spike_probe {
    use std::sync::atomic::AtomicU64;

    /// Nanos spent in `ProgramRegistry::record` (checks, uniform staging,
    /// bind groups, pass encoding for every pending dispatch).
    pub static PROGRAM_RECORD_NANOS: AtomicU64 = AtomicU64::new(0);
    /// Nanos `on_frame` spent waiting for the previous frame's submission.
    pub static GPU_WAIT_NANOS: AtomicU64 = AtomicU64::new(0);
    /// Nanos spent in `encoder.finish()` plus `queue.submit`.
    pub static SUBMIT_NANOS: AtomicU64 = AtomicU64::new(0);
    /// Nanos of the whole `on_frame` handler.
    pub static FRAME_NANOS: AtomicU64 = AtomicU64::new(0);
    /// Dispatches handed to `ProgramRegistry::record`.
    pub static DISPATCHES: AtomicU64 = AtomicU64::new(0);

    // Round two: the draw-list prototype.
    /// `max_texture_array_layers` the next device is requested with.
    pub static ARRAY_LAYER_LIMIT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(256);
    /// Nanos resolving + validating a draw list at record time (inline
    /// lists; a retained list pays a map lookup only).
    pub static RESOLVE_NANOS: AtomicU64 = AtomicU64::new(0);
    /// Nanos from `begin_render_pass` to the pass being dropped.
    pub static ENCODE_NANOS: AtomicU64 = AtomicU64::new(0);
    /// Nanos creating retained draw lists (resolve + validate + insert).
    pub static LIST_CREATE_NANOS: AtomicU64 = AtomicU64::new(0);
    /// Nanos patching retained draw lists.
    pub static LIST_PATCH_NANOS: AtomicU64 = AtomicU64::new(0);
    /// Nanos in instance sub-range updates.
    pub static UPDATE_NANOS: AtomicU64 = AtomicU64::new(0);
    /// Nanos of the whole spike draw-pass record (both phases).
    pub static DRAW_PASS_NANOS: AtomicU64 = AtomicU64::new(0);
    pub static DRAWS: AtomicU64 = AtomicU64::new(0);
    pub static STATE_SETS: AtomicU64 = AtomicU64::new(0);
    pub static PASSES: AtomicU64 = AtomicU64::new(0);
    pub static DROPPED_PASSES: AtomicU64 = AtomicU64::new(0);
    pub static BIND_GROUPS_CREATED: AtomicU64 = AtomicU64::new(0);
    /// Wait for the previous frame by polling instead of wgpu's sleeping
    /// fence wait.
    pub static SPIN_WAIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
}

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
