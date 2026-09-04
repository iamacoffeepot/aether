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
//!
//! Sizing a title to its text and right-aligning an accelerator both need
//! real glyph widths, so the bar drives the same single-flight
//! [`FontMetricsRequest`](aether_text::FontMetricsRequest) the tab strip and
//! the text controls do. One place computes the title widths and the plate
//! rect, and the draw and the hit test both read it, so a press always lands
//! on the title or the item the reader sees under the pointer.

use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::keycode::{KEY_DOWN, KEY_ENTER, KEY_ESCAPE, KEY_LEFT, KEY_RIGHT, KEY_UP};
use aether_kinds::mouse_button;
use aether_kinds::{Key, MouseButton, MouseButtonRelease, MouseMove};
use aether_text::FontMetricsResult;

use crate::set::{
    WidgetDefaults, accept_font_metrics_result, apply_text_theme, approx_text_width, even_split_widths,
    measured_text_width, pump_text_font_metrics, push_control_outlines, push_rect_border, quad, reply_if_hidden,
    slot_at_local_x, slot_left, text_origin_y,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, Theme, ThemeState};
use crate::{
    Collect, FocusLost, HoverLost, Menu, MenuBarConfig, MenuItem, MenuItemActivated, MenuOpenChanged, SetWidgetState,
    WidgetDrawItem, WidgetDrawList, WidgetFrame,
};

/// Thickness, in pixels, of the plate's outline ring and of an item divider.
/// One pixel: a divider is a hairline the eye groups items by, never a rule
/// that competes with the item labels.
const HAIRLINE_THICKNESS: f32 = 1.0;

/// What one state transition owes the parent: at most one activation and at
/// most one open/closed edge. Returned by the pure transition methods so the
/// handlers own the sending and the tests own nothing but the logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct MenuBarEffects {
    activated: Option<MenuItemActivated>,
    open_changed: Option<bool>,
}

impl MenuBarEffects {
    fn opened() -> Self {
        Self { activated: None, open_changed: Some(true) }
    }

    fn closed() -> Self {
        Self { activated: None, open_changed: Some(false) }
    }

    /// The activation first, the open edge second: a consumer sees the command
    /// it invoked before the menu reports itself gone.
    fn emit(self, ctx: &WasmCtx<'_>) {
        let Some(parent) = ctx.parent() else {
            return;
        };
        if let Some(activated) = self.activated {
            parent.send(&activated);
        }
        if let Some(open) = self.open_changed {
            parent.send(&MenuOpenChanged { open });
        }
    }
}

/// The menu bar widget. Holds its menus and which one is open plus the
/// cached theme / frame, the per-title pointer state, and the single-flight
/// font-metrics adapter the title widths are measured against.
pub struct MenuBarWidget {
    menus: Vec<Menu>,
    open_menu: Option<usize>,
    /// The item the pointer or the arrow keys are on inside the open plate.
    /// Only ever an enabled item — a disabled one is not a thing to land on.
    highlighted_item: Option<usize>,
    /// The title a left press armed while every menu was closed; the matching
    /// release back inside that same title opens it.
    pressed_title: Option<usize>,
    hovered_title: Option<usize>,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    /// Single-flight exact metrics for the active theme font.
    font_metrics: FontMetricsAdapter,
}

impl MenuBarWidget {
    /// Per-title pixel widths, left to right: each title's measured width plus
    /// one `pad` either side, so padding a title off its left edge *is*
    /// centering it.
    ///
    /// Until the metrics resolve there is no honest width to lay out from, so
    /// the bar splits the row evenly as an interim — the layout that holds for
    /// the frame or two between the first `Collect` and the metrics reply,
    /// never the intended look.
    fn title_widths(&self) -> Vec<f32> {
        let size = self.theme.label_size_pixels;
        let metrics = self.font_metrics.resolved();
        let measured: Option<Vec<f32>> = self
            .menus
            .iter()
            .map(|menu| {
                metrics.map(|metrics| self.theme.pad.mul_add(2.0, measured_text_width(metrics, &menu.title, size)))
            })
            .collect();
        measured.unwrap_or_else(|| even_split_widths(self.menus.len(), self.frame.width, self.theme.space(1)))
    }

    /// The title under a window-pixel pointer position. The bar holds the
    /// pointer grab while a menu is open, so every window position reaches it
    /// and the row's own vertical extent has to be part of the test.
    fn title_at(&self, x: f32, y: f32) -> Option<usize> {
        if !y.is_finite() || !self.frame.height.is_finite() || y < self.frame.y || y >= self.frame.y + self.frame.height
        {
            return None;
        }
        slot_at_local_x(&self.title_widths(), self.theme.space(1), x - self.frame.x)
    }

    /// One line's width at the body size: measured once the font's metrics
    /// resolve, and the crate's per-character approximation before that — the
    /// same interim frame or two the even title split covers.
    fn text_width(&self, text: &str) -> f32 {
        let size = self.theme.label_size_pixels;
        self.font_metrics.resolved().map_or_else(
            || approx_text_width(text.chars().count(), size),
            |metrics| measured_text_width(metrics, text, size),
        )
    }

    /// One item's content width: its label, plus a column gap and its
    /// accelerator when it advertises one.
    fn item_content_width(&self, item: &MenuItem) -> f32 {
        let label = self.text_width(&item.label);
        if item.shortcut.is_empty() {
            label
        } else {
            label + self.theme.space(2) + self.text_width(&item.shortcut)
        }
    }

    /// The plate's width: the widest item's content plus one `pad` either
    /// side, never narrower than the title it hangs from — a plate that
    /// undercut its own title would read as a misplaced popup.
    fn plate_width(&self, menu: &Menu, title_width: f32) -> f32 {
        let content = menu.items.iter().map(|item| self.item_content_width(item)).fold(0.0_f32, f32::max);
        self.theme.pad.mul_add(2.0, content).max(title_width)
    }

    /// The space a divider takes: the hairline with one `space(1)` either
    /// side, so the rule separates two groups instead of crowding one.
    fn divider_band(&self) -> f32 {
        self.theme.space(1).mul_add(2.0, HAIRLINE_THICKNESS)
    }

    /// The vertical extent one item occupies: its row, plus the divider band
    /// when it asks for a separator and there is a next item to separate it
    /// from. A trailing `separator_after` draws nothing — a rule against the
    /// plate's own bottom edge reads as a mistake.
    fn item_extent(&self, item: &MenuItem, last: bool) -> f32 {
        if item.separator_after && !last {
            self.theme.row_height + self.divider_band()
        } else {
            self.theme.row_height
        }
    }

    fn plate_height(&self, menu: &Menu) -> f32 {
        let last = menu.items.len().saturating_sub(1);
        menu.items.iter().enumerate().map(|(index, item)| self.item_extent(item, index == last)).sum()
    }

    /// The open menu with its plate's widget-local left edge and width — the
    /// one place the overlay draw and the item hit test agree on geometry.
    /// `None` while every menu is closed.
    fn open_plate(&self) -> Option<(&Menu, f32, f32)> {
        let index = self.open_menu?;
        let menu = self.menus.get(index)?;
        let widths = self.title_widths();
        let title_width = widths.get(index).copied().unwrap_or_default();
        Some((menu, slot_left(&widths, self.theme.space(1), index), self.plate_width(menu, title_width)))
    }

    /// The item under a window-pixel pointer position, or `None` when every
    /// menu is closed, the position misses the plate, or it lands in a divider
    /// band — the gap between two groups belongs to neither, exactly as the
    /// gap between two titles does.
    fn item_at(&self, x: f32, y: f32) -> Option<usize> {
        let row_height = self.theme.row_height;
        if !row_height.is_finite() || row_height <= 0.0 || !x.is_finite() || !y.is_finite() {
            return None;
        }
        if !self.frame.height.is_finite() {
            return None;
        }
        let (menu, plate_left, plate_width) = self.open_plate()?;
        let left = self.frame.x + plate_left;
        if x < left || x >= left + plate_width {
            return None;
        }

        let last = menu.items.len().saturating_sub(1);
        let mut top = self.frame.y + self.frame.height;
        for (index, item) in menu.items.iter().enumerate() {
            if y >= top && y < top + row_height {
                return Some(index);
            }
            top += self.item_extent(item, index == last);
        }
        None
    }

    fn item_enabled(&self, index: usize) -> bool {
        self.open_menu
            .and_then(|menu| self.menus.get(menu))
            .and_then(|menu| menu.items.get(index))
            .is_some_and(|item| item.enabled)
    }

    /// Open `index` on its first enabled item. Refused for an unavailable bar
    /// and for a menu with no items to show — an empty plate under a pointer
    /// grab is a trap, not a menu. Switching from one open menu to another
    /// reports nothing: the bar was already open, and the root already holds
    /// the grab.
    fn open_at(&mut self, index: usize) -> MenuBarEffects {
        if !self.state.is_available() || self.open_menu == Some(index) {
            return MenuBarEffects::default();
        }
        if self.menus.get(index).is_none_or(|menu| menu.items.is_empty()) {
            return MenuBarEffects::default();
        }
        let switching = self.open_menu.is_some();
        self.open_menu = Some(index);
        self.highlighted_item = self.first_enabled(index);
        if switching {
            MenuBarEffects::default()
        } else {
            MenuBarEffects::opened()
        }
    }

    /// Close without activating. A no-op — and silent — when nothing is open,
    /// so the root's grab is ended exactly once however the close arrived.
    fn dismiss(&mut self) -> MenuBarEffects {
        if self.open_menu.is_none() {
            return MenuBarEffects::default();
        }
        self.open_menu = None;
        self.highlighted_item = None;
        MenuBarEffects::closed()
    }

    /// Activate `item` of the open menu and close. A disabled item does
    /// nothing at all — not even close — so a mis-aimed press leaves the menu
    /// standing to try again.
    fn activate(&mut self, item: usize) -> MenuBarEffects {
        let Some(menu) = self.open_menu else {
            return MenuBarEffects::default();
        };
        if !self.state.is_available() || !self.item_enabled(item) {
            return MenuBarEffects::default();
        }
        let mut effects = self.dismiss();
        effects.activated = activation(menu, item);
        effects
    }

    /// Any left press while a menu is open: on an enabled item it activates,
    /// on a disabled one nothing, anywhere else — a title included — it
    /// closes, so pressing the open title reads as the toggle it looks like.
    fn press_while_open(&mut self, x: f32, y: f32) -> MenuBarEffects {
        match self.item_at(x, y) {
            Some(item) => self.activate(item),
            None => self.dismiss(),
        }
    }

    fn first_enabled(&self, menu: usize) -> Option<usize> {
        self.menus.get(menu)?.items.iter().position(|item| item.enabled)
    }

    /// Step the highlighted item, skipping disabled ones and clamping at the
    /// ends. Silent: nothing is activated until Enter or a press says so.
    fn step_item(&mut self, forward: bool) {
        let Some(menu) = self.open_menu.and_then(|index| self.menus.get(index)) else {
            return;
        };
        let next = self.highlighted_item.map_or_else(
            || menu.items.iter().position(|item| item.enabled),
            |current| next_enabled(&menu.items, current, forward),
        );
        if next.is_some() {
            self.highlighted_item = next;
        }
    }

    /// Move to the adjacent menu, clamping at the ends — the keyboard's
    /// version of the pointer crossing onto another title.
    fn step_menu(&mut self, forward: bool) -> MenuBarEffects {
        let Some(current) = self.open_menu else {
            return MenuBarEffects::default();
        };
        let next = if forward {
            (current + 1).min(self.menus.len().saturating_sub(1))
        } else {
            current.saturating_sub(1)
        };
        self.open_at(next)
    }

    /// The bar row: its own surface, a lit fill under the open or hovered
    /// title, each title at the body size, and the common validation / focus
    /// outlines.
    fn draw_items(&self) -> Vec<WidgetDrawItem> {
        let width = self.frame.width;
        let height = self.frame.height;
        let size = self.theme.label_size_pixels;
        let gap = self.theme.space(1);
        let text_y = text_origin_y(0.0, height, size);
        let ink_state = if self.state.control().enabled {
            ThemeState::Normal
        } else {
            ThemeState::Disabled
        };

        let mut items = Vec::with_capacity(self.menus.len().saturating_mul(2).saturating_add(5));
        items.push(quad(0.0, 0.0, width, height, self.theme.fill(self.theme.surface_raised, ink_state)));
        let mut left = 0.0;
        for (index, (menu, title_width)) in self.menus.iter().zip(self.title_widths()).enumerate() {
            if self.open_menu == Some(index) || self.hovered_title == Some(index) {
                let lit = self.theme.fill(self.theme.surface_raised, ThemeState::Hover);
                items.push(quad(left, 0.0, title_width, height, lit));
            }
            if !menu.title.is_empty() {
                items.push(WidgetDrawItem::Text {
                    x: left + self.theme.pad,
                    y: text_y,
                    font_id: self.theme.font_id,
                    text: menu.title.clone(),
                    size_pixels: size,
                    color: self.theme.fill(self.theme.text_primary, ink_state),
                    clip: None,
                });
            }
            left += title_width + gap;
        }

        push_control_outlines(&mut items, width, height, &self.state, &self.theme);
        items
    }

    /// The open menu's plate, in the widget's own local coordinates below its
    /// title. Empty while closed. Nothing here is clipped by the slot — that
    /// is what the overlay layer buys.
    fn overlay_items(&self) -> Vec<WidgetDrawItem> {
        let row_height = self.theme.row_height;
        if !row_height.is_finite() || row_height <= 0.0 || !self.frame.height.is_finite() {
            return Vec::new();
        }
        let Some((menu, left, width)) = self.open_plate() else {
            return Vec::new();
        };
        if !width.is_finite() || width <= 0.0 {
            return Vec::new();
        }

        let top = self.frame.height;
        let plate_height = self.plate_height(menu);
        let size = self.theme.label_size_pixels;
        let pad = self.theme.pad;
        let last = menu.items.len().saturating_sub(1);
        let mut items = Vec::with_capacity(menu.items.len().saturating_mul(3).saturating_add(5));
        items.push(quad(left, top, width, plate_height, self.theme.surface_raised));

        let mut row_top = top;
        for (index, item) in menu.items.iter().enumerate() {
            if self.highlighted_item == Some(index) {
                let lit = self.theme.fill(self.theme.surface_raised, ThemeState::Hover);
                items.push(quad(left, row_top, width, row_height, lit));
            }
            let text_y = text_origin_y(row_top, row_height, size);
            if !item.label.is_empty() {
                items.push(WidgetDrawItem::Text {
                    x: left + pad,
                    y: text_y,
                    font_id: self.theme.font_id,
                    text: item.label.clone(),
                    size_pixels: size,
                    color: if item.enabled {
                        self.theme.text_primary
                    } else {
                        self.theme.text_muted
                    },
                    clip: None,
                });
            }
            if !item.shortcut.is_empty() {
                items.push(WidgetDrawItem::Text {
                    x: left + width - pad - self.text_width(&item.shortcut),
                    y: text_y,
                    font_id: self.theme.font_id,
                    text: item.shortcut.clone(),
                    size_pixels: size,
                    color: self.theme.text_muted,
                    clip: None,
                });
            }
            if item.separator_after && index != last {
                let divider_y = row_top + row_height + self.theme.space(1);
                items.push(quad(left, divider_y, width, HAIRLINE_THICKNESS, self.theme.outline));
            }
            row_top += self.item_extent(item, index == last);
        }

        push_rect_border(&mut items, left, top, width, plate_height, HAIRLINE_THICKNESS, self.theme.outline);
        items
    }
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
        self.pressed_title = None;
        self.open_menu = None;
        self.highlighted_item = None;
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
        let desired_font_id = config.theme.font_id;
        Ok(MenuBarWidget {
            menus: config.menus,
            open_menu: None,
            highlighted_item: None,
            pressed_title: None,
            hovered_title: None,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            font_metrics: FontMetricsAdapter::new(desired_font_id),
        })
    }

    /// Kick off the font-metrics request for the initial theme font; the title
    /// widths and the accelerator column depend on it.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Replace the menus / theme in place from a re-sent config. An open menu
    /// closes, so the root gives up its pointer grab rather than holding it
    /// for a plate the new config may not describe.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: MenuBarConfig) {
        let closed = self.dismiss();
        self.menus = config.menus;
        self.pressed_title = None;
        self.hovered_title = None;
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        closed.emit(ctx);
        if self.state.replace(config.state) {
            emit_state_changed(ctx, &self.state);
        }
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Update external availability; a bar whose commands can no longer be
    /// reached closes its menu.
    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        if self.state.replace(set.state) {
            emit_state_changed(ctx, &self.state);
        }
        if !self.state.is_available() {
            self.pressed_title = None;
            self.hovered_title = None;
            self.dismiss().emit(ctx);
        }
    }

    /// Restyle: adopt the fanned theme and request metrics for its font.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        apply_text_theme(ctx, &mut self.font_metrics, &mut self.theme, set.theme);
    }

    /// Install a font-metrics reply; the next `Collect` lays the titles out
    /// against their real widths.
    #[handler::single]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_>, result: FontMetricsResult) {
        accept_font_metrics_result(ctx, &mut self.font_metrics, result);
    }

    /// Focus loss closes the menu. Overrides the shared default because
    /// `cancel_activation` cannot report the close, and an unreported close
    /// would leave the root holding a grab for a plate nobody can see.
    #[handler::single]
    fn on_focus_lost(&mut self, ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        self.state.lose_focus();
        self.pressed_title = None;
        self.dismiss().emit(ctx);
    }

    /// Leaving the bar clears the per-title hover as well as the widget's.
    #[handler::single]
    fn on_hover_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: HoverLost) {
        self.state.set_hovered(false);
        self.hovered_title = None;
    }

    /// While a menu is open every left press is the bar's: on an item it
    /// activates, anywhere else it closes. While closed a press on a title
    /// arms it for its matching release.
    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button != mouse_button::LEFT {
            return;
        }
        if self.open_menu.is_some() {
            self.press_while_open(press.x, press.y).emit(ctx);
        } else if self.state.is_available() {
            self.pressed_title = self.title_at(press.x, press.y);
        }
    }

    /// A left release back inside the armed title opens its menu.
    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if release.button != mouse_button::LEFT {
            return;
        }
        if let Some(armed) = self.pressed_title.take()
            && Some(armed) == self.title_at(release.x, release.y)
        {
            self.open_at(armed).emit(ctx);
        }
    }

    /// Motion tracks the title under the pointer, and while a menu is open it
    /// also switches menus and moves the highlight. The root forwards every
    /// move to the grabbed child, so this tracks the pointer over the plate
    /// rows that lie outside the bar's own slot.
    #[handler::single]
    fn on_mouse_move(&mut self, ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if !self.state.is_available() {
            return;
        }
        self.hovered_title = self.title_at(moved.x, moved.y);
        if self.open_menu.is_none() {
            return;
        }
        if let Some(title) = self.hovered_title {
            self.open_at(title).emit(ctx);
        }
        self.highlighted_item = self.item_at(moved.x, moved.y).filter(|&item| self.item_enabled(item));
    }

    /// Escape closes; while a menu is open Left/Right walk the menus,
    /// Up/Down walk the enabled items, and Enter activates the highlighted
    /// one. The bar opens by pointer only — a menu is a place to look, not a
    /// control the keyboard falls into.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        match key.code {
            KEY_ESCAPE => self.dismiss().emit(ctx),
            KEY_LEFT => self.step_menu(false).emit(ctx),
            KEY_RIGHT => self.step_menu(true).emit(ctx),
            KEY_UP => self.step_item(false),
            KEY_DOWN => self.step_item(true),
            KEY_ENTER => {
                if let Some(item) = self.highlighted_item {
                    self.activate(item).emit(ctx);
                }
            }
            _ => {}
        }
    }

    /// Reply the bar's local draw: the row of titles as ordinary items, the
    /// open menu's plate as overlay.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList {
                content_height: None,
                intrinsic: None,
                items: self.draw_items(),
                overlay: self.overlay_items(),
            });
        }
    }
}

/// The nearest enabled item on `forward`'s side of `from`, or `None` when the
/// walk runs off the end — the highlight clamps rather than wrapping, like
/// every other keyboard step in the set.
fn next_enabled(items: &[MenuItem], from: usize, forward: bool) -> Option<usize> {
    if forward {
        items.iter().enumerate().skip(from.saturating_add(1)).find_map(|(index, item)| item.enabled.then_some(index))
    } else {
        items[..from.min(items.len())].iter().enumerate().rev().find_map(|(index, item)| item.enabled.then_some(index))
    }
}

/// The activation event for a menu/item pair, or `None` for indices past the
/// wire's `u32` — unreachable for any authored menu, and silently dropping the
/// event beats reporting the wrong command.
fn activation(menu: usize, item: usize) -> Option<MenuItemActivated> {
    Some(MenuItemActivated { menu: u32::try_from(menu).ok()?, item: u32::try_from(item).ok()? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WidgetControlState;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;

    fn item(label: &str, enabled: bool, separator_after: bool) -> MenuItem {
        MenuItem { label: String::from(label), shortcut: String::new(), enabled, separator_after }
    }

    /// Two menus of three items each. The second item of the second menu is
    /// disabled and the first carries a separator, so one fixture exercises
    /// the skip and the divider band.
    fn menu_bar() -> MenuBarWidget {
        let menus = vec![
            Menu {
                title: String::from("File"),
                items: (0..3).map(|index| item(&format!("file {index}"), true, false)).collect(),
            },
            Menu {
                title: String::from("Edit"),
                items: vec![item("undo", true, true), item("redo", false, false), item("paste", true, false)],
            },
        ];
        MenuBarWidget {
            menus,
            open_menu: None,
            highlighted_item: None,
            pressed_title: None,
            hovered_title: None,
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 10.0, y: 20.0, width: 120.0, height: 24.0 },
            state: InteractionState::new(WidgetControlState::default()),
            font_metrics: FontMetricsAdapter::new(0),
        }
    }

    fn opened(index: usize) -> MenuBarWidget {
        let mut widget = menu_bar();
        assert_eq!(widget.open_at(index), MenuBarEffects::opened());
        widget
    }

    /// A window-pixel x inside the open plate — the second menu's plate hangs
    /// under its own title, not under the bar's left edge.
    fn plate_x(widget: &MenuBarWidget) -> f32 {
        let (_, left, _) = widget.open_plate().expect("an open plate");
        widget.frame.x + left + 1.0
    }

    fn row_text(items: &[WidgetDrawItem]) -> Vec<&str> {
        items
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Text { text, .. } => Some(text.as_str()),
                WidgetDrawItem::Quad { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
            })
            .collect()
    }

    #[test]
    fn a_title_opens_once_and_switching_menus_reports_nothing() {
        let mut widget = menu_bar();
        assert_eq!(widget.open_at(0), MenuBarEffects::opened());
        assert_eq!(widget.open_at(0), MenuBarEffects::default(), "the open menu re-opened reports nothing");
        assert_eq!(widget.open_at(1), MenuBarEffects::default(), "a switch is not a second open edge");
        assert_eq!(widget.open_menu, Some(1));
        assert_eq!(widget.dismiss(), MenuBarEffects::closed());
        assert_eq!(widget.dismiss(), MenuBarEffects::default(), "already closed reports nothing");
    }

    #[test]
    fn opening_highlights_the_first_enabled_item_and_arrows_skip_the_disabled_ones() {
        let mut widget = opened(1);
        assert_eq!(widget.highlighted_item, Some(0));
        widget.step_item(true);
        assert_eq!(widget.highlighted_item, Some(2), "the disabled item is stepped over, not landed on");
        widget.step_item(true);
        assert_eq!(widget.highlighted_item, Some(2), "the walk clamps at the last item");
        widget.step_item(false);
        assert_eq!(widget.highlighted_item, Some(0));
        widget.step_item(false);
        assert_eq!(widget.highlighted_item, Some(0), "the walk clamps at the first item");
    }

    #[test]
    fn a_menu_of_only_disabled_items_highlights_nothing() {
        let mut widget = menu_bar();
        widget.menus[0].items = vec![item("one", false, false), item("two", false, false)];
        assert_eq!(widget.open_at(0), MenuBarEffects::opened());
        assert_eq!(widget.highlighted_item, None);
        widget.step_item(true);
        assert_eq!(widget.highlighted_item, None);
    }

    #[test]
    fn a_press_on_an_enabled_item_activates_and_closes_once() {
        let mut widget = opened(0);
        // The plate hangs at frame.y + frame.height = 44, one row_height (24)
        // per item, under the first title's left edge.
        let hit = widget.press_while_open(20.0, 44.0 + 24.0);
        assert_eq!(
            hit,
            MenuBarEffects { activated: Some(MenuItemActivated { menu: 0, item: 1 }), open_changed: Some(false) }
        );
        assert_eq!(widget.open_menu, None);
    }

    #[test]
    fn a_press_on_a_disabled_item_does_nothing_and_leaves_the_menu_standing() {
        let mut widget = opened(1);
        // The second Edit item is disabled, and the first carries a separator,
        // so its row starts one divider band below the first row's end.
        let x = plate_x(&widget);
        let second_row_y = 44.0 + 24.0 + widget.divider_band();
        assert_eq!(widget.item_at(x, second_row_y), Some(1));
        assert_eq!(widget.press_while_open(x, second_row_y), MenuBarEffects::default());
        assert_eq!(widget.open_menu, Some(1), "a refused activation is not a close");
    }

    #[test]
    fn a_press_off_every_item_closes_without_activating() {
        let mut widget = opened(0);
        assert_eq!(widget.press_while_open(20.0, 30.0), MenuBarEffects::closed(), "a press back on the bar closes");
        assert_eq!(widget.open_menu, None);

        let mut widget = opened(0);
        assert_eq!(widget.press_while_open(1000.0, 1000.0), MenuBarEffects::closed());
    }

    #[test]
    fn items_are_hit_tested_below_the_title_with_an_exclusive_bottom_and_a_dividing_band() {
        let widget = opened(1);
        let plate_top = widget.frame.y + widget.frame.height;
        let x = plate_x(&widget);
        assert_eq!(widget.item_at(x, plate_top - 0.1), None, "the bar row is not an item row");
        assert_eq!(widget.item_at(x, plate_top), Some(0));
        assert_eq!(widget.item_at(x, plate_top + 23.999), Some(0));
        assert_eq!(widget.item_at(x, plate_top + 24.0), None, "the divider band belongs to no item");
        assert_eq!(widget.item_at(x, plate_top + 24.0 + widget.divider_band()), Some(1));
        assert_eq!(widget.item_at(x, plate_top + widget.plate_height(&widget.menus[1])), None, "past the last row");
        assert_eq!(widget.item_at(x - 2.0, plate_top), None, "left of the plate");
        assert_eq!(widget.item_at(f32::NAN, plate_top), None);

        assert_eq!(menu_bar().item_at(x, plate_top), None, "a closed bar has no item rows");
    }

    #[test]
    fn a_title_is_hit_tested_across_the_bar_row_only() {
        let widget = menu_bar();
        assert_eq!(widget.title_at(20.0, 20.0), Some(0), "the row's top edge is inclusive");
        assert_eq!(widget.title_at(20.0, 43.999), Some(0));
        assert_eq!(widget.title_at(20.0, 44.0), None, "the row's bottom edge is exclusive");
        assert_eq!(widget.title_at(20.0, 19.9), None, "above the row");
        // The pre-metrics split is even, so the second half of a 120-wide bar
        // is the second title.
        assert_eq!(widget.title_at(120.0, 30.0), Some(1));
        assert_eq!(widget.title_at(f32::NAN, 30.0), None);
    }

    #[test]
    fn an_empty_menu_and_an_unavailable_bar_never_open() {
        let mut empty = menu_bar();
        empty.menus[0].items.clear();
        assert_eq!(empty.open_at(0), MenuBarEffects::default());
        assert_eq!(empty.open_at(9), MenuBarEffects::default(), "a menu that is not there cannot open");
        assert_eq!(empty.open_menu, None);

        for control in [
            WidgetControlState { enabled: false, ..WidgetControlState::default() },
            WidgetControlState { visible: false, ..WidgetControlState::default() },
        ] {
            let mut widget = menu_bar();
            widget.state.replace(control);
            assert_eq!(widget.open_at(0), MenuBarEffects::default());
            assert_eq!(widget.open_menu, None);
        }
    }

    #[test]
    fn the_keyboard_walks_the_menus_and_activates_the_highlighted_item() {
        let mut widget = opened(0);
        assert_eq!(widget.step_menu(false), MenuBarEffects::default(), "left at the first menu is clamped");
        assert_eq!(widget.open_menu, Some(0));
        assert_eq!(widget.step_menu(true), MenuBarEffects::default(), "a switch reports no new open edge");
        assert_eq!(widget.open_menu, Some(1));
        assert_eq!(widget.highlighted_item, Some(0), "the new menu highlights its own first enabled item");

        widget.step_item(true);
        assert_eq!(
            widget.activate(widget.highlighted_item.expect("a highlight")),
            MenuBarEffects { activated: Some(MenuItemActivated { menu: 1, item: 2 }), open_changed: Some(false) }
        );
    }

    #[test]
    fn the_overlay_is_empty_while_closed_and_draws_the_open_menu_with_its_divider() {
        let widget = menu_bar();
        assert!(widget.overlay_items().is_empty(), "a closed bar draws no overlay");
        assert_eq!(row_text(&widget.draw_items()), vec!["File", "Edit"]);

        let mut widget = opened(1);
        widget.menus[1].items[2].shortcut = String::from("Cmd+V");
        assert_eq!(row_text(&widget.overlay_items()), vec!["undo", "redo", "paste", "Cmd+V"]);
        assert_eq!(
            widget.plate_height(&widget.menus[1]),
            3.0f32.mul_add(Theme::DEFAULT.row_height, widget.divider_band()),
            "the separator adds its band to the plate, once",
        );
    }
}
