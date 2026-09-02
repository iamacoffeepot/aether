// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The dropdown: one current choice, the alternatives in a list that opens
//! on demand.
//!
//! Closed, it is one row reading the current option (or the placeholder)
//! with a chevron at its end. Open, it keeps that row and draws up to
//! `open_row_count` option rows below it in its **overlay**
//! ([`WidgetDrawList::overlay`]) so the list escapes the slot clip and lands
//! over every ordinary draw of the cluster. While open it asks the root for
//! the pointer grab through [`crate::DropdownOpenChanged`], so a press anywhere on
//! the window reaches it: a press on a row selects and closes, any other
//! press closes without a change. The current row is drawn in the selection
//! role, never the accent — a chosen thing is a state, not a button.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};

use crate::set::{WidgetDefaults, reply_if_hidden};
use crate::state::{InteractionState, emit_state_changed};
use crate::theme::Theme;
use crate::{Collect, DropdownConfig, SetWidgetState, WidgetDrawList, WidgetFrame};

/// The dropdown widget. Holds its options and current choice plus the
/// cached theme / frame.
pub struct DropdownWidget {
    options: Vec<String>,
    selected_index: Option<usize>,
    placeholder: String,
    open_row_count: u32,
    open: bool,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
}

impl WidgetDefaults for DropdownWidget {
    fn widget_frame(&mut self) -> &mut WidgetFrame {
        &mut self.frame
    }

    fn widget_theme(&mut self) -> &mut Theme {
        &mut self.theme
    }

    fn widget_state(&mut self) -> &mut InteractionState {
        &mut self.state
    }

    fn cancel_activation(&mut self) {
        self.open = false;
    }
}

/// A dropdown. Spawned inline by a panel root with a [`DropdownConfig`];
/// reports [`crate::DropdownSelected`] on a change of choice and
/// [`crate::DropdownOpenChanged`] as its list opens and closes.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send
/// it its `DropdownConfig` again to replace the options or the choice in
/// place.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for DropdownWidget {
    type Config = DropdownConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.dropdown";

    fn init(config: DropdownConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(DropdownWidget {
            selected_index: initial_selection(config.initial_selected_index, config.options.len()),
            options: config.options,
            placeholder: config.placeholder,
            open_row_count: config.open_row_count,
            open: false,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
        })
    }

    /// Replace the options / choice / theme in place from a re-sent config.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: DropdownConfig) {
        self.selected_index = initial_selection(config.initial_selected_index, config.options.len());
        self.options = config.options;
        self.placeholder = config.placeholder;
        self.open_row_count = config.open_row_count;
        self.open = false;
        self.theme = config.theme;
        if self.state.replace(config.state) {
            emit_state_changed(ctx, &self.state);
        }
    }

    /// Update external availability; an unavailable dropdown closes.
    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        if self.state.replace(set.state) {
            emit_state_changed(ctx, &self.state);
        }
        if !self.state.is_available() {
            self.open = false;
        }
    }

    /// Reply the dropdown's local draw.
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

/// The boot selection clamped into the option vector; `None` when there is
/// nothing to select or nothing was asked for.
fn initial_selection(initial_selected_index: Option<u32>, option_count: usize) -> Option<usize> {
    let index = usize::try_from(initial_selected_index?).ok()?;
    (option_count > 0).then(|| index.min(option_count - 1))
}
