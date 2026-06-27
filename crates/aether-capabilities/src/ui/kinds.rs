use serde::{Deserialize, Serialize};

// ADR-0107 `aether.ui` widget kinds. Immediate-mode: the component
// lays out and sends these each frame; the `aether.ui` cap translates
// them to `draw_solid_quads` and `draw_text` calls the same tick.
// Screen-space; `rect` is `[x, y, width, height]` in window pixels.

/// `aether.ui.panel` — draw a flat-colored panel at `rect` in
/// screen-pixel space. `color` is a linear RGBA value; the alpha
/// channel scales the blend. Fire-and-forget; resend every frame.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.ui.panel")]
pub struct UiPanel {
    /// `[x, y, width, height]` in window pixels.
    pub rect: [f32; 4],
    pub color: [f32; 4],
}

/// `aether.ui.bar` — draw a two-layer progress bar at `rect` in
/// screen-pixel space. The track (`track_color`) fills the full rect;
/// the fill (`fill_color`) covers `frac` × width, clamped to [0, 1].
/// Fire-and-forget; resend every frame.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.ui.bar")]
pub struct UiBar {
    /// `[x, y, width, height]` in window pixels.
    pub rect: [f32; 4],
    /// Filled fraction, clamped to [0, 1] by the cap.
    pub frac: f32,
    pub track_color: [f32; 4],
    pub fill_color: [f32; 4],
}

/// `aether.ui.label` — draw a string at `(x, y)` in screen-pixel
/// space using the font registered under `font_id` (via
/// `aether.text.load_font`). Fire-and-forget; resend every frame.
/// An unknown `font_id` warn-drops in the `aether.text` cap.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.ui.label")]
pub struct UiLabel {
    pub x: f32,
    pub y: f32,
    pub font_id: u32,
    pub text: String,
    pub size_pixels: f32,
    pub color: [f32; 4],
}

/// `aether.ui.button` — draw a clickable button at `rect` in
/// screen-pixel space (ADR-0107 §3). The cap forwards the fill
/// (`color`) as a panel quad and the `text` label, and records the
/// rect + `id` + the sending component for hit-testing. A left-click
/// inside the rect replies `aether.ui.clicked { id }` to the sender
/// within one frame (topmost button wins on overlap). v1: the mouse-
/// button stream carries no button discriminant or release, so the
/// button activates on left-press only. Fire-and-forget; resend every
/// frame — `id` is the caller's stable handle for the widget.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.ui.button")]
pub struct UiButton {
    /// Caller-stable widget id echoed back in `UiClicked` on a hit.
    pub id: u32,
    /// `[x, y, width, height]` in window pixels.
    pub rect: [f32; 4],
    /// Background fill, linear RGBA.
    pub color: [f32; 4],
    /// Font for the label, registered via `aether.text.load_font`.
    pub font_id: u32,
    /// Label drawn at the button's top-left corner.
    pub text: String,
    pub size_pixels: f32,
    /// Label color, linear RGBA.
    pub text_color: [f32; 4],
}

/// `aether.ui.clicked` — the cap's interaction reply (ADR-0107 §3).
/// Sent to the component that drew the `UiButton` carrying the same
/// `id` when a left-click lands inside that button's rect. Delivered
/// by id to the recorded owner — the button mail's sender — within one
/// frame of the click.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.ui.clicked")]
pub struct UiClicked {
    /// The `id` of the `UiButton` that was clicked.
    pub id: u32,
}
