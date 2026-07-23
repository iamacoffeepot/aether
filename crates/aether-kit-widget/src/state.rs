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
pub(super) struct InteractionState {
    control: WidgetControlState,
    focused: bool,
    hovered: bool,
}

impl InteractionState {
    #[must_use]
    pub(super) fn new(control: WidgetControlState) -> Self {
        Self { control, focused: false, hovered: false }
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

    pub(super) fn gain_focus(&mut self) {
        self.focused = self.is_available();
    }

    /// Losing focus clears only the focus fact. Hover remains root-owned and
    /// survives until the panel sends [`crate::HoverLost`].
    pub(super) fn lose_focus(&mut self) {
        self.focused = false;
    }

    pub(super) fn set_hovered(&mut self, hovered: bool) {
        self.hovered = hovered && self.is_available();
    }

    pub(super) fn clear_transient(&mut self) {
        self.focused = false;
        self.hovered = false;
    }

    #[must_use]
    pub(super) fn focused(&self) -> bool {
        self.focused
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

pub(super) fn emit_state_changed(ctx: &WasmCtx<'_>, state: &InteractionState) {
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
    fn unavailable_update_clears_focus_hover_and_mutation() {
        let mut state = InteractionState::new(WidgetControlState::default());
        state.gain_focus();
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
        state.gain_focus();
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
