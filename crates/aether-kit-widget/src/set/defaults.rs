//! The handler set the interactive stock widgets adopt (ADR-0169).
//!
//! These are the handlers that absorb ambient state the panel root pushes
//! down — layout rect, theme, focus, and hover. None of them is widget
//! behavior: a widget's own handlers are the ones that read input and answer
//! [`Collect`](crate::Collect) with a draw list.
//!
//! One hook carries what varies. [`cancel_activation`] releases whatever
//! half-finished interaction the widget tracks — an armed press, a live drag,
//! a pending IME composition — which is the only reason `on_focus_lost`
//! differed across the family. A widget whose focus loss does more than
//! release (committing an edit buffer, say) overrides that handler outright.
//!
//! `SetWidgetState` is deliberately absent. Its per-widget bodies disagree on
//! which predicate cancels an activation — a momentary button cancels only
//! when it becomes unavailable, most controls also cancel on read-only, and
//! the text widgets split the two tiers across composition and drag — so a
//! shared body would have to pick one and quietly change the rest.
//!
//! [`cancel_activation`]: WidgetDefaults::cancel_activation

use aether_actor::{WasmCtx, handler_set};

use crate::state::InteractionState;
use crate::theme::{SetTheme, Theme};
use crate::{FocusGained, FocusLost, HoverGained, HoverLost, WidgetFrame};

#[handler_set]
pub trait WidgetDefaults {
    /// The widget's cached layout rect, assigned by the panel root.
    fn widget_frame(&mut self) -> &mut WidgetFrame;

    /// The widget's cached theme.
    fn widget_theme(&mut self) -> &mut Theme;

    /// The widget's focus / hover / control state.
    fn widget_state(&mut self) -> &mut InteractionState;

    /// Release any half-finished interaction: an armed press, a live drag, a
    /// pending IME composition. Called on focus loss, and callable from a
    /// widget's own control-state handler.
    fn cancel_activation(&mut self);

    /// Cache the layout rect the root assigned.
    #[handler::single]
    fn on_frame(&mut self, _ctx: &mut WasmCtx<'_>, frame: WidgetFrame) {
        *self.widget_frame() = frame;
    }

    /// Restyle: adopt the fanned theme.
    #[handler::single]
    fn on_set_theme(&mut self, _ctx: &mut WasmCtx<'_>, set: SetTheme) {
        *self.widget_theme() = set.theme;
    }

    /// Take focus, carrying through how it arrived so only a keyboard
    /// traversal lights a ring.
    #[handler::single]
    fn on_focus_gained(&mut self, _ctx: &mut WasmCtx<'_>, gained: FocusGained) {
        self.widget_state().gain_focus(gained.keyboard);
    }

    /// Release keyboard focus, cancelling any activation it was carrying.
    #[handler::single]
    fn on_focus_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        self.widget_state().lose_focus();
        self.cancel_activation();
    }

    /// Enter hover.
    #[handler::single]
    fn on_hover_gained(&mut self, _ctx: &mut WasmCtx<'_>, _gained: HoverGained) {
        self.widget_state().set_hovered(true);
    }

    /// Leave hover.
    #[handler::single]
    fn on_hover_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: HoverLost) {
        self.widget_state().set_hovered(false);
    }
}
