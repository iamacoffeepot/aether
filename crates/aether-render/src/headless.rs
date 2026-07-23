//! [`HeadlessRenderCapability`] **identity** (ADR-0122 identity/runtime
//! split): the chassis-without-GPU companion to [`crate::RenderCapability`].
//! Always-on like the primary ZST in the crate root, so a marker-only build
//! sees both identities; the runtime half is the nested `runtime::headless`
//! module, covered by the one `mod runtime;` gate.

use aether_actor::actor;

// The handler-argument and reply kinds the emitted `HandlesKind` markers lift
// verbatim from the runtime module's signatures must resolve at this file's
// root.
use aether_kinds::CaptureFrame;

use crate::kinds::{
    CreateTexture, CreateTextureResult, DestroyTexture, DrawMaterialCoverage, DrawMaterialTextured, DrawSolidQuads,
    DrawTexturedQuads, DrawTriangle, UpdateTexture, ViewProjection,
};

/// `HeadlessRenderCapability` **identity** (ADR-0122 identity/runtime
/// split). The chassis-without-GPU companion to [`crate::RenderCapability`],
/// claiming the same `aether.render` mailbox so desktop-designed
/// components loaded on headless can mail `DrawTriangle` / `aether.view_projection`
/// / `aether.render.capture_frame` against a known recipient —
/// `DrawTriangle` and `ViewProjection` no-op (the warn-storm sink-replacement role
/// pre-issue-603 Phase 2), `CaptureFrame` replies `Err` so MCP
/// `capture_frame` fails fast instead of timing out.
///
/// A ZST carrying only the addressing; the state-bearing runtime
/// (`HeadlessRenderCapabilityState`, holding the captured `HubOutbound`)
/// lives behind the default `runtime` gate in `runtime::headless` — no
/// wgpu dep, so it compiles on a no-GPU headless build.
///
/// Headless chassis composes one of [`Self`] / [`crate::RenderCapability`],
/// never both — the chassis builder rejects double-claiming a mailbox.
#[actor(singleton, runtime::headless)]
pub struct HeadlessRenderCapability;
