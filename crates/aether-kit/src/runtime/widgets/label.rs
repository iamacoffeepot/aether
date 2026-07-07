// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `runtime/widget.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The static label (issue 2660).
//!
//! Non-interactive text — the trivial widget. It takes no input and is not
//! focus-eligible (the root's focus register skips it); it only draws its
//! configured text each `Collect`.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};

use crate::runtime::widgets::text_origin_y;
use crate::theme::{SetTheme, Theme};
use crate::widgets::{Collect, LabelConfig, WidgetDrawItem, WidgetDrawList, WidgetFrame};

/// A static text label. Holds the text plus the cached theme / frame.
pub struct LabelWidget {
    text: String,
    theme: Theme,
    frame: WidgetFrame,
}

/// A label widget. Spawned inline by a panel root with a [`LabelConfig`];
/// draws its text and reports nothing up.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `LabelConfig` again to change the text or theme in place.
#[actor(instanced)]
impl WasmActor for LabelWidget {
    type Config = LabelConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.label";

    fn init(config: LabelConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(LabelWidget {
            text: config.text,
            theme: config.theme,
            frame: WidgetFrame {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
            },
        })
    }

    /// Change the text / theme in place from a re-sent config.
    #[handler]
    fn on_config(&mut self, _ctx: &mut WasmCtx<'_>, config: LabelConfig) {
        self.text = config.text;
        self.theme = config.theme;
    }

    /// Restyle: adopt the fanned theme.
    #[handler]
    fn on_set_theme(&mut self, _ctx: &mut WasmCtx<'_>, set: SetTheme) {
        self.theme = set.theme;
    }

    /// Cache the layout rect the root assigned.
    #[handler]
    fn on_frame(&mut self, _ctx: &mut WasmCtx<'_>, frame: WidgetFrame) {
        self.frame = frame;
    }

    /// Reply the label's local draw: its text at the theme's label size.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        let size = self.theme.label_size_pixels;
        let mut items: Vec<WidgetDrawItem> = Vec::new();
        if !self.text.is_empty() {
            items.push(WidgetDrawItem::Text {
                x: 0.0,
                y: text_origin_y(0.0, self.frame.height, size),
                font_id: self.theme.font_id,
                text: self.text.clone(),
                size_pixels: size,
                color: self.theme.text_primary,
            });
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList {
                intrinsic: None,
                items,
            });
        }
    }
}
