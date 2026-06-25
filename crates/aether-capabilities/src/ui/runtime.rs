//! The `aether.ui` runtime half (ADR-0122 identity/runtime split). Compiled
//! only under `feature = "ui-native"` (the `mod runtime;` declaration in the
//! parent carries the gate), so a transport-only build of the `UiCapability`
//! identity never names these types nor pulls `aether_substrate`. The
//! substrate-typed imports are gated once by this module rather than
//! line-by-line; the `#[actor] impl` reaches the state, ctx types, and helpers
//! through the single `use runtime::*` glob in the parent.

use aether_data::MailboxId;

pub use core::iter::once;
pub use core::mem::swap;

pub use crate::input::{InputCapability, InputMailboxExt};
pub use crate::lifecycle::{LifecycleCapability, LifecycleMailboxExt};
pub use crate::render::{DrawSolidQuads, RenderCapability, SolidQuad};
pub use crate::text::{DrawText, TextCapability};
pub use aether_kinds::QuadSpace;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

/// A button's hit-test record for one frame: where it was drawn, the
/// caller's widget `id`, and the component that drew it (the
/// `UiClicked` recipient on a hit).
pub struct ButtonRect {
    /// `[x, y, width, height]` in window pixels.
    pub(super) rect: [f32; 4],
    /// Caller-stable widget id echoed back in `UiClicked`.
    pub(super) id: u32,
    /// Mailbox of the component that sent the `UiButton`.
    pub(super) owner: MailboxId,
}

/// `aether.ui` runtime state (ADR-0122 split). Owns the cursor position
/// and button-rect double-buffer the handlers share. The addressing
/// identity is the distinct ZST `UiCapability`. Living in this private
/// module keeps it `pub`-enough to satisfy the `NativeActor::State`
/// interface without exposing it as crate-public API.
///
/// Plain-field shape (ADR-0078) — single-threaded, every handler runs
/// on the cap's dispatcher thread. The button-rect map is double-buffered:
/// `current` accumulates this frame's buttons, `Tick` swaps it into `last`,
/// and a click hit-tests against `last` — the one-frame latency ADR-0107 §3
/// specifies, deterministic regardless of button-mail vs click ordering
/// within a tick.
#[derive(Default)]
pub struct UiCapabilityState {
    /// Latest cursor position from `MouseMove`, window pixels.
    pub(super) cursor: [f32; 2],
    /// Buttons recorded during the in-progress frame.
    pub(super) current: Vec<ButtonRect>,
    /// Buttons from the last completed frame — the hit-test set.
    pub(super) last: Vec<ButtonRect>,
}
