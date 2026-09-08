//! The handler set the interactive stock widgets adopt (ADR-0169), and the
//! ambient state it acts on.
//!
//! These are the handlers that absorb ambient state the panel root pushes
//! down — layout rect, theme, focus, and hover. None of them is widget
//! behavior: a widget's own handlers are the ones that read input and answer
//! [`Collect`](crate::Collect) with a draw list.
//!
//! [`WidgetChrome`] is where that ambient state is reached from, split out of
//! the set because it is the one part that is pure field access. Every stock
//! widget's three accessor bodies were `&mut self.frame`, `&mut self.theme`,
//! and `&mut self.state`, sixteen times over, so [`widget_chrome!`] writes
//! them from the field names and each widget states only what varies.
//!
//! One hook carries what varies. [`cancel_activation`] releases whatever
//! half-finished interaction the widget tracks — an armed press, a live drag,
//! a pending IME composition — which is the only reason `on_focus_lost`
//! differed across the family. A widget whose focus loss does more than
//! release (committing an edit buffer, say) overrides that handler outright.
//!
//! The other thing that varies is whether the widget measures its own text.
//! [`WidgetChrome::widget_font_metrics`] answers that, and `on_set_theme`
//! reads it: a restyle that changes the theme font has to ask `aether.text`
//! for the new font's metrics, or the widget silently keeps drawing against
//! the old font's advances. Twelve widgets used to carry that as a one-line
//! `on_set_theme` override precisely because the set's own default got it
//! wrong; naming the adapter in [`widget_chrome!`] is what the override was
//! standing in for.
//!
//! `SetWidgetState` is deliberately absent. Its per-widget bodies disagree on
//! which predicate cancels an activation — a momentary button cancels only
//! when it becomes unavailable, most controls also cancel on read-only, and
//! the text widgets split the two tiers across composition and drag — so a
//! shared body would have to pick one and quietly change the rest.
//!
//! `FontMetricsResult` is absent for the same kind of reason from the other
//! side: four of the adopters draw no text and never request metrics, so
//! hosting the reply here would have them declare a kind they can only
//! no-op. Those that do measure keep a one-line handler over
//! [`accept_font_metrics_result`](super::accept_font_metrics_result).
//!
//! [`cancel_activation`]: WidgetDefaults::cancel_activation

use aether_actor::{WasmCtx, handler_set};

use crate::set::pump_text_font_metrics;
use crate::state::InteractionState;
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, Theme};
use crate::{FocusGained, FocusLost, HoverGained, HoverLost, WidgetFrame};

/// Where a widget keeps the ambient state [`WidgetDefaults`] maintains: the
/// rect the panel root assigned it, the theme fanned down to it, its own
/// focus / hover / control state, and — for a widget that measures its own
/// text — the single-flight font-metrics adapter.
///
/// Implement it with [`widget_chrome!`] rather than by hand; the accessor
/// bodies are field access and nothing else.
pub trait WidgetChrome {
    /// The widget's cached layout rect, assigned by the panel root.
    fn widget_frame(&mut self) -> &mut WidgetFrame;

    /// The widget's cached theme.
    fn widget_theme(&mut self) -> &mut Theme;

    /// The widget's focus / hover / control state.
    fn widget_state(&mut self) -> &mut InteractionState;

    /// The widget's single-flight font-metrics adapter, or `None` for a
    /// widget that draws no text and so never measures one.
    fn widget_font_metrics(&mut self) -> Option<&mut FontMetricsAdapter> {
        None
    }
}

/// Implement [`WidgetChrome`] for a widget that keeps its ambient state in
/// the three fields every stock widget names the same way — `frame`,
/// `theme`, and `state`. Name a fourth field to hand the set the widget's
/// [`FontMetricsAdapter`] as well, which is what makes a restyle request the
/// new theme font's metrics:
///
/// ```ignore
/// widget_chrome!(ToggleWidget);
/// widget_chrome!(ButtonWidget, font_metrics);
/// ```
///
/// A widget that keeps this state under other names writes the impl itself;
/// the macro is the sixteen-times case, not a requirement.
macro_rules! widget_chrome {
    ($widget:ty $(, $font_metrics:ident)?) => {
        impl $crate::set::defaults::WidgetChrome for $widget {
            fn widget_frame(&mut self) -> &mut $crate::WidgetFrame {
                &mut self.frame
            }

            fn widget_theme(&mut self) -> &mut $crate::theme::Theme {
                &mut self.theme
            }

            fn widget_state(&mut self) -> &mut $crate::state::InteractionState {
                &mut self.state
            }

            $(
                fn widget_font_metrics(
                    &mut self,
                ) -> ::core::option::Option<&mut $crate::text_edit::FontMetricsAdapter> {
                    ::core::option::Option::Some(&mut self.$font_metrics)
                }
            )?
        }
    };
}

pub(crate) use widget_chrome;

#[handler_set]
pub trait WidgetDefaults: WidgetChrome {
    /// Release any half-finished interaction: an armed press, a live drag, a
    /// pending IME composition. Called on focus loss, and callable from a
    /// widget's own control-state handler.
    fn cancel_activation(&mut self);

    /// Cache the layout rect the root assigned.
    #[handler::single]
    fn on_frame(&mut self, _ctx: &mut WasmCtx<'_>, frame: WidgetFrame) {
        *self.widget_frame() = frame;
    }

    /// Restyle: adopt the fanned theme, and — for a widget that measures its
    /// own text — start a request for the new theme font's metrics. Adopting
    /// the theme without that request leaves the widget drawing the new font
    /// against the old one's advances until something else happens to pump
    /// the adapter.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        let font_id = set.theme.font_id;
        *self.widget_theme() = set.theme;

        if let Some(font_metrics) = self.widget_font_metrics() {
            font_metrics.set_desired(font_id);
            pump_text_font_metrics(ctx, font_metrics);
        }
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
