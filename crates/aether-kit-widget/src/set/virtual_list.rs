// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! A fixed-row virtual list (issue 2921).
//!
//! The actor owns the complete item vector but realizes only the bounded row
//! window visible in its assigned frame. Selection is retained independently
//! from realization and keyboard movement reveals it without drawing the
//! offscreen rows.
//!
//! Selection is a state, not an affordance: the current row draws in the
//! theme's selection role, never in the accent that means "the primary
//! action". A model that holds no selection lights no row, and a list with no
//! items at all says so in one muted caption line instead of drawing an empty
//! rectangle.
//!
//! A row is always one configured row tall. A list holding fewer items than
//! its viewport draws that many short rows and leaves the rest of the frame
//! empty — it never spreads them to fill it, which would turn a two-item list
//! into a pair of slabs and its selected row into a half-screen block.
//!
//! The list measures, like every other content-sized widget in the kit. It
//! drives the same single-flight font-metrics request the label and the
//! tooltip do, and once the theme font's advances land it elides a row too
//! long for its frame with an ellipsis rather than letting the slot clip cut
//! it mid-glyph (the studio's gap 17). The same metrics give the widest row of
//! the whole item vector, which the list reports as its intrinsic width so a
//! column can be sized to what it holds.
//!
//! # The row under the pointer
//!
//! The list keeps its rows out of the host's hit table — the list owns them,
//! realizes a window of them, and scrolls that window under a pointer that has
//! not moved — so a host that wanted to explain the row a reader is resting on
//! had to redo the list's own geometry and got it wrong the moment the list
//! scrolled (the studio's gap 19). The list says it instead:
//! [`VirtualListHover`] carries the row under the pointer, or `None` once the
//! pointer has left the rows, and is sent whenever that answer *changes* — from
//! a pointer move, a wheel, a thumb drag, or a new item vector arriving under a
//! still pointer. The scroll bar's gutter is not a row, so a thumb drag reports
//! nothing rather than the row it happens to pass.
//!
//! A pointed-at row draws a face of its own: the kit's hover wash over the
//! plain surface, the same one a dropdown's open list has always drawn under
//! the pointer. It composes with the selection rather than replacing it — a
//! chosen row under the pointer is the selection carrying that wash — so all
//! four states are four faces. Before this the *widget-wide* hover flag lit the
//! selected row wherever in the list the pointer was, which lit the first gem
//! when the reader pointed at the fourth.
//!
//! # A row is two columns and a type step
//!
//! A [`VirtualListRow`] is its `text`, an optional `trailing` run, and the
//! `role` both are set at. The trailing run is the row's **second column**: a
//! version, a count, a key — set right-aligned against the row's own right
//! pad, with the widest trailing run among the *realized* rows deciding the
//! column every one of them shares. The leading run elides into what is left
//! and the trailing never does, because a name cut short still names the thing
//! while an amount cut short is a wrong number. `role` lets a list carry a
//! name at [`TextRole::Body`] and a detail at [`TextRole::Caption`] — muted,
//! like a caption-role label — without the host drawing its own rows.
//!
//! `ink` colours the **leading run only** ([`TextInk`]), so a name can say its
//! own tier without a suffix after it or a plate behind the row. The trailing
//! run keeps the row ink whatever the name is written in: a column of amounts
//! is read down one edge, and four colours down it is a column nobody can
//! compare. A named ink survives the row being chosen, because what a tier says
//! about a thing does not stop being true when the reader clicks it.
//!
//! # A verb can sit on the row
//!
//! Round-9 note 4 — "skills should be removed via 'x' button bound to row",
//! drawn as `"Spark" ——— [Change gem] [x]`. A row's `actions` are
//! [`RowAction`]s, and the list draws each as a real button at the row's right
//! end: the kit's own button face (`push_button_face` in `set`), so one
//! emphasis ladder, one elision rule, and one hover answer serve a verb whether
//! it stands in a slot of its own or inside a row this widget owns.
//!
//! They are a **third column**, reserved before the text like the second one:
//! the widest verb block among the realized rows is the column every row gives
//! up, so the names elide on one edge rather than at ragged points, and the
//! trailing run sits clear of the verbs instead of under them. A press on a
//! verb arms it and the release-inside fires [`VirtualListAction`] — the
//! button's own press-then-release-inside, so a press that slides off cancels,
//! which is what a `×` that unbinds a skill deserves. It reports **no**
//! selection: the whole reason a verb is on the row is that removing the third
//! skill should not cost a select first.
//!
//! Row verbs are **pointer-first**. The kit's keyboard traversal moves between
//! widgets (the root's Tab, `WidgetPanel::on_key`) and a list is one stop whose
//! arrows and page keys are its selection; there is no focus traversal *into* a
//! row, so nothing here binds a key to a verb. A host that needs the keyboard
//! reach gives the verb a button of its own beside the list.
//!
//! [`VirtualListConfig::ruled`] adds a hairline between rows, `n - 1` of them
//! for `n` realized rows. It is off by default: a list of choices is read down
//! its fills, and rules on one are chrome. It is for a list of *entries*,
//! where a reader has to see which trailing belongs to which name.
//!
//! # The scroll bar
//!
//! Round-4 note 3 — "binding a gem isn't scrollable, doesn't have a scroll bar
//! indicating how many entries are in the list. Would be nice. Or where in the
//! list you are." Both halves of that are one bar: its **length** is the
//! visible share of the vector, so a short thumb says the list is long, and
//! its **position** is where the reader is. It stands whenever the vector
//! overflows the viewport, never only on hover — a bar that appears when
//! touched cannot answer "how many entries are there" for a reader who has not
//! touched it. The wheel moves the window and so does dragging the thumb; the
//! two write the same `first_index`, which is the list's whole scroll state.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::keycode::{KEY_DOWN, KEY_PAGE_DOWN, KEY_PAGE_UP, KEY_UP};
use aether_kinds::mouse_button;
use aether_kinds::{Key, MouseButton, MouseButtonRelease, MouseMove, MouseWheel};
use aether_math::Rgba;
use aether_text::FontMetricsResult;

use crate::set::defaults::WidgetDefaults;
use crate::set::{
    ButtonFace, accept_font_metrics_result, apply_text_theme, approx_text_width, button_face_width, elide_to_width,
    measured_text_width, pump_text_font_metrics, push_button_face, push_control_outlines, quad, release_left,
    reply_if_hidden, text_origin_y,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, TextInk, TextRole, Theme, ThemeState};
use crate::{
    Collect, HoverLost, RowAction, SetWidgetState, VirtualListAction, VirtualListConfig, VirtualListHover,
    VirtualListRow, VirtualListSelected, WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisibleRowWindow {
    first_index: usize,
    end_exclusive_index: usize,
}

impl VisibleRowWindow {
    fn len(self) -> usize {
        self.end_exclusive_index.saturating_sub(self.first_index)
    }
}

/// How wide the scroll bar's track is, in spacing units — two, which is eight
/// pixels on the four-pixel grid: wide enough to grab with a pointer, narrow
/// enough that it reads as an edge of the list rather than a column in it.
const SCROLL_BAR_UNITS: u8 = 2;

/// How much clear space stands between a row's text and the scroll bar's
/// track, in spacing units. One: the bar is a mark on the list's edge, and a
/// row that runs up against it reads as text the bar is printing over
/// (round-5 note 8).
const SCROLL_BAR_GAP_UNITS: u8 = 1;

/// How much clear space stands between a row's leading run and its trailing
/// column, in spacing units. One — enough that the two read as two columns,
/// little enough that a short name and its amount still read as one row.
const TRAILING_GAP_UNITS: u8 = 1;

/// How much clear space stands between one row verb and the next, and between
/// the verb block and the text columns beside it, in spacing units. One — the
/// same gap the trailing column keeps, so a row of two columns and two verbs
/// reads as four things in a row rather than as a strip of controls.
const ACTION_GAP_UNITS: u8 = 1;

/// One verb of one row, addressed the way [`VirtualListAction`] reports it: an
/// index into the item vector, and an index into that row's own actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowActionIndex {
    row_index: usize,
    action_index: usize,
}

/// What a left press inside the list lands on.
///
/// The resolution order is the whole of it, and it is stated once here rather
/// than spelled down the press handler: the bar owns its gutter, a **verb owns
/// its own rect**, and the row owns everything else. A verb resolved after the
/// row would remove a skill *and* leave the reader holding a selection they
/// never asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PressTarget {
    ScrollBar(ScrollBar),
    Action(RowActionIndex),
    /// The row under the point, or `None` for the empty viewport below the last
    /// realized row — the list takes the press either way, and only an actual
    /// row is chosen by it.
    Row(Option<usize>),
}

/// Where one row verb stands, in widget-local pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ActionRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl ActionRect {
    fn contains(self, local_x: f32, local_y: f32) -> bool {
        local_x >= self.x
            && local_x < self.x + self.width
            && local_y >= self.y
            && local_y < self.y + self.height
            && self.width > 0.0
    }
}

/// How thick the rule between two rows of a `ruled` list is. A hairline: the
/// rule is there to separate entries, and anything heavier reads as a table
/// border the rows are trapped in.
const ROW_RULE_THICKNESS: f32 = 1.0;

/// The shortest a thumb may get, as a multiple of the track's width. A list of
/// thousands would otherwise compute a thumb a pixel tall — unreadable, and
/// impossible to grab — so past this the thumb stops shrinking and only its
/// travel goes on saying how much is off screen.
const MIN_THUMB_RATIO: f32 = 1.5;

/// The scroll bar's geometry in widget-local pixels: the track down the
/// frame's right edge, and the thumb standing in it.
///
/// The thumb's `height` is the visible share of the whole item vector and its
/// `top` is where the reader is, which is the pair of facts round-4 note 3
/// asked for. Both are derived from `first_index` every frame — the bar holds
/// no scroll state of its own, so a wheel, a drag, and a keyboard reveal all
/// move it by moving the one window the list already had.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ScrollBar {
    /// The track's left edge in widget-local pixels.
    left: f32,
    width: f32,
    height: f32,
    thumb_top: f32,
    thumb_height: f32,
}

impl ScrollBar {
    fn contains(self, local_x: f32, local_y: f32) -> bool {
        local_x >= self.left && local_x < self.left + self.width && local_y >= 0.0 && local_y < self.height
    }

    fn thumb_contains(self, local_y: f32) -> bool {
        local_y >= self.thumb_top && local_y < self.thumb_top + self.thumb_height
    }

    /// How far the thumb can travel: the track less the thumb itself. Zero for
    /// a thumb that fills its track, which is a list that does not overflow.
    fn travel(self) -> f32 {
        (self.height - self.thumb_height).max(0.0)
    }
}

/// The bar a list of `item_count` items showing `visible_row_count` of them
/// from `first_index` stands with, or `None` when there is nothing to say: a
/// vector that fits its viewport, an unlaid-out frame, or a frame too narrow
/// to give the track up without swallowing the rows.
#[allow(clippy::cast_precision_loss)] // a row count a reader could scroll cannot lose precision
fn scroll_bar(
    frame: &WidgetFrame,
    track_width: f32,
    first_index: usize,
    visible_row_count: usize,
    item_count: usize,
) -> Option<ScrollBar> {
    if !valid_frame(frame) || visible_row_count == 0 || item_count <= visible_row_count {
        return None;
    }
    let width = track_width.min(frame.width * 0.5);
    if !width.is_finite() || width < 1.0 {
        return None;
    }
    let height = frame.height;
    let share = visible_row_count as f32 / item_count as f32;
    let thumb_height = (height * share).max(width * MIN_THUMB_RATIO).min(height);
    let max_first_index = item_count - visible_row_count;
    let progress = first_index.min(max_first_index) as f32 / max_first_index as f32;
    Some(ScrollBar {
        left: frame.width - width,
        width,
        height,
        thumb_top: progress * (height - thumb_height),
        thumb_height,
    })
}

/// The first realized row a thumb whose top stands at `thumb_top` means — the
/// inverse of the `progress` [`scroll_bar`] draws with, so a drag and the bar
/// it moves cannot disagree about where the reader is.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn first_index_at(bar: ScrollBar, thumb_top: f32, visible_row_count: usize, item_count: usize) -> usize {
    let max_first_index = item_count.saturating_sub(visible_row_count);
    let travel = bar.travel();
    if max_first_index == 0 || travel <= 0.0 || !thumb_top.is_finite() {
        return 0;
    }
    let progress = (thumb_top / travel).clamp(0.0, 1.0);
    ((progress * max_first_index as f32).round() as usize).min(max_first_index)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionMove {
    Up,
    Down,
    PageUp,
    PageDown,
}

/// A fixed-row virtual list. The item vector is retained, but every collect
/// allocates draw items for only the current `VisibleRowWindow`.
pub struct VirtualListWidget {
    items: Vec<VirtualListRow>,
    /// The one line drawn in place of rows while `items` is empty; empty text
    /// draws nothing.
    empty_text: String,
    selected_index: Option<usize>,
    first_index: usize,
    visible_row_count: usize,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    pressed: bool,
    /// Single-flight exact metrics for the active theme font: a row is elided
    /// to the width it actually has, and the list reports the widest row it
    /// holds as its intrinsic width.
    font_metrics: FontMetricsAdapter,
    /// Whether a hairline stands between rows.
    ruled: bool,
    /// The widest measured row, remembered across frames. A virtual list
    /// exists so that a frame never touches every item, and the intrinsic
    /// width is the one number that has to — so it is measured once and
    /// forgotten ([`VirtualListWidget::forget_measurements`]) whenever the
    /// items, the font, or the theme's type size change under it.
    widest_row_width: Option<f32>,
    /// How far down the thumb the pointer grabbed it, while a thumb drag is
    /// live. `None` is no drag — the bar is otherwise pure geometry.
    thumb_grab_pixels: Option<f32>,
    /// Wheel pixels not yet worth a whole row. The window moves in rows, so a
    /// trackpad's stream of sub-row deltas would otherwise round to nothing
    /// and the list would not move at all.
    wheel_residual_pixels: f32,
    /// The row verb the pointer stands on, and the one a press armed. Per verb
    /// rather than per widget, like the segmented control's hovered / pressed
    /// segment: the row's fill answers the pointer for the row, and each verb
    /// answers for itself.
    hovered_action: Option<RowActionIndex>,
    pressed_action: Option<RowActionIndex>,
    /// Where the pointer last was in widget-local pixels, and the row that
    /// resolved to. The position is kept because the *row* under a still
    /// pointer changes on its own — the wheel and the thumb move the realized
    /// window beneath it — so the hover has to be recomputed from a remembered
    /// point rather than only when a `MouseMove` arrives.
    pointer_local: Option<(f32, f32)>,
    hovered_row: Option<usize>,
}

impl VirtualListWidget {
    fn window(&self) -> VisibleRowWindow {
        clamped_window(self.first_index, self.visible_row_count, self.items.len())
    }

    fn reveal_selection(&mut self) {
        let Some(selected_index) = self.selected_index else {
            self.first_index = 0;
            return;
        };
        self.first_index =
            reveal_window(selected_index, self.first_index, self.visible_row_count, self.items.len()).first_index;
    }

    fn select(&mut self, selected_index: usize) -> Option<u32> {
        if selected_index >= self.items.len() || self.selected_index == Some(selected_index) {
            return None;
        }
        self.selected_index = Some(selected_index);
        self.reveal_selection();
        u32::try_from(selected_index).ok()
    }

    fn select_if_mutable(&mut self, selected_index: usize) -> Option<u32> {
        if !self.state.can_mutate() {
            return None;
        }
        self.select(selected_index)
    }

    fn move_selection(&mut self, movement: SelectionMove) -> Option<u32> {
        let next = moved_selection(self.selected_index, movement, self.visible_row_count, self.items.len())?;
        self.select(next)
    }

    fn move_selection_if_mutable(&mut self, movement: SelectionMove) -> Option<u32> {
        if !self.state.can_mutate() {
            return None;
        }
        self.move_selection(movement)
    }

    fn emit(ctx: &WasmCtx<'_>, selected_index: u32) {
        if let Some(parent) = ctx.parent() {
            parent.send(&VirtualListSelected { selected_index });
        }
    }

    /// The row the pointer resolves to right now, or `None` when the pointer
    /// is off the list, over the scroll bar's gutter, or past the last
    /// realized row.
    ///
    /// The gutter is not a row: while a thumb drag is carrying the window the
    /// pointer is on the bar, and reporting whichever row happens to pass
    /// under it would stand a tooltip on a row the reader is not looking at.
    fn pointer_row(&self) -> Option<usize> {
        let (local_x, local_y) = self.pointer_local.filter(|_| self.state.is_available())?;
        (local_x >= 0.0 && local_x < self.row_width()).then(|| self.row_at_local_y(local_y)).flatten()
    }

    /// Recompute the hovered row and report it if it changed.
    ///
    /// Called from everything that can move a row out from under the pointer —
    /// the pointer itself, the wheel, a thumb drag, a fresh item vector — so
    /// the fact the host is told stays true while the list scrolls under a
    /// still pointer, which is the half of the studio's gap 19 that a host
    /// redoing the geometry itself could never get right.
    fn settle_hovered_row(&mut self, ctx: &WasmCtx<'_>) {
        let next = self.pointer_row();
        if self.hovered_row == next {
            return;
        }
        self.hovered_row = next;
        if let Some(parent) = ctx.parent() {
            parent.send(&VirtualListHover { row: next.and_then(|row| u32::try_from(row).ok()) });
        }
    }

    /// Report one row's verb. Not a selection, and never accompanied by one:
    /// the press that fires this chose nothing.
    fn emit_action(ctx: &WasmCtx<'_>, index: RowActionIndex) {
        let (Ok(row_index), Ok(action_index)) = (u32::try_from(index.row_index), u32::try_from(index.action_index))
        else {
            return;
        };
        if let Some(parent) = ctx.parent() {
            parent.send(&VirtualListAction { row_index, action_index });
        }
    }

    fn replace_control_state(&mut self, next: WidgetControlState) -> bool {
        let changed = self.state.replace(next);
        if changed && !self.state.can_mutate() {
            self.pressed = false;
            self.pressed_action = None;
            self.hovered_action = None;
        }
        if changed && !self.state.is_available() {
            self.thumb_grab_pixels = None;
        }
        changed
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        if self.replace_control_state(next) {
            emit_state_changed(ctx, &self.state);
            self.settle_hovered_row(ctx);
        }
    }

    /// One row's height: the viewport divided by the row count the list was
    /// *configured* for, never by the number it happens to have realized. A
    /// list holding fewer items than its viewport therefore draws its rows at
    /// their normal height with the rest of the viewport left empty — dividing
    /// by the realized count instead stretched two items over the whole frame,
    /// so a short list rendered as one giant row.
    fn row_height(&self) -> Option<f32> {
        if self.visible_row_count == 0 || !valid_frame(&self.frame) {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let divisor = self.visible_row_count as f32;
        let row_height = self.frame.height / divisor;
        (row_height.is_finite() && row_height > 0.0).then_some(row_height)
    }

    fn row_at_local_y(&self, local_y: f32) -> Option<usize> {
        let window = self.window();
        if !local_y.is_finite() || local_y < 0.0 || local_y >= self.frame.height {
            return None;
        }
        let row_height = self.row_height()?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let row_offset = (local_y / row_height).floor() as usize;
        (row_offset < window.len()).then(|| window.first_index + row_offset)
    }

    /// Drop the cached row measurement. Called wherever the items, the font,
    /// or the type scale change, which is every input the measurement has.
    fn forget_measurements(&mut self) {
        self.widest_row_width = None;
    }

    /// The width a row's two columns share: the row they are drawn in, less
    /// one `pad` at each end, so nothing in a row touches either edge of the
    /// space it was given.
    fn text_width_budget(&self) -> f32 {
        self.theme.pad.mul_add(-2.0, self.row_width()).max(0.0)
    }

    /// The width the *leading* run has once the row's right-hand furniture is
    /// reserved: the row's budget less the verb block, the trailing column, and
    /// one spacing unit of clear space before each. A window with neither
    /// reserves nothing, so an ordinary list is laid out exactly as it was.
    ///
    /// This is why the leading elides and nothing else does: both right-hand
    /// columns are subtracted *first* and the name takes what is left. An
    /// amount cut to `12…` is worse than no amount at all, a verb cut to `Rem…`
    /// is a control nobody can read, while a name cut to `Increased Critic…`
    /// still names the thing.
    fn leading_width_budget(&self, trailing_column: f32, actions_reserve: f32) -> f32 {
        let reserved = if trailing_column > 0.0 {
            trailing_column + self.theme.space(TRAILING_GAP_UNITS)
        } else {
            0.0
        };
        (self.text_width_budget() - reserved - actions_reserve).max(0.0)
    }

    /// One verb's width: its measured label plus one `pad` each side — exactly
    /// the intrinsic a [`ButtonWidget`](crate::set::ButtonWidget) reports, so a
    /// `×` on a row is the size it would be in a slot of its own. Approximated
    /// from the character count until the font's advances land, because a verb
    /// that occupied no width until then would let the name elide into the
    /// space it is about to take and then cut it again on the next frame.
    fn action_width(&self, action: &RowAction) -> f32 {
        self.font_metrics.resolved().map_or_else(
            || {
                self.theme
                    .pad
                    .mul_add(2.0, approx_text_width(action.label.chars().count(), self.theme.label_size_pixels))
            },
            |metrics| button_face_width(&action.label, &self.theme, metrics),
        )
    }

    /// The whole verb block one row carries: every verb, plus one gap between
    /// each pair. `0.0` for a row with no verbs.
    fn actions_width(&self, row: &VirtualListRow) -> f32 {
        let Some(pair_count) = row.actions.len().checked_sub(1) else {
            return 0.0;
        };
        #[allow(clippy::cast_precision_loss)] // a row carries verbs a reader can press, not thousands
        let gaps = pair_count as f32 * self.theme.space(ACTION_GAP_UNITS);
        row.actions.iter().map(|action| self.action_width(action)).sum::<f32>() + gaps
    }

    /// The verb block this window's rows share: the widest among the rows **on
    /// screen**, like the trailing column and for the same reason — a column
    /// sized by an off-screen row leaves a gap nothing stands in, and one row
    /// eliding at a different point from its neighbours reads as a ragged edge.
    fn actions_column(&self, window: VisibleRowWindow) -> f32 {
        self.items[window.first_index..window.end_exclusive_index]
            .iter()
            .map(|row| self.actions_width(row))
            .fold(0.0_f32, f32::max)
    }

    /// What the verbs take off the right end of every row's text: the shared
    /// block plus one gap of clear space, or nothing when no realized row
    /// carries a verb.
    fn actions_reserve(&self, window: VisibleRowWindow) -> f32 {
        match self.actions_column(window) {
            column if column > 0.0 => column + self.theme.space(ACTION_GAP_UNITS),
            _ => 0.0,
        }
    }

    /// Where each verb of one row stands. The block is right-aligned against
    /// the row's own right pad and the verbs run left to right in the order
    /// they were written, so the last one written is the one at the row's edge
    /// — the owner's `[Change gem] [x]`, with the `×` outermost.
    fn action_rects(&self, row: &VirtualListRow, row_y: f32, row_height: f32) -> Vec<ActionRect> {
        let gap = self.theme.space(ACTION_GAP_UNITS);
        let mut x = self.row_width() - self.theme.pad - self.actions_width(row);
        let mut rects = Vec::with_capacity(row.actions.len());
        for action in &row.actions {
            let width = self.action_width(action);
            rects.push(ActionRect { x, y: row_y, width, height: row_height });
            x += width + gap;
        }
        rects
    }

    /// The top of one item's row in widget-local pixels, or `None` for an item
    /// outside the realized window.
    fn row_top(&self, item_index: usize, row_height: f32) -> Option<f32> {
        let window = self.window();
        #[allow(clippy::cast_precision_loss)] // a realized row offset is at most a viewport's worth
        let row_offset = item_index.checked_sub(window.first_index).filter(|offset| *offset < window.len())? as f32;
        Some(row_offset * row_height)
    }

    /// The verb under a point, if the point is on one. Consulted *before* the
    /// row fill, so a press on a verb never also selects the row under it.
    fn action_at(&self, local_x: f32, local_y: f32) -> Option<RowActionIndex> {
        let row_index = self.row_at_local_y(local_y)?;
        let row_height = self.row_height()?;
        let row = self.items.get(row_index)?;
        let row_y = self.row_top(row_index, row_height)?;
        self.action_rects(row, row_y, row_height)
            .into_iter()
            .position(|rect| rect.contains(local_x, local_y))
            .map(|action_index| RowActionIndex { row_index, action_index })
    }

    /// What a left press at this widget-local point lands on, or `None` for a
    /// press a list that cannot be changed refuses. The bar is resolved first
    /// and needs no mutability: reading where you are in a read-only list is
    /// not a change to it.
    fn press_target(&self, local_x: f32, local_y: f32) -> Option<PressTarget> {
        if let Some(bar) = self.scroll_bar()
            && bar.contains(local_x, local_y)
        {
            return Some(PressTarget::ScrollBar(bar));
        }
        if !self.state.can_mutate() {
            return None;
        }
        if let Some(index) = self.action_at(local_x, local_y) {
            return Some(PressTarget::Action(index));
        }
        Some(PressTarget::Row(self.row_at_local_y(local_y)))
    }

    /// How one verb answers the pointer. A list that cannot be changed draws
    /// every verb disabled — read-only as well as disabled, because a verb that
    /// looks live and does nothing is worse than one that says it is dead —
    /// and otherwise each verb carries its own Pressed → Hover → Normal.
    fn action_theme_state(&self, index: RowActionIndex) -> ThemeState {
        if !self.state.can_mutate() {
            ThemeState::Disabled
        } else if self.pressed_action == Some(index) {
            ThemeState::Pressed
        } else if self.hovered_action == Some(index) {
            ThemeState::Hover
        } else {
            ThemeState::Normal
        }
    }

    /// Draw one row's verbs as button faces, at the rects the block resolves to.
    fn push_row_actions(
        &self,
        items: &mut Vec<WidgetDrawItem>,
        row: &VirtualListRow,
        item_index: usize,
        row_y: f32,
        row_height: f32,
    ) {
        for (action_index, (action, rect)) in
            row.actions.iter().zip(self.action_rects(row, row_y, row_height)).enumerate()
        {
            let face = ButtonFace {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                label: &action.label,
                emphasis: action.emphasis,
                tone: action.tone,
            };
            let theme_state = self.action_theme_state(RowActionIndex { row_index: item_index, action_index });
            push_button_face(items, &face, &self.theme, theme_state, self.font_metrics.resolved());
        }
    }

    /// One row's measured trailing width, or `0.0` for a row without one and
    /// while the font's advances are still in flight.
    fn trailing_width(&self, row: &VirtualListRow) -> f32 {
        let (Some(trailing), Some(metrics)) = (row.trailing.as_deref(), self.font_metrics.resolved()) else {
            return 0.0;
        };
        measured_text_width(metrics, trailing, self.theme.text_size_pixels(row.role))
    }

    /// The trailing column this window's rows share: the widest trailing run
    /// among the rows **on screen**. One column for the realized window rather
    /// than for the whole vector, because the reader compares what they can
    /// see — and a column sized by an off-screen row would leave a visible gap
    /// nothing stands in. `0.0` when no realized row has a trailing run, which
    /// is the ordinary single-column list.
    fn trailing_column(&self, window: VisibleRowWindow) -> f32 {
        self.items[window.first_index..window.end_exclusive_index]
            .iter()
            .map(|row| self.trailing_width(row))
            .fold(0.0_f32, f32::max)
    }

    /// How wide a row is: the frame less whatever the scroll bar's gutter
    /// takes off its right end. A row stops where the gutter starts — it does
    /// not run under the bar and get covered by it (round-5 note 8), which is
    /// what a full-frame row fill did.
    fn row_width(&self) -> f32 {
        (self.frame.width - self.bar_gutter_width()).max(0.0)
    }

    /// The track's configured width — a metric, not a measurement, so it
    /// scales with a theme scaled for a dense display.
    fn track_width(&self) -> f32 {
        self.theme.space(SCROLL_BAR_UNITS).max(1.0)
    }

    /// The bar this list stands with right now, or `None` when its vector
    /// fits its viewport.
    fn scroll_bar(&self) -> Option<ScrollBar> {
        scroll_bar(&self.frame, self.track_width(), self.first_index, self.visible_row_count, self.items.len())
    }

    /// How much of the frame's right end the bar owns: its track plus one
    /// spacing unit of gap. Zero when no bar stands, so a list that fits its
    /// viewport gives its whole frame to its rows.
    fn bar_gutter_width(&self) -> f32 {
        self.scroll_bar().map_or(0.0, |bar| bar.width + self.theme.space(SCROLL_BAR_GAP_UNITS))
    }

    /// The topmost row the window can start at.
    fn max_first_index(&self) -> usize {
        self.items.len().saturating_sub(self.visible_row_count)
    }

    /// Move the window to `first_index`, clamped. Selection is untouched: a
    /// reader scrolling to look at something has not chosen it.
    fn scroll_to(&mut self, first_index: usize) {
        self.first_index = first_index.min(self.max_first_index());
    }

    /// Scroll by content pixels, carrying the sub-row remainder. Positive
    /// moves the window down the vector.
    #[allow(clippy::cast_possible_truncation)] // the row delta is bounded by the wheel's own pixels
    fn scroll_by_pixels(&mut self, pixels: f32) {
        let Some(row_height) = self.row_height() else {
            return;
        };
        if !pixels.is_finite() {
            return;
        }
        let carried = self.wheel_residual_pixels + pixels;
        let rows = (carried / row_height).trunc();
        self.wheel_residual_pixels = row_height.mul_add(-rows, carried);
        let steps = rows as i64;
        let moved = if steps >= 0 {
            self.first_index.saturating_add(steps.unsigned_abs() as usize)
        } else {
            self.first_index.saturating_sub(steps.unsigned_abs() as usize)
        };
        self.scroll_to(moved);
    }

    /// Take the thumb at `local_y`, from the point on it the pointer grabbed —
    /// or, for a press on the bare track, from its middle, so the press
    /// carries the reader to where they pointed.
    fn press_scroll_bar(&mut self, bar: ScrollBar, local_y: f32) {
        self.thumb_grab_pixels = Some(if bar.thumb_contains(local_y) {
            local_y - bar.thumb_top
        } else {
            bar.thumb_height * 0.5
        });
        self.drag_thumb(local_y);
    }

    /// Move the window to wherever a live thumb drag now points.
    fn drag_thumb(&mut self, local_y: f32) {
        let (Some(grab), Some(bar)) = (self.thumb_grab_pixels, self.scroll_bar()) else {
            return;
        };
        self.scroll_to(first_index_at(bar, local_y - grab, self.visible_row_count, self.items.len()));
    }

    /// The bar's own draw: the track in the outline role, the thumb in the
    /// muted-text one so it reads as a mark on the list rather than as a
    /// control to press. Both are existing style roles — a scroll bar is not
    /// a new colour in the theme.
    fn scroll_bar_items(&self, bar: ScrollBar) -> [WidgetDrawItem; 2] {
        let thumb_state = if self.thumb_grab_pixels.is_some() {
            ThemeState::Pressed
        } else {
            self.state.supporting_theme_state(false)
        };
        [
            quad(bar.left, 0.0, bar.width, bar.height, self.theme.outline),
            quad(
                bar.left,
                bar.thumb_top,
                bar.width,
                bar.thumb_height,
                self.theme.fill(self.theme.text_muted, thumb_state),
            ),
        ]
    }

    /// One line as it will be drawn: elided to the row's own width with an
    /// [`ELLIPSIS`](crate::set::ELLIPSIS) once the theme font's metrics
    /// resolve, whole before that. The slot clip still bounds the row either
    /// way — this is what stops the clip from being the *first* thing that
    /// cuts, because a hard clip cuts mid-glyph and an ellipsis says a name
    /// was too long.
    fn fitted_text(&self, text: &str, size_pixels: f32, budget: f32) -> String {
        self.font_metrics.resolved().map_or_else(
            || String::from(text),
            |metrics| elide_to_width(text, budget, |run| measured_text_width(metrics, run, size_pixels)),
        )
    }

    /// The `[width, height]` this list asks a layout for: the widest row it
    /// holds plus one `pad` either side and the scroll bar's track when the
    /// vector overflows, by the configured row height times the configured
    /// viewport. `None` until the font's metrics resolve, and for a list with
    /// no rows to measure — a slot sized from a guess would resize the moment
    /// the real advances landed.
    ///
    /// The gutter is counted whenever the vector overflows rather than only
    /// once a frame exists to hang a bar on: the intrinsic is what *makes* the
    /// frame, so a width that ignored the bar would size a slot the bar then
    /// took a gutter's worth of text out of.
    fn intrinsic(&mut self) -> Option<[f32; 2]> {
        let widest = self.widest_row_width()?;
        #[allow(clippy::cast_precision_loss)] // a viewport of rows a reader could scroll cannot lose precision
        let height = self.theme.row_height * self.visible_row_count as f32;
        let gutter = if self.visible_row_count > 0 && self.items.len() > self.visible_row_count {
            self.track_width() + self.theme.space(SCROLL_BAR_GAP_UNITS)
        } else {
            0.0
        };
        let width = self.theme.pad.mul_add(2.0, widest) + gutter;
        (width.is_finite() && height.is_finite()).then_some([width, height])
    }

    /// The widest row in the whole item vector, measured once per change to
    /// the items or the font and cached. A row with a trailing run or a verb on
    /// it is as wide as all of its columns and the gaps between them: a slot
    /// sized from this has to hold the whole row, not just its name.
    fn widest_row_width(&mut self) -> Option<f32> {
        if let Some(widest) = self.widest_row_width {
            return Some(widest);
        }
        let metrics = self.font_metrics.resolved()?;
        if self.items.is_empty() {
            return None;
        }
        let gap = self.theme.space(TRAILING_GAP_UNITS);
        let widest = self
            .items
            .iter()
            .map(|row| {
                let size = self.theme.text_size_pixels(row.role);
                let trailing =
                    row.trailing.as_deref().map_or(0.0, |trailing| gap + measured_text_width(metrics, trailing, size));
                let actions = match self.actions_width(row) {
                    block if block > 0.0 => block + self.theme.space(ACTION_GAP_UNITS),
                    _ => 0.0,
                };
                measured_text_width(metrics, &row.text, size) + trailing + actions
            })
            .fold(0.0_f32, f32::max);
        self.widest_row_width = Some(widest);
        Some(widest)
    }

    /// The fill one row draws, from the two facts that can be true of it.
    ///
    /// Four faces, and the ladder between them is the point. A row the pointer
    /// is on takes the kit's role-agnostic hover wash over the plain surface —
    /// the same face a dropdown's open list has always drawn under the pointer,
    /// so the two lists answer a pointer alike. A chosen row is the selection
    /// role, a *state* rather than a wash. Chosen **and** pointed at composes
    /// the two: the selection carrying that same hover.
    ///
    /// Before this the widget-wide hover flag lit the *selected* row wherever
    /// in the list the pointer was, so pointing at the fourth gem lit the
    /// first — the owner's round-11 note 13, "the current behavior only has
    /// the selected element being activated when hovering over ANY item".
    fn row_fill(&self, selected: bool, hovered: bool) -> Rgba {
        let base = match (selected, hovered) {
            (true, _) => self.theme.selection,
            (false, true) => self.theme.fill(self.theme.surface_raised, ThemeState::Hover),
            (false, false) => self.theme.surface_raised,
        };
        let state = match self.state.supporting_theme_state(selected && self.pressed) {
            ThemeState::Normal if selected && hovered => ThemeState::Hover,
            state => state,
        };
        self.theme.fill(base, state)
    }

    /// The ink one run of a row is set in.
    ///
    /// A run with no ink of its own follows the row: `selection_text` on the
    /// chosen row, the muted ink at [`TextRole::Caption`] — a caption row is a
    /// quieter detail line and draws exactly as a caption-role label does —
    /// and the primary ink otherwise. A run that **names** an ink keeps it on
    /// the chosen row too: a name is written in its tier's colour because that
    /// is what the tier is, and a tier that disappears the moment the reader
    /// clicks the row is a tier the reader cannot compare.
    fn run_ink(&self, ink: TextInk, row: &VirtualListRow, selected: bool) -> Rgba {
        let base = match ink {
            TextInk::Inherited if selected => self.theme.selection_text,
            ink => self.theme.text_ink(ink, row.role),
        };
        self.theme.fill(base, self.state.supporting_theme_state(false))
    }

    /// The hairlines standing between the realized rows of a `ruled` list —
    /// `n - 1` of them for `n` rows, each on the row boundary it divides. A
    /// rule under the last row would underline the list rather than separate
    /// anything, and one above the first would be a second top edge.
    fn rule_items(&self, visible_row_count: usize, row_height: f32, row_width: f32) -> Vec<WidgetDrawItem> {
        if !self.ruled || visible_row_count < 2 {
            return Vec::new();
        }
        (1..visible_row_count)
            .map(|row_offset| {
                #[allow(clippy::cast_precision_loss)]
                let y = row_offset as f32 * row_height;
                quad(0.0, y, row_width, ROW_RULE_THICKNESS, self.theme.outline)
            })
            .collect()
    }

    /// The empty state: one caption-role, muted line at the top of the
    /// viewport. A list with nothing in it reads as told-you-so rather than as
    /// a control that failed to draw.
    fn empty_draw_items(&self) -> Vec<WidgetDrawItem> {
        if self.empty_text.is_empty() || !valid_frame(&self.frame) {
            return Vec::new();
        }
        let size = self.theme.text_size_pixels(TextRole::Caption);
        alloc::vec![WidgetDrawItem::Text {
            x: self.theme.pad,
            y: text_origin_y(0.0, self.theme.row_height.min(self.frame.height), size),
            font_id: self.theme.font_id,
            text: self.fitted_text(&self.empty_text, size, self.text_width_budget()),
            size_pixels: size,
            color: self.theme.fill(self.theme.text_muted, self.state.supporting_theme_state(false)),
            clip: None,
        }]
    }

    fn draw_items(&self) -> Vec<WidgetDrawItem> {
        if !self.state.is_visible() {
            return Vec::new();
        }
        if self.items.is_empty() {
            return self.empty_draw_items();
        }
        let window = self.window();
        let visible_row_count = window.len();
        let Some(row_height) = self.row_height() else {
            return Vec::new();
        };

        let row_width = self.row_width();
        let trailing_column = self.trailing_column(window);
        let actions_reserve = self.actions_reserve(window);
        let leading_budget = self.leading_width_budget(trailing_column, actions_reserve);
        let mut items = Vec::with_capacity(visible_row_count.saturating_mul(3).saturating_add(8));
        for (row_offset, item) in self.items[window.first_index..window.end_exclusive_index].iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let row_y = row_offset as f32 * row_height;
            let item_index = window.first_index + row_offset;
            let selected = self.selected_index == Some(item_index);
            let hovered = self.hovered_row == Some(item_index);
            items.push(quad(0.0, row_y, row_width, row_height, self.row_fill(selected, hovered)));

            let size = self.theme.text_size_pixels(item.role);
            items.push(WidgetDrawItem::Text {
                x: self.theme.pad,
                y: text_origin_y(row_y, row_height, size),
                font_id: self.theme.font_id,
                text: self.fitted_text(&item.text, size, leading_budget),
                size_pixels: size,
                color: self.run_ink(item.ink, item, selected),
                clip: None,
            });
            // The trailing run is set flush against the row's right pad — or
            // against the verb block when one stands there — so every row's
            // second column ends on one edge. It is drawn whole: the column was
            // reserved for the widest of them.
            if let Some(trailing) = item.trailing.as_deref().filter(|_| trailing_column > 0.0) {
                items.push(WidgetDrawItem::Text {
                    x: row_width - self.theme.pad - actions_reserve - self.trailing_width(item),
                    y: text_origin_y(row_y, row_height, size),
                    font_id: self.theme.font_id,
                    text: String::from(trailing),
                    size_pixels: size,
                    color: self.run_ink(TextInk::Inherited, item, selected),
                    clip: None,
                });
            }
            self.push_row_actions(&mut items, item, item_index, row_y, row_height);
        }
        items.extend(self.rule_items(visible_row_count, row_height, row_width));
        if let Some(bar) = self.scroll_bar() {
            items.extend(self.scroll_bar_items(bar));
        }
        push_control_outlines(&mut items, self.frame.width, self.frame.height, &self.state, &self.theme);
        items
    }
}

impl WidgetDefaults for VirtualListWidget {
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
        self.pressed = false;
        self.pressed_action = None;
        self.thumb_grab_pixels = None;
    }
}

/// A fixed-row virtual list. Spawned inline by a panel root with a
/// [`VirtualListConfig`]; reports [`VirtualListSelected`] when selection
/// changes, [`VirtualListAction`] when a verb bound to a row is pressed, and
/// [`VirtualListHover`] when the row under the pointer changes.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `VirtualListConfig` again to replace the item vector or viewport. An
/// item is a `VirtualListRow { text, trailing, role, ink, actions }` — write
/// plain strings through `VirtualListRow::from` for a one-column list, set
/// `trailing` for a second right-aligned column, `ink` to colour the name,
/// hang `actions` (`RowAction::text` / `RowAction::danger`) on a row for verbs
/// at its right end, and set `ruled` on the config to divide the rows with a
/// hairline.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for VirtualListWidget {
    type Config = VirtualListConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.virtual_list";

    fn init(config: VirtualListConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let font_id = config.theme.font_id;
        let visible_row_count = usize_from_u32(config.visible_row_count);
        let selected_index = initial_selection(config.initial_selected_index, config.items.len());
        let first_index = selected_index.map_or(0, |selected_index| {
            reveal_window(selected_index, 0, visible_row_count, config.items.len()).first_index
        });
        Ok(Self {
            items: config.items,
            empty_text: config.empty_text,
            selected_index,
            first_index,
            visible_row_count,
            theme: config.theme,
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            state: InteractionState::new(config.state),
            pressed: false,
            ruled: config.ruled,
            font_metrics: FontMetricsAdapter::new(font_id),
            widest_row_width: None,
            thumb_grab_pixels: None,
            wheel_residual_pixels: 0.0,
            hovered_action: None,
            pressed_action: None,
            pointer_local: None,
            hovered_row: None,
        })
    }

    /// Ask for the theme font's metrics; rows are elided against real
    /// advances as soon as there are any (inline children run `wire`).
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: VirtualListConfig) {
        self.items = config.items;
        self.empty_text = config.empty_text;
        self.ruled = config.ruled;
        self.visible_row_count = usize_from_u32(config.visible_row_count);
        self.selected_index = initial_selection(config.initial_selected_index, self.items.len());
        self.first_index = 0;
        self.thumb_grab_pixels = None;
        self.wheel_residual_pixels = 0.0;
        self.hovered_action = None;
        self.pressed_action = None;
        self.reveal_selection();
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        self.forget_measurements();
        self.apply_control_state(ctx, config.state);
        // A fresh vector under a still pointer is a different row under it.
        self.settle_hovered_row(ctx);
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Restyle: adopt the fanned theme and request metrics for its font. The
    /// list declares this rather than adopting the shared default, because a
    /// new font or type size invalidates every row it measured.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        apply_text_theme(ctx, &mut self.font_metrics, &mut self.theme, set.theme);
        self.forget_measurements();
    }

    /// Install a font-metrics reply; the next `Collect` elides and measures
    /// against real advances.
    #[handler::single]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_>, result: FontMetricsResult) {
        accept_font_metrics_result(ctx, &mut self.font_metrics, result);
        self.forget_measurements();
    }

    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        self.apply_control_state(ctx, set.state);
    }

    /// A press on the scroll bar takes the thumb; a press on a row's verb arms
    /// that verb and chooses nothing; anywhere else in the frame chooses the
    /// row under it. The bar is checked first and does not need the list to be
    /// mutable: reading where you are in a read-only list is not a change to
    /// it.
    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button != mouse_button::LEFT || !self.state.is_available() {
            return;
        }
        let (local_x, local_y) = (press.x - self.frame.x, press.y - self.frame.y);
        match self.press_target(local_x, local_y) {
            Some(PressTarget::ScrollBar(bar)) => self.press_scroll_bar(bar, local_y),
            Some(PressTarget::Action(index)) => self.pressed_action = Some(index),
            Some(PressTarget::Row(row_index)) => {
                self.pressed = true;
                if let Some(selected_index) = row_index.and_then(|row_index| self.select_if_mutable(row_index)) {
                    Self::emit(ctx, selected_index);
                }
            }
            None => {}
        }
    }

    /// Carry a live thumb drag, or follow the pointer across the rows and the
    /// verbs on them. The root captures the pointer on press, so the drag keeps
    /// following even once it leaves the narrow track.
    #[handler::single]
    fn on_mouse_move(&mut self, ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        self.pointer_local = Some((moved.x - self.frame.x, moved.y - self.frame.y));
        if self.thumb_grab_pixels.is_some() {
            self.drag_thumb(moved.y - self.frame.y);
        } else {
            self.hovered_action = self
                .state
                .can_mutate()
                .then(|| self.action_at(moved.x - self.frame.x, moved.y - self.frame.y))
                .flatten();
        }
        self.settle_hovered_row(ctx);
    }

    /// The pointer left the list, so no row and no verb of it is under the
    /// pointer any more — the widget-wide hover fact the shared handler keeps
    /// says nothing about *which* row or verb it was over.
    #[handler::single]
    fn on_hover_lost(&mut self, ctx: &mut WasmCtx<'_>, _lost: HoverLost) {
        self.state.set_hovered(false);
        self.hovered_action = None;
        self.pointer_local = None;
        self.settle_hovered_row(ctx);
    }

    /// The wheel moves the realized window and nothing else — the reader is
    /// looking, not choosing. Positive `delta_y` is a roll away from the
    /// reader, which moves the content down and the window up: the same
    /// negation the kit's scroll actor applies.
    #[handler::single]
    fn on_mouse_wheel(&mut self, ctx: &mut WasmCtx<'_>, wheel: MouseWheel) {
        if self.state.is_available() {
            self.scroll_by_pixels(-wheel.delta_y);
            self.settle_hovered_row(ctx);
        }
    }

    /// A release inside the verb it was armed on fires that verb — the
    /// button's own press-then-release-inside, so a press that slides off
    /// cancels rather than removing the row it drifted away from.
    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        let armed = if release.button == mouse_button::LEFT {
            self.pressed_action.take()
        } else {
            None
        };
        if let Some(armed) = armed
            && self.action_at(release.x - self.frame.x, release.y - self.frame.y) == Some(armed)
        {
            Self::emit_action(ctx, armed);
        }

        release_left(&mut self.pressed, false, release);
        release_left(&mut self.thumb_grab_pixels, None, release);
    }

    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if !self.state.can_mutate() {
            return;
        }
        let movement = match key.code {
            KEY_UP => SelectionMove::Up,
            KEY_DOWN => SelectionMove::Down,
            KEY_PAGE_UP => SelectionMove::PageUp,
            KEY_PAGE_DOWN => SelectionMove::PageDown,
            _ => return,
        };
        if let Some(selected_index) = self.move_selection_if_mutable(movement) {
            Self::emit(ctx, selected_index);
        }
    }

    /// Reply the realized rows, each elided to the width it has, plus the
    /// intrinsic the widest row asks for.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        let intrinsic = self.intrinsic();
        let items = self.draw_items();
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic, items, overlay: Vec::new() });
        }
    }
}

fn usize_from_u32(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn initial_selection(initial_selected_index: Option<u32>, item_count: usize) -> Option<usize> {
    let initial_selected_index = initial_selected_index?;
    if item_count == 0 {
        return None;
    }
    Some(usize_from_u32(initial_selected_index).min(item_count - 1))
}

fn clamped_window(first_index: usize, requested_visible_row_count: usize, item_count: usize) -> VisibleRowWindow {
    let visible_row_count = requested_visible_row_count.min(item_count);
    let max_first_index = item_count.saturating_sub(visible_row_count);
    let first_index = first_index.min(max_first_index);
    VisibleRowWindow { first_index, end_exclusive_index: first_index.saturating_add(visible_row_count).min(item_count) }
}

fn reveal_window(
    selected_index: usize,
    first_index: usize,
    requested_visible_row_count: usize,
    item_count: usize,
) -> VisibleRowWindow {
    let mut window = clamped_window(first_index, requested_visible_row_count, item_count);
    let visible_row_count = window.len();
    if visible_row_count == 0 || selected_index >= item_count {
        return window;
    }
    if selected_index < window.first_index {
        window = clamped_window(selected_index, visible_row_count, item_count);
    } else if selected_index >= window.end_exclusive_index {
        let first_index = selected_index.saturating_add(1).saturating_sub(visible_row_count);
        window = clamped_window(first_index, visible_row_count, item_count);
    }
    window
}

fn moved_selection(
    selected_index: Option<usize>,
    movement: SelectionMove,
    visible_row_count: usize,
    item_count: usize,
) -> Option<usize> {
    let selected_index = selected_index?;
    if item_count == 0 || visible_row_count == 0 {
        return None;
    }
    let last_index = item_count - 1;
    Some(match movement {
        SelectionMove::Up => selected_index.saturating_sub(1),
        SelectionMove::Down => selected_index.saturating_add(1).min(last_index),
        SelectionMove::PageUp => selected_index.saturating_sub(visible_row_count),
        SelectionMove::PageDown => selected_index.saturating_add(visible_row_count).min(last_index),
    })
}

fn valid_frame(frame: &WidgetFrame) -> bool {
    frame.x.is_finite()
        && frame.y.is_finite()
        && frame.width.is_finite()
        && frame.height.is_finite()
        && frame.width > 0.0
        && frame.height > 0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set::ELLIPSIS;
    use crate::{WidgetDrawItem, WidgetValidation};
    use aether_kinds::{CachedFontMetrics, FontMetrics};
    use alloc::format;
    use alloc::vec;

    fn list(item_count: usize, visible_row_count: usize, selected_index: usize) -> VirtualListWidget {
        let items = (0..item_count).map(|index| VirtualListRow::from(format!("row {index}"))).collect();
        let selected_index = (item_count > 0).then_some(selected_index.min(item_count.saturating_sub(1)));
        VirtualListWidget {
            items,
            ruled: false,
            empty_text: String::new(),
            selected_index,
            first_index: 0,
            visible_row_count,
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 10.0, y: 20.0, width: 100.0, height: 120.0 },
            state: InteractionState::new(WidgetControlState::default()),
            pressed: false,
            font_metrics: FontMetricsAdapter::new(Theme::DEFAULT.font_id),
            widest_row_width: None,
            thumb_grab_pixels: None,
            wheel_residual_pixels: 0.0,
            hovered_action: None,
            pressed_action: None,
            pointer_local: None,
            hovered_row: None,
        }
    }

    /// The same list with a resolved metric table whose every glyph advances
    /// half an em, so a row's width is `chars * size / 2` — exact without
    /// depending on a real font file.
    fn measured_list(item_count: usize, visible_row_count: usize) -> VirtualListWidget {
        let mut widget = list(item_count, visible_row_count, 0);
        widget.font_metrics.take_pending_request();
        widget.font_metrics.accept_reply(Some(CachedFontMetrics::new(&FontMetrics {
            units_per_em: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            line_gap: 0.0,
            default_advance: 500.0,
            advances: Vec::new(),
        })));
        widget
    }

    fn row_text(widget: &VirtualListWidget) -> Vec<String> {
        widget
            .draw_items()
            .into_iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Text { text, .. } => Some(text),
                WidgetDrawItem::Quad { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
            })
            .collect()
    }

    /// The runs one list draws, each with the ink it is written in, in draw
    /// order — so a row of two columns reads as its leading run then its
    /// trailing one.
    fn row_runs(widget: &VirtualListWidget) -> Vec<(String, Rgba)> {
        widget
            .draw_items()
            .into_iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Text { text, color, .. } => Some((text, color)),
                WidgetDrawItem::Quad { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
            })
            .collect()
    }

    #[test]
    fn the_row_under_the_pointer_follows_the_realized_window() {
        // Tripwire: the studio's gap 19 — the list keeps its rows out of the
        // host's hit table, so the only way a host could follow the pointer
        // was to divide the list's rectangle by its visible row count itself,
        // which names the right item only while every item is realized. This
        // is that arithmetic done where the window actually lives: the
        // pointer has not moved, the wheel has, and the answer moves with it.
        // A hover computed from the pointer alone would still say row 2 after
        // the scroll and stand a tooltip on the wrong gem.
        let mut widget = measured_list(200, 5);
        let row_height = widget.row_height().expect("a laid-out list has a row height");
        widget.pointer_local = Some((widget.theme.pad, row_height.mul_add(2.0, 1.0)));
        assert_eq!(widget.pointer_row(), Some(2));

        widget.scroll_by_pixels(row_height * 7.0);
        assert_eq!(widget.window().first_index, 7, "the wheel moved the window under a pointer that did not move");
        assert_eq!(widget.pointer_row(), Some(9), "so the same point is a different item");

        widget.pointer_local = Some((widget.row_width() + 1.0, row_height.mul_add(2.0, 1.0)));
        assert_eq!(widget.pointer_row(), None, "the scroll bar's gutter is not a row");

        widget.pointer_local = None;
        assert_eq!(widget.pointer_row(), None, "and a pointer that left the list is on nothing");
    }

    #[test]
    fn pointed_chosen_and_both_at_once_are_three_fills_and_none_of_them_is_the_plain_row() {
        // Tripwire: the owner's round-11 note 13 — "the current behavior only
        // has the selected element being activated when hovering over ANY item
        // in the list". The widget-wide hover flag lit the *chosen* row
        // wherever the pointer was, so a list answered the pointer by
        // brightening a row somewhere else. The four faces have to be four:
        // collapse hovered onto plain and the row under the pointer says
        // nothing, collapse hovered onto selected and pointing at a row claims
        // it was chosen, and collapse the composite onto either and the reader
        // cannot tell whether the row they are pointing at is the current one.
        let widget = measured_list(10, 5);
        let faces = [
            widget.row_fill(false, false),
            widget.row_fill(false, true),
            widget.row_fill(true, false),
            widget.row_fill(true, true),
        ];

        for (first, second) in (0..faces.len()).flat_map(|i| (i + 1..faces.len()).map(move |j| (i, j))) {
            assert_ne!(faces[first], faces[second], "row face {first} and row face {second} are one fill");
        }
    }

    #[test]
    fn a_named_ink_colours_the_name_alone_and_outlives_the_row_being_chosen() {
        // Tripwire: the owner's round-11 note 7 — an item's rarity is said by
        // the colour of its name and by nothing else. Two ways to lose it.
        // Apply the row's ink to the whole row and the trailing column comes
        // out in four colours, so a reader can no longer compare the numbers
        // they are lined up to compare. Let the chosen row's `selection_text`
        // win over a named ink — which is what the ink resolution did before
        // there was one — and the tier vanishes on the one row the reader
        // pointed at, which is the row they are asking about.
        let theme = Theme::DEFAULT;
        let mut widget = measured_list(2, 2);
        widget.items = vec![
            VirtualListRow { trailing: Some(String::from("21/20")), ..VirtualListRow::from("Astral Plate") }
                .with_ink(TextInk::RarityLegendary),
            VirtualListRow { trailing: Some(String::from("1")), ..VirtualListRow::from("Iron Ring") },
        ];
        widget.selected_index = Some(0);
        widget.forget_measurements();

        let runs = row_runs(&widget);
        assert_eq!(runs.len(), 4, "two rows of two columns: {runs:?}");
        assert_eq!(runs[0].1, theme.rarity_legendary, "the chosen row's name kept its tier");
        assert_eq!(runs[1].1, theme.selection_text, "its amount did not take the tier with it");
        assert_eq!(runs[2].1, theme.text_primary, "an inkless row is written exactly as it was");
        assert_eq!(runs[3].1, theme.text_primary);
    }

    #[test]
    fn window_clamps_zero_one_beginning_middle_and_tail() {
        assert_eq!(clamped_window(0, 5, 0), VisibleRowWindow { first_index: 0, end_exclusive_index: 0 });
        assert_eq!(clamped_window(8, 0, 10), VisibleRowWindow { first_index: 8, end_exclusive_index: 8 });
        assert_eq!(clamped_window(8, 1, 10), VisibleRowWindow { first_index: 8, end_exclusive_index: 9 });
        assert_eq!(clamped_window(0, 5, 100), VisibleRowWindow { first_index: 0, end_exclusive_index: 5 });
        assert_eq!(clamped_window(40, 5, 100), VisibleRowWindow { first_index: 40, end_exclusive_index: 45 });
        assert_eq!(clamped_window(99, 5, 100), VisibleRowWindow { first_index: 95, end_exclusive_index: 100 });
        assert_eq!(
            clamped_window(usize::MAX, usize::MAX, 100),
            VisibleRowWindow { first_index: 0, end_exclusive_index: 100 }
        );
    }

    #[test]
    fn every_window_is_bounded_and_has_at_most_the_requested_rows() {
        for item_count in 0..32 {
            for requested in 0..12 {
                for first_index in 0..40 {
                    let window = clamped_window(first_index, requested, item_count);
                    assert!(window.first_index <= window.end_exclusive_index);
                    assert!(window.end_exclusive_index <= item_count);
                    assert!(window.len() <= requested);
                    assert_eq!(window.len(), requested.min(item_count));
                }
            }
        }
    }

    #[test]
    fn initial_selection_is_none_for_empty_and_clamped_for_nonempty() {
        assert_eq!(initial_selection(Some(0), 0), None);
        assert_eq!(initial_selection(None, 5), None, "no selection asked for is no selection");
        assert_eq!(initial_selection(Some(0), 1), Some(0));
        assert_eq!(initial_selection(Some(99), 5), Some(4));
        assert_eq!(initial_selection(Some(u32::MAX), usize::MAX), Some(usize_from_u32(u32::MAX)));
    }

    #[test]
    fn reveal_moves_only_enough_to_include_selection() {
        assert_eq!(reveal_window(4, 0, 5, 100), VisibleRowWindow { first_index: 0, end_exclusive_index: 5 });
        assert_eq!(reveal_window(5, 0, 5, 100), VisibleRowWindow { first_index: 1, end_exclusive_index: 6 });
        assert_eq!(reveal_window(2, 10, 5, 100), VisibleRowWindow { first_index: 2, end_exclusive_index: 7 });
        assert_eq!(reveal_window(99, 90, 5, 100), VisibleRowWindow { first_index: 95, end_exclusive_index: 100 });
        assert_eq!(reveal_window(0, 0, 0, 100), VisibleRowWindow { first_index: 0, end_exclusive_index: 0 });
    }

    #[test]
    fn arrow_and_page_movement_clamp_and_require_a_nonzero_viewport() {
        assert_eq!(moved_selection(Some(0), SelectionMove::Up, 5, 100), Some(0));
        assert_eq!(moved_selection(Some(0), SelectionMove::Down, 5, 100), Some(1));
        assert_eq!(moved_selection(Some(5), SelectionMove::PageUp, 5, 100), Some(0));
        assert_eq!(moved_selection(Some(5), SelectionMove::PageDown, 5, 100), Some(10));
        assert_eq!(moved_selection(Some(99), SelectionMove::PageDown, 5, 100), Some(99));
        assert_eq!(moved_selection(Some(0), SelectionMove::Down, 0, 100), None);
        assert_eq!(moved_selection(None, SelectionMove::Down, 5, 0), None);
    }

    #[test]
    fn row_hit_uses_realized_rows_and_rejects_invalid_or_exclusive_bottom() {
        let mut widget = list(200, 5, 0);
        assert_eq!(widget.row_at_local_y(0.0), Some(0));
        assert_eq!(widget.row_at_local_y(23.999), Some(0));
        assert_eq!(widget.row_at_local_y(24.0), Some(1));
        assert_eq!(widget.row_at_local_y(119.999), Some(4));
        assert_eq!(widget.row_at_local_y(120.0), None);
        assert_eq!(widget.row_at_local_y(-0.1), None);
        assert_eq!(widget.row_at_local_y(f32::NAN), None);
        widget.frame.height = f32::INFINITY;
        assert_eq!(widget.row_at_local_y(0.0), None);

        let short = list(2, 5, 0);
        assert_eq!(short.row_at_local_y(23.999), Some(0));
        assert_eq!(short.row_at_local_y(24.0), Some(1));
        assert_eq!(short.row_at_local_y(48.0), None, "the empty viewport under the last item is not a row");
    }

    #[test]
    fn a_short_list_keeps_its_rows_one_configured_row_tall() {
        // Tripwire: row height divides the viewport by the *configured* row
        // count. Dividing by the realized count stretched a two-item list over
        // its whole frame — two slabs, and a selected row half the viewport
        // high, which is what a short list looked like in the studio.
        let widget = list(2, 5, 1);
        let items = widget.draw_items();
        let quads: Vec<_> = items
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Quad { y, height, color, .. } => Some((*y, *height, *color)),
                WidgetDrawItem::Text { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
            })
            .collect();

        assert_eq!(quads.len(), 2, "two items realize two row quads and no filler for the empty viewport");
        assert_eq!(quads[0], (0.0, 24.0, widget.theme.surface_raised));
        assert_eq!(quads[1], (24.0, 24.0, widget.theme.selection), "the selected row is one row high");
    }

    #[test]
    fn draw_realizes_exactly_the_current_window_text() {
        let mut widget = list(200, 5, 6);
        widget.first_index = 2;
        let items = widget.draw_items();
        let text: Vec<&str> = items
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Text { text, .. } => Some(text.as_str()),
                WidgetDrawItem::Quad { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
            })
            .collect();
        assert_eq!(text, vec!["row 2", "row 3", "row 4", "row 5", "row 6"]);
        assert_eq!(items.len(), 12, "five row quads, five labels, and the bar's track and thumb only");
    }

    #[test]
    fn an_empty_list_draws_its_line_as_one_muted_caption_and_otherwise_nothing() {
        let mut widget = list(0, 5, 0);
        assert!(widget.draw_items().is_empty(), "an empty list with no line to say draws nothing at all");

        widget.empty_text = String::from("No saved builds");
        let items = widget.draw_items();
        assert_eq!(items.len(), 1, "the empty state is one line, with no row chrome behind it");
        let WidgetDrawItem::Text { text, size_pixels, color, .. } = &items[0] else {
            panic!("the empty state must draw text, not a quad");
        };
        assert_eq!(text, "No saved builds");
        assert_eq!(*size_pixels, widget.theme.caption_size_pixels, "the empty line is set at the caption step");
        assert_eq!(*color, widget.theme.text_muted, "and inked muted");
    }

    #[test]
    fn the_selection_role_fills_the_current_row_and_no_row_without_one() {
        // Tripwire: the accent means "the primary action" and nothing else, so
        // a chosen row must never carry it, and a model holding no selection
        // must light no row at all.
        let mut widget = list(4, 4, 2);
        let row_fills = |widget: &VirtualListWidget| {
            widget
                .draw_items()
                .into_iter()
                .filter_map(|item| match item {
                    WidgetDrawItem::Quad { color, .. } => Some(color),
                    WidgetDrawItem::Text { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            row_fills(&widget),
            vec![
                widget.theme.surface_raised,
                widget.theme.surface_raised,
                widget.theme.selection,
                widget.theme.surface_raised
            ]
        );

        widget.selected_index = None;
        assert!(row_fills(&widget).iter().all(|color| *color == widget.theme.surface_raised));
    }

    #[test]
    fn a_row_too_long_for_the_frame_is_elided_and_only_once_the_font_resolves() {
        // Tripwire: an unmeasured row must draw whole — the slot clip is the
        // fallback, and eliding against a guessed advance would cut a name
        // that fits. Once the advances land the row is cut to the frame less
        // one pad each side, with the mark inside that budget.
        let long = String::from("a name far too long for this narrow list");
        let mut unmeasured = list(1, 5, 0);
        unmeasured.items = alloc::vec![VirtualListRow::from(long.clone())];
        assert_eq!(row_text(&unmeasured), alloc::vec![long.clone()], "no metrics, no elision");

        let mut widget = measured_list(1, 5);
        widget.items = alloc::vec![VirtualListRow::from(long)];
        let drawn = row_text(&widget);
        assert!(drawn[0].ends_with(ELLIPSIS), "the cut row says it was cut: {drawn:?}");
        let size = widget.theme.label_size_pixels;
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        assert!(
            measured_text_width(metrics, &drawn[0], size) <= widget.text_width_budget(),
            "and the mark is inside the budget, not appended past it: {drawn:?}",
        );
    }

    /// The drawn runs of a list as `(x, text)`, in draw order — a row's
    /// leading run then its trailing one.
    fn drawn_runs(widget: &VirtualListWidget) -> Vec<(f32, String)> {
        widget
            .draw_items()
            .into_iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Text { x, text, .. } => Some((x, text)),
                WidgetDrawItem::Quad { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
            })
            .collect()
    }

    #[test]
    fn one_trailing_column_serves_every_visible_row_and_only_the_name_elides() {
        // Tripwire: two failures the second column exists to prevent. If each
        // row placed its own trailing run against the right pad, the two would
        // still line up — but the *leading* budget would differ per row, so the
        // names would elide at ragged points; and if the trailing were elided
        // like the leading, a reader would get `21/…`, which is a wrong number
        // rather than a shortened one. The column is the widest trailing among
        // the visible rows, subtracted from every row's leading budget first.
        let mut widget = measured_list(2, 5);
        widget.items = vec![
            VirtualListRow {
                text: String::from("a gem name far too long for this narrow list"),
                trailing: Some(String::from("21/20")),
                role: TextRole::Body,
                ink: TextInk::default(),
                actions: Vec::new(),
            },
            VirtualListRow {
                text: String::from("short"),
                trailing: Some(String::from("1")),
                role: TextRole::Body,
                ink: TextInk::default(),
                actions: Vec::new(),
            },
        ];
        widget.forget_measurements();

        let size = widget.theme.label_size_pixels;
        let (column, wide_width, narrow_width) = {
            let metrics = widget.font_metrics.resolved().expect("the test table is installed");
            let wide = measured_text_width(metrics, "21/20", size);
            (wide, wide, measured_text_width(metrics, "1", size))
        };
        assert!(narrow_width < wide_width, "the two trailing runs are different widths");
        assert_eq!(widget.trailing_column(widget.window()), column, "the widest of them is the column");

        let runs = drawn_runs(&widget);
        assert_eq!(runs.len(), 4, "two rows, two runs each: {runs:?}");
        assert_eq!(runs[1].1, "21/20");
        assert_eq!(runs[3].1, "1", "the narrow amount is drawn whole, not padded and not cut");
        let right_edge = widget.row_width() - widget.theme.pad;
        assert!((runs[1].0 + wide_width - right_edge).abs() < f32::EPSILON, "the wide amount ends at the right pad");
        assert!((runs[3].0 + narrow_width - right_edge).abs() < f32::EPSILON, "and so does the narrow one");

        assert!(runs[0].1.ends_with(ELLIPSIS), "the name gave way: {:?}", runs[0].1);
        let budget = widget.leading_width_budget(column, 0.0);
        assert_eq!(budget, widget.text_width_budget() - column - widget.theme.space(TRAILING_GAP_UNITS));
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        assert!(measured_text_width(metrics, &runs[0].1, size) <= budget, "and stopped clear of the column");
    }

    /// A measured list whose every row carries the owner's pair of verbs —
    /// `[Change] [x]`, the second destructive — on a frame wide enough to hold
    /// a name beside them.
    fn actioned_list(item_count: usize, frame_width: f32) -> VirtualListWidget {
        let mut widget = measured_list(item_count, 5);
        widget.frame.width = frame_width;
        widget.items = (0..item_count)
            .map(|index| {
                VirtualListRow::from(format!("skill {index}"))
                    .with_actions(vec![RowAction::text("Change"), RowAction::danger("x")])
            })
            .collect();
        widget.forget_measurements();
        widget
    }

    /// The vertical middle of the `row_offset`-th realized row.
    fn row_middle_y(widget: &VirtualListWidget, row_offset: usize) -> f32 {
        let row_height = widget.row_height().expect("a laid-out list has a row height");
        #[allow(clippy::cast_precision_loss)]
        let top = row_offset as f32 * row_height;
        row_height.mul_add(0.5, top)
    }

    /// The rects the verbs of the `row_offset`-th realized row stand at.
    fn realized_action_rects(widget: &VirtualListWidget, row_offset: usize) -> Vec<ActionRect> {
        let row_height = widget.row_height().expect("a laid-out list has a row height");
        #[allow(clippy::cast_precision_loss)]
        let row_y = row_offset as f32 * row_height;
        let item = &widget.items[widget.window().first_index + row_offset];
        widget.action_rects(item, row_y, row_height)
    }

    #[test]
    fn every_row_ends_its_verbs_on_its_own_right_pad_one_gap_apart() {
        // Tripwire: the owner's round-11 note 4 — "the buttons should be flush
        // with each other and the last button should be flush with the end of
        // the list item". Three ways to lose that, none of which the
        // two-equal-verbs case above would catch. Left-align a row's block
        // inside the *shared* column and every row carrying fewer or narrower
        // verbs than the widest one ends short of the edge with a band of
        // slack after it. Right-align against the frame instead of the row and
        // the block slides under the scroll bar's gutter. Add the gap once for
        // the block rather than once per pair and a third verb overlaps its
        // neighbour.
        let mut widget = actioned_list(40, 240.0);
        widget.items[1] = VirtualListRow::from("one verb").with_actions(vec![RowAction::text("Change")]);
        widget.items[2] = VirtualListRow::from("three verbs").with_actions(vec![
            RowAction::text("Change"),
            RowAction::text("Copy"),
            RowAction::danger("x"),
        ]);
        widget.items[3] = VirtualListRow::from("no verbs at all");
        widget.forget_measurements();

        let gap = widget.theme.space(ACTION_GAP_UNITS);
        let right_edge = widget.row_width() - widget.theme.pad;
        assert!(widget.row_width() < widget.frame.width, "this list scrolls, so the gutter really is off the row");

        for row_offset in 0..widget.window().len() {
            let rects = realized_action_rects(&widget, row_offset);
            let Some(last) = rects.last() else {
                continue;
            };
            assert!(
                (last.x + last.width - right_edge).abs() < 1e-3,
                "row {row_offset} leaves slack after its last verb: {rects:?}",
            );
            for pair in rects.windows(2) {
                assert!(
                    (pair[1].x - (pair[0].x + pair[0].width) - gap).abs() < 1e-3,
                    "row {row_offset} does not hold one gap unit between its verbs: {rects:?}",
                );
            }
        }
    }

    #[test]
    fn a_press_on_a_row_verb_is_that_verb_and_never_also_the_row_under_it() {
        // Tripwire: the studio's gap 32 — round-9 note 4, "skills should be
        // removed via 'x' button bound to row". Two failures live in the
        // resolution order. Resolve the row first and the `×` selects the skill
        // it is about to remove, leaving the reader holding a selection they
        // never asked for; resolve no verb at all and the row is back to being
        // a plate that only selects, which is the gap. The verb owns its rect,
        // the row owns the rest.
        let widget = actioned_list(200, 240.0);
        let rects = realized_action_rects(&widget, 2);
        let middle_y = row_middle_y(&widget, 2);

        assert_eq!(rects.len(), 2, "both verbs stand: {rects:?}");
        assert!(
            (rects[1].x + rects[1].width - (widget.row_width() - widget.theme.pad)).abs() < f32::EPSILON,
            "the verb written last ends at the row's right pad: {rects:?}",
        );
        assert_eq!(
            rects[1].x - (rects[0].x + rects[0].width),
            widget.theme.space(ACTION_GAP_UNITS),
            "with one spacing unit between the pair",
        );

        assert_eq!(
            widget.press_target(rects[1].x + 1.0, middle_y),
            Some(PressTarget::Action(RowActionIndex { row_index: 2, action_index: 1 })),
            "a press inside the × is the ×",
        );
        assert_eq!(
            widget.press_target(rects[0].x + 1.0, middle_y),
            Some(PressTarget::Action(RowActionIndex { row_index: 2, action_index: 0 })),
            "and a press inside the first verb is that one, not the one beside it",
        );
        assert_eq!(
            widget.press_target(widget.theme.pad, middle_y),
            Some(PressTarget::Row(Some(2))),
            "a press on the row's own text still chooses the row",
        );
        assert_eq!(
            widget.press_target(rects[0].x - 1.0, middle_y),
            Some(PressTarget::Row(Some(2))),
            "and so does the pixel just short of the block — the gap belongs to the row",
        );
    }

    #[test]
    fn scrolling_carries_a_verb_with_its_own_row_and_reports_the_item_it_belongs_to() {
        // Tripwire: the list realizes a window, so the row *offset* under the
        // pointer is not the item index. A verb that reported its offset would
        // remove the wrong skill the moment the reader scrolled — and one whose
        // rect was computed from the item index rather than the offset would
        // stand off the bottom of the frame entirely.
        let mut widget = actioned_list(200, 240.0);
        let top_row_middle = row_middle_y(&widget, 0);
        let inside_the_cross = realized_action_rects(&widget, 0)[1].x + 1.0;
        assert_eq!(
            widget.press_target(inside_the_cross, top_row_middle),
            Some(PressTarget::Action(RowActionIndex { row_index: 0, action_index: 1 })),
        );

        widget.first_index = 7;
        assert_eq!(
            widget.press_target(inside_the_cross, top_row_middle),
            Some(PressTarget::Action(RowActionIndex { row_index: 7, action_index: 1 })),
            "the same point is now the eighth skill's ×, because the window moved under it",
        );
        assert_eq!(realized_action_rects(&widget, 0)[1].y, 0.0, "and the verb is drawn at the top of the frame");
    }

    #[test]
    fn a_row_name_elides_clear_of_the_verbs_and_the_verbs_are_drawn_whole() {
        // Tripwire: the verbs are the row's third column and are reserved
        // *first*, exactly as the trailing column is. Charge the name against
        // the whole row and it runs under the buttons — the round-5 note-8
        // defect one column further in; elide the verb instead and the reader
        // gets a control labelled `Ch…`, which is not a control.
        let mut widget = actioned_list(2, 240.0);
        widget.items[0] = VirtualListRow {
            text: String::from("a skill gem with a name far too long for this row"),
            trailing: Some(String::from("21/20")),
            role: TextRole::Body,
            ink: TextInk::default(),
            actions: vec![RowAction::text("Change"), RowAction::danger("x")],
        };
        widget.forget_measurements();

        let window = widget.window();
        let reserve = widget.actions_reserve(window);
        assert_eq!(
            reserve,
            widget.actions_column(window) + widget.theme.space(ACTION_GAP_UNITS),
            "the reserve is the shared block plus one gap of clear space",
        );

        let runs = drawn_runs(&widget);
        let size = widget.theme.label_size_pixels;
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        let (name_x, name) = runs[0].clone();
        assert!(name.ends_with(ELLIPSIS), "the name gave way to the verbs: {name:?}");
        let budget = widget.leading_width_budget(widget.trailing_column(window), reserve);
        assert!(measured_text_width(metrics, &name, size) <= budget, "and stopped inside the budget: {name:?}");

        let (trailing_x, trailing) = runs[1].clone();
        assert_eq!(trailing, "21/20", "the amount is drawn whole");
        assert!(
            (trailing_x + measured_text_width(metrics, &trailing, size)
                - (widget.row_width() - widget.theme.pad - reserve))
                .abs()
                < f32::EPSILON,
            "and ends against the verb block rather than under it",
        );

        let labels: Vec<String> = runs[2..4].iter().map(|(_, run)| run.clone()).collect();
        assert_eq!(labels, vec![String::from("Change"), String::from("x")], "both verbs read whole: {labels:?}");
        assert!(name_x < trailing_x, "the row still reads name, amount, verbs from the left");

        // The second row carries the same verbs, so both rows give up the same
        // width and their names elide on one edge.
        assert_eq!(widget.actions_width(&widget.items[0]), widget.actions_width(&widget.items[1]));
    }

    #[test]
    fn a_list_that_cannot_be_changed_draws_its_verbs_dead_and_refuses_their_presses() {
        // Tripwire: a read-only list is as unable to remove a skill as a
        // disabled one, so both must *say* so. A verb that kept its live ink
        // and swallowed the press is the worst of the three outcomes — the
        // reader presses `×`, nothing happens, and nothing said it would not.
        let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
        let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
        for control in [disabled, read_only] {
            let mut widget = actioned_list(200, 240.0);
            let inside_the_cross = realized_action_rects(&widget, 1)[1].x + 1.0;
            let middle_y = row_middle_y(&widget, 1);
            widget.replace_control_state(control.clone());

            assert_eq!(
                widget.action_theme_state(RowActionIndex { row_index: 1, action_index: 1 }),
                ThemeState::Disabled,
                "the verb draws dead",
            );
            assert_eq!(widget.press_target(inside_the_cross, middle_y), None, "and takes no press");
            assert!(
                widget
                    .draw_items()
                    .iter()
                    .any(|item| matches!(item, WidgetDrawItem::Text { text, .. } if text == "x"),),
                "it is still drawn — a verb that vanished would say the row lost its remove, not that it is dead",
            );
        }

        // Hover and press are per verb, and the pointer leaving the list drops
        // the one it was over: a stale hover would light a verb the pointer is
        // nowhere near.
        let mut widget = actioned_list(200, 240.0);
        let hovered = RowActionIndex { row_index: 3, action_index: 0 };
        widget.hovered_action = Some(hovered);
        widget.pressed_action = Some(RowActionIndex { row_index: 3, action_index: 1 });
        assert_eq!(widget.action_theme_state(hovered), ThemeState::Hover);
        assert_eq!(
            widget.action_theme_state(RowActionIndex { row_index: 3, action_index: 1 }),
            ThemeState::Pressed,
            "the armed verb outranks the hovered one on its own rect",
        );
        assert_eq!(
            widget.action_theme_state(RowActionIndex { row_index: 4, action_index: 0 }),
            ThemeState::Normal,
            "and a verb the pointer is not on answers nothing",
        );
        widget.cancel_activation();
        assert_eq!(widget.pressed_action, None, "focus loss disarms the verb like any other activation");
    }

    #[test]
    fn a_ruled_list_draws_one_hairline_between_each_pair_of_realized_rows() {
        // Tripwire: `n - 1`, never `n`. A rule under the last row underlines
        // the list rather than dividing anything, and a rule above the first
        // draws a second top edge on the frame the panel already bounded.
        let mut widget = list(3, 5, 0);
        assert!(widget.rule_items(3, 24.0, 100.0).is_empty(), "an unruled list draws none");

        widget.ruled = true;
        let rules = widget.rule_items(3, 24.0, 100.0);
        assert_eq!(rules.len(), 2, "three rows, two rules");
        for (index, rule) in rules.iter().enumerate() {
            let WidgetDrawItem::Quad { x, y, width, height, color, .. } = rule else {
                panic!("a rule is a quad: {rule:?}");
            };
            #[allow(clippy::cast_precision_loss)]
            let expected_y = (index + 1) as f32 * 24.0;
            assert_eq!((*x, *y, *width, *height), (0.0, expected_y, 100.0, ROW_RULE_THICKNESS));
            assert_eq!(*color, widget.theme.outline, "a divider is the outline role, not a colour of its own");
        }

        assert!(widget.rule_items(1, 24.0, 100.0).is_empty(), "one row has nothing to divide");
        assert_eq!(
            widget.draw_items().iter().filter(|item| matches!(item, WidgetDrawItem::Quad { height, .. } if (*height - ROW_RULE_THICKNESS).abs() < f32::EPSILON)).count(),
            2,
            "and the rules reach the list's own draw",
        );
    }

    #[test]
    fn a_row_stops_a_spacing_unit_short_of_the_bar_standing_beside_it() {
        // Tripwire: round-5 note 8 — "the scrollbar has no padding with the
        // inner content to the left so it just draws over it". A row laid out
        // across the whole frame runs under the track, so the bar prints on
        // top of the row's fill and, for a long enough name, its text. The
        // gutter is the track plus one spacing unit, and both the fill and the
        // elision budget stop at it.
        let mut widget = measured_list(200, 5);
        widget.items =
            (0..200).map(|index| VirtualListRow::from(format!("a skill gem with a long name {index}"))).collect();
        widget.forget_measurements();
        let bar = widget.scroll_bar().expect("a vector past its viewport stands a bar");

        let size = widget.theme.label_size_pixels;
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        for item in widget.draw_items() {
            match item {
                WidgetDrawItem::Quad { x, width, .. } => {
                    assert!(x + width <= bar.left || x >= bar.left, "a row fill straddles the bar's left edge");
                }
                WidgetDrawItem::Text { x, text, .. } => {
                    let right = x + measured_text_width(metrics, &text, size);
                    assert!(right < bar.left, "{text:?} runs to {right}, past the bar at {}", bar.left);
                }
                WidgetDrawItem::TexturedQuad { .. } => panic!("a list draws no textures"),
            }
        }

        let row_fill_right = widget.row_width();
        assert_eq!(
            bar.left - row_fill_right,
            widget.theme.space(SCROLL_BAR_GAP_UNITS),
            "and the gap between the row and the track is one spacing unit",
        );
    }

    #[test]
    fn the_intrinsic_width_is_the_widest_row_in_the_whole_vector_plus_a_pad_each_side() {
        // Tripwire: the intrinsic must measure the *items*, not the realized
        // window — a width that changed as the reader scrolled would resize
        // the column under them. It is also the one thing here that touches
        // every item, so it is cached until an input to it changes.
        let mut widget = measured_list(40, 5);
        widget.items[17] = VirtualListRow::from("the widest row of them all");
        widget.forget_measurements();

        let size = widget.theme.label_size_pixels;
        let expected = {
            let metrics = widget.font_metrics.resolved().expect("the test table is installed");
            widget.theme.pad.mul_add(2.0, measured_text_width(metrics, &widget.items[17].text, size))
                + widget.track_width()
                + widget.theme.space(SCROLL_BAR_GAP_UNITS)
        };
        let [width, height] = widget.intrinsic().expect("a measured, non-empty list reports an intrinsic");
        assert!((width - expected).abs() < f32::EPSILON, "{width} is not the widest row plus a pad each side");
        assert_eq!(height, widget.theme.row_height * 5.0, "the height is the configured viewport, not the item count");

        widget.first_index = 30;
        assert_eq!(widget.intrinsic().map(|size| size[0]), Some(width), "scrolling past the widest row keeps it");

        assert_eq!(list(40, 5, 0).intrinsic(), None, "an unmeasured list asks for nothing");
        assert_eq!(measured_list(0, 5).intrinsic(), None, "and neither does one with no rows to measure");
    }

    #[test]
    fn the_thumb_is_the_visible_share_long_and_stands_where_the_reader_is() {
        // Tripwire: round-4 note 3 asked the bar for two facts — how many
        // entries there are, and where in them you are. The first is the
        // thumb's length as a fraction of the track, the second is its
        // position, and a bar that got either from anything but the realized
        // window would answer the wrong question.
        let mut widget = list(20, 5, 0);
        let bar = widget.scroll_bar().expect("a vector past its viewport stands a bar");
        assert_eq!((bar.left, bar.width), (100.0 - widget.track_width(), widget.track_width()));
        assert_eq!(bar.height, 120.0, "the track is the whole viewport");
        assert!((bar.thumb_height - 120.0 * 5.0 / 20.0).abs() < f32::EPSILON, "five of twenty: {bar:?}");
        assert_eq!(bar.thumb_top, 0.0, "at the top of the vector the thumb is at the top of the track");

        widget.first_index = 15;
        let tail = widget.scroll_bar().expect("bar");
        assert!((tail.thumb_top - tail.travel()).abs() < 1e-3, "at the end it reaches the bottom: {tail:?}");

        widget.first_index = 7;
        let middle = widget.scroll_bar().expect("bar");
        assert!(middle.thumb_top > 0.0 && middle.thumb_top < middle.travel(), "and in between: {middle:?}");
    }

    #[test]
    fn a_list_that_fits_its_viewport_stands_no_bar_and_keeps_its_whole_width() {
        // Tripwire: the bar is present whenever the list overflows and absent
        // when it does not — never on hover. A bar over a list that fits is a
        // control saying there is more to see when there is not, and it would
        // also take a track's width of text away for nothing.
        let short = list(3, 5, 0);
        assert_eq!(short.scroll_bar(), None);
        assert_eq!(short.bar_gutter_width(), 0.0);
        assert_eq!(short.row_width(), 100.0);
        assert_eq!(short.text_width_budget(), short.theme.pad.mul_add(-2.0, 100.0));

        let long = list(200, 5, 0);
        assert!(long.scroll_bar().is_some());
        assert_eq!(long.bar_gutter_width(), long.track_width() + long.theme.space(SCROLL_BAR_GAP_UNITS));
        assert_eq!(long.text_width_budget(), long.theme.pad.mul_add(-2.0, long.row_width()));

        let mut unlaid = list(200, 5, 0);
        unlaid.frame = WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
        assert_eq!(unlaid.scroll_bar(), None, "an unlaid-out frame has no bar to place");
    }

    #[test]
    fn a_very_long_list_keeps_a_thumb_big_enough_to_see_and_to_grab() {
        // Tripwire: the share of a hundred thousand rows is a fraction of a
        // pixel. Past the floor the thumb stops shrinking and only its travel
        // goes on saying how much is off screen.
        let widget = list(100_000, 5, 0);
        let bar = widget.scroll_bar().expect("bar");
        assert!(bar.width.mul_add(-MIN_THUMB_RATIO, bar.thumb_height).abs() < f32::EPSILON, "{bar:?}");
        assert!(bar.thumb_height < bar.height, "and it is still a thumb inside a track, not the whole track");
    }

    #[test]
    fn dragging_the_thumb_and_the_bar_it_draws_agree_about_where_the_reader_is() {
        // Tripwire: the drag maps a thumb position back to a first row and the
        // draw maps a first row forward to a thumb position. Two rules that
        // disagreed would make the thumb jump away from the pointer on the
        // first frame of every drag.
        let mut widget = list(200, 5, 0);
        let bar = widget.scroll_bar().expect("bar");
        assert_eq!(first_index_at(bar, -10.0, 5, 200), 0, "above the track is the top of the vector");
        assert_eq!(first_index_at(bar, bar.travel() + 10.0, 5, 200), 195, "and below it the end");
        for first_index in [0usize, 1, 40, 97, 194, 195] {
            widget.first_index = first_index;
            let drawn = widget.scroll_bar().expect("bar");
            assert_eq!(first_index_at(drawn, drawn.thumb_top, 5, 200), first_index, "round trip at {first_index}");
        }

        // A press on the thumb keeps the point that was grabbed under the
        // pointer; a press on the bare track carries the reader to it.
        widget.first_index = 0;
        let bar = widget.scroll_bar().expect("bar");
        widget.press_scroll_bar(bar, bar.thumb_height * 0.5);
        assert_eq!(widget.first_index, 0, "grabbing the thumb where it stands moves nothing");
        widget.drag_thumb(bar.thumb_height.mul_add(0.5, bar.travel()));
        assert_eq!(widget.first_index, 195, "and dragging it the length of the track reaches the end");

        widget.thumb_grab_pixels = None;
        widget.first_index = 0;
        widget.press_scroll_bar(bar, bar.height * 0.5);
        assert!(widget.first_index > 0, "a press on the bare track carries the reader there: {}", widget.first_index);
    }

    #[test]
    fn the_wheel_moves_the_window_in_whole_rows_and_carries_the_remainder() {
        // Tripwire: the window is a row index, so a trackpad's stream of
        // sub-row deltas would round to nothing and the list would sit still.
        // Selection is untouched either way — a reader scrolling to look at
        // something has not chosen it.
        let mut widget = list(200, 5, 3);
        let row_height = widget.row_height().expect("a laid-out list has a row height");
        assert_eq!(row_height, 24.0);

        for _ in 0..4 {
            widget.scroll_by_pixels(row_height * 0.25);
        }
        assert_eq!(widget.first_index, 1, "four quarter-rows are one row");
        assert_eq!(widget.selected_index, Some(3), "and the selection did not move with the window");

        widget.scroll_by_pixels(-row_height * 8.0);
        assert_eq!(widget.first_index, 0, "the window clamps at the top");
        widget.scroll_by_pixels(row_height * 1000.0);
        assert_eq!(widget.first_index, 195, "and at the last full page");
        widget.scroll_by_pixels(f32::NAN);
        assert_eq!(widget.first_index, 195, "a non-finite wheel moves nothing");
    }

    #[test]
    fn selection_reports_only_actual_changes_and_reveals_them() {
        let mut widget = list(200, 5, 0);
        assert_eq!(widget.select(0), None);
        assert_eq!(widget.move_selection(SelectionMove::PageDown), Some(5));
        assert_eq!(widget.window(), VisibleRowWindow { first_index: 1, end_exclusive_index: 6 });
        assert_eq!(widget.move_selection(SelectionMove::Down), Some(6));
        assert_eq!(widget.window(), VisibleRowWindow { first_index: 2, end_exclusive_index: 7 });
        assert_eq!(widget.select(200), None);
    }

    #[test]
    fn disabled_and_read_only_state_block_selection_mutation() {
        let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
        let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
        for control in [disabled, read_only] {
            let mut widget = list(20, 5, 0);
            widget.replace_control_state(control);
            assert!(!widget.state.can_mutate());
            assert_eq!(widget.move_selection_if_mutable(SelectionMove::PageDown), None);
            assert_eq!(widget.select_if_mutable(4), None);
            assert_eq!(widget.selected_index, Some(0));
        }

        let mut widget = list(20, 5, 0);
        let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
        assert!(widget.replace_control_state(read_only.clone()));
        assert!(!widget.replace_control_state(read_only), "same state emits no second change");
        assert!(!widget.state.can_mutate());
        widget.state.gain_focus(true);
        assert!(widget.state.focused(), "read-only remains keyboard-focusable");
    }

    #[test]
    fn hidden_draw_is_empty_while_retaining_the_bounded_window() {
        let mut widget = list(200, 5, 6);
        widget.first_index = 2;
        let hidden = WidgetControlState { visible: false, ..WidgetControlState::default() };
        widget.replace_control_state(hidden);
        assert!(widget.draw_items().is_empty());
        assert_eq!(widget.window(), VisibleRowWindow { first_index: 2, end_exclusive_index: 7 });
    }

    #[test]
    fn hover_focus_and_control_state_follow_shared_interaction_rules() {
        let mut widget = list(20, 5, 0);
        widget.state.set_hovered(true);
        widget.state.gain_focus(true);
        assert_eq!(widget.state.theme_state(false), ThemeState::Hover);
        assert!(widget.state.focused());
        widget.state.lose_focus();
        assert_eq!(widget.state.theme_state(false), ThemeState::Hover);
        let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
        assert!(widget.replace_control_state(disabled));
        assert!(!widget.state.focused());
        assert_eq!(widget.state.theme_state(false), ThemeState::Disabled);
    }

    #[test]
    fn validation_outline_precedes_the_inset_focus_outline() {
        let mut widget = list(20, 5, 0);
        let control = WidgetControlState {
            validation: WidgetValidation::Warning { message: String::from("warning") },
            ..WidgetControlState::default()
        };
        widget.replace_control_state(control);
        widget.state.gain_focus(true);
        let items = widget.draw_items();
        assert_eq!(items.len(), 20, "ten row items, the bar's two quads, and two four-quad outlines");
        for item in &items[12..16] {
            assert!(matches!(item, WidgetDrawItem::Quad { color, .. } if *color == widget.theme.warning));
        }
        for item in &items[16..20] {
            assert!(matches!(item, WidgetDrawItem::Quad { color, .. } if *color == widget.theme.accent));
        }
    }
}
