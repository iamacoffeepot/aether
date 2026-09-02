// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The menu bar: one row of application menus.
//!
//! The bar is the place a screen's commands live — File, Edit, View, Help —
//! so a verb that is not a control on the pane still has an address a
//! person can find. Each title is sized to its text plus padding; a press
//! on a title opens that menu's items in the widget's overlay
//! ([`crate::WidgetDrawList::overlay`]) below the title, under the root's
//! pointer grab ([`crate::MenuOpenChanged`]); while open, the pointer moving
//! over another title opens that one instead. A press on an enabled item
//! activates it ([`crate::MenuItemActivated`]) and closes; Escape or a press
//! elsewhere closes without activating. Items advertise their accelerator
//! at the right edge in muted ink; the accelerator itself is the root's to
//! honour.

use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};

use crate::set::{WidgetDefaults, reply_if_hidden};
use crate::state::{InteractionState, emit_state_changed};
use crate::theme::Theme;
use crate::{Collect, Menu, MenuBarConfig, SetWidgetState, WidgetDrawList, WidgetFrame};

/// The menu bar widget. Holds its menus and which one is open plus the
/// cached theme / frame.
pub struct MenuBarWidget {
    menus: Vec<Menu>,
    open_menu: Option<usize>,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
}

impl WidgetDefaults for MenuBarWidget {
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
        self.open_menu = None;
    }
}

/// A menu bar. Spawned inline by a panel root with a [`MenuBarConfig`];
/// reports [`crate::MenuItemActivated`] on an activation and
/// [`crate::MenuOpenChanged`] as its menus open and close.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send
/// it its `MenuBarConfig` again to replace the menus in place.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for MenuBarWidget {
    type Config = MenuBarConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.menu_bar";

    fn init(config: MenuBarConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(MenuBarWidget {
            menus: config.menus,
            open_menu: None,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
        })
    }

    /// Replace the menus / theme in place from a re-sent config.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: MenuBarConfig) {
        self.menus = config.menus;
        self.open_menu = None;
        self.theme = config.theme;
        if self.state.replace(config.state) {
            emit_state_changed(ctx, &self.state);
        }
    }

    /// Update external availability; an unavailable bar closes its menu.
    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        if self.state.replace(set.state) {
            emit_state_changed(ctx, &self.state);
        }
        if !self.state.is_available() {
            self.open_menu = None;
        }
    }

    /// Reply the bar's local draw.
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
