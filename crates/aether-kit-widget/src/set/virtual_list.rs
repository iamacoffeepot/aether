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
use aether_text::FontMetricsResult;

use crate::set::defaults::WidgetDefaults;
use crate::set::{
    accept_font_metrics_result, apply_text_theme, elide_to_width, measured_text_width, pump_text_font_metrics,
    push_control_outlines, quad, release_left, reply_if_hidden, text_origin_y,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, TextRole, Theme, ThemeState};
use crate::{
    Collect, SetWidgetState, VirtualListConfig, VirtualListSelected, WidgetControlState, WidgetDrawItem,
    WidgetDrawList, WidgetFrame,
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
    items: Vec<String>,
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

    fn replace_control_state(&mut self, next: WidgetControlState) -> bool {
        let changed = self.state.replace(next);
        if changed && !self.state.can_mutate() {
            self.pressed = false;
        }
        if changed && !self.state.is_available() {
            self.thumb_grab_pixels = None;
        }
        changed
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        if self.replace_control_state(next) {
            emit_state_changed(ctx, &self.state);
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

    /// The width a row's text actually has: the row it is drawn in, less one
    /// `pad` at each end, so an elided row does not touch either edge of the
    /// space it was given.
    fn text_width_budget(&self) -> f32 {
        self.theme.pad.mul_add(-2.0, self.row_width()).max(0.0)
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
    fn fitted_text(&self, text: &str, size_pixels: f32) -> String {
        self.font_metrics.resolved().map_or_else(
            || String::from(text),
            |metrics| {
                elide_to_width(text, self.text_width_budget(), |run| measured_text_width(metrics, run, size_pixels))
            },
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
    /// the items or the font and cached.
    fn widest_row_width(&mut self) -> Option<f32> {
        if let Some(widest) = self.widest_row_width {
            return Some(widest);
        }
        let metrics = self.font_metrics.resolved()?;
        if self.items.is_empty() {
            return None;
        }
        let size = self.theme.label_size_pixels;
        let widest = self.items.iter().map(|item| measured_text_width(metrics, item, size)).fold(0.0_f32, f32::max);
        self.widest_row_width = Some(widest);
        Some(widest)
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
            text: self.fitted_text(&self.empty_text, size),
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
        let mut items = Vec::with_capacity(visible_row_count.saturating_mul(2).saturating_add(8));
        for (row_offset, item) in self.items[window.first_index..window.end_exclusive_index].iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let row_y = row_offset as f32 * row_height;
            let item_index = window.first_index + row_offset;
            let selected = self.selected_index == Some(item_index);
            let base = if selected {
                self.theme.selection
            } else {
                self.theme.surface_raised
            };
            let row_state = if selected {
                self.state.theme_state(self.pressed)
            } else {
                self.state.supporting_theme_state(false)
            };
            items.push(quad(0.0, row_y, row_width, row_height, self.theme.fill(base, row_state)));
            let text_base = if selected {
                self.theme.selection_text
            } else {
                self.theme.text_primary
            };
            items.push(WidgetDrawItem::Text {
                x: self.theme.pad,
                y: text_origin_y(row_y, row_height, self.theme.label_size_pixels),
                font_id: self.theme.font_id,
                text: self.fitted_text(item, self.theme.label_size_pixels),
                size_pixels: self.theme.label_size_pixels,
                color: self.theme.fill(text_base, self.state.supporting_theme_state(false)),
                clip: None,
            });
        }
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
        self.thumb_grab_pixels = None;
    }
}

/// A fixed-row virtual list. Spawned inline by a panel root with a
/// [`VirtualListConfig`]; reports [`VirtualListSelected`] when selection
/// changes.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `VirtualListConfig` again to replace the item vector or viewport.
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
            font_metrics: FontMetricsAdapter::new(font_id),
            widest_row_width: None,
            thumb_grab_pixels: None,
            wheel_residual_pixels: 0.0,
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
        self.visible_row_count = usize_from_u32(config.visible_row_count);
        self.selected_index = initial_selection(config.initial_selected_index, self.items.len());
        self.first_index = 0;
        self.thumb_grab_pixels = None;
        self.wheel_residual_pixels = 0.0;
        self.reveal_selection();
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        self.forget_measurements();
        self.apply_control_state(ctx, config.state);
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

    /// A press on the scroll bar takes the thumb; anywhere else in the frame
    /// chooses the row under it. The bar is checked first and does not need
    /// the list to be mutable: reading where you are in a read-only list is
    /// not a change to it.
    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button != mouse_button::LEFT || !self.state.is_available() {
            return;
        }
        let (local_x, local_y) = (press.x - self.frame.x, press.y - self.frame.y);
        if let Some(bar) = self.scroll_bar()
            && bar.contains(local_x, local_y)
        {
            self.press_scroll_bar(bar, local_y);
            return;
        }
        if !self.state.can_mutate() {
            return;
        }
        self.pressed = true;
        if let Some(selected_index) = self.row_at_local_y(local_y)
            && let Some(selected_index) = self.select_if_mutable(selected_index)
        {
            Self::emit(ctx, selected_index);
        }
    }

    /// Carry a live thumb drag. The root captures the pointer on press, so the
    /// drag keeps following even once it leaves the narrow track.
    #[handler::single]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if self.thumb_grab_pixels.is_some() {
            self.drag_thumb(moved.y - self.frame.y);
        }
    }

    /// The wheel moves the realized window and nothing else — the reader is
    /// looking, not choosing. Positive `delta_y` is a roll away from the
    /// reader, which moves the content down and the window up: the same
    /// negation the kit's scroll actor applies.
    #[handler::single]
    fn on_mouse_wheel(&mut self, _ctx: &mut WasmCtx<'_>, wheel: MouseWheel) {
        if self.state.is_available() {
            self.scroll_by_pixels(-wheel.delta_y);
        }
    }

    #[handler::single]
    fn on_mouse_button_release(&mut self, _ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
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
        let items = (0..item_count).map(|index| format!("row {index}")).collect();
        let selected_index = (item_count > 0).then_some(selected_index.min(item_count.saturating_sub(1)));
        VirtualListWidget {
            items,
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
        unmeasured.items = alloc::vec![long.clone()];
        assert_eq!(row_text(&unmeasured), alloc::vec![long.clone()], "no metrics, no elision");

        let mut widget = measured_list(1, 5);
        widget.items = alloc::vec![long];
        let drawn = row_text(&widget);
        assert!(drawn[0].ends_with(ELLIPSIS), "the cut row says it was cut: {drawn:?}");
        let size = widget.theme.label_size_pixels;
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        assert!(
            measured_text_width(metrics, &drawn[0], size) <= widget.text_width_budget(),
            "and the mark is inside the budget, not appended past it: {drawn:?}",
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
        widget.items = (0..200).map(|index| format!("a skill gem with a long name {index}")).collect();
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
        widget.items[17] = String::from("the widest row of them all");
        widget.forget_measurements();

        let size = widget.theme.label_size_pixels;
        let expected = {
            let metrics = widget.font_metrics.resolved().expect("the test table is installed");
            widget.theme.pad.mul_add(2.0, measured_text_width(metrics, &widget.items[17], size))
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
