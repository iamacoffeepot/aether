//! Shared stock-widget interaction state.
//!
//! External control state comes from config or `SetWidgetState`; focus and
//! hover are frame-local facts delivered by the panel root. Keeping those
//! sources separate prevents a restyle or availability update from resetting a
//! value while still giving every control one fill-priority rule.

use aether_actor::WasmCtx;
use aether_math::Rgba;

use crate::theme::{Theme, ThemeState};
use crate::{WidgetControlState, WidgetStateChanged, WidgetValidation};

#[derive(Debug, Clone)]
pub struct InteractionState {
    control: WidgetControlState,
    focused: bool,
    /// How the focus this widget holds arrived. A ring is the keyboard's
    /// "you are here" marker, so only keyboard-arrived focus draws one; the
    /// caret and the routing consequences of focus do not consult this.
    focus_from_keyboard: bool,
    hovered: bool,
}

impl InteractionState {
    #[must_use]
    pub(super) fn new(control: WidgetControlState) -> Self {
        Self { control, focused: false, focus_from_keyboard: false, hovered: false }
    }

    #[must_use]
    pub(super) fn control(&self) -> &WidgetControlState {
        &self.control
    }

    /// Replace external state and clear interaction facts that cannot survive
    /// unavailability. Returns whether the external value actually changed.
    pub(super) fn replace(&mut self, next: WidgetControlState) -> bool {
        if self.control == next {
            return false;
        }
        self.control = next;
        if !self.is_available() {
            self.clear_transient();
        }
        true
    }

    #[must_use]
    pub(super) fn is_visible(&self) -> bool {
        self.control.visible
    }

    #[must_use]
    pub(super) fn is_available(&self) -> bool {
        self.control.visible && self.control.enabled
    }

    #[must_use]
    pub(super) fn can_mutate(&self) -> bool {
        self.is_available() && !self.control.read_only
    }

    /// Take focus, recording whether it arrived from the keyboard — the root
    /// passes `true` for Tab traversal, `false` for a pointer press or an
    /// availability move (see [`crate::FocusGained`]).
    pub(super) fn gain_focus(&mut self, keyboard: bool) {
        self.focused = self.is_available();
        self.focus_from_keyboard = self.focused && keyboard;
    }

    /// Losing focus clears only the focus fact. Hover remains root-owned and
    /// survives until the panel sends [`crate::HoverLost`].
    pub(super) fn lose_focus(&mut self) {
        self.focused = false;
        self.focus_from_keyboard = false;
    }

    pub(super) fn set_hovered(&mut self, hovered: bool) {
        self.hovered = hovered && self.is_available();
    }

    pub(super) fn clear_transient(&mut self) {
        self.focused = false;
        self.focus_from_keyboard = false;
        self.hovered = false;
    }

    #[must_use]
    pub(super) fn focused(&self) -> bool {
        self.focused
    }

    /// Whether this widget should *show* that it is focused — i.e. draw its
    /// focus ring. True only while focus arrived from the keyboard: a person
    /// who just clicked a tab knows where they are and does not need a box
    /// drawn around it, while a person walking the panel with Tab has no other
    /// way to tell. A caret is not a ring — a text control keeps drawing one
    /// whichever way focus arrived, so [`focused`](Self::focused) is what that
    /// draw asks.
    #[must_use]
    pub(super) fn focus_visible(&self) -> bool {
        self.focused && self.focus_from_keyboard
    }

    /// Whether the root reports the pointer over this widget. Read by the
    /// widgets that reveal clipped content on hover.
    #[must_use]
    pub(super) fn hovered(&self) -> bool {
        self.hovered
    }

    /// Exclusive fill priority: Disabled → Pressed → Hover → Normal.
    #[must_use]
    pub(super) fn theme_state(&self, pressed: bool) -> ThemeState {
        if !self.control.enabled {
            ThemeState::Disabled
        } else if pressed {
            ThemeState::Pressed
        } else if self.hovered {
            ThemeState::Hover
        } else {
            ThemeState::Normal
        }
    }

    /// State for a supporting surface whose hover overlay would obscure the
    /// control's active value geometry.
    #[must_use]
    pub(super) fn supporting_theme_state(&self, pressed: bool) -> ThemeState {
        if !self.control.enabled {
            ThemeState::Disabled
        } else if pressed {
            ThemeState::Pressed
        } else {
            ThemeState::Normal
        }
    }

    #[must_use]
    pub(super) fn validation_color(&self, theme: &Theme) -> Option<Rgba> {
        match self.control.validation {
            WidgetValidation::Valid => None,
            WidgetValidation::Warning { .. } => Some(theme.warning),
            WidgetValidation::Error { .. } => Some(theme.error),
        }
    }
}

pub fn emit_state_changed(ctx: &WasmCtx<'_>, state: &InteractionState) {
    if let Some(parent) = ctx.parent() {
        parent.send(&WidgetStateChanged { state: state.control().clone() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    #[test]
    fn fill_priority_keeps_disabled_above_press_above_hover() {
        let mut state = InteractionState::new(WidgetControlState::default());
        state.set_hovered(true);
        assert_eq!(state.theme_state(false), ThemeState::Hover);
        assert_eq!(state.theme_state(true), ThemeState::Pressed);

        let mut disabled = state.control().clone();
        disabled.enabled = false;
        assert!(state.replace(disabled));
        assert_eq!(state.theme_state(true), ThemeState::Disabled);
    }

    #[test]
    fn only_keyboard_focus_is_visible_focus() {
        // Tripwire: the owner's note. A pointer press focuses the control it
        // hit — routing depends on that — but must not leave a ring behind on
        // a tab the person just clicked.
        let mut state = InteractionState::new(WidgetControlState::default());
        state.gain_focus(false);
        assert!(state.focused(), "a press still moves focus");
        assert!(!state.focus_visible(), "a pressed control draws no ring");

        state.gain_focus(true);
        assert!(state.focus_visible(), "Tab traversal is what a ring marks");

        state.lose_focus();
        assert!(!state.focus_visible());
        state.gain_focus(false);
        assert!(!state.focus_visible(), "the keyboard fact does not survive a re-focus by pointer");
    }

    #[test]
    fn an_unavailable_widget_takes_neither_focus_nor_its_ring() {
        let hidden = WidgetControlState { visible: false, ..WidgetControlState::default() };
        let mut state = InteractionState::new(hidden);
        state.gain_focus(true);
        assert!(!state.focused());
        assert!(!state.focus_visible());
    }

    #[test]
    fn unavailable_update_clears_focus_hover_and_mutation() {
        let mut state = InteractionState::new(WidgetControlState::default());
        state.gain_focus(true);
        state.set_hovered(true);

        let mut hidden = state.control().clone();
        hidden.visible = false;
        assert!(state.replace(hidden));
        assert!(!state.focused());
        assert_eq!(state.theme_state(false), ThemeState::Normal);
        assert!(!state.can_mutate());
    }

    #[test]
    fn focus_loss_preserves_root_owned_hover() {
        let mut state = InteractionState::new(WidgetControlState::default());
        state.gain_focus(true);
        state.set_hovered(true);

        state.lose_focus();

        assert!(!state.focused());
        assert_eq!(state.theme_state(false), ThemeState::Hover);
    }

    #[test]
    fn validation_selects_named_theme_role() {
        let mut state = InteractionState::new(WidgetControlState::default());
        let mut warning = state.control().clone();
        warning.validation = WidgetValidation::Warning { message: String::from("check range") };
        state.replace(warning);
        assert_eq!(state.validation_color(&Theme::DEFAULT), Some(Theme::DEFAULT.warning));

        let mut error = state.control().clone();
        error.validation = WidgetValidation::Error { message: String::from("invalid") };
        state.replace(error);
        assert_eq!(state.validation_color(&Theme::DEFAULT), Some(Theme::DEFAULT.error));
    }
}
