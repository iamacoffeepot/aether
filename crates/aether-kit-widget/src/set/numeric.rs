// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! Typed, steppable numeric editor (issue 2926).
//!
//! The visible edit buffer is deliberately separate from the last committed
//! number. Invalid intermediates remain visible without emitting a numeric
//! value; valid edits preview their clamped/snapped value without rewriting the
//! buffer. Enter or blur commits and canonicalizes, while an invalid commit
//! reverts. Up/Down step and commit immediately. Clipboard edits use the same
//! selection and parse paths as typed edits.
//!
//! The value and its steppers are **one** control, not a field with a second
//! box bolted to its right: one fill covers the whole frame, the stepper
//! column is a region inside that frame separated from the value by a
//! hairline, and validation / focus outlines ring the lot. A stepper button
//! composites its hover or pressed overlay over the control's own fill, so the
//! button under the pointer lights up without ever reading as its own surface.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_clipboard::{GetClipboardTextResult, SetClipboardTextResult};
use aether_kinds::keycode::{KEY_DOWN, KEY_ENTER, KEY_UP};
use aether_kinds::{
    CachedFontMetrics, ImePreedit, Key, Modifiers, MouseButton, MouseButtonRelease, MouseMove, TextInput, mouse_button,
};
use aether_math::Rgba;
use aether_text::FontMetricsResult;
use alloc::string::{String, ToString};

use crate::set::defaults::WidgetDefaults;
use crate::set::{
    SingleLineEdit, accept_clipboard_paste, accept_font_metrics_result, apply_text_theme, arm_text_drag, edit_command,
    measured_text_width, pump_text_font_metrics, push_triangle, quad, release_left, reply_single_line_edit,
    report_clipboard_copy, run_edit_key, single_line_box_fill, single_line_hit_byte,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::{EditPolicy, FontMetricsAdapter, TextEditState, TextSpan};
use crate::theme::{SetTheme, Theme, ThemeState};
use crate::{
    Collect, FocusLost, HoverLost, NumericChanged, NumericConfig, SetWidgetState, WidgetControlState, WidgetDrawItem,
    WidgetFrame,
};

/// Retained edit-buffer bound; comfortably exceeds every canonical finite
/// `f32` literal while preventing unbounded typed or pasted intermediates.
const NUMERIC_EDIT_MAX_CHARS: u32 = 32;

/// How much of a stepper button the arrow inside it fills, on both axes. Small
/// enough that the arrow reads as a mark on a button rather than as the button.
const ARROW_EXTENT_FRACTION: f32 = 0.45;

/// Rows [`push_triangle`] uses for one arrow at the sizes a stepper button
/// comes to — only a `Vec::with_capacity` hint, never a correctness bound.
const TRIANGLE_ROWS_PER_ARROW: usize = 8;

/// How long a stepper button must be held before it starts repeating, in the
/// root's per-frame `Collect`s — half a second at sixty a second. Long enough
/// that a deliberate single click never turns into two, short enough that a
/// person holding the button to travel is not left waiting.
const STEPPER_REPEAT_DELAY_FRAMES: u32 = 30;

/// How often a held stepper repeats once it has started, in the same frames —
/// ten steps a second. The cadence a held arrow *key* has is the platform's,
/// so these two are not the same number by construction; this is the one the
/// widget owns.
const STEPPER_REPEAT_INTERVAL_FRAMES: u32 = 6;

/// Whether a stepper held for `frames_held` collects steps again this frame.
///
/// Round-4 note 14 asked for a held arrow key to keep stepping; a press and
/// hold on the arrow *button* is the same gesture with the other hand, and a
/// spinner that answers one and not the other is a spinner whose two routes
/// disagree. A widget never sees a tick, so the cadence is counted in frames
/// the root asked it to draw — the same clock the toast region ages by.
fn stepper_repeats_at(frames_held: u32) -> bool {
    frames_held >= STEPPER_REPEAT_DELAY_FRAMES
        && (frames_held - STEPPER_REPEAT_DELAY_FRAMES) % STEPPER_REPEAT_INTERVAL_FRAMES == 0
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct NumericEmission {
    value: f32,
    committed: bool,
}

#[derive(Debug, Clone, Copy)]
struct NumericBounds {
    min: f32,
    max: f32,
}

impl NumericBounds {
    fn from_config(min: f32, max: f32) -> Self {
        let min = if min.is_finite() {
            min
        } else {
            f32::MIN
        };
        let max = if max.is_finite() {
            max
        } else {
            f32::MAX
        };
        if min <= max {
            Self { min, max }
        } else {
            Self { min: max, max: min }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepDirection {
    Down,
    Up,
}

/// The two stacked stepper buttons inside the editor's frame, at its right
/// end. The column is square — one row height wide — so each button is a
/// comfortable target, and it never takes more than half the frame, which is
/// what keeps a narrow numeric from becoming two arrows and no text.
#[derive(Debug, Clone, Copy)]
struct StepperColumn {
    /// The column's left edge in widget-local pixels.
    left: f32,
    width: f32,
    /// The boundary between the up button (above) and the down button (below).
    split_y: f32,
    height: f32,
}

impl StepperColumn {
    /// The column a `width` × `height` numeric frame reserves, or `None` when
    /// the frame is too small to give one up without swallowing the text.
    fn of(width: f32, height: f32) -> Option<Self> {
        let column = height.min(width * 0.5);
        (column >= 1.0 && height >= 2.0).then_some(Self {
            left: width - column,
            width: column,
            split_y: height * 0.5,
            height,
        })
    }

    /// Which button a widget-local point lands on, `None` outside the column.
    fn hit(self, local_x: f32, local_y: f32) -> Option<StepDirection> {
        let inside =
            local_x >= self.left && local_x < self.left + self.width && local_y >= 0.0 && local_y < self.height;
        inside.then_some(if local_y < self.split_y {
            StepDirection::Up
        } else {
            StepDirection::Down
        })
    }

    /// One button's local rect as `(top, height)`.
    fn button_span(self, direction: StepDirection) -> (f32, f32) {
        match direction {
            StepDirection::Up => (0.0, self.split_y),
            StepDirection::Down => (self.split_y, self.height - self.split_y),
        }
    }
}

/// A single-line numeric editor with an independent display buffer and
/// committed number.
pub struct NumericWidget {
    min: f32,
    max: f32,
    step: f32,
    committed_value: f32,
    edit: TextEditState,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    modifiers: Modifiers,
    dragging: bool,
    paste_pending: bool,
    /// Which stepper button the pointer is over, for its hover overlay.
    hovered_stepper: Option<StepDirection>,
    /// Which stepper button is held down, for its pressed overlay.
    pressed_stepper: Option<StepDirection>,
    /// How many collects that button has been held for, which is how the
    /// press-and-hold repeat tells the time ([`stepper_repeats_at`]).
    stepper_held_frames: u32,
    /// Single-flight exact metrics for the active theme font: what places the
    /// caret and what sizes the field to its range.
    font_metrics: FontMetricsAdapter,
}

impl NumericWidget {
    fn configured(config: NumericConfig) -> Self {
        let font_metrics = FontMetricsAdapter::new(config.theme.font_id);
        let mut widget = Self {
            min: config.min,
            max: config.max,
            step: config.step,
            committed_value: 0.0,
            edit: TextEditState::default(),
            theme: config.theme,
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            state: InteractionState::new(config.state),
            modifiers: Modifiers::default(),
            dragging: false,
            paste_pending: false,
            hovered_stepper: None,
            pressed_stepper: None,
            stepper_held_frames: 0,
            font_metrics,
        };
        let initial = widget.normalize(config.initial).or_else(|| widget.normalize(0.0)).unwrap_or(0.0);
        widget.committed_value = initial;
        widget.edit = TextEditState::new(Self::canonical(initial));
        widget
    }

    fn bounds(&self) -> NumericBounds {
        NumericBounds::from_config(self.min, self.max)
    }

    fn normalize(&self, raw: f32) -> Option<f32> {
        if !raw.is_finite() {
            return None;
        }
        let bounds = self.bounds();
        let clamped = raw.clamp(bounds.min, bounds.max);
        if !self.step.is_finite() || self.step <= 0.0 {
            return Some(clamped);
        }
        let steps = ((f64::from(clamped) - f64::from(bounds.min)) / f64::from(self.step)).round();
        let snapped = steps.mul_add(f64::from(self.step), f64::from(bounds.min));
        // The finite check below rejects an f64 result outside f32's range.
        #[allow(clippy::cast_possible_truncation)]
        let snapped = snapped as f32;
        Some(if snapped.is_finite() {
            snapped.clamp(bounds.min, bounds.max)
        } else {
            clamped
        })
    }

    fn canonical(value: f32) -> String {
        if value == 0.0 {
            String::from("0")
        } else {
            value.to_string()
        }
    }

    fn policy() -> EditPolicy {
        EditPolicy { single_line: true, max_chars: NUMERIC_EDIT_MAX_CHARS }
    }

    fn parsed_buffer(&self) -> Option<f32> {
        self.edit.value().parse::<f32>().ok().and_then(|value| self.normalize(value))
    }

    fn preview(&self) -> Option<NumericEmission> {
        self.parsed_buffer().map(|value| NumericEmission { value, committed: false })
    }

    fn insert_text(&mut self, text: &str) -> Option<NumericEmission> {
        self.edit.clear_composition();
        if self.edit.insert(text, Self::policy()) {
            self.preview()
        } else {
            None
        }
    }

    fn commit_buffer(&mut self) -> Option<NumericEmission> {
        let Some(value) = self.parsed_buffer() else {
            self.edit = TextEditState::new(Self::canonical(self.committed_value));
            return None;
        };
        self.committed_value = value;
        self.edit = TextEditState::new(Self::canonical(value));
        Some(NumericEmission { value, committed: true })
    }

    /// One arrow press — tapped or auto-repeated — as a step. `None` for a key
    /// that is not an arrow.
    ///
    /// Round-4 note 14: "holding the up/down arrow on an input box for numbers
    /// should increment/decrement the value consistently." The platform's own
    /// key repeat reaches a widget as repeated `aether.key` presses, so this
    /// stays **stateless**. The button and the toggle arm a key precisely so a
    /// repeat cannot fire a second click; a numeric that copied that arm would
    /// step once and then sit still under a held arrow.
    fn key_step(&mut self, code: u32) -> Option<NumericEmission> {
        let direction = match code {
            KEY_UP => StepDirection::Up,
            KEY_DOWN => StepDirection::Down,
            _ => return None,
        };
        Some(self.stepped(direction))
    }

    /// Advance a live press-and-hold on a stepper by one frame, yielding the
    /// step it owes. The repeat stops the moment the pointer leaves the button
    /// it is holding, which is what lets a person who changed their mind slide
    /// off it rather than having to undo what it kept doing.
    fn held_stepper_step(&mut self) -> Option<NumericEmission> {
        let direction = self.pressed_stepper?;
        if !self.state.can_mutate() || self.hovered_stepper != Some(direction) {
            return None;
        }
        self.stepper_held_frames = self.stepper_held_frames.saturating_add(1);
        stepper_repeats_at(self.stepper_held_frames).then(|| self.stepped(direction))
    }

    fn stepped(&mut self, direction: StepDirection) -> NumericEmission {
        let base = self.parsed_buffer().unwrap_or(self.committed_value);
        let amount = if self.step.is_finite() && self.step > 0.0 {
            self.step
        } else {
            1.0
        };
        let raw = match direction {
            StepDirection::Down => f64::from(base) - f64::from(amount),
            StepDirection::Up => f64::from(base) + f64::from(amount),
        };
        let bounds = self.bounds();
        let raw = if raw > f64::from(f32::MAX) {
            bounds.max
        } else if raw < f64::from(f32::MIN) {
            bounds.min
        } else {
            // The two guards above prove `raw` is inside f32's finite range.
            #[allow(clippy::cast_possible_truncation)]
            let raw = raw as f32;
            raw
        };
        let value = self.normalize(raw).unwrap_or(self.committed_value);
        self.committed_value = value;
        self.edit = TextEditState::new(Self::canonical(value));
        NumericEmission { value, committed: true }
    }

    fn emit(ctx: &WasmCtx<'_>, emission: NumericEmission) {
        if let Some(parent) = ctx.parent() {
            parent.send(&NumericChanged { value: emission.value, committed: emission.committed });
        }
    }

    fn resolved_metrics(&self) -> Option<&CachedFontMetrics> {
        self.font_metrics.resolved()
    }

    /// The measured width of the widest value this range can hold: whichever
    /// bound renders longer, formatted exactly the way the field formats a
    /// committed value.
    ///
    /// Both bounds, not just `max`, because `-100 .. 20` is widest at its
    /// minimum — the sign is a character like any other. The formatted text is
    /// capped at the edit buffer's own character bound so an effectively
    /// unbounded range (a non-finite bound resolves to `f32::MAX`) asks for a
    /// field rather than a wall.
    ///
    /// The endpoints, and only the endpoints, because this number has to be
    /// **stable**: a width that also weighed the value on screen would resize
    /// the slot under the reader on every keystroke. A fractional `step` can
    /// therefore render an interior value longer than either bound (`0 .. 100`
    /// by `0.5` holds `"12.5"`, wider than `"100"`); that one is the clip's
    /// job, not the width's — see
    /// [`SingleLineEdit::content_clip`](crate::set::SingleLineEdit).
    fn widest_value_width(&self, metrics: &CachedFontMetrics) -> f32 {
        let bounds = self.bounds();
        [bounds.min, bounds.max]
            .map(|value| Self::canonical(value).chars().take(NUMERIC_EDIT_MAX_CHARS as usize).collect::<String>())
            .iter()
            .map(|text| measured_text_width(metrics, text, self.theme.value_size_pixels))
            .fold(0.0, f32::max)
    }

    /// The size a layout should give this numeric: the widest value its range
    /// allows, one pad each side of it, and the stepper column at a row's
    /// height — so a consumer sizes the field to what it can hold instead of
    /// guessing.
    ///
    /// Round-4 note 6 is the failure this answers from the widget's side: a
    /// level field sized by eye rather than from this number is a field three
    /// digits do not fit in, and the digits then ran under the arrows. A host
    /// that takes the intrinsic gets a box whose value has one `pad` at each
    /// end and a square column beyond it.
    ///
    /// `None` until the theme font's metrics land, the same pre-measurement
    /// silence the button keeps: a slot sized from the per-character
    /// approximation would be resized the moment the real advances arrived.
    fn intrinsic(&self) -> Option<[f32; 2]> {
        let width = self.theme.pad.mul_add(2.0, self.widest_value_width(self.resolved_metrics()?));
        Some([width + self.theme.row_height, self.theme.row_height])
    }

    fn steppers(&self) -> Option<StepperColumn> {
        StepperColumn::of(self.frame.width, self.frame.height)
    }

    /// Which stepper button a window-space point lands on. `None` once the
    /// control is unavailable, so a disabled numeric's arrows are inert
    /// targets rather than live ones drawn grey.
    fn stepper_at(&self, event_x: f32, event_y: f32) -> Option<StepDirection> {
        if !self.state.can_mutate() {
            return None;
        }
        self.steppers()?.hit(event_x - self.frame.x, event_y - self.frame.y)
    }

    /// Which presentation one stepper button is in. `Disabled` follows the
    /// control, not the button: a numeric that cannot be mutated has no live
    /// arrows, so both draw faded however the pointer moves over them.
    fn stepper_state(&self, direction: StepDirection) -> ThemeState {
        if !self.state.can_mutate() {
            ThemeState::Disabled
        } else if self.pressed_stepper == Some(direction) {
            ThemeState::Pressed
        } else if self.hovered_stepper == Some(direction) {
            ThemeState::Hover
        } else {
            ThemeState::Normal
        }
    }

    /// The stepper column's draw, inside the control's one frame: the hairline
    /// that separates the arrows from the value they change, an overlay on the
    /// button the pointer is on or holding, and the two arrows.
    ///
    /// No fill of its own. `box_fill` is the colour the frame is already
    /// painted, and a button's overlay composites over exactly that, so an
    /// untouched column is bare control surface and a touched button is that
    /// same surface lifted — never a second box beside the value.
    fn stepper_items(&self, column: StepperColumn, box_fill: Rgba) -> Vec<WidgetDrawItem> {
        let theme = &self.theme;
        let mut items = Vec::with_capacity(3 + TRIANGLE_ROWS_PER_ARROW * 2);
        for direction in [StepDirection::Up, StepDirection::Down] {
            let (top, height) = column.button_span(direction);
            let button_state = self.stepper_state(direction);
            if matches!(button_state, ThemeState::Hover | ThemeState::Pressed) {
                items.push(quad(column.left, top, column.width, height, theme.fill(box_fill, button_state)));
            }

            let arrow_width = (column.width * ARROW_EXTENT_FRACTION).max(1.0);
            let arrow_height = (height * ARROW_EXTENT_FRACTION).max(1.0);
            push_triangle(
                &mut items,
                column.width.mul_add(0.5, column.left),
                (height - arrow_height).mul_add(0.5, top),
                arrow_width,
                arrow_height,
                direction == StepDirection::Up,
                theme.fill(theme.text_primary, button_state),
            );
        }
        // The one seam inside the control: a hairline separating the arrows
        // from the value they change.
        items.push(quad(column.left, 0.0, 1.0, column.height, theme.outline));
        items
    }

    fn hit_byte(&self, event_x: f32) -> usize {
        single_line_hit_byte(
            self.edit.value(),
            self.resolved_metrics(),
            self.theme.value_size_pixels,
            event_x - self.frame.x - self.theme.pad,
        )
    }

    fn theme_state(&self) -> ThemeState {
        if self.state.focused() {
            self.state.supporting_theme_state(self.dragging)
        } else {
            self.state.theme_state(self.dragging)
        }
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        if self.state.replace(next) {
            if !self.state.can_mutate() {
                self.edit.clear_composition();
                self.paste_pending = false;
            }
            if !self.state.is_available() {
                self.dragging = false;
                self.hovered_stepper = None;
                self.pressed_stepper = None;
            }
            emit_state_changed(ctx, &self.state);
        }
    }
}

impl WidgetDefaults for NumericWidget {
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
        self.dragging = false;
        self.paste_pending = false;
        self.pressed_stepper = None;
        self.edit.clear_composition();
    }
}

/// A numeric editor. Spawned inline by a panel root with a [`NumericConfig`];
/// reports preview and committed [`NumericChanged`] events.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for NumericWidget {
    type Config = NumericConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.numeric";

    fn init(config: NumericConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self::configured(config))
    }

    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: NumericConfig) {
        self.min = config.min;
        self.max = config.max;
        self.step = config.step;
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        let initial = self.normalize(config.initial).or_else(|| self.normalize(0.0)).unwrap_or(0.0);
        self.committed_value = initial;
        self.edit = TextEditState::new(Self::canonical(initial));
        self.dragging = false;
        self.paste_pending = false;
        self.hovered_stepper = None;
        self.pressed_stepper = None;
        self.apply_control_state(ctx, config.state);
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        self.apply_control_state(ctx, set.state);
    }

    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        apply_text_theme(ctx, &mut self.font_metrics, &mut self.theme, set.theme);
    }

    #[handler::single]
    fn on_focus_lost(&mut self, ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        if self.state.can_mutate()
            && let Some(emission) = self.commit_buffer()
        {
            Self::emit(ctx, emission);
        }
        self.state.lose_focus();
        self.dragging = false;
        self.paste_pending = false;
        self.pressed_stepper = None;
        self.edit.clear_composition();
    }

    #[handler::single]
    fn on_text_input(&mut self, ctx: &mut WasmCtx<'_>, input: TextInput) {
        if self.state.can_mutate()
            && let Some(emission) = self.insert_text(&input.text)
        {
            Self::emit(ctx, emission);
        }
    }

    /// Enter commits and Up/Down step; every other editing key resolves
    /// through the set's shared vocabulary, so the numeric buffer honours the
    /// same select-all / copy / cut / paste, Delete, Home/End, and word-motion
    /// chords a text field does — Cmd as well as Ctrl. A repeated press is
    /// another edit, never a suppressed repeat: round-4 note 14 wants a held
    /// arrow to keep stepping, and the platform's key repeat is exactly a
    /// stream of presses ([`NumericWidget::key_step`]).
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if !self.state.is_available() {
            return;
        }
        match key.code {
            KEY_ENTER if self.state.can_mutate() => {
                if let Some(emission) = self.commit_buffer() {
                    Self::emit(ctx, emission);
                }
            }
            code @ (KEY_DOWN | KEY_UP) if self.state.can_mutate() => {
                if let Some(emission) = self.key_step(code) {
                    Self::emit(ctx, emission);
                }
            }
            code => {
                let Some(command) = edit_command(code, self.modifiers) else {
                    return;
                };
                if run_edit_key(ctx, &mut self.edit, &mut self.paste_pending, command, self.state.can_mutate())
                    && let Some(emission) = self.preview()
                {
                    Self::emit(ctx, emission);
                }
            }
        }
    }

    #[handler::single]
    //noinspection DuplicatedCode -- actor macros require one pointer handler per concrete widget type.
    /// A press on a stepper button steps the value there and then and starts
    /// the press-and-hold repeat; anywhere else in the box places the caret
    /// and arms a selection drag.
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button == mouse_button::LEFT
            && let Some(direction) = self.stepper_at(press.x, press.y)
        {
            self.pressed_stepper = Some(direction);
            // The pointer is demonstrably on that button, whether or not a
            // move has arrived to say so — and the held repeat reads this to
            // decide it is still there.
            self.hovered_stepper = Some(direction);
            self.stepper_held_frames = 0;
            Self::emit(ctx, self.stepped(direction));
            return;
        }
        let Some(event_x) = arm_text_drag(&self.state, &mut self.dragging, press) else {
            return;
        };
        self.edit.place_caret(self.hit_byte(event_x));
    }

    #[handler::single]
    /// Track which stepper the pointer is over (its hover overlay), and
    /// extend the selection while a text drag is live.
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        self.hovered_stepper = self.stepper_at(moved.x, moved.y);
        if self.dragging && self.state.is_available() {
            let byte = self.hit_byte(moved.x);
            self.edit.extend_to(byte);
        }
    }

    #[handler::single]
    fn on_mouse_button_release(&mut self, _ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        release_left(&mut self.dragging, false, release);
        release_left(&mut self.pressed_stepper, None, release);
    }

    /// The pointer leaving the control clears the stepper hover the root's
    /// hover fact alone cannot: hover is per-widget, the overlay per-button.
    #[handler::single]
    fn on_hover_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: HoverLost) {
        self.state.set_hovered(false);
        self.hovered_stepper = None;
    }

    #[handler::single]
    fn on_modifiers(&mut self, _ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        if self.state.is_available() {
            self.modifiers = modifiers;
        }
    }

    #[handler::single]
    fn on_ime_preedit(&mut self, _ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        if !self.state.can_mutate() {
            return;
        }
        let cursor = preedit
            .cursor_begin
            .zip(preedit.cursor_end)
            .map(|(begin, end)| TextSpan::new(begin as usize, end as usize));
        self.edit.set_composition(preedit.text, cursor);
    }

    #[handler::single]
    fn on_get_clipboard_text_result(&mut self, ctx: &mut WasmCtx<'_>, result: GetClipboardTextResult) {
        if accept_clipboard_paste(
            &mut self.paste_pending,
            &mut self.edit,
            Self::policy(),
            self.state.can_mutate(),
            result,
        ) && let Some(emission) = self.preview()
        {
            Self::emit(ctx, emission);
        }
    }

    #[handler::single]
    #[allow(clippy::unused_self)]
    fn on_set_clipboard_text_result(&mut self, _ctx: &mut WasmCtx<'_>, result: SetClipboardTextResult) {
        report_clipboard_copy(&result);
    }

    #[handler::single]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_>, result: FontMetricsResult) {
        accept_font_metrics_result(ctx, &mut self.font_metrics, result);
    }

    /// Advance a held stepper's repeat by one frame, then reply the control's
    /// one draw: the shared single-line box, the stepper column inside it, the
    /// outlines around the lot, and the width this numeric's own range asks a
    /// layout for.
    ///
    /// The repeat is timed here because a widget never sees a tick — the
    /// root's per-frame `Collect` is the only regular pulse that reaches a
    /// child, the same clock the toast region ages its notices by.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if let Some(emission) = self.held_stepper_step() {
            Self::emit(ctx, emission);
        }
        let theme_state = self.theme_state();
        let displayed = self.edit.displayed();
        let mut edit = SingleLineEdit::new(
            &displayed,
            self.resolved_metrics(),
            &self.theme,
            &self.state,
            theme_state,
            &self.frame,
        );
        edit.intrinsic = self.intrinsic();
        if let Some(column) = self.steppers() {
            edit.gutter = column.width;
            edit.gutter_items = self.stepper_items(column, single_line_box_fill(&self.theme, theme_state));
        }
        reply_single_line_edit(ctx, edit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_kinds::FontMetrics;
    use alloc::vec::Vec;

    use crate::set::{EditCommand, apply_edit_command};

    fn numeric(min: f32, max: f32, step: f32, initial: f32) -> NumericWidget {
        NumericWidget::configured(NumericConfig {
            min,
            max,
            step,
            initial,
            theme: Theme::DEFAULT,
            state: WidgetControlState::default(),
        })
    }

    fn replace_buffer(widget: &mut NumericWidget, text: &str) -> Option<NumericEmission> {
        widget.edit.select_all();
        widget.insert_text(text)
    }

    #[test]
    fn invalid_intermediates_and_non_finite_values_stay_visible_without_events() {
        let mut widget = numeric(-10.0, 20.0, 0.5, 2.0);
        for invalid in ["", "-", ".", "NaN", "inf", "-inf"] {
            widget.edit.select_all();
            widget.edit.delete_backward();
            let emission = if invalid.is_empty() {
                widget.preview()
            } else {
                widget.insert_text(invalid)
            };
            assert_eq!(emission, None, "{invalid:?} is not a finite numeric preview");
            assert_eq!(widget.edit.value(), invalid);
        }
        assert_eq!(widget.committed_value, 2.0);
    }

    #[test]
    fn preview_clamps_and_snaps_without_rewriting_then_commit_canonicalizes() {
        let mut widget = numeric(0.0, 10.0, 0.5, 2.0);
        assert_eq!(replace_buffer(&mut widget, "7.26"), Some(NumericEmission { value: 7.5, committed: false }));
        assert_eq!(widget.edit.value(), "7.26", "preview preserves the authored buffer");
        assert_eq!(widget.commit_buffer(), Some(NumericEmission { value: 7.5, committed: true }));
        assert_eq!(widget.edit.value(), "7.5");

        assert_eq!(replace_buffer(&mut widget, "99"), Some(NumericEmission { value: 10.0, committed: false }));
        assert_eq!(widget.edit.value(), "99");
    }

    #[test]
    fn invalid_commit_reverts_to_the_last_canonical_value_without_event() {
        let mut widget = numeric(-10.0, 20.0, 0.5, 2.0);
        assert_eq!(replace_buffer(&mut widget, "-"), None);
        assert_eq!(widget.commit_buffer(), None);
        assert_eq!(widget.edit.value(), "2");
        assert_eq!(widget.committed_value, 2.0);
    }

    #[test]
    fn step_uses_the_current_valid_value_or_falls_back_to_committed() {
        let mut widget = numeric(-10.0, 20.0, 0.5, 2.0);
        assert_eq!(replace_buffer(&mut widget, "4.2"), Some(NumericEmission { value: 4.0, committed: false }));
        assert_eq!(widget.stepped(StepDirection::Up), NumericEmission { value: 4.5, committed: true });
        assert_eq!(widget.edit.value(), "4.5");

        assert_eq!(replace_buffer(&mut widget, "-"), None);
        assert_eq!(widget.stepped(StepDirection::Down), NumericEmission { value: 4.0, committed: true });
        assert_eq!(widget.edit.value(), "4");
    }

    #[test]
    fn cut_and_paste_run_through_the_same_preview_path_typing_does() {
        // The buffer/committed split is numeric's own: a cut empties the
        // buffer to an unparseable state that must emit nothing, and the
        // paste back must preview rather than commit.
        let mut widget = numeric(-10.0, 20.0, 0.5, 12.5);
        widget.edit.select_all();
        let cut = apply_edit_command(&mut widget.edit, EditCommand::Cut, true);
        assert_eq!(cut.copy.as_deref(), Some("12.5"));
        assert!(cut.changed);
        assert_eq!(widget.edit.value(), "");
        assert_eq!(widget.preview(), None, "an empty buffer is not a number");

        assert_eq!(widget.insert_text("12.5"), Some(NumericEmission { value: 12.5, committed: false }));
        assert_eq!(widget.edit.value(), "12.5");
    }

    #[test]
    fn a_held_backspace_keeps_deleting_and_delete_forward_is_wired() {
        // Tripwire: the owner's note. Editing keys must not inherit the
        // button's repeat suppression — every repeated press is another edit.
        let mut widget = numeric(-100.0, 100.0, 0.0, 0.0);
        widget.edit = TextEditState::new(String::from("12345"));
        for _ in 0..3 {
            apply_edit_command(&mut widget.edit, EditCommand::DeleteBackward, true);
        }
        assert_eq!(widget.edit.value(), "12", "three repeats delete three characters");

        widget.edit.move_to_start(false);
        apply_edit_command(&mut widget.edit, EditCommand::DeleteForward, true);
        assert_eq!(widget.edit.value(), "2");
    }

    #[test]
    fn typed_insertions_respect_internal_cap_and_normal_numeric_edits_continue() {
        let mut widget = numeric(-f32::MAX, f32::MAX, 0.0, 0.0);
        widget.edit.select_all();
        widget.edit.delete_backward();
        for _ in 0..40 {
            widget.insert_text("1");
        }
        assert_eq!(widget.edit.value().chars().count(), NUMERIC_EDIT_MAX_CHARS as usize);
        assert_eq!(widget.edit.value(), "11111111111111111111111111111111");

        assert_eq!(replace_buffer(&mut widget, "7.5"), Some(NumericEmission { value: 7.5, committed: false }));
        assert_eq!(widget.edit.value(), "7.5");
    }

    #[test]
    fn pointer_placement_and_selection_never_split_multibyte_text() {
        let mut widget = numeric(-10.0, 20.0, 0.5, 2.0);
        widget.frame = WidgetFrame { x: 10.0, y: 0.0, width: 100.0, height: 24.0 };
        widget.edit = TextEditState::new(String::from("1é2"));
        let byte = widget.hit_byte(widget.theme.value_size_pixels.mul_add(0.75, 10.0 + widget.theme.pad));
        assert!([0, 1, 3, 4].contains(&byte), "hit byte {byte} must be a UTF-8 boundary");
        widget.edit.place_caret(byte);
        widget.edit.move_right(true);
        let selection = widget.edit.selection();
        assert!(widget.edit.value().is_char_boundary(selection.start_byte));
        assert!(widget.edit.value().is_char_boundary(selection.end_byte));
    }

    #[test]
    fn the_stepper_column_splits_into_an_up_and_a_down_target_at_the_right_end() {
        // The owner's note: the value must be clickable up and down, so each
        // half of the column has to be a real target and the text has to keep
        // the rest of the frame.
        let column = StepperColumn::of(120.0, 24.0).expect("a normal numeric row has steppers");
        assert_eq!((column.left, column.width), (96.0, 24.0), "a square column, one row height wide");
        assert_eq!(column.hit(100.0, 4.0), Some(StepDirection::Up), "the top half steps up");
        assert_eq!(column.hit(100.0, 20.0), Some(StepDirection::Down), "the bottom half steps down");
        assert_eq!(column.hit(95.9, 12.0), None, "left of the column is the text box");
        assert_eq!(column.hit(100.0, 24.0), None, "below the frame is nobody's");

        let narrow = StepperColumn::of(20.0, 24.0).expect("a narrow numeric still gets steppers");
        assert_eq!(narrow.width, 10.0, "the column never takes more than half the frame");
        assert!(StepperColumn::of(0.0, 0.0).is_none(), "an unlaid-out frame has no column to place");
    }

    #[test]
    fn a_stepper_press_steps_and_commits_the_same_way_the_arrow_keys_do() {
        // Tripwire: the button must reuse the clamp/snap/commit path, not a
        // second one that could drift from what Up/Down does.
        let mut widget = numeric(0.0, 10.0, 0.5, 2.0);
        widget.frame = WidgetFrame { x: 10.0, y: 20.0, width: 120.0, height: 24.0 };

        assert_eq!(widget.stepper_at(10.0 + 100.0, 20.0 + 4.0), Some(StepDirection::Up));
        assert_eq!(widget.stepper_at(10.0 + 100.0, 20.0 + 20.0), Some(StepDirection::Down));
        assert_eq!(widget.stepper_at(10.0 + 20.0, 20.0 + 12.0), None, "a press in the text box places a caret");

        assert_eq!(widget.stepped(StepDirection::Up), NumericEmission { value: 2.5, committed: true });
        assert_eq!(widget.edit.value(), "2.5", "the buffer is canonicalized like a keyed step");

        widget.committed_value = 10.0;
        widget.edit = TextEditState::new(String::from("10"));
        assert_eq!(widget.stepped(StepDirection::Up), NumericEmission { value: 10.0, committed: true }, "clamped");
    }

    #[test]
    fn a_held_arrow_key_steps_on_every_repeat() {
        // Tripwire: round-4 note 14. The platform's key repeat arrives as
        // repeated `aether.key` presses, so the step path must hold no arm.
        // The button and the toggle deliberately arm a key so a repeat cannot
        // fire a second click; copying that here is the bug — a held arrow
        // would step once and then sit still.
        let mut widget = numeric(0.0, 10.0, 0.5, 2.0);
        for step in 1..=4u8 {
            let emission = widget.key_step(KEY_UP).expect("an arrow is a step, however many times it arrives");
            assert_eq!(emission, NumericEmission { value: 0.5f32.mul_add(f32::from(step), 2.0), committed: true });
        }
        assert_eq!(widget.committed_value, 4.0, "four repeats moved four steps");

        for _ in 0..2 {
            widget.key_step(KEY_DOWN).expect("and the other way");
        }
        assert_eq!(widget.committed_value, 3.0);
        assert_eq!(widget.key_step(KEY_ENTER), None, "a key that is not an arrow is not a step");
    }

    #[test]
    fn a_held_stepper_button_repeats_after_a_delay_and_stops_when_the_pointer_leaves() {
        // Tripwire: round-4 note 14 with the other hand. A press and hold on
        // the arrow *button* is the same gesture as a held arrow key, so a
        // spinner that answers one and not the other has two routes that
        // disagree. The delay is what keeps a deliberate single click from
        // becoming two, and letting go of the button — by releasing it or by
        // sliding off it — has to stop the repeat.
        assert!(!stepper_repeats_at(0), "the press itself already stepped; the frame after it must not");
        assert!(!stepper_repeats_at(STEPPER_REPEAT_DELAY_FRAMES - 1), "nothing before the delay");
        assert!(stepper_repeats_at(STEPPER_REPEAT_DELAY_FRAMES), "then the first repeat");
        assert!(!stepper_repeats_at(STEPPER_REPEAT_DELAY_FRAMES + 1));
        assert!(stepper_repeats_at(STEPPER_REPEAT_DELAY_FRAMES + STEPPER_REPEAT_INTERVAL_FRAMES), "then the cadence");

        let mut widget = numeric(0.0, 100.0, 1.0, 0.0);
        widget.frame = WidgetFrame { x: 0.0, y: 0.0, width: 120.0, height: 24.0 };
        assert_eq!(widget.held_stepper_step(), None, "nothing is held, so no frame owes a step");

        widget.pressed_stepper = Some(StepDirection::Up);
        widget.hovered_stepper = Some(StepDirection::Up);
        widget.stepper_held_frames = 0;
        let steps = (0..STEPPER_REPEAT_DELAY_FRAMES + STEPPER_REPEAT_INTERVAL_FRAMES)
            .filter(|_| widget.held_stepper_step().is_some())
            .count();
        assert_eq!(steps, 2, "one repeat at the delay and one an interval later");
        assert_eq!(widget.committed_value, 2.0);

        widget.hovered_stepper = None;
        for _ in 0..STEPPER_REPEAT_DELAY_FRAMES * 2 {
            assert_eq!(widget.held_stepper_step(), None, "a pointer slid off the button repeats nothing");
        }
        assert_eq!(widget.committed_value, 2.0);

        let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
        widget.hovered_stepper = Some(StepDirection::Up);
        widget.state.replace(read_only);
        for _ in 0..STEPPER_REPEAT_DELAY_FRAMES * 2 {
            assert_eq!(widget.held_stepper_step(), None, "and neither does a control that cannot be mutated");
        }
    }

    #[test]
    fn an_unavailable_numeric_has_no_live_stepper_targets() {
        let mut widget = numeric(0.0, 10.0, 0.5, 2.0);
        widget.frame = WidgetFrame { x: 0.0, y: 0.0, width: 120.0, height: 24.0 };
        let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
        widget.state.replace(read_only);
        assert_eq!(widget.stepper_at(100.0, 4.0), None, "a read-only value is not steppable by pointer either");
    }

    /// Every glyph half the draw size wide, so a measured width is exactly
    /// `chars * size * 0.5` and the intrinsic arithmetic is checkable by hand.
    fn uniform_metrics() -> CachedFontMetrics {
        CachedFontMetrics::new(&FontMetrics {
            units_per_em: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            line_gap: 0.0,
            default_advance: 500.0,
            advances: Vec::new(),
        })
    }

    fn with_metrics(widget: &mut NumericWidget) {
        widget.font_metrics.take_pending_request();
        widget.font_metrics.accept_reply(Some(uniform_metrics()));
    }

    #[test]
    fn the_stepper_column_overlays_the_control_fill_instead_of_painting_a_second_box() {
        // Tripwire: the owner's note. The steppers are part of the numeric,
        // not a box beside it — only the button under the pointer paints, and
        // it paints the control's own fill lifted, never another palette role.
        let mut widget = numeric(0.0, 10.0, 0.5, 2.0);
        widget.frame = WidgetFrame { x: 0.0, y: 0.0, width: 120.0, height: 24.0 };
        widget.hovered_stepper = Some(StepDirection::Up);
        let column = widget.steppers().expect("steppers");
        let box_fill = single_line_box_fill(&Theme::DEFAULT, ThemeState::Normal);
        let items = widget.stepper_items(column, box_fill);

        let fills: Vec<_> = items
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Quad { x, y, width, height, color, .. }
                    if (*width - column.width).abs() < 1e-4 && *height > 1.0 =>
                {
                    Some((*x, *y, *color))
                }
                _ => None,
            })
            .collect();
        assert_eq!(fills.len(), 1, "only the touched button paints; items were {items:?}");
        assert_eq!((fills[0].0, fills[0].1), (column.left, 0.0), "the hovered up button, at the column's left edge");
        assert_eq!(fills[0].2, Theme::DEFAULT.fill(box_fill, ThemeState::Hover), "the control's fill, lifted");

        assert!(
            !items
                .iter()
                .any(|item| matches!(item, WidgetDrawItem::Quad { color, .. } if *color == Theme::DEFAULT.surface)),
            "no part of the column fills itself from a second surface role; items were {items:?}",
        );
        let hairlines = items.iter().filter(|item| {
            matches!(item, WidgetDrawItem::Quad { x, width, height, color, .. }
                if *x == column.left && *width == 1.0 && *height == column.height && *color == Theme::DEFAULT.outline)
        });
        assert_eq!(hairlines.count(), 1, "one hairline is the whole seam; items were {items:?}");
    }

    #[test]
    fn the_intrinsic_width_fits_the_widest_value_the_range_allows_plus_pads_and_the_column() {
        // Tripwire: a consumer sizes the field from this number, so it has to
        // hold the range's longest formatted value — here the negative bound,
        // not `max` — with a pad each side and the stepper column beside it.
        let mut widget = numeric(-100.0, 20.0, 1.0, 0.0);
        assert_eq!(widget.intrinsic(), None, "no guessed size before the font's advances land");

        with_metrics(&mut widget);
        let theme = &Theme::DEFAULT;
        let advance = theme.value_size_pixels * 0.5;
        let intrinsic = widget.intrinsic().expect("measured");
        assert_eq!(
            intrinsic,
            [advance.mul_add(4.0, theme.pad * 2.0) + theme.row_height, theme.row_height],
            "\"-100\" plus two pads plus a square stepper column",
        );

        let mut wider_max = numeric(-1.0, 20000.0, 1.0, 0.0);
        with_metrics(&mut wider_max);
        assert_eq!(
            wider_max.intrinsic().expect("measured")[0],
            advance.mul_add(5.0, theme.pad * 2.0) + theme.row_height,
            "the widest bound wins whichever end it is",
        );
    }

    #[test]
    fn an_unbounded_range_asks_for_a_field_rather_than_a_wall() {
        // Tripwire: the bounds fall back to f32::MIN / MAX, whose canonical
        // text is 39 characters — wider than the buffer can ever hold, so the
        // reported width is capped at the buffer's own bound.
        let mut widget = numeric(f32::NAN, f32::NAN, 1.0, 0.0);
        with_metrics(&mut widget);
        let theme = &Theme::DEFAULT;
        let advance = theme.value_size_pixels * 0.5;
        #[allow(clippy::cast_precision_loss)]
        let capped = NUMERIC_EDIT_MAX_CHARS as f32;
        assert_eq!(
            widget.intrinsic().expect("measured")[0],
            advance.mul_add(capped, theme.pad * 2.0) + theme.row_height
        );
    }

    #[test]
    fn three_digits_fit_the_field_the_range_asks_for_and_stop_a_pad_short_of_the_seam() {
        // Tripwire: round-4 note 6 — "lack of symmetry in the space of the
        // left padding and right padding of the number text. Three digits
        // overflows and is drawn behind the arrows." At the width this range
        // asks for, the widest value it can hold starts one pad in and ends
        // exactly one pad short of the hairline; and whatever width the host
        // actually gives, the clip holds the value off the arrows.
        let mut widget = numeric(1.0, 100.0, 1.0, 1.0);
        with_metrics(&mut widget);
        let theme = &Theme::DEFAULT;
        let advance = theme.value_size_pixels * 0.5;
        let [width, height] = widget.intrinsic().expect("measured");
        assert_eq!(
            [width, height],
            [advance.mul_add(3.0, theme.pad * 2.0) + theme.row_height, theme.row_height],
            "\"100\" plus a pad each side plus the square stepper column",
        );

        widget.frame = WidgetFrame { x: 0.0, y: 0.0, width, height };
        widget.committed_value = 100.0;
        widget.edit = TextEditState::new(String::from("100"));
        let column = widget.steppers().expect("a laid-out numeric has its column");
        assert_eq!(column.left, width - theme.row_height, "the column is the square the intrinsic reserved");

        let clip = content_clip_of(&widget, column).expect("a gutter means a seam to be held off");
        assert_eq!(clip.width, column.left - theme.pad, "the value's box ends one pad before the hairline");
        let metrics = widget.resolved_metrics().expect("measured");
        assert_eq!(
            theme.pad + measured_text_width(metrics, "100", theme.value_size_pixels),
            clip.width,
            "so the widest value the range holds ends exactly at that margin: left pad == right pad",
        );

        // Half the width the field asked for: the value is cut at its own
        // margin rather than printing across the hairline.
        widget.frame.width = width * 0.5;
        let narrow_column = widget.steppers().expect("column");
        let narrow = content_clip_of(&widget, narrow_column).expect("clip");
        assert!(narrow.width < narrow_column.left, "the clip never reaches the arrows: {narrow:?}");
    }

    /// The bound the collect path puts on the value's own draws, read without
    /// standing up an actor: the same `SingleLineEdit` `on_collect` builds.
    fn content_clip_of(widget: &NumericWidget, column: StepperColumn) -> Option<crate::WidgetClipRect> {
        let displayed = widget.edit.displayed();
        let mut edit = SingleLineEdit::new(
            &displayed,
            widget.resolved_metrics(),
            &widget.theme,
            &widget.state,
            ThemeState::Normal,
            &widget.frame,
        );
        edit.gutter = column.width;
        edit.content_clip()
    }

    #[test]
    fn canonical_zero_drops_negative_sign_and_reversed_bounds_are_safe() {
        assert_eq!(NumericWidget::canonical(-0.0), "0");
        let widget = numeric(10.0, -10.0, 1.0, 99.0);
        assert_eq!(widget.committed_value, 10.0);
        assert_eq!(widget.edit.value(), "10");
    }
}
