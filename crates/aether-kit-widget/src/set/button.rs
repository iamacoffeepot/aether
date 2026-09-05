// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (the full rationale is on the same allow in `lib.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The momentary push button (issue 2660).
//!
//! A left press inside the button arms it (the root holds the pointer
//! capture); the matching release fires [`ButtonClicked`] only if it lands
//! back inside — a press-then-release-inside, so a press that drags off and
//! releases elsewhere cancels. The armed state draws the pressed overlay.
//!
//! The label sits centered in the frame, both axes. Centering it horizontally
//! needs the label's real width, so the button drives the same single-flight
//! [`FontMetricsRequest`](aether_text::FontMetricsRequest) the text controls
//! do and keeps the left-padded draw until the measurement lands — a guessed
//! width would center the label wrong and then visibly jump. The measurement
//! also gives the button its intrinsic size, so a layout can size a slot to
//! the label it holds.
//!
//! **How loud the button is** is [`ButtonEmphasis`] and [`ButtonTone`], and
//! that is the whole of it: the plate, the stroke, and the label's ink come
//! from the pair, while the measurement, the centering, the elision, the
//! reported intrinsic, and the hit rectangle are the same at every step of
//! the ladder. A quieter button is a quieter look, never a smaller target.
//!
//! A frame narrower than that intrinsic width **elides** the label before
//! centering it. Centering alone keeps the margins equal only while the label
//! fits; past that it hangs the run off both ends for the root's slot clip to
//! cut mid-glyph, and a label that did not fit then reads as a label that ends
//! oddly. Eliding first keeps the run inside the frame at any width, so the
//! margins stay equal and a cut label says so.
//!
//! It elides against the frame **less** a pad each side while the frame can
//! afford that, and against the whole frame when it cannot. The pads belong to
//! the intrinsic width — what a layout should give this button — and a frame
//! smaller than them is a caller saying there is no more room, not a reason to
//! draw nothing: a one-glyph control sized to its own mark has a frame narrower
//! than two pads at every display scale.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::mouse_button;
use aether_kinds::{Key, KeyRelease, MouseButton, MouseButtonRelease};
use aether_text::FontMetricsResult;

use crate::set::defaults::WidgetDefaults;
use crate::set::{
    ActivationArms, ButtonFace, accept_font_metrics_result, apply_text_theme, button_face_width,
    pump_text_font_metrics, push_border, push_button_face, reply_if_hidden,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, Theme};
use crate::{
    ButtonClicked, ButtonConfig, ButtonEmphasis, ButtonTone, Collect, SetWidgetState, WidgetControlState,
    WidgetDrawItem, WidgetDrawList, WidgetFrame,
};

/// A momentary push button. Holds its label plus the cached theme / frame /
/// focus, the armed (`pressed`) state, and the single-flight font-metrics
/// adapter that feeds the centered label and the reported intrinsic size.
pub struct ButtonWidget {
    label: String,
    /// How loudly this verb asks to be pressed.
    emphasis: ButtonEmphasis,
    /// What it does to the reader's work.
    tone: ButtonTone,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    /// Shared pointer/keyboard activation state; a release-inside fires the click.
    arms: ActivationArms,
    /// Single-flight exact metrics for the active theme font.
    font_metrics: FontMetricsAdapter,
}

impl ButtonWidget {
    /// Resolve a release: returns `true` (a click fired) only if the button
    /// was armed and the release landed back inside. Disarms either way.
    fn release_at(&mut self, x: f32, y: f32) -> bool {
        self.arms.release_pointer(&self.frame, self.state.is_available(), x, y)
    }

    fn pressed(&self) -> bool {
        self.arms.pressed()
    }

    fn clear_arms(&mut self) {
        self.arms.clear();
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        if self.state.replace(next) {
            if !self.state.is_available() {
                self.clear_arms();
            }
            emit_state_changed(ctx, &self.state);
        }
    }

    fn emit_click(ctx: &WasmCtx<'_>) {
        if let Some(parent) = ctx.parent() {
            parent.send(&ButtonClicked);
        }
    }

    /// Apply one key press. Returns whether activation fires immediately.
    fn press_key(&mut self, code: u32) -> bool {
        self.arms.press_key(self.state.is_available(), code)
    }

    /// Apply one matching key release. Returns whether activation fires now.
    fn release_key(&mut self, code: u32) -> bool {
        self.arms.release_key(self.state.is_available(), code)
    }
}

impl WidgetDefaults for ButtonWidget {
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
        self.clear_arms();
    }
}

/// A push-button widget. Spawned inline by a panel root with a
/// [`ButtonConfig`]; reports [`ButtonClicked`] up on a completed click.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `ButtonConfig` again to relabel or restyle it in place.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for ButtonWidget {
    type Config = ButtonConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.button";

    fn init(config: ButtonConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let desired_font_id = config.theme.font_id;
        Ok(ButtonWidget {
            label: config.label,
            emphasis: config.emphasis,
            tone: config.tone,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            arms: ActivationArms::default(),
            font_metrics: FontMetricsAdapter::new(desired_font_id),
        })
    }

    /// Kick off the font-metrics request for the initial theme font.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Relabel / restyle in place from a re-sent config, and request metrics
    /// for the new theme font.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: ButtonConfig) {
        self.label = config.label;
        self.emphasis = config.emphasis;
        self.tone = config.tone;
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        self.apply_control_state(ctx, config.state);
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Restyle: adopt the fanned theme and request metrics for its font.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        apply_text_theme(ctx, &mut self.font_metrics, &mut self.theme, set.theme);
    }

    /// Install a font-metrics reply; the next `Collect` centers the label.
    #[handler::single]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_>, result: FontMetricsResult) {
        accept_font_metrics_result(ctx, &mut self.font_metrics, result);
    }

    /// Read-only and validation are deliberately inapplicable to a momentary
    /// button; visibility/enabled still control routing and presentation.
    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        self.apply_control_state(ctx, set.state);
    }

    /// A left press inside arms the button.
    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, press: MouseButton) {
        self.arms.press_mouse_button(&self.frame, self.state.is_available(), press);
    }

    /// A left release fires the click if it lands back inside while armed.
    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if release.button != mouse_button::LEFT {
            return;
        }
        if self.release_at(release.x, release.y) {
            Self::emit_click(ctx);
        }
    }

    /// Enter activates once on its first press; Space arms until its matching
    /// release. Key-repeat presses are ignored while either key is armed.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if self.press_key(key.code) {
            Self::emit_click(ctx);
        }
    }

    #[handler::single]
    fn on_key_release(&mut self, ctx: &mut WasmCtx<'_>, release: KeyRelease) {
        if self.release_key(release.code) {
            Self::emit_click(ctx);
        }
    }

    /// Reply the button's local draw — the plate or wash its emphasis calls
    /// for, its stroke, the centered label, and a focus ring — plus the
    /// intrinsic size the label asks for once it is measured.
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
                intrinsic: self.intrinsic(),
                items: self.draw_items(),
                overlay: Vec::new(),
            });
        }
    }
}

impl ButtonWidget {
    /// The label plus one pad each side, at the theme's row height: what a
    /// layout needs to size a slot to this button's own label. `None` until
    /// the label is measured.
    ///
    /// The same number at every emphasis. A host measures its cells from
    /// this, so a verb ranked down to text would otherwise move the row it
    /// sits in — the rank is a look, not a size.
    fn intrinsic(&self) -> Option<[f32; 2]> {
        self.font_metrics
            .resolved()
            .map(|metrics| [button_face_width(&self.label, &self.theme, metrics), self.theme.row_height])
    }

    /// The button's local draw: the shared face — plate or wash, stroke, and
    /// centered label — plus the keyboard focus ring the face itself has no
    /// opinion about.
    fn draw_items(&self) -> Vec<WidgetDrawItem> {
        let (width, height) = (self.frame.width, self.frame.height);
        let mut items: Vec<WidgetDrawItem> = Vec::new();
        let face =
            ButtonFace { x: 0.0, y: 0.0, width, height, label: &self.label, emphasis: self.emphasis, tone: self.tone };
        push_button_face(
            &mut items,
            &face,
            &self.theme,
            self.state.theme_state(self.pressed()),
            self.font_metrics.resolved(),
        );

        // Keyboard focus only: the button a pointer just pressed shows its
        // press, and a ring left over from the click says nothing more.
        if self.state.focus_visible() {
            push_border(&mut items, width, height, 2.0, self.theme.accent);
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_kinds::keycode::{KEY_ENTER, KEY_SPACE};
    use aether_kinds::{CachedFontMetrics, FontMetrics, GlyphAdvance};
    use aether_math::Rgba;

    use crate::set::ELLIPSIS;

    use crate::WidgetControlState;
    use crate::set::{BUTTON_STROKE_THICKNESS, KeyboardArm, button_run, centered_text_x, measured_text_width};

    /// The label run and local x this button draws — the shared
    /// [`button_run`] rule against the button's own frame, theme and metrics,
    /// which is exactly what `push_button_face` puts on the screen for it.
    fn draw_run(button: &ButtonWidget) -> Option<(String, f32)> {
        button_run(&button.label, button.frame.width, &button.theme, button.font_metrics.resolved())
    }

    fn button() -> ButtonWidget {
        ButtonWidget {
            label: String::from("go"),
            emphasis: ButtonEmphasis::Filled,
            tone: ButtonTone::Neutral,
            theme: Theme::DEFAULT,
            state: InteractionState::new(WidgetControlState::default()),
            frame: WidgetFrame { x: 10.0, y: 10.0, width: 40.0, height: 20.0 },
            arms: ActivationArms::default(),
            font_metrics: FontMetricsAdapter::new(0),
        }
    }

    #[test]
    fn a_label_is_centered_at_every_frame_width_and_the_intrinsic_reserves_a_pad_each_side() {
        // Tripwire: the owner's asymmetric-Remove-button note. Whatever frame
        // the consumer hands down, the margins either side of the label must
        // be equal — the old `.max(pad)` clamp made them 8 and 2 on a frame a
        // few pixels under the intrinsic width.
        for width in [100.0_f32, 90.0, 84.0, 80.0, 41.0] {
            let text_width = 40.0_f32;
            let x = centered_text_x(width, text_width);
            let right_margin = width - (x + text_width);
            assert!((x - right_margin).abs() < 1e-4, "frame {width}: margins {x} / {right_margin} are not equal");
        }
        assert_eq!(centered_text_x(100.0, 40.0), 30.0, "even margins either side");
        assert_eq!(centered_text_x(40.0, 60.0), 0.0, "a label wider than its whole frame keeps its start visible");
    }

    /// A button whose metrics have resolved against a font of the given
    /// `advance` per glyph, at the theme numbers a caller really uses.
    fn measured_button(label: &str, pad: f32, size: f32, width: f32, advance: f32) -> ButtonWidget {
        let mut button = button();
        let mut font_metrics = FontMetricsAdapter::new(0);
        assert_eq!(font_metrics.take_pending_request(), Some(0));
        assert!(!font_metrics.accept_reply(Some(CachedFontMetrics::new(&FontMetrics {
            units_per_em: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            line_gap: 0.0,
            default_advance: advance,
            advances: alloc::vec![GlyphAdvance { codepoint: u32::from(ELLIPSIS), advance_units: advance }],
        }))));
        button.font_metrics = font_metrics;
        button.label = String::from(label);
        button.theme.pad = pad;
        button.theme.label_size_pixels = size;
        button.frame.width = width;
        button
    }

    #[test]
    fn a_control_too_narrow_for_its_pads_draws_its_mark_instead_of_nothing() {
        // Tripwire: **the pads are the intrinsic's, and a button gives them
        // up before it gives up its label.** A one-glyph control sized to
        // its own mark — the ascendancy inset's collapse button, a square
        // of one body line carrying a `−` at the body size — has a frame
        // narrower than two pads at any display scale. Elided against the
        // padded budget alone, neither the mark nor the ellipsis fits, so
        // the run came back empty and the button drew a filled square with
        // nothing on it: a control that says nothing about what it does,
        // which is what a capture of the studio caught.
        //
        // The numbers are the inset's own at scale 2 — pad 16, a 32-pixel
        // mark of 0.6 em, a 44-pixel frame — so this reproduces the screen
        // that had it rather than a shape invented for the test.
        let button = measured_button("\u{2212}", 16.0, 32.0, 44.0, 600.0);
        let metrics = button.font_metrics.resolved().expect("measured");
        let mark = measured_text_width(metrics, &button.label, button.theme.label_size_pixels);

        assert!(mark > button.theme.pad.mul_add(-2.0, button.frame.width), "the frame cannot afford both pads");
        assert!(mark <= button.frame.width, "and the mark does fit the frame itself, which is the whole point");

        let (run, run_x) = draw_run(&button).expect("a control with room for its mark must draw it");
        assert_eq!(run, button.label, "the mark is drawn whole, not elided to an ellipsis or to nothing");
        assert!(
            (run_x - (button.frame.width - mark) / 2.0).abs() < 1e-3,
            "and centred in the frame it was given: {run_x} on a {} frame",
            button.frame.width,
        );

        // The preference is unchanged where the frame can afford it: a
        // wide button still elides against the padded budget, so a long
        // label keeps a pad of clear space beside its ellipsis.
        let wide = measured_button("Regenerate terrain", 16.0, 32.0, 200.0, 600.0);
        let run = draw_run(&wide).expect("a wide button draws").0;
        let measured = measured_text_width(metrics, &run, wide.theme.label_size_pixels);
        assert!(run.ends_with(ELLIPSIS), "a label past the measure is still cut with the mark that says so");
        assert!(
            measured <= wide.theme.pad.mul_add(-2.0, wide.frame.width),
            "a frame that can afford its pads still keeps them: {run:?} is {measured} wide",
        );
    }

    #[test]
    fn a_button_too_narrow_for_its_label_elides_it_and_still_centers_it() {
        // Tripwire: the same defect the tab strip's last tab had. Centering a
        // run wider than its frame hangs it off both ends, and the root's slot
        // clip then cuts whichever end leaves the frame — so the margins the
        // reader actually sees are not equal, and the label ends on a sliced
        // glyph rather than a mark saying it was cut.
        let mut button = button();
        let mut font_metrics = FontMetricsAdapter::new(0);
        assert_eq!(font_metrics.take_pending_request(), Some(0));
        assert!(!font_metrics.accept_reply(Some(CachedFontMetrics::new(&FontMetrics {
            units_per_em: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            line_gap: 0.0,
            default_advance: 500.0,
            advances: alloc::vec![GlyphAdvance { codepoint: u32::from(ELLIPSIS), advance_units: 500.0 }],
        }))));
        button.font_metrics = font_metrics;
        button.label = String::from("Regenerate terrain");

        for width in [400.0_f32, 200.0, 60.0, 30.0] {
            button.frame.width = width;
            let metrics = button.font_metrics.resolved().expect("measured");
            let size = button.theme.label_size_pixels;
            let (run, run_x) = draw_run(&button).expect("every width here has room for at least the elision mark");
            let run_width = measured_text_width(metrics, &run, size);
            let (left_margin, right_margin) = (run_x, width - (run_x + run_width));
            assert!(
                (left_margin - right_margin).abs() < 1e-3,
                "width {width}: {run:?} sits {left_margin} from the left and {right_margin} from the right",
            );
            assert!(run_width <= width, "width {width}: the run {run:?} is {run_width} wide and leaves the frame");
        }
    }

    #[test]
    fn the_reported_intrinsic_width_is_the_label_plus_one_pad_each_side() {
        // Tripwire: a layout sizes a slot from this number, so it has to be
        // exactly what `centered_text_x` then reproduces as equal margins.
        let button = button();
        let text_width = 40.0_f32;
        let intrinsic = button.theme.pad.mul_add(2.0, text_width);
        assert_eq!(centered_text_x(intrinsic, text_width), button.theme.pad, "the intrinsic width pads exactly once");
    }

    /// The same button at a chosen rank and tone.
    fn styled(emphasis: ButtonEmphasis, tone: ButtonTone) -> ButtonWidget {
        ButtonWidget { emphasis, tone, ..button() }
    }

    /// The full-frame fill a button draws under its label — its plate, or the
    /// wash a plateless rank shows the pointer. `None` when it draws neither.
    fn plate(button: &ButtonWidget) -> Option<Rgba> {
        button.draw_items().iter().find_map(|item| match item {
            WidgetDrawItem::Quad { width, height, color, .. }
                if *width == button.frame.width && *height == button.frame.height =>
            {
                Some(*color)
            }
            _ => None,
        })
    }

    /// The hairline rows of a button's stroke — the quads that are neither
    /// the full-frame plate nor as thick as the focus ring.
    fn stroke(button: &ButtonWidget) -> Vec<Rgba> {
        button
            .draw_items()
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Quad { width, height, color, .. }
                    if (*width == BUTTON_STROKE_THICKNESS || *height == BUTTON_STROKE_THICKNESS) =>
                {
                    Some(*color)
                }
                _ => None,
            })
            .collect()
    }

    /// Every colour the button puts on the screen.
    fn inks(button: &ButtonWidget) -> Vec<Rgba> {
        button
            .draw_items()
            .iter()
            .map(|item| match item {
                WidgetDrawItem::Quad { color, .. } | WidgetDrawItem::Text { color, .. } => *color,
                WidgetDrawItem::TexturedQuad { tint, .. } => *tint,
            })
            .collect()
    }

    #[test]
    fn only_the_filled_rank_plates_a_verb_in_the_accent() {
        // Tripwire: the owner's round-8 note 5 — "a single yellow button for
        // everything is kinda meh". The accent means *the* primary action, so
        // exactly one rank may wear it as a plate; the rest are a quiet plate,
        // a stroke, and nothing at all. A ladder whose steps resolve to the
        // same fill is the defect back with four names on it.
        let theme = Theme::DEFAULT;
        assert_eq!(plate(&styled(ButtonEmphasis::Filled, ButtonTone::Neutral)), Some(theme.accent));

        let tonal = plate(&styled(ButtonEmphasis::Tonal, ButtonTone::Neutral)).expect("a tonal verb keeps a plate");
        assert_ne!(tonal, theme.accent, "the second rank is not the first one repainted");
        assert_ne!(tonal, theme.surface_raised, "nor the bare surface: a plate nobody can see is not a button");
        assert_ne!(tonal, theme.selection, "and never the chosen-row look — one meaning per visual token");

        assert_eq!(plate(&styled(ButtonEmphasis::Outlined, ButtonTone::Neutral)), None, "an outlined verb has no fill");
        assert_eq!(plate(&styled(ButtonEmphasis::Text, ButtonTone::Neutral)), None, "and a text verb has no chrome");

        assert!(
            stroke(&styled(ButtonEmphasis::Outlined, ButtonTone::Neutral)).iter().all(|ink| *ink == theme.edge()),
            "the outlined rank is drawn in the control-edge role derived from the outline",
        );
        assert!(stroke(&styled(ButtonEmphasis::Text, ButtonTone::Neutral)).is_empty(), "the text rank strokes nothing");
        assert!(
            !inks(&styled(ButtonEmphasis::Outlined, ButtonTone::Neutral)).contains(&theme.accent)
                && !inks(&styled(ButtonEmphasis::Text, ButtonTone::Neutral)).contains(&theme.accent),
            "a secondary verb spends the accent nowhere, ink included",
        );
    }

    #[test]
    fn the_four_ranks_draw_four_different_faces() {
        // Tripwire: the owner's round-11 note 4 — a tonal `Change gem` and an
        // outlined-danger `×` on one row "read alike". A rank is told apart by
        // its *face* before its colour, so the four have to differ in whether
        // they carry a plate, a stroke, or neither, and the two that carry
        // colour have to clear their background by a measured step. Two ranks
        // that resolve to the same (plate, stroke) shape are the defect back
        // with four names on it, and a stroke lost in the divider role — which
        // is what `outline` gave the outlined rank — is the text rank wearing
        // a border nobody can see.
        let theme = Theme::DEFAULT;
        let face = |emphasis| {
            let button = styled(emphasis, ButtonTone::Neutral);
            (plate(&button), stroke(&button).first().copied())
        };

        let faces = [
            face(ButtonEmphasis::Filled),
            face(ButtonEmphasis::Tonal),
            face(ButtonEmphasis::Outlined),
            face(ButtonEmphasis::Text),
        ];
        for (first, second) in (0..faces.len()).flat_map(|i| (i + 1..faces.len()).map(move |j| (i, j))) {
            assert_ne!(faces[first], faces[second], "rank {first} and rank {second} draw one face");
        }

        for (rank, ink) in faces.iter().enumerate().filter_map(|(rank, face)| Some((rank, face.0.or(face.1)?))) {
            let ratio = Theme::contrast_ratio(ink, theme.surface_raised);
            assert!(ratio >= 3.0, "rank {rank}'s face reads at only {ratio} against the surface it stands on");
        }
    }

    #[test]
    fn a_verb_that_throws_work_away_reads_in_the_error_role_at_every_rank() {
        // Tripwire: the tone has to reach every branch of the ladder. A
        // danger verb that keeps the accent at one rank is a delete button
        // that looks like the primary action — the single worst confusion
        // this table can produce — and one that loses the error colour at
        // another says nothing about what it destroys.
        let theme = Theme::DEFAULT;
        for emphasis in [ButtonEmphasis::Filled, ButtonEmphasis::Tonal, ButtonEmphasis::Outlined, ButtonEmphasis::Text]
        {
            let button = styled(emphasis, ButtonTone::Danger);
            let inks = inks(&button);
            assert!(!inks.contains(&theme.accent), "{emphasis:?} danger wears the accent: {inks:?}");
            match emphasis {
                ButtonEmphasis::Filled => assert_eq!(plate(&button), Some(theme.error), "the loudest danger is plated"),
                _ => assert!(inks.contains(&theme.error), "{emphasis:?} danger drops the error role: {inks:?}"),
            }
        }
    }

    #[test]
    fn a_plateless_rank_still_answers_the_pointer() {
        // Tripwire: the filled ranks carry the hover in their plate
        // (`Theme::fill`), so an outlined or text button with no plate has
        // nowhere to put it — and a verb that does not light up under the
        // pointer reads as a label. The wash is the same role-agnostic
        // overlay every other widget hovers with.
        let theme = Theme::DEFAULT;
        for emphasis in [ButtonEmphasis::Outlined, ButtonEmphasis::Text] {
            let mut button = styled(emphasis, ButtonTone::Neutral);
            assert_eq!(plate(&button), None, "{emphasis:?} draws nothing at rest");
            button.state.set_hovered(true);
            assert_eq!(plate(&button), Some(theme.hover_overlay), "{emphasis:?} washes under the pointer");
            button.arms.press_pointer(&button.frame, true, 20.0, 20.0);
            assert_eq!(plate(&button), Some(theme.pressed_overlay), "{emphasis:?} darkens under the press");
        }
    }

    #[test]
    fn the_rank_moves_neither_the_label_nor_the_size_a_layout_reserves() {
        // Tripwire: a host sizes a cell from the reported intrinsic and the
        // button centres its run in whatever frame comes back. If a rank
        // changed either — an outlined button reserving room for its stroke,
        // say — a row of verbs would shift as one of them was ranked down,
        // and a dialog's confirm and cancel would stop lining up.
        let filled = measured_button("Regenerate terrain", 8.0, 14.0, 90.0, 500.0);
        let (run, run_x) = draw_run(&filled).expect("a measured button draws");
        let intrinsic = filled.intrinsic().expect("and reports its size");
        for emphasis in [ButtonEmphasis::Tonal, ButtonEmphasis::Outlined, ButtonEmphasis::Text] {
            for tone in [ButtonTone::Neutral, ButtonTone::Danger] {
                let ranked =
                    ButtonWidget { emphasis, tone, ..measured_button("Regenerate terrain", 8.0, 14.0, 90.0, 500.0) };
                assert_eq!(draw_run(&ranked), Some((run.clone(), run_x)), "{emphasis:?}/{tone:?} moved the label");
                assert_eq!(ranked.intrinsic(), Some(intrinsic), "{emphasis:?}/{tone:?} changed the reserved size");
            }
        }
    }

    #[test]
    fn press_inside_then_release_inside_clicks() {
        let mut b = button();
        b.arms.press_pointer(&b.frame, b.state.is_available(), 20.0, 20.0);
        assert!(b.arms.pointer_pressed);
        assert!(b.release_at(30.0, 25.0), "press-inside then release-inside is a click");
        assert!(!b.arms.pointer_pressed, "disarmed after release");
    }

    #[test]
    fn press_inside_then_release_outside_cancels() {
        let mut b = button();
        b.arms.press_pointer(&b.frame, b.state.is_available(), 20.0, 20.0);
        assert!(!b.release_at(200.0, 200.0), "a release that drifts off the button does not click");
        assert!(!b.arms.pointer_pressed, "disarmed even on a cancel");
    }

    #[test]
    fn press_outside_never_arms() {
        let mut b = button();
        b.arms.press_pointer(&b.frame, b.state.is_available(), 200.0, 200.0);
        assert!(!b.arms.pointer_pressed);
        assert!(!b.release_at(20.0, 20.0), "a release with no prior inside-press does not click");
    }

    #[test]
    fn enter_fires_once_per_press_release_pair_and_ignores_repeat() {
        let mut b = button();
        assert!(b.press_key(KEY_ENTER));
        assert!(!b.press_key(KEY_ENTER), "repeat while armed cannot duplicate");
        assert!(!b.release_key(KEY_ENTER), "Enter fires on press, not release");
        assert!(b.press_key(KEY_ENTER), "matching release rearms the next press");
    }

    #[test]
    fn space_fires_only_on_matching_release_and_cancels_with_focus_loss() {
        let mut b = button();
        assert!(!b.press_key(KEY_SPACE));
        assert_eq!(b.arms.keyboard_arm, Some(KeyboardArm::Space));
        assert!(b.release_key(KEY_SPACE));
        assert_eq!(b.arms.keyboard_arm, None);

        b.press_key(KEY_SPACE);
        b.state.lose_focus();
        b.clear_arms();
        assert!(!b.release_key(KEY_SPACE));
    }
}
