// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The tab strip: one row of tabs selecting one of several parallel content
//! sets.
//!
//! Each tab is sized to its label plus padding — never equal thirds of the
//! row — and the selected tab is marked twice, by the selection role and an
//! underline, so it is prominent at a glance. A press selects; a focused
//! Left/Right moves the selection and clamps at the ends. The strip owns
//! nothing but the choice: which content the selected tab shows is the
//! root's business.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};

use crate::set::{WidgetDefaults, reply_if_hidden};
use crate::state::{InteractionState, emit_state_changed};
use crate::theme::Theme;
use crate::{Collect, SetWidgetState, TabStripConfig, WidgetDrawList, WidgetFrame};

/// The tab strip widget. Holds its labels and selected tab plus the cached
/// theme / frame.
pub struct TabStripWidget {
    labels: Vec<String>,
    selected_index: usize,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
}

impl WidgetDefaults for TabStripWidget {
    fn widget_frame(&mut self) -> &mut WidgetFrame {
        &mut self.frame
    }

    fn widget_theme(&mut self) -> &mut Theme {
        &mut self.theme
    }

    fn widget_state(&mut self) -> &mut InteractionState {
        &mut self.state
    }

    fn cancel_activation(&mut self) {}
}

/// A tab strip. Spawned inline by a panel root with a [`TabStripConfig`];
/// reports [`crate::TabSelected`] on a change of tab.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send
/// it its `TabStripConfig` again to replace the labels or the selection in
/// place.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for TabStripWidget {
    type Config = TabStripConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.tab_strip";

    fn init(config: TabStripConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(TabStripWidget {
            selected_index: clamp_index(config.initial_index, config.labels.len()),
            labels: config.labels,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
        })
    }

    /// Replace the labels / selection / theme in place from a re-sent config.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: TabStripConfig) {
        self.selected_index = clamp_index(config.initial_index, config.labels.len());
        self.labels = config.labels;
        self.theme = config.theme;
        if self.state.replace(config.state) {
            emit_state_changed(ctx, &self.state);
        }
    }

    /// Update external availability without changing the tabs.
    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        if self.state.replace(set.state) {
            emit_state_changed(ctx, &self.state);
        }
    }

    /// Reply the strip's local draw.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic: None, items: Vec::new(), overlay: Vec::new() });
        }
    }
}

/// `index` clamped into the label vector (`0` for an empty strip).
fn clamp_index(index: u32, label_count: usize) -> usize {
    usize::try_from(index).map_or(0, |index| index.min(label_count.saturating_sub(1)))
}
