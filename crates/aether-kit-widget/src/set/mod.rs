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
//! - [`DialogWidget`] — the plate a modal stands on: a title, a rule under
//!   it, and a body rectangle the host lays its own children into.
//! - [`TooltipWidget`] — an anchored plate saying what the thing under the
//!   pointer is, drawn in the overlay layer.
//! - [`ToastWidget`] — the one region transient notices appear in, coloured
//!   by severity and gone on their own.
//! - [`SplitterWidget`] — the drag handle on the edge between two regions.
//!
//! Two of the surfaces here are not widgets, because neither is a control:
//!
//! - [`placement`] — where an overlay plate stands beside its anchor and
//!   inside its region ([`place_plate`]), shared by the tooltip and the
//!   popover.
//! - [`popover`] — a plate hosting *other* children over the primary view.
//!   Hosting interactive children is a root's job in this kit, so a popover
//!   is a value a root owns ([`Popover`]) rather than an actor; the module
//!   doc says why.
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
pub mod dialog;
pub mod dropdown;
pub mod image;
pub mod label;
pub mod menu_bar;
pub mod numeric;
pub mod placement;
pub mod popover;
pub mod radio;
pub mod segmented;
pub mod slider;
pub mod splitter;
pub mod tab_strip;
pub mod text_area;
pub mod text_field;
pub mod toast;
pub mod toggle;
pub mod tooltip;
pub mod virtual_list;

pub use button::ButtonWidget;
pub use defaults::WidgetDefaults;
pub use dialog::{DialogConfig, DialogPlaced, DialogWidget};
pub use dropdown::DropdownWidget;
pub use image::ImageWidget;
pub use label::LabelWidget;
pub use menu_bar::MenuBarWidget;
pub use numeric::NumericWidget;
pub use placement::{PlacementBounds, PlacementSide, place_plate, place_plate_avoiding};
pub use popover::Popover;
pub use radio::RadioGroupWidget;
pub use segmented::SegmentedWidget;
pub use slider::SliderWidget;
pub use splitter::{SplitterAxis, SplitterConfig, SplitterHover, SplitterMoved, SplitterWidget};
pub use tab_strip::TabStripWidget;
pub use text_area::TextAreaWidget;
pub use text_field::TextFieldWidget;
pub use toast::{ToastConfig, ToastNotice, ToastRegionChanged, ToastSeverity, ToastWidget};
pub use toggle::ToggleWidget;
pub use tooltip::{TooltipConfig, TooltipIcon, TooltipLine, TooltipSection, TooltipShed, TooltipWidget};
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
use crate::{
    ButtonEmphasis, ButtonTone, WidgetClipRect, WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame,
};

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
/// `width`-wide frame, never left of the frame's own left edge.
///
/// Centering is the whole rule: the margins either side are equal at every
/// width, which is what a reader checks first and what the owner's
/// asymmetric-Remove-button note was about. Clamping the origin to `pad` — the
/// earlier rule — looked harmless but broke exactly that, because a frame
/// narrower than `text_width + 2 * pad` got a full pad on the left and
/// whatever was left over on the right. `pad` is therefore what the button's
/// *intrinsic width* reserves ([`ButtonWidget`]'s
/// `WidgetDrawList::intrinsic` is `measured + 2 * pad`), not a floor the draw
/// re-applies; a frame at or above that width centers with at least a pad
/// either side by construction.
///
/// The `0.0` clamp only catches a label wider than its whole frame, where no
/// origin is symmetric and hanging off the left would lose the start of the
/// text as well as the end.
fn centered_text_x(width: f32, text_width: f32) -> f32 {
    ((width - text_width) * 0.5).max(0.0)
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
        parent.send(&WidgetDrawList { content_height: None, intrinsic: None, items: Vec::new(), overlay: Vec::new() });
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
        parent.send(&WidgetDrawList {
            content_height: None,
            intrinsic: None,
            items: draw_items(),
            overlay: Vec::new(),
        });
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

/// The wash a control with **no plate of its own** answers the pointer with.
///
/// A filled control carries hover and press in its plate ([`Theme::fill`]
/// composites the overlay into the fill). A control that draws no plate — an
/// outlined or text button, a filled tab — has nowhere to put that, so the
/// same role-agnostic overlay is drawn as its whole background instead.
/// Without it the quiet ranks would be the ones that never answer the
/// pointer, which reads as "not a control".
pub(crate) fn pointer_wash(theme: &Theme, state: ThemeState) -> Option<Rgba> {
    match state {
        ThemeState::Hover => Some(theme.hover_overlay),
        ThemeState::Pressed => Some(theme.pressed_overlay),
        ThemeState::Normal | ThemeState::Disabled => None,
    }
}

/// Push a `thickness`-pixel border ring around the whole `width` × `height`
/// local rect. A focused widget draws this from `theme.accent` so the focus
/// ring reads without the root holding any per-widget-type visual knowledge.
pub(crate) fn push_border(items: &mut Vec<WidgetDrawItem>, width: f32, height: f32, thickness: f32, color: Rgba) {
    push_rect_border(items, 0.0, 0.0, width, height, thickness, color);
}

/// The hairline a button's outline is stroked at.
pub(crate) const BUTTON_STROKE_THICKNESS: f32 = 1.0;

/// The three inks one (emphasis, tone) pair resolves to: the plate under the
/// label, the stroke around it, and the label's own colour. `None` is a part
/// the emphasis does not draw at all — a text button has neither plate nor
/// stroke, which is what makes it the quietest thing on the screen.
pub(crate) struct ButtonInk {
    plate: Option<Rgba>,
    stroke: Option<Rgba>,
    label: Rgba,
}

/// The plate, stroke, and label ink one rank of the button ladder draws in.
///
/// **Every rank draws a different face**, and that is the ladder's whole job:
/// a filled verb is a saturated plate, a tonal one a quiet plate at a fixed
/// contrast step off the surface ([`Theme::tonal`]), an outlined one no plate
/// and a stroke that clears the same step ([`Theme::edge`]), and a text one
/// neither. Two ranks that resolve to the same face are the owner's round-11
/// note 4 — a `Change gem` and a `×` on one row that read alike — so the ranks
/// are separated by *structure* (plate / stroke / nothing) before colour, and
/// the two that carry colour are separated from their background by measured
/// contrast rather than by a fixed mix.
///
/// The one rule worth stating: on the quiet emphases a *neutral* verb reads in
/// the primary ink, not in the accent. The accent is the primary action's token
/// (`designing-a-screen.md` §6), and a screen whose four secondary verbs are
/// all lettered in it has spent the token again — which is the owner's "a
/// single yellow button for everything" in a thinner form. A danger verb keeps
/// its colour at every rank, because what it destroys does not get quieter.
pub(crate) fn button_ink(theme: &Theme, emphasis: ButtonEmphasis, tone: ButtonTone) -> ButtonInk {
    let role = match tone {
        ButtonTone::Neutral => theme.accent,
        ButtonTone::Danger => theme.error,
    };
    let quiet = match tone {
        ButtonTone::Neutral => theme.text_primary,
        ButtonTone::Danger => theme.error,
    };
    match emphasis {
        ButtonEmphasis::Filled => ButtonInk { plate: Some(role), stroke: None, label: theme.accent_text },
        ButtonEmphasis::Tonal => ButtonInk { plate: Some(theme.tonal(role)), stroke: None, label: quiet },
        ButtonEmphasis::Outlined => ButtonInk {
            plate: None,
            stroke: Some(match tone {
                ButtonTone::Neutral => theme.edge(),
                ButtonTone::Danger => theme.error,
            }),
            label: quiet,
        },
        ButtonEmphasis::Text => ButtonInk { plate: None, stroke: None, label: quiet },
    }
}

/// The label run a `width`-wide button face has room for, and the local x it is
/// drawn at, relative to that face's own left edge.
///
/// The whole label centered when it fits; elided into the frame and *then*
/// centered when the frame is narrower than the intrinsic, so the margins are
/// equal at any width and a cut label carries the mark saying so instead of
/// being sliced by the root's slot clip. Left-padded and whole while the
/// measurement is still outstanding. `None` when there is nothing to draw: an
/// empty label, or a frame too narrow to hold even the elision mark.
///
/// **The pads are given up before the label is.** A pad each side is what the
/// *intrinsic* reserves — what a layout should give this button if it can —
/// not room the draw has to leave inside whatever frame it was actually
/// handed. Charged against the padded budget alone, a control the size of its
/// own mark drew **nothing at all**: a `−` does not fit a frame minus two pads,
/// and neither does the ellipsis that would say it was cut, so `elide_to_width`
/// answered with the empty string and the ascendancy inset's collapse button
/// rendered as an empty outlined square. So the padded budget is the preference
/// and the frame is the limit.
pub(crate) fn button_run(
    label: &str,
    width: f32,
    theme: &Theme,
    metrics: Option<&CachedFontMetrics>,
) -> Option<(String, f32)> {
    let size = theme.label_size_pixels;
    let (run, run_x) = metrics.map_or_else(
        || (String::from(label), theme.pad),
        |metrics| {
            let measure = |run: &str| measured_text_width(metrics, run, size);
            let padded = elide_to_width(label, theme.pad.mul_add(-2.0, width), measure);
            let run = if padded.is_empty() {
                elide_to_width(label, width, measure)
            } else {
                padded
            };
            let run_x = centered_text_x(width, measure(&run));
            (run, run_x)
        },
    );
    (!run.is_empty()).then_some((run, run_x))
}

/// The width a button face asks for: its measured label plus one `pad` each
/// side. The caller owns the "not measured yet" case, because the two that ask
/// answer it differently — a button reports no intrinsic at all until the
/// advances land, while a verb inside a list row approximates rather than
/// occupying no width and letting the name elide into the space it is about to
/// take.
pub(crate) fn button_face_width(label: &str, theme: &Theme, metrics: &CachedFontMetrics) -> f32 {
    theme.pad.mul_add(2.0, measured_text_width(metrics, label, theme.label_size_pixels))
}

/// One button face: the rect it occupies in its drawer's local coordinates, and
/// the verb it carries.
pub(crate) struct ButtonFace<'a> {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub label: &'a str,
    pub emphasis: ButtonEmphasis,
    pub tone: ButtonTone,
}

/// Push a button face — its plate or pointer wash, its stroke, and its
/// centered, elided label — into `items`.
///
/// Shared so that a verb drawn inside another widget's interior (a virtual
/// list's row actions) is the *same* control as one a layout gave a slot to:
/// one ladder, one elision rule, one hover answer. A second copy of this draw
/// would be a second button that drifts from the first.
pub(crate) fn push_button_face(
    items: &mut Vec<WidgetDrawItem>,
    face: &ButtonFace<'_>,
    theme: &Theme,
    theme_state: ThemeState,
    metrics: Option<&CachedFontMetrics>,
) {
    let ink = button_ink(theme, face.emphasis, face.tone);
    match ink.plate {
        Some(plate) => items.push(quad(face.x, face.y, face.width, face.height, theme.fill(plate, theme_state))),
        None => {
            if let Some(wash) = pointer_wash(theme, theme_state) {
                items.push(quad(face.x, face.y, face.width, face.height, wash));
            }
        }
    }
    if let Some(stroke) = ink.stroke {
        push_rect_border(
            items,
            face.x,
            face.y,
            face.width,
            face.height,
            BUTTON_STROKE_THICKNESS,
            theme.fill(stroke, theme_state),
        );
    }
    if let Some((run, run_x)) = button_run(face.label, face.width, theme, metrics) {
        items.push(WidgetDrawItem::Text {
            x: face.x + run_x,
            y: text_origin_y(face.y, face.height, theme.label_size_pixels),
            font_id: theme.font_id,
            text: run,
            size_pixels: theme.label_size_pixels,
            color: theme.fill(ink.label, theme_state),
            clip: None,
        });
    }
}

/// The most rows [`push_triangle`] builds an arrow from. An arrow this size is
/// a handful of pixels tall, so the cap only bounds a pathological frame.
const TRIANGLE_MAX_ROWS: usize = 16;

/// Push a solid isoceles triangle, built from horizontal quad rows, centered
/// on `center_x` and filling the `width` × `height` box whose top is `top_y`.
/// `pointing_up` puts the apex at the top.
///
/// A triangle rather than a `▲` glyph because the kit's draw list has no
/// polygon and the theme's font is whatever the consumer loaded — asking it for
/// an arrowhead is asking for a missing-glyph box on the one control whose
/// whole point is being clickable.
pub(crate) fn push_triangle(
    items: &mut Vec<WidgetDrawItem>,
    center_x: f32,
    top_y: f32,
    width: f32,
    height: f32,
    pointing_up: bool,
    color: Rgba,
) {
    if !(width > 0.0 && height > 0.0) {
        return;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rows = (height.ceil() as usize).clamp(1, TRIANGLE_MAX_ROWS);
    #[allow(clippy::cast_precision_loss)]
    let row_height = height / rows as f32;
    for row in 0..rows {
        #[allow(clippy::cast_precision_loss)]
        let center_fraction = (row as f32 + 0.5) / rows as f32;
        let fraction = if pointing_up {
            center_fraction
        } else {
            1.0 - center_fraction
        };
        let row_width = width * fraction;
        #[allow(clippy::cast_precision_loss)]
        let y = row_height.mul_add(row as f32, top_y);
        items.push(quad(center_x - row_width * 0.5, y, row_width, row_height, color));
    }
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

/// The one fill a single-line control's frame is painted with, whole.
///
/// The numeric's stepper column composites its per-button hover / pressed
/// overlay over this same colour rather than filling itself from another
/// palette role, which is what keeps the column a region of the control's own
/// surface instead of a second box butted against it.
pub(super) fn single_line_box_fill(theme: &Theme, theme_state: ThemeState) -> Rgba {
    theme.fill(theme.surface_raised, theme_state)
}

fn single_line_edit_draw_items(edit: &SingleLineEdit<'_>) -> Vec<WidgetDrawItem> {
    let SingleLineEdit { displayed, metrics, theme, state, theme_state, frame, .. } = edit;
    let (theme_state, width, height) = (*theme_state, frame.width, frame.height);
    let metrics = *metrics;
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
    // Everything the reader typed lives inside the value's own box, never in
    // the gutter beside it (round-4 note 6).
    let content_clip = edit.content_clip();
    let content_quad = |x: f32, y: f32, quad_width: f32, quad_height: f32, color: Rgba| WidgetDrawItem::Quad {
        x,
        y,
        width: quad_width,
        height: quad_height,
        color,
        clip: content_clip,
    };

    let mut items = Vec::new();
    items.push(quad(0.0, 0.0, width, height, single_line_box_fill(theme, theme_state)));
    if let Some(span) = displayed.selection_span {
        let x0 = pad + prefix_width(span.start_byte);
        let x1 = pad + prefix_width(span.end_byte);
        items.push(content_quad(x0, pad, (x1 - x0).max(1.0), caret_height, theme.accent));
    }
    if let Some(span) = displayed.preedit_cursor_span.filter(|span| !span.is_collapsed()) {
        let x0 = pad + prefix_width(span.start_byte);
        let x1 = pad + prefix_width(span.end_byte);
        items.push(content_quad(x0, pad, (x1 - x0).max(1.0), caret_height, theme.accent));
    }
    if !displayed.text.is_empty() {
        items.push(WidgetDrawItem::Text {
            x: pad,
            y: text_y,
            font_id: theme.font_id,
            text: displayed.text.clone(),
            size_pixels: size,
            color: theme.fill(theme.text_primary, theme_state),
            clip: content_clip,
        });
    }
    if let Some(span) = displayed.preedit_span {
        let x0 = pad + prefix_width(span.start_byte);
        let x1 = pad + prefix_width(span.end_byte);
        items.push(content_quad(x0, text_baseline_y(0.0, height, size), (x1 - x0).max(1.0), 1.0, theme.accent));
        if let Some(cursor) = displayed.preedit_cursor_span.filter(|cursor| cursor.is_collapsed()) {
            let cursor_x = pad + prefix_width(cursor.end_byte);
            items.push(content_quad(cursor_x, pad, 1.0, caret_height, theme.accent));
        }
    }
    // The caret marks the insertion point, which a pointer click establishes
    // just as a Tab does, so it follows plain focus rather than the ring's
    // keyboard-only rule.
    if state.focused() && !displayed.composing {
        let caret_x = pad + prefix_width(displayed.caret_byte);
        items.push(content_quad(caret_x, pad, 1.0, caret_height, theme.accent));
    }
    items.extend(edit.gutter_items.iter().cloned());
    push_control_outlines(&mut items, width, height, state, theme);
    items
}

/// How far below a `Screen` draw's origin `aether.text` puts the baseline, as
/// a fraction of the draw size: the font's ascent. The kit ships (and every
/// stock widget draws with) `RobotoMono`, whose hhea ascent is `2146 / 2048`
/// em — well over one em, which is why an origin computed as if the line were
/// `size_pixels` tall sank the glyphs.
const FONT_ASCENT_RATIO: f32 = 1.047_851_6;

/// How far below the baseline a descender reaches, as a fraction of the draw
/// size — `RobotoMono`'s hhea descent, `555 / 2048` em. The shipped font's
/// ratio applied uniformly, like [`FONT_ASCENT_RATIO`] beside it.
const FONT_DESCENT_RATIO: f32 = 0.271_0;

/// How tall one line of text is, as a fraction of the draw size: everything
/// from the pen origin (`aether.text` puts the baseline one ascent below it)
/// down past the deepest descender. This is the run's own box, which the
/// root's clip subtraction reads to tell a fill standing over a line from one
/// merely inside the same scissor.
pub(crate) const FONT_LINE_BOX_RATIO: f32 = FONT_ASCENT_RATIO + FONT_DESCENT_RATIO;

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

/// The height of the **cap band** one line of `size_pixels` text draws in: the
/// box from the baseline up to the top of a capital, which is what a reader
/// sees as "the text" (it is also the box [`text_baseline_y`] centers). It is
/// what an inline mark beside a run — a tooltip line's icon — is scaled to, so
/// the mark and the words read as one line rather than as a picture with a
/// caption.
#[must_use]
pub fn text_cap_height(size_pixels: f32) -> f32 {
    size_pixels * FONT_CAP_HEIGHT_RATIO
}

/// Everything one single-line editor's frame draw needs. A struct rather than
/// an argument list because the numeric editor adds two of the fields (a
/// reserved gutter and the chrome that lives in it) the text field leaves at
/// their empty defaults.
pub(super) struct SingleLineEdit<'a> {
    pub(super) displayed: &'a DisplayedEdit,
    pub(super) metrics: Option<&'a CachedFontMetrics>,
    pub(super) theme: &'a Theme,
    pub(super) state: &'a InteractionState,
    pub(super) theme_state: ThemeState,
    pub(super) frame: &'a WidgetFrame,
    /// Pixels reserved at the frame's right end for widget-owned chrome — the
    /// numeric editor's stepper column. The text box, and the width the hover
    /// reveal measures overflow against, end there.
    pub(super) gutter: f32,
    /// The chrome that fills that gutter, drawn over the box and under the
    /// validation / focus outlines so a ring still frames the whole control.
    pub(super) gutter_items: Vec<WidgetDrawItem>,
    /// The `[width, height]` this control asks a layout for, once it can
    /// measure what it holds — the numeric's range plus its chrome. `None`
    /// until the theme font's metrics resolve, so a layout never sizes a slot
    /// to a guess.
    pub(super) intrinsic: Option<[f32; 2]>,
}

impl<'a> SingleLineEdit<'a> {
    /// The default shape: no gutter, no chrome, no intrinsic — one plain edit
    /// box that takes the frame it is given.
    pub(super) fn new(
        displayed: &'a DisplayedEdit,
        metrics: Option<&'a CachedFontMetrics>,
        theme: &'a Theme,
        state: &'a InteractionState,
        theme_state: ThemeState,
        frame: &'a WidgetFrame,
    ) -> Self {
        Self {
            displayed,
            metrics,
            theme,
            state,
            theme_state,
            frame,
            gutter: 0.0,
            gutter_items: Vec::new(),
            intrinsic: None,
        }
    }

    /// The width the text box actually gets, once the gutter is taken out.
    fn content_width(&self) -> f32 {
        (self.frame.width - self.gutter).max(0.0)
    }

    /// The bound every drawn part of the value carries when the control keeps
    /// a gutter — the box less one `pad` at the gutter end, which is the same
    /// `pad` the text starts at.
    ///
    /// Round-4 note 6: "lack of symmetry in the space of the left padding and
    /// right padding of the number text. Three digits overflows and is drawn
    /// behind the arrows." The two are one defect. The value is *padded* on
    /// the left by starting at `pad` and on the right by ending a `pad` short
    /// of the seam, and it is *held there* by this clip, so a number wider
    /// than its box is cut at its own margin instead of printing across the
    /// hairline and under the arrows. A control with no gutter has no seam to
    /// hold it off and keeps `None`: its slot clip is already its own frame.
    fn content_clip(&self) -> Option<WidgetClipRect> {
        (self.gutter > 0.0).then(|| WidgetClipRect {
            x: 0.0,
            y: 0.0,
            width: (self.content_width() - self.theme.pad).max(0.0),
            height: self.frame.height,
        })
    }

    /// The width the value has to fit for it to be readable where it is — the
    /// edge [`Self::content_clip`] cuts at, not the box that clip sits in. The
    /// two differ by the trailing `pad` on a control with a gutter, and a run
    /// measured against the wider one is a run the clip cuts while the hover
    /// reveal still calls it a fit: a whole `pad` of run widths truncated with
    /// nothing to hover for. `content_width` when there is no gutter, where
    /// the clip is `None` and the two edges are the same one.
    fn reveal_content_width(&self) -> f32 {
        self.content_clip().map_or_else(|| self.content_width(), |clip| clip.width)
    }
}

/// The hover reveal a single-line editor owes when its value does not fit the
/// box it is drawn in. Empty unless the pointer is on it and its metrics have
/// resolved — an unmeasured run cannot be called too wide.
pub(super) fn single_line_edit_overlay(edit: &SingleLineEdit<'_>) -> Vec<WidgetDrawItem> {
    let theme = edit.theme;
    let size = theme.value_size_pixels;
    edit.metrics.filter(|_| edit.state.hovered()).map_or_else(Vec::new, |metrics| {
        overflow_reveal_items(
            &RevealPlate {
                theme,
                text: &edit.displayed.text,
                text_x: theme.pad,
                size_pixels: size,
                ink: theme.fill(theme.text_primary, edit.theme_state),
                content_width: edit.reveal_content_width(),
                row_height: edit.frame.height,
            },
            &|run: &str| measured_text_width(metrics, run, size),
        )
    })
}

/// Reply one single-line editor's frame: its ordinary draw, plus the hover
/// overflow plate when the contents are too wide for the box. Shared by the
/// text field and the numeric editor, which draw the same box.
pub(super) fn reply_single_line_edit(ctx: &WasmCtx<'_>, edit: SingleLineEdit<'_>) {
    if reply_if_hidden(ctx, edit.state) {
        return;
    }
    let intrinsic = edit.intrinsic;
    let items = single_line_edit_draw_items(&edit);
    let overlay = single_line_edit_overlay(&edit);

    if let Some(parent) = ctx.parent() {
        parent.send(&WidgetDrawList { content_height: None, intrinsic, items, overlay });
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

/// How wide the kit lets a hover reveal or a tooltip run before it wraps, in
/// body characters. A reading measure, not a limit the content chose: past
/// roughly this the eye loses the line it is on coming back from the right
/// edge, and a plate that is one enormously long line is exactly the "breaks
/// up weirdly" the owner saw.
pub const REVEAL_WRAP_CHARS: usize = 40;

/// The pixel width [`REVEAL_WRAP_CHARS`] comes to at `size_pixels`, by the
/// per-character approximation. A *maximum* is the one place the approximation
/// is honest on its own terms — the wrap points themselves are decided by the
/// caller's real `measure`, so a proportional font wraps exactly.
#[must_use]
pub fn reveal_wrap_width(size_pixels: f32) -> f32 {
    approx_text_width(REVEAL_WRAP_CHARS, size_pixels)
}

/// The mark a run that did not fit ends with. One character, not three dots:
/// three dots is a sentence's own punctuation and reads as one where the text
/// happens to end in prose.
pub const ELLIPSIS: char = '…';

/// `text` cut to the widest whole-character prefix that still leaves room for
/// an [`ELLIPSIS`] inside `max_width`, or `text` itself when it already fits.
/// `measure` reports the pixel width of a candidate, the same contract
/// [`wrap_to_width`] takes.
///
/// This is what a row cut by a clip is missing (the list's gap 17): a clip
/// slices the glyph the boundary lands on, so a name that did not fit looks
/// like a name that ends oddly, while an ellipsis says a name was cut. The cut
/// is always on a character boundary — never inside a `char`, and never inside
/// a glyph, because the last kept character is the last one measured to fit.
///
/// A `max_width` too narrow for even the ellipsis yields the empty string:
/// there is no honest mark to draw in a column that narrow, and drawing one
/// anyway would be the only thing in the row.
#[must_use]
pub fn elide_to_width(text: &str, max_width: f32, measure: impl Fn(&str) -> f32) -> String {
    if !max_width.is_finite() || measure(text) <= max_width {
        return String::from(text);
    }
    let with_ellipsis = |kept: &str| {
        let mut candidate = String::with_capacity(kept.len() + ELLIPSIS.len_utf8());
        candidate.push_str(kept);
        candidate.push(ELLIPSIS);
        candidate
    };
    let boundaries: Vec<usize> = text.char_indices().map(|(byte, _)| byte).collect();
    let Some(last) = boundaries.len().checked_sub(1) else {
        return String::new();
    };

    // The widest prefix that fits, by bisection over character counts: the
    // whole run is already known not to fit, and fitting is monotone in the
    // count, so this is `log n` measures rather than one per character.
    let (mut low, mut high) = (0usize, last);
    let mut best = None;
    while low <= high {
        let mid = low + (high - low) / 2;
        let candidate = with_ellipsis(&text[..boundaries[mid]]);
        if measure(&candidate) <= max_width {
            best = Some(candidate);
            low = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            high = mid - 1;
        }
    }
    best.unwrap_or_default()
}

/// Break `text` into lines no wider than `max_width`, splitting only between
/// words. `measure` reports the pixel width of a candidate line, so a caller
/// wraps against whatever it will actually draw with — exact glyph advances
/// once the font's metrics resolve, an approximation before that.
///
/// A `\n` in the source is an author's own break and is honoured, interior
/// blank lines included, so a caller can divide a tooltip into paragraphs.
/// Leading and trailing blank lines are the exception and are dropped: a break
/// needs content on both sides of it, and a plate padded by an empty row at
/// the top or bottom reads as a box the wrong size rather than as a space the
/// author asked for. A single word wider than `max_width` keeps its own line
/// unsplit and over budget: `max_width` is a reading measure, and breaking a
/// word in half to respect it reads far worse than one long line.
#[must_use]
pub fn wrap_to_width(text: &str, max_width: f32, measure: impl Fn(&str) -> f32) -> Vec<String> {
    wrap_to_width_hanging(text, max_width, 0.0, measure).into_iter().map(|line| line.text).collect()
}

/// One line [`wrap_to_width_hanging`] produced, with the indent it is drawn
/// at: `0.0` for the first line of a paragraph, the hanging indent for every
/// continuation.
#[derive(Debug, Clone, PartialEq)]
pub struct WrappedLine {
    pub indent_pixels: f32,
    pub text: String,
}

/// [`wrap_to_width`] with a **hanging indent**: the first line of a paragraph
/// starts at the margin and every continuation is inset by `indent_pixels`,
/// wrapping that much earlier so the right edge stays where it was. A wrapped
/// entry then reads as one entry rather than as two — which is what a stat
/// line on a hover card needs, and is ordinary typography (the studio's
/// gap 18).
///
/// `0.0` is exactly [`wrap_to_width`]. A `\n` starts a new paragraph, so the
/// line after an author's own break is a first line again, not a continuation.
#[must_use]
pub fn wrap_to_width_hanging(
    text: &str,
    max_width: f32,
    indent_pixels: f32,
    measure: impl Fn(&str) -> f32,
) -> Vec<WrappedLine> {
    let indent = if indent_pixels.is_finite() {
        indent_pixels.max(0.0)
    } else {
        0.0
    };
    let mut lines: Vec<WrappedLine> = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut first = true;
        for word in paragraph.split_whitespace() {
            if line.is_empty() {
                line.push_str(word);
                continue;
            }
            let mut candidate = String::with_capacity(line.len() + 1 + word.len());
            candidate.push_str(&line);
            candidate.push(' ');
            candidate.push_str(word);
            let budget = if first {
                max_width
            } else {
                max_width - indent
            };
            if measure(&candidate) <= budget {
                line = candidate;
            } else {
                lines.push(WrappedLine {
                    indent_pixels: if first {
                        0.0
                    } else {
                        indent
                    },
                    text: mem::replace(&mut line, String::from(word)),
                });
                first = false;
            }
        }
        lines.push(WrappedLine {
            indent_pixels: if first {
                0.0
            } else {
                indent
            },
            text: line,
        });
    }
    // A trailing empty line is the split's artifact, not an author's break;
    // any blank at either end would otherwise pad every plate. Interior ones
    // stay — that is the paragraph break the author wrote.
    while lines.last().is_some_and(|line| line.text.is_empty()) {
        lines.pop();
    }
    while lines.first().is_some_and(|line| line.text.is_empty()) {
        lines.remove(0);
    }
    lines
}

/// The plate a widget raises over its own slot when its text does not fit and
/// the pointer is on it. See [`overflow_reveal_items`].
pub(crate) struct RevealPlate<'a> {
    pub(crate) theme: &'a Theme,
    pub(crate) text: &'a str,
    /// Where the run starts inside the plate — one `pad`, and the plate's
    /// width accounts for that same margin at the far end. It is the plate's
    /// own inset, never the origin the caller aligns its in-frame run by: a
    /// `Start`-aligned run's origin is `0.0`, which would lay the glyphs under
    /// the plate's own ring.
    pub(crate) text_x: f32,
    pub(crate) size_pixels: f32,
    pub(crate) ink: Rgba,
    /// The width the run has to fit in before a plate is owed. It is the edge
    /// the caller's own clip cuts the run at, not the box that clip sits in:
    /// measure against the wider box and every run between the two edges is
    /// truncated on screen while the reveal calls it a fit.
    pub(crate) content_width: f32,
    /// One wrapped line's height — the widget's own row height.
    pub(crate) row_height: f32,
}

/// The overlay plate a widget raises when its text does not fit the frame it
/// lives in and the pointer is over it: the run redrawn whole on a
/// `surface_raised` plate with a one-pixel `outline` ring, starting at the
/// widget's own origin. The plate covers the widget and whatever sits to its
/// right, and the root cuts ordinary text out from under an overlay fill so
/// the covered widgets' glyphs do not print through it.
///
/// The run wraps at [`reveal_wrap_width`] on word boundaries
/// ([`wrap_to_width`]), and the plate is sized to its *longest wrapped line*
/// plus a margin either side and to one `row_height` per line — so the box is
/// neat and measured however long the text is, instead of running off the
/// window in one line.
///
/// Empty unless the run actually overflows `content_width` — a widget whose
/// text fits raises nothing, so the reveal reads as "there is more here"
/// rather than as chrome.
pub(crate) fn overflow_reveal_items(plate: &RevealPlate<'_>, measure: &dyn Fn(&str) -> f32) -> Vec<WidgetDrawItem> {
    let RevealPlate { theme, text, text_x, size_pixels, ink, content_width, row_height } = *plate;
    let text_width = measure(text);
    if text.is_empty() || !text_width.is_finite() || text_x + text_width <= content_width || row_height <= 0.0 {
        return Vec::new();
    }

    let lines = wrap_to_width(text, reveal_wrap_width(size_pixels), measure);
    let longest = lines.iter().map(|line| measure(line)).fold(0.0_f32, f32::max);
    let plate_width = text_x + longest + theme.pad;
    #[allow(clippy::cast_precision_loss)]
    let plate_height = lines.len() as f32 * row_height;
    if !plate_width.is_finite() || plate_height <= 0.0 {
        return Vec::new();
    }

    let mut items = Vec::with_capacity(5 + lines.len());
    items.push(quad(0.0, 0.0, plate_width, plate_height, theme.surface_raised));
    push_border(&mut items, plate_width, plate_height, 1.0, theme.outline);
    for (index, line) in lines.into_iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let row_top = index as f32 * row_height;
        items.push(WidgetDrawItem::Text {
            x: text_x,
            y: text_origin_y(row_top, row_height, size_pixels),
            font_id: theme.font_id,
            text: line,
            size_pixels,
            color: ink,
            clip: None,
        });
    }
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

/// `natural` content widths **spread across** a `row_width`-wide row with
/// `gap` between them: every cell keeps its own content width and the
/// leftover is shared equally, or — when the contents do not fit at all —
/// [`fit_row_widths`]'s water-fill shrinks the widest.
///
/// This is the sizing a row of cells that owns its whole frame wants: a
/// filled tab strip divides the bar between its tabs, so there is no width
/// left over to leave blank, but dividing it *evenly* ignores what is in each
/// cell. At the studio's own pane that put `Build` in a share three times
/// wider than the word and elided `Equipment` to `Equipm…` in the share
/// beside it — a row with room for every label cutting one of them, which is
/// the one thing §5 of the screen-design method forbids outright.
///
/// So content comes first and the slack is the only thing shared: each cell
/// gets its measured run plus its pads, and every cell then takes an equal
/// share of what is left over. Equal *slack*, not equal width — the cells
/// stay proportional to their words while the row still ends exactly at the
/// frame's right edge. Only when the contents overflow does anything shrink,
/// and then the water-fill lands the shortfall on the widest cells alone, so
/// the first label to be cut is the longest one rather than whichever cell
/// happened to be last.
fn spread_row_widths(natural: Vec<f32>, row_width: f32, gap: f32) -> Vec<f32> {
    let count = natural.len();
    if count == 0 {
        return natural;
    }
    #[allow(clippy::cast_precision_loss)]
    let slots = count as f32;
    let available = (slots - 1.0).mul_add(-gap, row_width).max(0.0);
    let content: f32 = natural.iter().sum();
    if content > available {
        return fit_row_widths(natural, row_width, gap);
    }
    let slack = (available - content) / slots;
    natural.into_iter().map(|width| width + slack).collect()
}

/// `natural` content widths fitted into a `row_width`-wide row with `gap`
/// between them: returned untouched when they already fit, and shrunk to fit
/// when they do not.
///
/// A row of content-sized cells can be handed a frame narrower than its
/// content asks for — a tab strip sits in a pane the reader can drag in, and
/// six tabs stop fitting long before the pane hits its minimum. Laying the
/// cells out at their natural widths anyway does not make the row wider; it
/// runs the last cell off the right edge, where the root's slot clip slices
/// it. The reader then sees the *last* cell alone with its padding cut — the
/// owner's "padding on right of text isn't symmetric" — while every cell
/// before it looks correct, which is exactly what makes the cause hard to see
/// from the screen.
///
/// The shrink is a water-fill, not a proportional scale: each cell takes the
/// smaller of what it asked for and an equal share of what is left, shortest
/// first, so a narrow cell keeps its natural width and the pressure lands on
/// the wide ones that caused it. Proportional scaling would take pixels off
/// `Tree` to pay for `Sequences`, shrinking a cell that fits perfectly well.
/// At the extreme — every cell over its share — the rule degenerates to the
/// even split, the same layout a row with no measurement uses.
///
/// Cells are shrunk, never their text: a caller draws the fitted width and
/// elides its run into it ([`elide_to_width`]), so a cut label says it was
/// cut instead of ending on a sliced glyph.
fn fit_row_widths(mut natural: Vec<f32>, row_width: f32, gap: f32) -> Vec<f32> {
    let count = natural.len();
    if count == 0 {
        return natural;
    }
    #[allow(clippy::cast_precision_loss)]
    let slots = count as f32;
    let available = (slots - 1.0).mul_add(-gap, row_width).max(0.0);
    if natural.iter().sum::<f32>() <= available {
        return natural;
    }

    let mut order: Vec<usize> = (0..count).collect();
    order.sort_unstable_by(|left, right| natural[*left].total_cmp(&natural[*right]));
    let mut budget = available;
    let mut left_to_place = count;
    for index in order {
        #[allow(clippy::cast_precision_loss)]
        let share = budget / left_to_place as f32;
        let taken = natural[index].min(share).max(0.0);
        natural[index] = taken;
        budget -= taken;
        left_to_place -= 1;
    }
    natural
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_kinds::WindowId;

    /// A fixed-advance measure, so a wrap point is arithmetic a reader can
    /// check: every character is `MONO_ADVANCE` pixels wide.
    const MONO_ADVANCE: f32 = 10.0;

    #[allow(clippy::cast_precision_loss)]
    fn mono(run: &str) -> f32 {
        run.chars().count() as f32 * MONO_ADVANCE
    }

    #[test]
    fn a_run_is_cut_on_a_character_boundary_with_room_kept_for_the_ellipsis() {
        // Tripwire: the ellipsis has to be *inside* the budget, not appended
        // past it — a cut that ignores the mark's own width is a row that
        // still overflows, only now by one glyph. And the cut must land on a
        // character boundary, which for a multi-byte run is the difference
        // between an elision and a panic.
        assert_eq!(elide_to_width("abcdef", 100.0, mono), "abcdef", "a run that fits is untouched");
        assert_eq!(elide_to_width("abcdef", 50.0, mono), "abcd…", "four characters plus the mark is five");
        assert_eq!(elide_to_width("abcdef", 15.0, mono), "…", "only the mark fits");
        assert_eq!(elide_to_width("abcdef", 5.0, mono), "", "not even the mark fits, so nothing is drawn");
        assert_eq!(elide_to_width("", 5.0, mono), "", "an empty run measures zero and is returned whole");

        let wide = elide_to_width("→→→→→→", 40.0, mono);
        assert_eq!(wide, "→→→…", "a multi-byte run is cut between characters");
        assert!(wide.ends_with(ELLIPSIS));
    }

    /// A reveal over a 100-pixel-wide, 24-pixel-tall slot.
    fn plate<'a>(theme: &'a Theme, text: &'a str) -> RevealPlate<'a> {
        RevealPlate {
            theme,
            text,
            text_x: theme.pad,
            size_pixels: 14.0,
            ink: theme.text_primary,
            content_width: 100.0,
            row_height: 24.0,
        }
    }

    fn plate_lines(items: &[WidgetDrawItem]) -> Vec<&str> {
        items
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Text { text, .. } => Some(text.as_str()),
                WidgetDrawItem::Quad { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
            })
            .collect()
    }

    #[test]
    fn the_overflow_plate_appears_only_for_a_run_that_does_not_fit() {
        // Tripwire: the reveal is a signal, not chrome. A plate raised over a
        // run that already fits would cover the widget to its right on every
        // hover, for nothing.
        let theme = Theme::DEFAULT;
        assert!(overflow_reveal_items(&plate(&theme, "short"), &mono).is_empty(), "a run inside the slot raises none");
        assert!(overflow_reveal_items(&plate(&theme, ""), &mono).is_empty(), "an empty run has nothing to reveal");

        let overflows = overflow_reveal_items(&plate(&theme, "far too long to fit"), &mono);
        let WidgetDrawItem::Quad { x, y, height, .. } = overflows[0] else {
            panic!("the plate leads with its fill");
        };
        assert_eq!((x, y, height), (0.0, 0.0, 24.0), "the plate starts at the widget's own origin, one line tall");
        assert_eq!(plate_lines(&overflows), vec!["far too long to fit"], "a run under the measure stays one line");
    }

    #[test]
    fn a_long_reveal_wraps_at_words_and_sizes_its_box_to_the_longest_line() {
        // The owner's note: an over-long reveal used to run off in one line.
        // The plate must wrap at the reading measure, never mid-word, and be
        // exactly as wide as its longest wrapped line plus a margin each side.
        let theme = Theme::DEFAULT;
        let text = "Raise or lower the selected terrain region by the configured brush strength";
        let items = overflow_reveal_items(&plate(&theme, text), &mono);
        let lines = plate_lines(&items);

        let wrap_width = reveal_wrap_width(14.0);
        assert!(lines.len() > 1, "a run this long must wrap");
        for line in &lines {
            assert!(mono(line) <= wrap_width || !line.contains(' '), "line {line:?} exceeds the measure");
            assert!(text.contains(line), "line {line:?} is not a run of the source — a word was split");
        }
        assert_eq!(lines.join(" "), text, "wrapping loses no word and adds none");

        let WidgetDrawItem::Quad { width, height, .. } = items[0] else {
            panic!("the plate leads with its fill");
        };
        let longest = lines.iter().copied().map(mono).fold(0.0_f32, f32::max);
        assert_eq!(width, theme.pad.mul_add(2.0, longest), "the box is the longest line plus a margin each side");
        #[allow(clippy::cast_precision_loss)]
        let expected_height = lines.len() as f32 * 24.0;
        assert_eq!(height, expected_height, "one row per wrapped line");
    }

    #[test]
    fn wrapping_never_splits_a_word_and_honours_an_authored_break() {
        // A word wider than the measure is over budget on its own line rather
        // than cut in half, and an explicit newline is the author's break.
        assert_eq!(
            wrap_to_width("antidisestablishmentarianism x", 50.0, mono),
            vec!["antidisestablishmentarianism", "x"]
        );
        assert_eq!(wrap_to_width("a\nb", 1000.0, mono), vec!["a", "b"], "an authored break survives a wide measure");
        assert!(wrap_to_width("   ", 100.0, mono).is_empty(), "blank text wraps to no lines at all");
        assert_eq!(wrap_to_width("one two three", 75.0, mono), vec!["one two", "three"], "breaks land between words");
    }

    /// Modifier state for a chord test, named by the keys held — any
    /// combination of `"ctrl"`, `"meta"`, `"alt"`, `"shift"` in one string, so
    /// each call site reads as the chord it is testing rather than as four
    /// positional bools.
    fn mods(held: &str) -> Modifiers {
        Modifiers {
            window: WindowId(1),
            ctrl: held.contains("ctrl"),
            meta: held.contains("meta"),
            alt: held.contains("alt"),
            shift: held.contains("shift"),
        }
    }

    #[test]
    fn the_clipboard_chords_answer_to_cmd_as_well_as_ctrl() {
        // Tripwire: the owner's note. On macOS the chord modifier is Cmd, and
        // a widget cannot ask the substrate which platform it is on, so both
        // must resolve to the same command everywhere.
        for chord in [mods("ctrl"), mods("meta")] {
            assert_eq!(edit_command(KEY_A, chord), Some(EditCommand::SelectAll));
            assert_eq!(edit_command(KEY_C, chord), Some(EditCommand::Copy));
            assert_eq!(edit_command(KEY_X, chord), Some(EditCommand::Cut));
            assert_eq!(edit_command(KEY_V, chord), Some(EditCommand::Paste));
        }
        let bare = mods("");
        assert_eq!(edit_command(KEY_A, bare), None, "a bare `a` is a typed character, not select-all");
        assert_eq!(edit_command(KEY_V, bare), None);
    }

    #[test]
    fn deletion_and_caret_motion_resolve_to_the_step_the_modifiers_name() {
        let bare = mods("");
        assert_eq!(edit_command(KEY_BACKSPACE, bare), Some(EditCommand::DeleteBackward));
        assert_eq!(edit_command(KEY_DELETE, bare), Some(EditCommand::DeleteForward));
        assert_eq!(
            edit_command(KEY_LEFT, bare),
            Some(EditCommand::MoveLeft { step: EditStep::Character, extend: false })
        );
        assert_eq!(
            edit_command(KEY_RIGHT, mods("shift")),
            Some(EditCommand::MoveRight { step: EditStep::Character, extend: true }),
            "Shift extends whatever the step is",
        );
        assert_eq!(
            edit_command(KEY_LEFT, mods("alt")),
            Some(EditCommand::MoveLeft { step: EditStep::Word, extend: false }),
            "Alt+Left is a word step",
        );
        assert_eq!(
            edit_command(KEY_LEFT, mods("ctrl")),
            Some(EditCommand::MoveLeft { step: EditStep::Word, extend: false }),
            "Ctrl+Left is the same word step on the other platforms",
        );
        assert_eq!(
            edit_command(KEY_RIGHT, mods("meta")),
            Some(EditCommand::MoveRight { step: EditStep::LineEdge, extend: false }),
            "Cmd+Right is the line edge, not a word",
        );
        assert_eq!(
            edit_command(KEY_HOME, bare),
            Some(EditCommand::MoveLeft { step: EditStep::LineEdge, extend: false })
        );
        assert_eq!(
            edit_command(KEY_END, mods("ctrl")),
            Some(EditCommand::MoveRight { step: EditStep::DocumentEdge, extend: false }),
            "the chord widens Home/End to the whole buffer",
        );
        assert_eq!(edit_command(KEY_ENTER, bare), None, "Enter is each control's own");
    }

    #[test]
    fn a_read_only_control_still_selects_and_copies_but_never_edits() {
        // Tripwire: `mutable` gates the destructive half only. A read-only
        // field a person cannot copy out of is worse than useless.
        let mut edit = TextEditState::new(String::from("locked"));
        assert_eq!(apply_edit_command(&mut edit, EditCommand::SelectAll, false), EditEffect::default());
        let copy = apply_edit_command(&mut edit, EditCommand::Copy, false);
        assert_eq!(copy.copy.as_deref(), Some("locked"));
        assert!(!copy.changed);

        for destructive in [EditCommand::Cut, EditCommand::Paste, EditCommand::DeleteBackward] {
            assert_eq!(apply_edit_command(&mut edit, destructive, false), EditEffect::default(), "{destructive:?}");
        }
        assert_eq!(edit.value(), "locked", "nothing destructive got through");

        let paste = apply_edit_command(&mut edit, EditCommand::Paste, true);
        assert!(paste.request_paste, "a mutable control asks the clipboard");
    }

    #[test]
    fn a_collapsed_selection_copies_nothing_rather_than_an_empty_string() {
        // Tripwire: an empty clipboard write would silently destroy whatever
        // the person had copied before pressing Ctrl+C on nothing.
        let mut edit = TextEditState::new(String::from("abc"));
        edit.place_caret(1);
        assert_eq!(apply_edit_command(&mut edit, EditCommand::Copy, true).copy, None);
        assert_eq!(apply_edit_command(&mut edit, EditCommand::Cut, true), EditEffect::default());
        assert_eq!(edit.value(), "abc", "a cut with nothing selected deletes nothing");
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
