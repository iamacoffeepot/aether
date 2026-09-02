//! The concrete widget set: module-composable `#[actor(instanced, composable)]`
//! child actors a panel root spawns as inline children and drives by mail in four
//! lanes — config / style / layout-frame data-down, value events-up — over the
//! ADR-0117 draw-compositing protocol.
//!
//! - [`SliderWidget`] — a horizontal value slider, dragged or
//!   arrow-nudged.
//! - [`TextFieldWidget`] — a single-line editable string.
//! - [`TextAreaWidget`] — a multiline measured editor with line scrolling.
//! - [`RadioGroupWidget`] — a vertical list of exclusive options.
//! - [`ButtonWidget`] — a momentary push button.
//! - [`LabelWidget`] — static, non-interactive text.
//! - [`ImageWidget`] — a static, non-interactive borrowed texture.
//! - [`VirtualListWidget`] — a fixed-row virtualized item list.
//! - [`ToggleWidget`] — a boolean switch.
//! - [`SegmentedWidget`] — a horizontal exclusive choice.
//! - [`NumericWidget`] — a typed and steppable bounded number.
//! - [`DropdownWidget`] — one current choice with its alternatives in a list
//!   that opens on demand, drawn in the overlay layer.
//! - [`TabStripWidget`] — one row of content-sized tabs selecting a parallel
//!   content set.
//! - [`MenuBarWidget`] — a row of application menus whose items open in the
//!   overlay layer.
//!
//! Each caches its assigned [`WidgetFrame`] rect
//! and its [`Theme`], answers every
//! [`Collect`](crate::Collect) with a
//! [`WidgetDrawList`] drawn in its own local
//! coordinates (colors resolved through [`Theme::fill`]),
//! and reports value changes up to its parent. Widgets never subscribe to
//! input; the root forwards it — see [`super::focus::Focus`] and
//! [`super::panel::WidgetPanel`].
//!
//! Inline children run `wire` after `init`, like loaded actors, but still rely
//! on the root's first `WidgetFrame` for layout and the first `Collect` for
//! their first draw.

pub mod button;
pub mod defaults;
pub mod dropdown;
pub mod image;
pub mod label;
pub mod menu_bar;
pub mod numeric;
pub mod radio;
pub mod segmented;
pub mod slider;
pub mod tab_strip;
pub mod text_area;
pub mod text_field;
pub mod toggle;
pub mod virtual_list;

pub use button::ButtonWidget;
pub use defaults::WidgetDefaults;
pub use dropdown::DropdownWidget;
pub use image::ImageWidget;
pub use label::LabelWidget;
pub use menu_bar::MenuBarWidget;
pub use numeric::NumericWidget;
pub use radio::RadioGroupWidget;
pub use segmented::SegmentedWidget;
pub use slider::SliderWidget;
pub use tab_strip::TabStripWidget;
pub use text_area::TextAreaWidget;
pub use text_field::TextFieldWidget;
pub use toggle::ToggleWidget;
pub use virtual_list::VirtualListWidget;

use alloc::string::String;
use alloc::vec::Vec;
use core::mem;

use aether_actor::WasmCtx;
use aether_clipboard::{ClipboardCapability, ClipboardMailboxExt, GetClipboardTextResult, SetClipboardTextResult};
use aether_kinds::keycode::{
    KEY_A, KEY_BACKSPACE, KEY_C, KEY_DELETE, KEY_END, KEY_ENTER, KEY_HOME, KEY_LEFT, KEY_RIGHT, KEY_SPACE, KEY_V, KEY_X,
};
use aether_kinds::{CachedFontMetrics, Modifiers, MouseButton, MouseButtonRelease, mouse_button};
use aether_math::Rgba;
use aether_text::{FontMetricsRequest, FontMetricsResult, FontRef, TextCapability};

use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::{DisplayedEdit, EditPolicy, FontMetricsAdapter, SingleLineLayout, TextEditState};
use crate::theme::{Theme, ThemeState};
use crate::{WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardArm {
    Enter,
    Space,
}

#[derive(Debug, Default)]
struct ActivationArms {
    pointer_pressed: bool,
    keyboard_arm: Option<KeyboardArm>,
}

impl ActivationArms {
    fn contains(frame: &WidgetFrame, x: f32, y: f32) -> bool {
        x >= frame.x && x <= frame.x + frame.width && y >= frame.y && y <= frame.y + frame.height
    }

    fn press_pointer(&mut self, frame: &WidgetFrame, eligible: bool, x: f32, y: f32) {
        if eligible && Self::contains(frame, x, y) {
            self.pointer_pressed = true;
        }
    }

    fn press_mouse_button(&mut self, frame: &WidgetFrame, eligible: bool, press: MouseButton) {
        if press.button == mouse_button::LEFT {
            self.press_pointer(frame, eligible, press.x, press.y);
        }
    }

    fn release_pointer(&mut self, frame: &WidgetFrame, eligible: bool, x: f32, y: f32) -> bool {
        let activates = eligible && self.pointer_pressed && Self::contains(frame, x, y);
        self.pointer_pressed = false;
        activates
    }

    fn press_key(&mut self, eligible: bool, code: u32) -> bool {
        if !eligible || self.keyboard_arm.is_some() {
            return false;
        }
        match code {
            KEY_ENTER => {
                self.keyboard_arm = Some(KeyboardArm::Enter);
                true
            }
            KEY_SPACE => {
                self.keyboard_arm = Some(KeyboardArm::Space);
                false
            }
            _ => false,
        }
    }

    fn release_key(&mut self, eligible: bool, code: u32) -> bool {
        match (code, self.keyboard_arm) {
            (KEY_ENTER, Some(KeyboardArm::Enter)) => {
                self.keyboard_arm = None;
                false
            }
            (KEY_SPACE, Some(KeyboardArm::Space)) => {
                self.keyboard_arm = None;
                eligible
            }
            _ => false,
        }
    }

    fn pressed(&self) -> bool {
        self.pointer_pressed || self.keyboard_arm == Some(KeyboardArm::Space)
    }

    fn clear(&mut self) {
        self.pointer_pressed = false;
        self.keyboard_arm = None;
    }
}

/// How far one caret movement travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EditStep {
    /// One `char`.
    Character,
    /// To the far side of the adjacent word.
    Word,
    /// To the near end of the line the caret is on.
    LineEdge,
    /// To the near end of the whole buffer.
    DocumentEdge,
}

/// One editing intent a key press resolved to, independent of which control
/// received it. Every text control in the set maps its `Key` mail through
/// [`edit_command`] so the chords cannot drift apart between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EditCommand {
    SelectAll,
    Copy,
    Cut,
    Paste,
    DeleteBackward,
    DeleteForward,
    MoveLeft { step: EditStep, extend: bool },
    MoveRight { step: EditStep, extend: bool },
}

/// Whether the platform's editing-chord modifier is held. Both `ctrl` and
/// `meta` count, always: Cmd is the chord on macOS and Ctrl everywhere else,
/// and a widget cannot ask which platform its window is on — the substrate
/// reports the physical modifiers and nothing more. Accepting either is what
/// the owner's Cmd+A note asks for, and it costs nothing, because no control in
/// the set binds the two modifiers to different meanings.
fn edit_chord(modifiers: Modifiers) -> bool {
    modifiers.ctrl || modifiers.meta
}

/// The caret-movement distance an arrow press asks for under `modifiers`:
/// Cmd/meta jumps to the line edge (the macOS convention), Ctrl or Alt steps a
/// word (the Windows/Linux and macOS conventions respectively), bare arrows
/// step one character.
fn arrow_step(modifiers: Modifiers) -> EditStep {
    if modifiers.meta {
        EditStep::LineEdge
    } else if modifiers.ctrl || modifiers.alt {
        EditStep::Word
    } else {
        EditStep::Character
    }
}

/// Resolve one key press into the editing intent it names, or `None` when the
/// key is not part of the shared editing vocabulary (Enter, Up/Down, and every
/// other key stay each control's own business).
///
/// Nothing here is suppressed on repeat: an editing key held down is meant to
/// keep editing, which is exactly the difference between this and the button's
/// [`ActivationArms::press_key`], where a repeat must not fire a second click.
pub(super) fn edit_command(code: u32, modifiers: Modifiers) -> Option<EditCommand> {
    let chord = edit_chord(modifiers);
    let extend = modifiers.shift;
    let command = match code {
        KEY_A if chord => EditCommand::SelectAll,
        KEY_C if chord => EditCommand::Copy,
        KEY_X if chord => EditCommand::Cut,
        KEY_V if chord => EditCommand::Paste,
        KEY_BACKSPACE => EditCommand::DeleteBackward,
        KEY_DELETE => EditCommand::DeleteForward,
        KEY_LEFT => EditCommand::MoveLeft { step: arrow_step(modifiers), extend },
        KEY_RIGHT => EditCommand::MoveRight { step: arrow_step(modifiers), extend },
        KEY_HOME if chord => EditCommand::MoveLeft { step: EditStep::DocumentEdge, extend },
        KEY_END if chord => EditCommand::MoveRight { step: EditStep::DocumentEdge, extend },
        KEY_HOME => EditCommand::MoveLeft { step: EditStep::LineEdge, extend },
        KEY_END => EditCommand::MoveRight { step: EditStep::LineEdge, extend },
        _ => return None,
    };
    Some(command)
}

/// What applying an [`EditCommand`] leaves for the widget to do. The edit
/// itself already happened; these are the parts only the widget can carry out.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct EditEffect {
    /// The committed text changed — re-preview, re-scroll, re-emit.
    pub(super) changed: bool,
    /// Put this run on the clipboard (a copy, or the copied half of a cut).
    pub(super) copy: Option<String>,
    /// Ask the clipboard for its text; its reply completes the paste.
    pub(super) request_paste: bool,
}

/// Apply one resolved command to an editing core. `mutable` gates every
/// command that would change the text — a read-only or unavailable control
/// still selects, copies, and moves its caret.
pub(super) fn apply_edit_command(edit: &mut TextEditState, command: EditCommand, mutable: bool) -> EditEffect {
    let mut effect = EditEffect::default();
    match command {
        EditCommand::SelectAll => edit.select_all(),
        EditCommand::Copy => effect.copy = selected_text(edit),
        EditCommand::Cut if mutable => {
            effect.copy = selected_text(edit);
            if effect.copy.is_some() {
                edit.clear_composition();
                edit.delete_backward();
                effect.changed = true;
            }
        }
        EditCommand::Paste if mutable => effect.request_paste = true,
        EditCommand::DeleteBackward | EditCommand::DeleteForward if !mutable => {}
        EditCommand::DeleteBackward => {
            edit.clear_composition();
            effect.changed = changed_by(edit, TextEditState::delete_backward);
        }
        EditCommand::DeleteForward => {
            edit.clear_composition();
            effect.changed = changed_by(edit, TextEditState::delete_forward);
        }
        EditCommand::MoveLeft { step, extend } => match step {
            EditStep::Character => edit.move_left(extend),
            EditStep::Word => edit.move_word_left(extend),
            EditStep::LineEdge => edit.move_to_line_start(extend),
            EditStep::DocumentEdge => edit.move_to_start(extend),
        },
        EditCommand::MoveRight { step, extend } => match step {
            EditStep::Character => edit.move_right(extend),
            EditStep::Word => edit.move_word_right(extend),
            EditStep::LineEdge => edit.move_to_line_end(extend),
            EditStep::DocumentEdge => edit.move_to_end(extend),
        },
        EditCommand::Cut | EditCommand::Paste => {}
    }
    effect
}

/// Resolve `key` against the shared editing vocabulary and carry it out,
/// clipboard traffic included. Returns whether the committed text changed, so
/// a control that previews or rescrolls on every edit can act on one bool.
///
/// `paste_pending` is the control's own single-flight guard: a second Paste
/// while a clipboard read is outstanding is dropped rather than queued.
pub(super) fn run_edit_key(
    ctx: &mut WasmCtx<'_>,
    edit: &mut TextEditState,
    paste_pending: &mut bool,
    command: EditCommand,
    mutable: bool,
) -> bool {
    let effect = apply_edit_command(edit, command, mutable);
    if let Some(text) = effect.copy {
        ctx.actor::<ClipboardCapability>().set_text(&text);
    }
    if effect.request_paste && !*paste_pending {
        *paste_pending = true;
        ctx.actor::<ClipboardCapability>().get_text();
    }
    effect.changed
}

/// Settle one outstanding clipboard read into `edit`. Returns whether text
/// actually landed. A reply that arrives with no paste in flight, or after the
/// control stopped being mutable, is dropped.
pub(super) fn accept_clipboard_paste(
    paste_pending: &mut bool,
    edit: &mut TextEditState,
    policy: EditPolicy,
    mutable: bool,
    result: GetClipboardTextResult,
) -> bool {
    if !mem::take(paste_pending) {
        return false;
    }
    match result {
        GetClipboardTextResult::Ok { text } if mutable => {
            edit.clear_composition();
            edit.insert(&text, policy)
        }
        GetClipboardTextResult::Ok { .. } => false,
        GetClipboardTextResult::Err { error } => {
            tracing::warn!(target: "aether_kit_widget", %error, "widget clipboard paste failed");
            false
        }
    }
}

/// Log a failed clipboard write. Nothing to undo — the copy simply did not
/// land — but a silent failure would leave a paste pasting stale text.
pub(super) fn report_clipboard_copy(result: &SetClipboardTextResult) {
    if let SetClipboardTextResult::Err { error } = result {
        tracing::warn!(target: "aether_kit_widget", %error, "widget clipboard copy failed");
    }
}

/// The selected run as an owned `String`, `None` when the selection is
/// collapsed (there is nothing to copy, and an empty clipboard write would
/// silently destroy what was on it).
fn selected_text(edit: &TextEditState) -> Option<String> {
    let selection = edit.selection();
    (!selection.is_collapsed()).then(|| String::from(&edit.value()[selection.start_byte..selection.end_byte]))
}

/// Run `edit_fn` and report whether it actually changed the committed text —
/// what a numeric control needs to decide whether to re-preview.
fn changed_by(edit: &mut TextEditState, edit_fn: impl FnOnce(&mut TextEditState)) -> bool {
    let before = edit.value().len();
    edit_fn(edit);
    edit.value().len() != before
}

fn text_control_theme_state(state: &InteractionState, dragging: bool) -> ThemeState {
    if state.focused() {
        state.supporting_theme_state(dragging)
    } else {
        state.theme_state(dragging)
    }
}

fn apply_text_control_state(
    ctx: &WasmCtx<'_>,
    state: &mut InteractionState,
    edit: &mut TextEditState,
    dragging: &mut bool,
    next: WidgetControlState,
) {
    if state.replace(next) {
        if !state.can_mutate() {
            edit.clear_composition();
        }
        if !state.is_available() {
            *dragging = false;
        }
        emit_state_changed(ctx, state);
    }
}

fn pump_text_font_metrics(ctx: &mut WasmCtx<'_>, font_metrics: &mut FontMetricsAdapter) {
    if let Some(id) = font_metrics.take_pending_request() {
        ctx.actor::<TextCapability>().send(&FontMetricsRequest { font: FontRef::Id(id) });
    }
}

fn apply_text_theme(ctx: &mut WasmCtx<'_>, font_metrics: &mut FontMetricsAdapter, theme: &mut Theme, next: Theme) {
    font_metrics.set_desired(next.font_id);
    *theme = next;
    pump_text_font_metrics(ctx, font_metrics);
}

/// Install a font-metrics reply and pump whatever newer request the settled
/// flight deferred. A stale reply — its font is no longer the desired one —
/// is dropped by the adapter.
fn accept_font_metrics_result(ctx: &mut WasmCtx<'_>, font_metrics: &mut FontMetricsAdapter, result: FontMetricsResult) {
    let pump_deferred = match result {
        FontMetricsResult::Ok { metrics } => font_metrics.accept_reply(Some(CachedFontMetrics::new(&metrics))),
        FontMetricsResult::Err { error } => {
            tracing::warn!(target: "aether_kit_widget", %error, "widget font metrics failed");
            font_metrics.accept_reply(None)
        }
    };
    if pump_deferred {
        pump_text_font_metrics(ctx, font_metrics);
    }
}

/// The measured pixel width of one line of `text` at `size_pixels` — the sum
/// of its glyphs' advances. A widget that sizes or centers against its text
/// calls this only once the font's metrics resolve, and keeps its unmeasured
/// draw until then rather than guessing a width from the per-character
/// approximation ([`APPROX_ADVANCE_RATIO`]), which would place the text wrong
/// and then visibly jump.
fn measured_text_width(metrics: &CachedFontMetrics, text: &str, size_pixels: f32) -> f32 {
    SingleLineLayout::build(text, metrics, size_pixels).width()
}

/// The local x at which a run `text_width` pixels wide sits centered in a
/// `width`-wide frame, never left of `pad`. A label wider than the frame
/// allows therefore falls back to the same left-padded origin an unmeasured
/// draw uses, instead of hanging off the left edge.
fn centered_text_x(width: f32, text_width: f32, pad: f32) -> f32 {
    ((width - text_width) * 0.5).max(pad)
}

fn release_left<T>(pressed: &mut T, released: T, release: MouseButtonRelease) {
    if release.button == mouse_button::LEFT {
        *pressed = released;
    }
}

fn arm_text_drag(state: &InteractionState, dragging: &mut bool, press: MouseButton) -> Option<f32> {
    if press.button != mouse_button::LEFT || !state.is_available() {
        return None;
    }
    *dragging = true;
    Some(press.x)
}

fn update_text_modifiers(state: &InteractionState, modifiers: &mut Modifiers, next: Modifiers) {
    if state.is_available() {
        *modifiers = next;
    }
}

fn apply_static_control_state(ctx: &WasmCtx<'_>, state: &mut InteractionState, next: WidgetControlState) {
    if state.replace(next) {
        emit_state_changed(ctx, state);
    }
}

fn clamp_option_index(index: u32, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (index as usize).min(len - 1)
    }
}

/// Discharge the hidden-widget branch of the always-reply compositing
/// protocol. Hidden controls retain their slot, so every `Collect` must still
/// produce one empty draw-list reply.
pub(super) fn reply_if_hidden(ctx: &WasmCtx<'_>, state: &InteractionState) -> bool {
    if state.is_visible() {
        return false;
    }
    if let Some(parent) = ctx.parent() {
        parent.send(&WidgetDrawList { intrinsic: None, items: Vec::new(), overlay: Vec::new() });
    }
    true
}

fn reply_with_draw_items(
    ctx: &WasmCtx<'_>,
    state: &InteractionState,
    draw_items: impl FnOnce() -> Vec<WidgetDrawItem>,
) {
    if reply_if_hidden(ctx, state) {
        return;
    }
    if let Some(parent) = ctx.parent() {
        parent.send(&WidgetDrawList { intrinsic: None, items: draw_items(), overlay: Vec::new() });
    }
}

/// A flat-colored quad in a widget's own local coordinates — the shared
/// constructor the widgets build their chrome from.
pub(crate) fn quad(x: f32, y: f32, width: f32, height: f32, color: Rgba) -> WidgetDrawItem {
    WidgetDrawItem::Quad { x, y, width, height, color, clip: None }
}

/// Push a `thickness`-pixel border ring around the `width` × `height` local
/// rect whose top-left is `(x, y)` — four thin quads (top, bottom, left,
/// right). The offset form is what an overlay plate needs: a dropdown's list
/// and a menu's items are rings around a rect the widget's own origin is not
/// the corner of.
pub(crate) fn push_rect_border(
    items: &mut Vec<WidgetDrawItem>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    thickness: f32,
    color: Rgba,
) {
    items.push(quad(x, y, width, thickness, color));
    items.push(quad(x, y + height - thickness, width, thickness, color));
    items.push(quad(x, y, thickness, height, color));
    items.push(quad(x + width - thickness, y, thickness, height, color));
}

/// Push a `thickness`-pixel border ring around the whole `width` × `height`
/// local rect. A focused widget draws this from `theme.accent` so the focus
/// ring reads without the root holding any per-widget-type visual knowledge.
pub(crate) fn push_border(items: &mut Vec<WidgetDrawItem>, width: f32, height: f32, thickness: f32, color: Rgba) {
    push_rect_border(items, 0.0, 0.0, width, height, thickness, color);
}

fn push_inset_border(
    items: &mut Vec<WidgetDrawItem>,
    width: f32,
    height: f32,
    inset: f32,
    thickness: f32,
    color: Rgba,
) {
    let inner_width = inset.mul_add(-2.0, width).max(0.0);
    let inner_height = inset.mul_add(-2.0, height).max(0.0);
    items.push(quad(inset, inset, inner_width, thickness, color));
    items.push(quad(inset, inset + inner_height - thickness, inner_width, thickness, color));
    items.push(quad(inset, inset, thickness, inner_height, color));
    items.push(quad(inset + inner_width - thickness, inset, thickness, inner_height, color));
}

/// Draw validation and focus as orthogonal outlines. Validation owns the outer
/// ring; when both are present focus moves inward so neither signal covers the
/// other.
///
/// The focus ring is *keyboard* focus's marker only
/// ([`InteractionState::focus_visible`]): a control the pointer just pressed is
/// obviously the one you are on, so boxing it adds a mark that says nothing.
pub(super) fn push_control_outlines(
    items: &mut Vec<WidgetDrawItem>,
    width: f32,
    height: f32,
    state: &InteractionState,
    theme: &Theme,
) {
    let validation = state.validation_color(theme);
    if let Some(color) = validation {
        push_border(items, width, height, 2.0, color);
    }
    if state.focus_visible() {
        push_inset_border(
            items,
            width,
            height,
            if validation.is_some() {
                2.0
            } else {
                0.0
            },
            2.0,
            theme.accent,
        );
    }
}

/// A rough per-character advance for caret placement and content sizing, as a
/// fraction of the font size. The exact-metric path (a `CachedFontMetrics`
/// measure) needs the font's metrics fanned down to the widget, which the
/// `Theme` does not carry in v1; this proportional approximation keeps caret
/// motion local and synchronous. The byte-offset caret *logic* (which the unit
/// tests pin) is exact regardless — only the pixel placement approximates.
pub(crate) const APPROX_ADVANCE_RATIO: f32 = 0.5;

/// The approximate pixel width of `char_count` characters at `size_pixels`,
/// using [`APPROX_ADVANCE_RATIO`].
pub(crate) fn approx_text_width(char_count: usize, size_pixels: f32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let count = char_count as f32;
    count * size_pixels * APPROX_ADVANCE_RATIO
}

fn single_line_hit_byte(text: &str, metrics: Option<&CachedFontMetrics>, size_pixels: f32, local_x: f32) -> usize {
    if let Some(metrics) = metrics {
        return SingleLineLayout::build(text, metrics, size_pixels).hit_test(local_x);
    }
    let advance = (size_pixels * APPROX_ADVANCE_RATIO).max(1.0);
    let index = if local_x <= 0.0 {
        0
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rounded = (local_x / advance + 0.5) as usize;
        rounded.min(text.chars().count())
    };
    text.char_indices().nth(index).map_or(text.len(), |(byte, _)| byte)
}

fn single_line_edit_draw_items(
    displayed: &DisplayedEdit,
    metrics: Option<&CachedFontMetrics>,
    theme: &Theme,
    state: &InteractionState,
    theme_state: ThemeState,
    width: f32,
    height: f32,
) -> Vec<WidgetDrawItem> {
    let pad = theme.pad;
    let size = theme.value_size_pixels;
    let text_y = text_origin_y(0.0, height, size);
    let caret_height = pad.mul_add(-2.0, height).max(1.0);
    let layout = metrics.map(|metrics| SingleLineLayout::build(&displayed.text, metrics, size));
    let prefix_width = |byte: usize| {
        layout.as_ref().map_or_else(
            || approx_text_width(displayed.text[..byte].chars().count(), size),
            |layout| layout.caret_x(byte),
        )
    };

    let mut items = Vec::new();
    items.push(quad(0.0, 0.0, width, height, theme.fill(theme.surface_raised, theme_state)));
    if let Some(span) = displayed.selection_span {
        let x0 = pad + prefix_width(span.start_byte);
        let x1 = pad + prefix_width(span.end_byte);
        items.push(quad(x0, pad, (x1 - x0).max(1.0), caret_height, theme.accent));
    }
    if let Some(span) = displayed.preedit_cursor_span.filter(|span| !span.is_collapsed()) {
        let x0 = pad + prefix_width(span.start_byte);
        let x1 = pad + prefix_width(span.end_byte);
        items.push(quad(x0, pad, (x1 - x0).max(1.0), caret_height, theme.accent));
    }
    if !displayed.text.is_empty() {
        items.push(WidgetDrawItem::Text {
            x: pad,
            y: text_y,
            font_id: theme.font_id,
            text: displayed.text.clone(),
            size_pixels: size,
            color: theme.fill(theme.text_primary, theme_state),
            clip: None,
        });
    }
    if let Some(span) = displayed.preedit_span {
        let x0 = pad + prefix_width(span.start_byte);
        let x1 = pad + prefix_width(span.end_byte);
        items.push(quad(x0, text_baseline_y(0.0, height, size), (x1 - x0).max(1.0), 1.0, theme.accent));
        if let Some(cursor) = displayed.preedit_cursor_span.filter(|cursor| cursor.is_collapsed()) {
            let cursor_x = pad + prefix_width(cursor.end_byte);
            items.push(quad(cursor_x, pad, 1.0, caret_height, theme.accent));
        }
    }
    if state.focused() && !displayed.composing {
        let caret_x = pad + prefix_width(displayed.caret_byte);
        items.push(quad(caret_x, pad, 1.0, caret_height, theme.accent));
    }
    push_control_outlines(&mut items, width, height, state, theme);
    items
}

/// How far below a `Screen` draw's origin `aether.text` puts the baseline, as
/// a fraction of the draw size: the font's ascent. The kit ships (and every
/// stock widget draws with) `RobotoMono`, whose hhea ascent is `2146 / 2048`
/// em — well over one em, which is why an origin computed as if the line were
/// `size_pixels` tall sank the glyphs.
const FONT_ASCENT_RATIO: f32 = 1.047_851_6;

/// How far above the baseline a capital letter reaches, as a fraction of the
/// draw size — `RobotoMono`'s OS/2 cap height, `1456 / 2048` em. The cap box is
/// what a reader sees as "the text", so it is the box the row centers.
const FONT_CAP_HEIGHT_RATIO: f32 = 0.710_937_5;

/// The `Screen`-space baseline y for a single line of `size_pixels` text
/// vertically centered in a row `row_height` tall whose top is `row_top`
/// (widget-local): half a cap height below the row's middle, so the cap box
/// the reader sees is centered and the descenders hang into the lower half.
#[must_use]
pub fn text_baseline_y(row_top: f32, row_height: f32, size_pixels: f32) -> f32 {
    size_pixels.mul_add(FONT_CAP_HEIGHT_RATIO, row_height).mul_add(0.5, row_top)
}

/// Reply one single-line editor's frame: its ordinary draw, plus the hover
/// overflow plate when the contents are too wide for the box. Shared by the
/// text field and the numeric editor, which draw the same box.
pub(super) fn reply_single_line_edit(
    ctx: &WasmCtx<'_>,
    displayed: &DisplayedEdit,
    metrics: Option<&CachedFontMetrics>,
    theme: &Theme,
    state: &InteractionState,
    theme_state: ThemeState,
    frame: &WidgetFrame,
) {
    if reply_if_hidden(ctx, state) {
        return;
    }
    let size = theme.value_size_pixels;
    let items = single_line_edit_draw_items(displayed, metrics, theme, state, theme_state, frame.width, frame.height);

    let measured =
        metrics.filter(|_| state.hovered()).map(|metrics| measured_text_width(metrics, &displayed.text, size));
    let overlay = measured.map_or_else(Vec::new, |text_width| {
        overflow_reveal_items(
            theme,
            &displayed.text,
            theme.pad,
            text_width,
            size,
            theme.fill(theme.text_primary, theme_state),
            frame,
        )
    });

    if let Some(parent) = ctx.parent() {
        parent.send(&WidgetDrawList { intrinsic: None, items, overlay });
    }
}

/// The `Screen`-space `DrawText` origin y that vertically centers a single
/// line of `size_pixels` text in a row `row_height` tall whose top is
/// `row_top` (widget-local) — the one rule every widget that draws one line
/// of text places it by.
///
/// `aether.text` treats a `Screen` draw `origin` as the *pen* start and puts
/// the baseline one **ascent** below it, so the origin is the baseline minus
/// that ascent. The theme does not fan the font's own metrics to widgets (see
/// `APPROX_ADVANCE_RATIO`), so the ratios are the shipped font's, applied
/// uniformly: getting them from a `CachedFontMetrics` would mean the table
/// carrying ascent (it carries advances only) and every widget holding a
/// metrics adapter (five do not).
#[must_use]
pub fn text_origin_y(row_top: f32, row_height: f32, size_pixels: f32) -> f32 {
    size_pixels.mul_add(-FONT_ASCENT_RATIO, text_baseline_y(row_top, row_height, size_pixels))
}

/// The overlay plate a widget raises when its one line of text does not fit
/// the frame it lives in and the pointer is over it: the run redrawn whole on
/// a `surface_raised` plate with a one-pixel `outline` ring, starting at the
/// widget's own origin and reaching one `pad` past the run's end. The plate
/// therefore covers the widget and whatever sits to its right, and the root
/// cuts ordinary text out from under an overlay fill so the covered widgets'
/// glyphs do not print through it.
///
/// Empty unless the run actually overflows (`text_x + text_width` past the
/// frame's right edge) — a widget whose text fits raises nothing, so the
/// reveal reads as "there is more here" rather than as chrome.
pub(crate) fn overflow_reveal_items(
    theme: &Theme,
    text: &str,
    text_x: f32,
    text_width: f32,
    size_pixels: f32,
    ink: Rgba,
    frame: &WidgetFrame,
) -> Vec<WidgetDrawItem> {
    let plate_width = text_x + text_width + theme.pad;
    if text.is_empty() || !plate_width.is_finite() || text_x + text_width <= frame.width || frame.height <= 0.0 {
        return Vec::new();
    }

    let mut items = Vec::with_capacity(6);
    items.push(quad(0.0, 0.0, plate_width, frame.height, theme.surface_raised));
    push_border(&mut items, plate_width, frame.height, 1.0, theme.outline);
    items.push(WidgetDrawItem::Text {
        x: text_x,
        y: text_origin_y(0.0, frame.height, size_pixels),
        font_id: theme.font_id,
        text: String::from(text),
        size_pixels,
        color: ink,
        clip: None,
    });
    items
}
/// The slot a row-local `x` lands in, over `widths` laid out left to right
/// from `0.0` with `gap` between them. `None` in a gap, left of the first
/// slot, or past the last — a row of content-sized targets (a tab strip's
/// tabs, a menu bar's titles) is a row of separate targets, not one
/// partitioned bar, so the space between two of them belongs to neither.
fn slot_at_local_x(widths: &[f32], gap: f32, local_x: f32) -> Option<usize> {
    if !local_x.is_finite() || local_x < 0.0 {
        return None;
    }
    let mut left = 0.0;
    for (index, width) in widths.iter().enumerate() {
        if local_x < left + width {
            return (local_x >= left).then_some(index);
        }
        left += width + gap;
    }
    None
}

/// The local x of slot `index`'s left edge in that same layout.
fn slot_left(widths: &[f32], gap: f32, index: usize) -> f32 {
    widths.iter().take(index).map(|width| width + gap).sum()
}

/// The interim widths a content-sized row lays out with before its font's
/// metrics arrive: the row split evenly, the gaps taken out first. Replaced by
/// the measured widths on the first `Collect` after the reply lands.
fn even_split_widths(count: usize, width: f32, gap: f32) -> Vec<f32> {
    if count == 0 {
        return Vec::new();
    }
    #[allow(clippy::cast_precision_loss)]
    let slots = count as f32;
    alloc::vec![((slots - 1.0).mul_add(-gap, width) / slots).max(0.0); count]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> WidgetFrame {
        WidgetFrame { x: 0.0, y: 0.0, width: 100.0, height: 24.0 }
    }

    #[test]
    fn the_overflow_plate_appears_only_for_a_run_that_does_not_fit() {
        // Tripwire: the reveal is a signal, not chrome. A plate raised over a
        // run that already fits would cover the widget to its right on every
        // hover, for nothing.
        let theme = Theme::DEFAULT;
        let fits = overflow_reveal_items(&theme, "short", theme.pad, 40.0, 14.0, theme.text_primary, &frame());
        assert!(fits.is_empty(), "a run inside the frame raises no plate");

        let empty = overflow_reveal_items(&theme, "", theme.pad, 400.0, 14.0, theme.text_primary, &frame());
        assert!(empty.is_empty(), "there is nothing to reveal about an empty run");

        let overflows =
            overflow_reveal_items(&theme, "far too long", theme.pad, 160.0, 14.0, theme.text_primary, &frame());
        let WidgetDrawItem::Quad { x, y, width, height, .. } = overflows[0] else {
            panic!("the plate leads with its fill");
        };
        assert_eq!((x, y, height), (0.0, 0.0, 24.0), "the plate starts at the widget's own origin");
        assert_eq!(width, theme.pad.mul_add(2.0, 160.0), "and reaches one pad past the run");
        assert!(width > frame().width, "a plate that did not outgrow the frame would reveal nothing");
        assert!(
            matches!(overflows.last(), Some(WidgetDrawItem::Text { text, .. }) if text == "far too long"),
            "the whole run draws over the plate",
        );
    }

    #[test]
    fn a_pointer_buckets_into_the_slot_it_is_over_and_into_no_slot_in_a_gap() {
        let widths = [30.0, 50.0, 20.0];
        assert_eq!(slot_at_local_x(&widths, 4.0, 0.0), Some(0));
        assert_eq!(slot_at_local_x(&widths, 4.0, 29.9), Some(0));
        assert_eq!(slot_at_local_x(&widths, 4.0, 31.0), None, "the gap after the first slot selects nothing");
        assert_eq!(slot_at_local_x(&widths, 4.0, 34.0), Some(1));
        assert_eq!(slot_at_local_x(&widths, 4.0, 88.0), Some(2));
        assert_eq!(slot_at_local_x(&widths, 4.0, 108.0), None, "past the last slot is off the row");
        assert_eq!(slot_at_local_x(&widths, 4.0, -1.0), None);
        assert_eq!(slot_at_local_x(&[], 4.0, 0.0), None);
    }

    #[test]
    fn unequal_slot_widths_bucket_by_their_own_extents() {
        // The bug an even split hides: with widths 10 / 90, x = 50 is the
        // second slot, while halves-of-the-row arithmetic calls it the first.
        assert_eq!(slot_at_local_x(&[10.0, 90.0], 0.0, 50.0), Some(1));
    }

    #[test]
    fn a_slot_starts_past_every_earlier_slot_and_the_gaps_between_them() {
        let widths = [30.0, 50.0, 20.0];
        assert_eq!(slot_left(&widths, 4.0, 0), 0.0);
        assert_eq!(slot_left(&widths, 4.0, 1), 34.0);
        assert_eq!(slot_left(&widths, 4.0, 2), 88.0);
        assert_eq!(slot_at_local_x(&widths, 4.0, slot_left(&widths, 4.0, 2)), Some(2), "the left edge is inclusive");
    }
}
