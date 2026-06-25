//! `aether.ui` cap (ADR-0107). Translates immediate-mode widget mail into
//! `draw_solid_quads` and `draw_text` sends the same tick — a CPU-only
//! translator with no retained widget state across frames. Components lay
//! out and resend every frame; the cap forwards.
//!
//! Three handlers, each fire-and-forget:
//!
//! - **`on_panel`** → one `DrawSolidQuads` (screen-space) to `aether.render`.
//! - **`on_bar`** → two `SolidQuad`s in one `DrawSolidQuads` (track + frac-
//!   sized fill, screen-space) to `aether.render`.
//! - **`on_label`** → one `DrawText` (screen-space) to `aether.text`. The
//!   string flows from the screen-pixel `(x, y)` along the baseline, where
//!   `(0, 0)` is the window's top-left corner.

// Handler-signature kinds must be importable at file root because `#[actor]`
// emits `impl HandlesKind<K> for X {}` markers always-on, outside the
// `ui-native` runtime gate, so they reference these kinds from here.
use aether_kinds::{MouseButton, MouseMove, Tick};

use super::kinds::{UiBar, UiButton, UiLabel, UiPanel};

// The struct-hosted `#[actor(singleton)]` below stays always-on (the macro
// divides what it emits). Everything that names an `aether_substrate` type —
// the handler/init ctx, the runtime state, the `#[runtime] impl NativeActor`,
// and the helpers — lives in the `runtime` module (a sibling file in this
// dir), gated once by `feature = "ui-native"`; `#[actor]` reads it off disk to
// lift the identity. The kind types (`UiPanel` / `UiBar` / …) stay always-on
// via the imports above — the always-on `HandlesKind<K>` markers name them.
use aether_actor::actor;

/// `aether.ui` cap **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`),
/// the per-handler `HandlesKind` markers, and the name-inventory entry,
/// all emitted always-on by `#[actor]`. The state-bearing runtime
/// (`UiCapabilityState`, which holds the cursor position and button-rect
/// double-buffer) lives behind the one `feature = "ui-native"` gate, so
/// a transport-only build never names `UiCapabilityState` nor pulls
/// `aether_substrate` through this cap.
#[actor(singleton)]
pub struct UiCapability;
