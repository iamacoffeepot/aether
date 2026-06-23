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

#[cfg(test)]
mod tests {
    use super::{UiBar, UiButton, UiClicked, UiLabel, UiPanel};
    use aether_data::Kind;

    #[test]
    fn ui_panel_roundtrip() {
        let p = UiPanel {
            rect: [10.0, 20.0, 100.0, 50.0],
            color: [0.1, 0.2, 0.3, 0.8],
        };
        let bytes = p.encode_into_bytes();
        let back: UiPanel =
            UiPanel::decode_from_bytes(&bytes).expect("test setup: kind codec decodes UiPanel");
        assert_eq!(back.rect, [10.0, 20.0, 100.0, 50.0]);
        assert_eq!(back.color, [0.1, 0.2, 0.3, 0.8]);
    }

    #[test]
    fn ui_bar_roundtrip() {
        let b = UiBar {
            rect: [0.0, 0.0, 200.0, 24.0],
            frac: 0.75,
            track_color: [0.2, 0.2, 0.2, 1.0],
            fill_color: [0.1, 0.8, 0.2, 1.0],
        };
        let bytes = b.encode_into_bytes();
        let back: UiBar =
            UiBar::decode_from_bytes(&bytes).expect("test setup: kind codec decodes UiBar");
        assert_eq!(back.rect, [0.0, 0.0, 200.0, 24.0]);
        assert_eq!(back.frac, 0.75);
        assert_eq!(back.track_color, [0.2, 0.2, 0.2, 1.0]);
        assert_eq!(back.fill_color, [0.1, 0.8, 0.2, 1.0]);
    }

    #[test]
    fn ui_label_roundtrip() {
        let l = UiLabel {
            x: 10.0,
            y: 20.0,
            font_id: 2,
            text: "HP: 100".to_string(),
            size_pixels: 16.0,
            color: [1.0, 1.0, 1.0, 1.0],
        };
        let bytes = l.encode_into_bytes();
        let back: UiLabel =
            UiLabel::decode_from_bytes(&bytes).expect("test setup: kind codec decodes UiLabel");
        assert_eq!(back.x, 10.0);
        assert_eq!(back.y, 20.0);
        assert_eq!(back.font_id, 2);
        assert_eq!(back.text, "HP: 100");
        assert_eq!(back.size_pixels, 16.0);
        assert_eq!(back.color, [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn ui_button_roundtrip() {
        let b = UiButton {
            id: 7,
            rect: [4.0, 8.0, 120.0, 32.0],
            color: [0.15, 0.15, 0.2, 1.0],
            font_id: 1,
            text: "Play".to_string(),
            size_pixels: 18.0,
            text_color: [0.9, 0.9, 0.9, 1.0],
        };
        let bytes = b.encode_into_bytes();
        let back: UiButton =
            UiButton::decode_from_bytes(&bytes).expect("test setup: kind codec decodes UiButton");
        assert_eq!(back.id, 7);
        assert_eq!(back.rect, [4.0, 8.0, 120.0, 32.0]);
        assert_eq!(back.color, [0.15, 0.15, 0.2, 1.0]);
        assert_eq!(back.font_id, 1);
        assert_eq!(back.text, "Play");
        assert_eq!(back.size_pixels, 18.0);
        assert_eq!(back.text_color, [0.9, 0.9, 0.9, 1.0]);
    }

    #[test]
    fn ui_clicked_roundtrip() {
        let c = UiClicked { id: 7 };
        let bytes = c.encode_into_bytes();
        let back: UiClicked =
            UiClicked::decode_from_bytes(&bytes).expect("test setup: kind codec decodes UiClicked");
        assert_eq!(back.id, 7);
    }
}
