// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `lib.rs`).
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
//! A row is one configured row tall until some row asks otherwise (see [A row
//! that is a table entry](#a-row-that-is-a-table-entry)). A list holding fewer
//! items than its viewport draws that many short rows and leaves the rest of
//! the frame empty — it never spreads them to fill it, which would turn a
//! two-item list into a pair of slabs and its selected row into a half-screen
//! block.
//!
//! The list measures, like every other content-sized widget in the kit. It
//! drives the same single-flight font-metrics request the label and the
//! tooltip do, and once the theme font's advances land it elides a row too
//! long for its frame with an ellipsis rather than letting the slot clip cut
//! it mid-glyph (the studio's gap 17). The same metrics give the widest row of
//! the whole item vector, which the list reports as its intrinsic width so a
//! column can be sized to what it holds.
//!
//! # What the list reports about its own size
//!
//! Two numbers ride up with the draw list. `WidgetDrawList::intrinsic` is the
//! size the list asks a **layout** for — the widest row of the whole vector
//! plus a pad each side, by the configured viewport's height. `content_height`
//! is what the whole vector stands at, which is the scroll span said in
//! pixels: the offset table's last sum for a table, the configured pitch by
//! the item count for a fixed-pitch list. A host draws the container around a
//! list from the second — a four-row table gets a four-row plate rather than a
//! tall empty box — because only the widget can answer it, the wrapping being
//! the widget's and the font metrics with it (the studio's gap 41).
//!
//! # The row under the pointer
//!
//! The list keeps its rows out of the host's hit table — the list owns them,
//! realizes a window of them, and scrolls that window under a pointer that has
//! not moved — so a host that wanted to explain the row a reader is resting on
//! had to redo the list's own geometry and got it wrong the moment the list
//! scrolled (the studio's gap 19). The list says it instead:
//! [`VirtualListHover`](crate::VirtualListHover) carries the row under the pointer, or `None` once the
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
//! A [`VirtualListRow`] is its `text`, a `trailing` run of [`InkedSpan`](crate::InkedSpan)s, and
//! the `role` both are set at. The trailing run is the row's **second column**:
//! a version, a count, a key, a run of tags — set right-aligned against the
//! row's own right pad, with the widest trailing run among the *realized* rows
//! deciding the column every one of them shares. The leading run elides into
//! what is left, because a name cut short still names the thing while an amount
//! cut short is a wrong number. `role` lets a list carry a name at
//! [`TextRole::Body`](crate::TextRole::Body) and a detail at [`TextRole::Caption`](crate::TextRole::Caption) — muted, like a
//! caption-role label — without the host drawing its own rows.
//!
//! The trailing run is spans rather than one string because a tag wears the ink
//! of what it names: `Spell Fire Duration` is three words in three inks on one
//! line, one word gap apart. The run right-aligns as a whole, and a run too
//! wide for the column gives way by **dropping whole spans off its end** — a
//! span is a word that means something on its own, so half of one is worse than
//! none of it, and the leading run's ellipsis stays the only cut mark on the
//! row. The head span always stays: the column exists for the fact in it.
//!
//! `ink` colours a run: the leading `text` carries the row's, each trailing
//! span carries its own ([`TextInk`](crate::TextInk)), so a name can say its own tier without a
//! suffix after it or a plate behind the row. A named ink survives the row
//! being chosen — what a tier or a damage type says about a thing does not stop
//! being true when the reader clicks it — while a span left `Inherited` follows
//! the row into `selection_text` the way it always did. A column of *amounts*
//! wants one ink down its edge and gets it by naming none.
//!
//! # A verb can sit on the row
//!
//! Round-9 note 4 — "skills should be removed via 'x' button bound to row",
//! drawn as `"Spark" ——— [Change gem][x]`. A row's `actions` are
//! [`RowAction`](crate::RowAction)s, and the list draws each as a real button at the row's right
//! end: the kit's own button face (`push_button_face` in `set`), so one
//! emphasis ladder, one elision rule, and one hover answer serve a verb whether
//! it stands in a slot of its own or inside a row this widget owns.
//!
//! The verbs are **flush** (round-12 note 1): nothing between one face and the
//! next, and the last face ends on the row's own right edge rather than on its
//! right pad. Two touching faces are told apart by a hairline in
//! [`Theme::edge`] on every boundary inside the block, because the rank a row
//! verb takes is [`ButtonEmphasis::Text`](crate::ButtonEmphasis) — a label and
//! no face at all — and two labels touching with nothing between them read as
//! one word. Where a verb already draws an outlined stroke the hairline lands
//! on it in the very same token, so a mixed block gains no second line.
//!
//! They are a **third column**, reserved before the text like the second one:
//! the widest verb block among the realized rows is the column every row gives
//! up, so the names elide on one edge rather than at ragged points, and the
//! trailing run sits clear of the verbs instead of under them. The reserve is
//! that block plus one gap of clear space *less* one pad, since the block
//! stands in the pad the text budget already gave up. A press on a
//! verb arms it and the release-inside fires [`VirtualListAction`](crate::VirtualListAction) — the
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
//! # A row that is a table entry
//!
//! A statistic and the sentence that qualifies it are one entry, and a table
//! of them is read by its **spacing** before its type
//! (`designing-a-screen.md` §4): gaps inside a group small, gaps between
//! groups large, whitespace before any rule. A list of evenly spaced rows has
//! no gaps to make large, which is why a table drawn on one had to spend a
//! blank row to open a block and had nowhere to put a note but on a row of its
//! own — where it reads as a statistic whose value failed to draw (the
//! studio's gaps 38 and 39).
//!
//! Four fields on [`VirtualListRow`] answer that, and each is opt-in:
//!
//! - **`note`** is the row's second line — caption size, muted ink, wrapped to
//!   the row's own text budget, three lines at most with the last elided. The
//!   sentence touches the number it qualifies and nothing else.
//! - **`indent`** starts the leading run and the note that many spacing units
//!   in, and takes the same width off what they are elided and wrapped
//!   against. The trailing column and the verbs do not move: a value
//!   right-aligns on one edge whatever rung its name sits on.
//! - **`space_before`** is ground above the row, not a taller plate — a group
//!   gap, and the first row's is honoured too.
//! - **`rule_above`** puts a hairline in [`Theme::outline`] across the row's
//!   text budget at the **top of that space**: the rule, then the air, then
//!   the row.
//!
//! Set any of them on any row and **every** row of that list is as tall as
//! what it holds: its role's pitch (the theme's `row_height` is the body
//! pitch, and the other roles scale by their type step against the body size),
//! plus a line per line of its note, plus its own space. The list then keeps a
//! prefix-sum **offset table** — one `f32` per item, rebuilt when the vector,
//! the frame, the font or the theme changes — and the realized window, the hit
//! test, the reported hover, the scroll span and the thumb are all read from
//! it by bisection rather than by walking from the top.
//!
//! Set none of them and there is no table: the pitch is the frame divided by
//! the configured row count exactly as it always was, the bar is drawn from
//! the same three counts, and a vector of ten thousand plain rows spends
//! nothing on heights it did not ask for. That fast path is the reason the
//! four fields are a vocabulary a *table* opts into rather than a cost every
//! list pays.
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
//!
//! The end of that travel is the **last window**: the first row whose top
//! clears a viewport of the content's end, rather than the last row starting
//! before it. A frame is rarely an exact prefix sum of its rows — a plate
//! capped by a pane's height never is — and rounding the other way left the
//! final row hanging below the frame's edge with nothing left to roll, which
//! is round-17 note 1, "on defense extended stats cannot scroll to bottom"
//! (the studio's gap 41a). The slack, never more than one row, falls above
//! the last window's start.
//!
//! The bar stands off the rows by a **gutter**, and the gutter is the host's:
//! [`VirtualListConfig::scroll_bar_gap_units`], two spacing units by default,
//! because a control inside a plate sits at least two units from its edge
//! (`designing-a-screen.md` §6) and from the rows' side the rail is that edge.
//! One unit was the whole gutter until round 15, and the owner read it as
//! touching the values twice — round-14 note 5 and round-17 note 7, "the
//! scrollbar is still too close to content to the left side". The gutter comes
//! out of the row's own width, so the fill, the trailing column and the
//! leading run's elision all stop on the gutter's left edge rather than
//! running under the track.
//!
//! Unless the strip is the **host's**: with
//! [`VirtualListConfig::host_scroll_strip`] the track stands one gutter past
//! the frame's right edge — the way a pane's rail is drawn past the body it
//! scrolls — and the rows give up nothing, so a value's right edge does not
//! move when the vector starts to overflow (round-16 note 3, "the scrollbar
//! should EXTEND the panel slightly to exist and be adjacent"). The host owes
//! the widget that column: [`VirtualListConfig::scroll_strip_width`] is how
//! wide it is, and the slot's clip has to reach across it, since a clip of the
//! frame alone erases a track drawn outside it.

mod actions;
mod draw;
mod measure;
mod pointer;
mod rows;
mod scroll;
mod scroll_bar;
mod selection;

#[cfg(test)]
mod fixture;

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::keycode::{KEY_DOWN, KEY_PAGE_DOWN, KEY_PAGE_UP, KEY_UP};
use aether_kinds::mouse_button;
use aether_kinds::{Key, MouseButton, MouseButtonRelease, MouseMove, MouseWheel};
use aether_text::FontMetricsResult;

use crate::set::defaults::WidgetDefaults;
use crate::set::{accept_font_metrics_result, apply_text_theme, pump_text_font_metrics, release_left, reply_if_hidden};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, Theme};
use crate::{
    Collect, HoverLost, SetWidgetState, VirtualListConfig, VirtualListRow, WidgetControlState, WidgetDrawList,
    WidgetFrame,
};

use actions::RowActionIndex;
use pointer::PressTarget;
use rows::rows_vary;
use scroll_bar::BarPlacement;
use selection::{SelectionMove, reveal_window};

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
    /// The gutter between the rows and the scroll bar's track, in spacing
    /// units — [`VirtualListConfig::scroll_bar_gap_units`], which the host
    /// sets and the rows give up.
    scroll_bar_gap_units: u8,
    /// Where the bar stands — [`VirtualListConfig::host_scroll_strip`].
    bar_placement: BarPlacement,
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
    /// The content-space top of every row: `items.len() + 1` prefix sums whose
    /// last entry is the content height. `None` is the **fixed-pitch fast
    /// path** — no row of the vector carries a note, an indent, a space or a
    /// rule, so every row is the one pitch the frame divides into and there is
    /// no table to keep.
    ///
    /// One `f32` per item is `O(rows)` memory over the whole vector, which is
    /// the one thing a virtual list otherwise refuses to spend: it is spent
    /// only by a list that asked for rows of its own heights, and a prefix sum
    /// is what lets the window, the hit test, the bar and the reported hover
    /// answer in `O(log rows)` rather than by walking from the top.
    row_tops: Option<Vec<f32>>,
    /// The `(width, height)` [`Self::row_tops`] was built for, or `None` for a
    /// table that has to be rebuilt. A note wraps to the width the row has and
    /// the gutter it gives up depends on the height, so the sums are only true
    /// for one frame.
    row_tops_frame: Option<(f32, f32)>,
    /// Whether any row of the vector asks for a height of its own — a note, an
    /// indent, a space, or a rule. Answered when the vector arrives rather
    /// than per frame, because it is the question the fast path is decided by
    /// and a virtual list exists so that a frame never walks every item.
    rows_vary: bool,
}

impl VirtualListWidget {
    /// The list a host's [`VirtualListConfig`] makes — everything `init` does,
    /// less the ctx it does not use. The hop from config to fields is one
    /// callable thing rather than a struct literal only the actor entry point
    /// can reach, so a test can drive a flag the way a host sets it instead of
    /// assigning the field the flag maps to and proving nothing about the
    /// mapping.
    ///
    /// The boot window is counted in rows at the one pitch, because there is
    /// no frame yet to measure a table against; [`Self::refresh_row_layout`]
    /// re-reveals the selection once there is.
    fn from_config(config: VirtualListConfig) -> Self {
        let font_id = config.theme.font_id;
        let visible_row_count = usize_from_u32(config.visible_row_count);
        let selected_index = initial_selection(config.initial_selected_index, config.items.len());
        let first_index = selected_index.map_or(0, |selected_index| {
            reveal_window(selected_index, 0, visible_row_count, config.items.len()).first_index
        });

        Self {
            rows_vary: rows_vary(&config.items),
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
            scroll_bar_gap_units: config.scroll_bar_gap_units,
            bar_placement: BarPlacement::of(config.host_scroll_strip),
            font_metrics: FontMetricsAdapter::new(font_id),
            widest_row_width: None,
            thumb_grab_pixels: None,
            wheel_residual_pixels: 0.0,
            hovered_action: None,
            pressed_action: None,
            pointer_local: None,
            hovered_row: None,
            row_tops: None,
            row_tops_frame: None,
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
/// [`VirtualListConfig`]; reports [`VirtualListSelected`](crate::VirtualListSelected) when selection
/// changes, [`VirtualListAction`](crate::VirtualListAction) when a verb bound to
/// a row is pressed, and [`VirtualListHover`](crate::VirtualListHover) when the
/// row under the pointer changes.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `VirtualListConfig` again to replace the item vector or viewport. An
/// item is a
/// `VirtualListRow { text, trailing, role, ink, actions, note, indent, space_before, rule_above }`
/// — write plain strings through `VirtualListRow::from` for a one-column list,
/// set `trailing` for a second right-aligned column, `ink` to colour the name,
/// hang `actions` (`RowAction::text` / `RowAction::danger`) on a row for verbs
/// at its right end, and set `ruled` on the config to divide the rows with a
/// hairline.
///
/// For a **table** rather than a list of choices, the last four are the
/// vocabulary: `with_note` for the sentence under a statistic, `with_indent`
/// for a figure derived from the one above it, `with_space_before` to open a
/// block, `with_rule_above` for the hairline over that space. Any of them
/// makes every row of the list as tall as what it holds.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for VirtualListWidget {
    type Config = VirtualListConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.virtual_list";

    fn init(config: VirtualListConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self::from_config(config))
    }

    /// Ask for the theme font's metrics; rows are elided against real
    /// advances as soon as there are any (inline children run `wire`).
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: VirtualListConfig) {
        self.rows_vary = rows_vary(&config.items);
        self.items = config.items;
        self.empty_text = config.empty_text;
        self.ruled = config.ruled;
        self.scroll_bar_gap_units = config.scroll_bar_gap_units;
        self.bar_placement = BarPlacement::of(config.host_scroll_strip);
        self.visible_row_count = usize_from_u32(config.visible_row_count);
        self.selected_index = initial_selection(config.initial_selected_index, self.items.len());
        self.first_index = 0;
        self.thumb_grab_pixels = None;
        self.wheel_residual_pixels = 0.0;
        self.hovered_action = None;
        self.pressed_action = None;
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        self.forget_measurements();
        // The heights are a function of the theme, so the table is rebuilt
        // once it has landed and before anything asks where a row stands.
        self.refresh_row_layout();
        self.reveal_selection();
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
        self.refresh_row_layout();
        let (local_x, local_y) = (press.x - self.frame.x, press.y - self.frame.y);
        self.pointer_local = Some((local_x, local_y));
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
        // A press on a row reveals it, which can move the window: the pointer
        // is where it was and the row under it may not be.
        self.settle_hovered_row(ctx);
    }

    /// Carry a live thumb drag, or follow the pointer across the rows and the
    /// verbs on them. The root captures the pointer on press, so the drag keeps
    /// following even once it leaves the narrow track.
    #[handler::single]
    fn on_mouse_move(&mut self, ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        self.refresh_row_layout();
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
            self.refresh_row_layout();
            self.scroll_by_pixels(-wheel.delta_y);
            self.settle_hovered_row(ctx);
        }
    }

    /// A release inside the verb it was armed on fires that verb — the
    /// button's own press-then-release-inside, so a press that slides off
    /// cancels rather than removing the row it drifted away from.
    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        self.refresh_row_layout();
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
        self.refresh_row_layout();
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
        // A keyboard reveal moves the window as surely as the wheel does, so
        // the row under a pointer that has not moved is a different row now.
        self.settle_hovered_row(ctx);
    }

    /// Reply the realized rows, each elided to the width it has, plus the
    /// intrinsic the widest row asks for and the height the whole vector
    /// stands at.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        self.refresh_row_layout();
        let intrinsic = self.intrinsic();
        let content_height = self.content_height();
        let items = self.draw_items();
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic, content_height, items, overlay: Vec::new() });
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
    use crate::set::virtual_list::fixture::list;
    use crate::theme::ThemeState;

    #[test]
    fn initial_selection_is_none_for_empty_and_clamped_for_nonempty() {
        assert_eq!(initial_selection(Some(0), 0), None);
        assert_eq!(initial_selection(None, 5), None, "no selection asked for is no selection");
        assert_eq!(initial_selection(Some(0), 1), Some(0));
        assert_eq!(initial_selection(Some(99), 5), Some(4));
        assert_eq!(initial_selection(Some(u32::MAX), usize::MAX), Some(usize_from_u32(u32::MAX)));
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
}
