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
//! is what the whole vector stands at, which is the scroll extent said in
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
//! A [`VirtualListRow`] is its `text`, a `trailing` run of [`InkedSpan`]s, and
//! the `role` both are set at. The trailing run is the row's **second column**:
//! a version, a count, a key, a run of tags — set right-aligned against the
//! row's own right pad, with the widest trailing run among the *realized* rows
//! deciding the column every one of them shares. The leading run elides into
//! what is left, because a name cut short still names the thing while an amount
//! cut short is a wrong number. `role` lets a list carry a name at
//! [`TextRole::Body`] and a detail at [`TextRole::Caption`] — muted, like a
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
//! span carries its own ([`TextInk`]), so a name can say its own tier without a
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
//! [`RowAction`]s, and the list draws each as a real button at the row's right
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
//! test, the reported hover, the scroll extent and the thumb are all read from
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
    reply_if_hidden, text_origin_y, wrap_to_width,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, TextInk, TextRole, Theme, ThemeState};
use crate::{
    Collect, HoverLost, InkedSpan, RowAction, SetWidgetState, VirtualListAction, VirtualListConfig, VirtualListHover,
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

/// How much clear space stands between a row's leading run and its trailing
/// column, in spacing units. One — enough that the two read as two columns,
/// little enough that a short name and its amount still read as one row.
const TRAILING_GAP_UNITS: u8 = 1;

/// The theme's word gap — how much clear space stands between one span of a
/// trailing run and the next, in spacing units. One: the spans are words on
/// one line, so they are spaced like words rather than like columns.
const TRAILING_SPAN_GAP_UNITS: u8 = 1;

/// How much clear space stands between the verb block and the text columns
/// beside it, in spacing units. One — the same gap the trailing column keeps,
/// so a row of two columns and a block of verbs reads as three things in a row.
///
/// Nothing stands between one verb and the next: they are **flush**, and the
/// last one ends on the row's own right edge rather than on its right pad
/// (round-12 note 1).
const ACTION_BLOCK_GAP_UNITS: u8 = 1;

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

/// Where the parts of one realized row stand, in widget-local pixels.
///
/// A row is a slot, a plate inside it, and a first line inside that. The three
/// are one rectangle for the ordinary row and three for a table entry: the
/// slot opens with the row's `space_before` **ground**, the plate is the fill
/// under the whole entry, and the first line is the band its name, its
/// trailing column and its verbs are centred in, with the note's lines under
/// it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RowBands {
    /// The top of the whole slot — the top of the row's space, which is where
    /// a `rule_above` hairline stands.
    slot_top: f32,
    /// The top of the plate, below that space.
    plate_top: f32,
    /// How tall the plate is: the row less its space.
    plate_height: f32,
    /// The band the row's own line stands in — its role's pitch, which is the
    /// whole plate for a row without a note.
    line_height: f32,
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

/// How far a note is set in past its own row's indent, in spacing units. One:
/// enough that the sentence reads as hanging off the name above it, little
/// enough that it stays inside the same entry.
const NOTE_INDENT_UNITS: u8 = 1;

/// A note line's height as a multiple of the caption size. Tighter than a
/// row's pitch, because the lines of one note are one paragraph and the space
/// between them is leading rather than a gap between rows.
const NOTE_LINE_HEIGHT_RATIO: f32 = 1.3;

/// The most lines one row's note may take. Three: a note is a sentence about
/// the row above it, and a row that grows past three lines has stopped being
/// an entry in a table and become a paragraph the host should draw itself.
/// Past the cap the third line carries what is left of the sentence, elided
/// with an [`ELLIPSIS`](crate::set::ELLIPSIS), so the cut says it is a cut.
const MAX_NOTE_LINES: usize = 3;

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

/// How far the viewport reaches, how far the whole vector reaches, and where
/// the reader stands in it — the three facts the scroll bar is drawn from.
///
/// The unit is **rows** for a list at one pitch and **content pixels** for one
/// whose rows differ, and the bar does not care which: it is a ratio of the
/// three either way. Stating it once is what keeps the two kinds of list
/// scrolling alike, and keeps a list of uniform rows measuring exactly the bar
/// it measured before rows had heights of their own.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ScrollExtent {
    offset: f32,
    viewport: f32,
    content: f32,
}

impl ScrollExtent {
    /// How far the offset can travel before the last of the content stands at
    /// the bottom of the viewport. `0.0` for content that fits.
    fn travel(self) -> f32 {
        (self.content - self.viewport).max(0.0)
    }
}

/// Where a list's scroll bar stands: in a gutter cut out of the list's own
/// frame, or in a strip the host reserved past the frame's right edge
/// ([`VirtualListConfig::host_scroll_strip`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BarPlacement {
    InsideFrame,
    HostStrip,
}

impl BarPlacement {
    fn of(host_scroll_strip: bool) -> Self {
        if host_scroll_strip {
            Self::HostStrip
        } else {
            Self::InsideFrame
        }
    }
}

/// The column the track stands in, in widget-local pixels: down the frame's
/// right end for the list's own bar, and past that end for one the host has
/// reserved a strip for.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TrackColumn {
    left: f32,
    width: f32,
}

/// The bar a list standing at `extent` draws in `track`, or `None` when there
/// is nothing to say: a vector that fits its viewport, or an unlaid-out frame.
fn scroll_bar(frame: &WidgetFrame, track: TrackColumn, extent: ScrollExtent) -> Option<ScrollBar> {
    if !valid_frame(frame) || extent.viewport <= 0.0 || extent.content <= extent.viewport {
        return None;
    }
    let width = track.width;
    let height = frame.height;
    let share = extent.viewport / extent.content;
    let thumb_height = (height * share).max(width * MIN_THUMB_RATIO).min(height);
    let progress = (extent.offset / extent.travel()).clamp(0.0, 1.0);
    Some(ScrollBar { left: track.left, width, height, thumb_top: progress * (height - thumb_height), thumb_height })
}

/// The scroll offset a thumb whose top stands at `thumb_top` means, in
/// `extent`'s own unit — the inverse of the `progress` [`scroll_bar`] draws
/// with, so a drag and the bar it moves cannot disagree about where the reader
/// is.
fn scroll_offset_at(bar: ScrollBar, thumb_top: f32, extent: ScrollExtent) -> f32 {
    let travel = bar.travel();
    if travel <= 0.0 || !thumb_top.is_finite() {
        return 0.0;
    }
    (thumb_top / travel).clamp(0.0, 1.0) * extent.travel()
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

/// Whether any of `items` asks for a height of its own.
fn rows_vary(items: &[VirtualListRow]) -> bool {
    items.iter().any(|row| row.note.is_some() || row.indent > 0 || row.space_before > 0 || row.rule_above)
}

impl VirtualListWidget {
    /// The list a host's [`VirtualListConfig`] makes — everything `init` does,
    /// less the ctx it does not use. The hop from config to fields is one
    /// callable thing rather than a struct literal only the actor entry point
    /// can reach, so a test can drive a flag the way a host sets it instead of
    /// assigning the field the flag maps to and proving nothing about the
    /// mapping.
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

    /// The rows realized right now: the configured count from `first_index`
    /// while every row is one height, and every row the frame reaches once the
    /// offset table stands — a table of tall and short rows shows as many as
    /// fit rather than as many as were asked for. At least one row is realized
    /// either way, so a row taller than the whole viewport still draws.
    fn window(&self) -> VisibleRowWindow {
        let Some(tops) = &self.row_tops else {
            return clamped_window(self.first_index, self.visible_row_count, self.items.len());
        };
        if self.items.is_empty() || self.visible_row_count == 0 || !valid_frame(&self.frame) {
            return VisibleRowWindow { first_index: 0, end_exclusive_index: 0 };
        }
        let first_index = self.first_index.min(self.max_first_index());
        let limit = tops[first_index] + self.frame.height;
        let end_exclusive_index = tops.partition_point(|top| *top < limit).clamp(first_index + 1, self.items.len());
        VisibleRowWindow { first_index, end_exclusive_index }
    }

    /// Move the window so the selected row stands in it, without moving it
    /// further than that. A count of rows on the fast path; once the offset
    /// table stands, the topmost row that still leaves the selected row's own
    /// bottom edge inside the viewport, because a row below the selection may
    /// be two lines tall and "one viewport of rows" is no longer a count.
    fn reveal_selection(&mut self) {
        let Some(selected_index) = self.selected_index else {
            self.first_index = 0;
            return;
        };
        if self.row_tops.is_none() {
            self.first_index =
                reveal_window(selected_index, self.first_index, self.visible_row_count, self.items.len()).first_index;
            return;
        }
        let window = self.window();
        if selected_index < window.first_index {
            self.first_index = selected_index;
            return;
        }
        if selected_index < window.end_exclusive_index {
            self.first_index = window.first_index;
            return;
        }
        let bottom = self.content_top(selected_index.saturating_add(1)) - self.frame.height;
        let Some(tops) = &self.row_tops else {
            return;
        };
        self.first_index = tops.partition_point(|top| *top < bottom).min(selected_index);
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

    /// One row's height while every row is one height: the viewport divided by
    /// the row count the list was *configured* for, never by the number it
    /// happens to have realized. A list holding fewer items than its viewport
    /// therefore draws its rows at their normal height with the rest of the
    /// viewport left empty — dividing by the realized count instead stretched
    /// two items over the whole frame, so a short list rendered as one giant
    /// row.
    ///
    /// `None` once the list keeps an offset table, and for a frame no row can
    /// stand in.
    fn row_height(&self) -> Option<f32> {
        if self.row_tops.is_some() || self.visible_row_count == 0 || !valid_frame(&self.frame) {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let divisor = self.visible_row_count as f32;
        let row_height = self.frame.height / divisor;
        (row_height.is_finite() && row_height > 0.0).then_some(row_height)
    }

    /// The row the point `local_y` lands in, or `None` for a point off the
    /// rows.
    ///
    /// A row's `space_before` gap belongs to the row **under** it — it is that
    /// row's own space, and a press in it is a press aimed at the row it opens
    /// rather than at the one it closed.
    fn row_at_local_y(&self, local_y: f32) -> Option<usize> {
        let window = self.window();
        if !local_y.is_finite() || local_y < 0.0 || local_y >= self.frame.height {
            return None;
        }
        let Some(tops) = &self.row_tops else {
            let row_height = self.row_height()?;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let row_offset = (local_y / row_height).floor() as usize;
            return (row_offset < window.len()).then(|| window.first_index + row_offset);
        };
        let content_y = tops.get(window.first_index)? + local_y;
        let index = tops.partition_point(|top| *top <= content_y).checked_sub(1)?;
        (index >= window.first_index && index < window.end_exclusive_index).then_some(index)
    }

    /// Drop the cached row measurement and the offset table. Called wherever
    /// the items, the font, or the type scale change, which is every input
    /// either of them has.
    fn forget_measurements(&mut self) {
        self.widest_row_width = None;
        self.row_tops_frame = None;
    }

    /// The pitch one row of `role` stands at.
    ///
    /// The theme's `row_height` is the **body** pitch and every other role
    /// scales by its own type step against the body size, so a caption row is
    /// shorter and a heading row taller in exactly the proportion their sizes
    /// differ. One number to tune rather than four, and a role that grows in a
    /// restyled theme takes its rows with it. A theme whose body size is zero
    /// falls back to the pitch itself rather than collapsing every row.
    fn role_row_height(&self, role: TextRole) -> f32 {
        if self.theme.label_size_pixels <= 0.0 {
            return self.theme.row_height.max(0.0);
        }
        (self.theme.row_height * self.theme.text_size_pixels(role) / self.theme.label_size_pixels).max(0.0)
    }

    /// How tall one line of a note is: the caption size by
    /// [`NOTE_LINE_HEIGHT_RATIO`].
    fn note_line_height(&self) -> f32 {
        (self.theme.text_size_pixels(TextRole::Caption) * NOTE_LINE_HEIGHT_RATIO).max(0.0)
    }

    /// How far into the row's text budget a note starts: the row's own indent
    /// and one unit more.
    fn note_indent(&self, row: &VirtualListRow) -> f32 {
        self.theme.space(row.indent.saturating_add(NOTE_INDENT_UNITS))
    }

    /// The note this row has to say. A note of nothing but space is not a
    /// note: it would grow the row by a line that draws nothing.
    fn note_of(row: &VirtualListRow) -> Option<&str> {
        row.note.as_deref().map(str::trim).filter(|note| !note.is_empty())
    }

    /// The width a note wraps to in a row `row_width` wide: the row's text
    /// budget less the note's own indent. A note runs the whole budget,
    /// because the trailing column and the verbs stand on the row's *first*
    /// line and the note is the line under them.
    fn note_budget(&self, row: &VirtualListRow, row_width: f32) -> f32 {
        (text_budget_of(row_width, self.theme.pad) - self.note_indent(row)).max(0.0)
    }

    /// One row's note as the lines it will be drawn on: word-wrapped to
    /// `budget`, capped at [`MAX_NOTE_LINES`], with the last of those carrying
    /// what is left of the sentence and eliding it.
    ///
    /// A row that has a note always gets at least one line — a single word
    /// wider than the budget keeps its own line rather than being broken in
    /// half — and the whole note stands on one line until the font's advances
    /// land, because wrapping against a guess and again against the metrics
    /// would change every row's height a frame after it drew.
    fn note_lines(&self, row: &VirtualListRow, budget: f32) -> Vec<String> {
        let Some(note) = Self::note_of(row) else {
            return Vec::new();
        };
        let size = self.theme.text_size_pixels(TextRole::Caption);
        let Some(metrics) = self.font_metrics.resolved() else {
            return alloc::vec![String::from(note)];
        };
        let mut lines = wrap_to_width(note, budget, |run| measured_text_width(metrics, run, size));
        if lines.len() > MAX_NOTE_LINES {
            let mut rest = String::new();
            for line in lines.split_off(MAX_NOTE_LINES - 1) {
                if !rest.is_empty() {
                    rest.push(' ');
                }
                rest.push_str(&line);
            }
            lines.push(self.fitted_text(&rest, size, budget));
        }
        lines
    }

    /// One row's whole height in a row `row_width` wide: the ground above it,
    /// its role's pitch, and a line for each line of its note.
    fn item_height(&self, row: &VirtualListRow, row_width: f32) -> f32 {
        #[allow(clippy::cast_precision_loss)] // a note is at most MAX_NOTE_LINES lines
        let note_lines = self.note_lines(row, self.note_budget(row, row_width)).len() as f32;
        note_lines.mul_add(self.note_line_height(), self.theme.space(row.space_before) + self.role_row_height(row.role))
    }

    /// The offset table for the whole vector at `row_width`: `items.len() + 1`
    /// running sums, the last of which is the content height.
    fn build_row_tops(&self, row_width: f32) -> Vec<f32> {
        let mut tops = Vec::with_capacity(self.items.len().saturating_add(1));
        let mut top = 0.0;
        tops.push(top);
        for row in &self.items {
            top += self.item_height(row, row_width);
            tops.push(top);
        }
        tops
    }

    /// Rebuild the offset table when the frame it was built for is not the
    /// frame the list has now, and keep no table at all for a vector no row of
    /// which asks for a height of its own — the fixed-pitch fast path, where
    /// the geometry is one multiply and the list costs exactly what it always
    /// did.
    ///
    /// Called from every handler that goes on to consult the geometry, because
    /// the heights are a function of the frame and the frame arrives through a
    /// shared handler that knows nothing about rows.
    ///
    /// The gutter the scroll bar takes is itself a function of the heights, so
    /// the table is built once at the full frame and again a gutter narrower
    /// when that first pass overflows. Narrowing a row can only wrap a note
    /// onto *more* lines, so content that overflowed still overflows and the
    /// second pass is the last one.
    ///
    /// The **first** table a list builds re-reveals the selection, because the
    /// window it was booted with was picked before any table existed: `init`
    /// has no frame to measure against, so it counts rows at the one pitch,
    /// and a table's rows are not that pitch. Without this a list opened at a
    /// selected row deep in its vector draws a window the selection is not in
    /// and shows no highlight at all until the reader scrolls.
    fn refresh_row_layout(&mut self) {
        // A frame no row can stand in draws nothing either way, and wrapping a
        // note against a width of zero would break it into one line per word.
        if !self.rows_vary || !valid_frame(&self.frame) {
            self.row_tops = None;
            self.row_tops_frame = None;
            return;
        }
        let frame = (self.frame.width, self.frame.height);
        if self.row_tops.is_some() && self.row_tops_frame == Some(frame) {
            return;
        }
        let full_width = self.frame.width.max(0.0);
        let mut tops = self.build_row_tops(full_width);
        if tops.last().copied().unwrap_or(0.0) > self.frame.height {
            let gutter = self.bar_reserve_width();
            tops = self.build_row_tops((full_width - gutter).max(0.0));
        }
        let first_table = self.row_tops.is_none();
        self.row_tops = Some(tops);
        self.row_tops_frame = Some(frame);
        if first_table {
            self.reveal_selection();
        }
    }

    /// The content-space top of one row's slot — the distance from the top of
    /// the whole vector to the top of that row's `space_before` gap. `0.0`
    /// without a table, where content space is counted in rows rather than in
    /// pixels.
    fn content_top(&self, item_index: usize) -> f32 {
        self.row_tops.as_ref().and_then(|tops| tops.get(item_index).copied()).unwrap_or(0.0)
    }

    /// What the scroll bar is drawn from: rows for the fast path — the
    /// vector's length, the configured viewport and `first_index`, exactly the
    /// three counts the bar was drawn from before rows had heights of their
    /// own — and content pixels once the offset table stands.
    #[allow(clippy::cast_precision_loss)] // a row count a reader could scroll cannot lose precision
    fn scroll_extent(&self) -> ScrollExtent {
        let first_index = self.first_index.min(self.max_first_index());
        self.row_tops.as_ref().map_or(
            ScrollExtent {
                offset: first_index as f32,
                viewport: self.visible_row_count as f32,
                content: self.items.len() as f32,
            },
            |tops| ScrollExtent {
                offset: tops.get(first_index).copied().unwrap_or(0.0),
                viewport: self.frame.height,
                content: tops.last().copied().unwrap_or(0.0),
            },
        )
    }

    /// The first row a scroll offset in [`ScrollExtent`]'s own unit means:
    /// that row count rounded on the fast path, and the last row whose top is
    /// at or above the offset once the table stands.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn first_index_at_offset(&self, offset: f32) -> usize {
        let max_first_index = self.max_first_index();
        if !offset.is_finite() || offset <= 0.0 {
            return 0;
        }
        let Some(tops) = &self.row_tops else {
            return (offset.round() as usize).min(max_first_index);
        };
        // The end of the travel is the *last window* rather than the row that
        // happens to start before it: a thumb dragged to the bottom of its
        // track and a wheel rolled past the end both mean "show the end of the
        // content", and rounding those down is the same row-short stop
        // `max_first_index` rounds up out of (gap 41a).
        if offset >= self.last_window_top() {
            return max_first_index;
        }
        tops.partition_point(|top| *top <= offset).saturating_sub(1).min(max_first_index)
    }

    /// The width a row's two columns share: the row they are drawn in, less
    /// one `pad` at each end, so nothing in a row touches either edge of the
    /// space it was given.
    fn text_width_budget(&self) -> f32 {
        text_budget_of(self.row_width(), self.theme.pad)
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

    /// The whole verb block one row carries: every verb, edge to edge. `0.0`
    /// for a row with no verbs.
    ///
    /// Nothing is added between them — the owner's round-12 note 1, "flush
    /// means touching" — so a two-verb block is exactly its two faces wide.
    fn actions_width(&self, row: &VirtualListRow) -> f32 {
        row.actions.iter().map(|action| self.action_width(action)).sum()
    }

    /// What a verb block of `block` pixels takes off the right end of a row's
    /// **text budget**, which is the row less one pad at each end.
    ///
    /// The block ends on the row's own right edge rather than on its right pad,
    /// so the pad the text budget already gave up is pad the block is standing
    /// in: the reserve is the block plus its one gap of clear space, *less*
    /// that pad. `0.0` for a row with no verbs, and never negative — a theme
    /// whose pad is wider than a whole verb block reserves nothing rather than
    /// handing the text more room than the row has.
    fn block_reserve(&self, block: f32) -> f32 {
        match block {
            block if block > 0.0 => (block + self.theme.space(ACTION_BLOCK_GAP_UNITS) - self.theme.pad).max(0.0),
            _ => 0.0,
        }
    }

    /// What one row's own verbs take off its text budget — the intrinsic's
    /// half of [`Self::actions_reserve`], which measures the shared column.
    fn actions_reserve_for(&self, row: &VirtualListRow) -> f32 {
        self.block_reserve(self.actions_width(row))
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
    /// block's reserve, or nothing when no realized row carries a verb.
    fn actions_reserve(&self, window: VisibleRowWindow) -> f32 {
        self.block_reserve(self.actions_column(window))
    }

    /// Where each verb of one row stands. The block is right-aligned against
    /// the **row's own right edge** — not its right pad — and the verbs run
    /// left to right in the order they were written, touching, so the last one
    /// written is the one on the edge: the owner's `[Change gem][x]`, with the
    /// `×` outermost and nothing after it.
    ///
    /// Round 11 read "flush" as one spacing unit between the verbs and the
    /// block sitting on the row's right pad; round-12 note 1 says the pad and
    /// the gaps both go, so a pressable face runs to the row's edge and the
    /// pair reads as one block of verbs rather than two loose controls.
    /// A verb stands on the row's **first line** rather than over its whole
    /// height: a row with a note is two lines of one entry, and a face drawn
    /// down both of them would read as a control over the sentence too.
    fn action_rects(&self, row: &VirtualListRow, bands: RowBands) -> Vec<ActionRect> {
        let mut x = self.row_width() - self.actions_width(row);
        let mut rects = Vec::with_capacity(row.actions.len());
        for action in &row.actions {
            let width = self.action_width(action);
            rects.push(ActionRect { x, y: bands.plate_top, width, height: bands.line_height });
            x += width;
        }
        rects
    }

    /// Where one realized row's parts stand in widget-local pixels, or `None`
    /// for an item outside the realized window.
    fn row_bands(&self, item_index: usize) -> Option<RowBands> {
        let window = self.window();
        let row_offset = item_index.checked_sub(window.first_index).filter(|offset| *offset < window.len())?;
        let Some(tops) = &self.row_tops else {
            let row_height = self.row_height()?;
            #[allow(clippy::cast_precision_loss)] // a realized row offset is at most a viewport's worth
            let slot_top = row_offset as f32 * row_height;
            return Some(RowBands { slot_top, plate_top: slot_top, plate_height: row_height, line_height: row_height });
        };
        let row = self.items.get(item_index)?;
        let slot_top = tops.get(item_index)? - tops.get(window.first_index)?;
        let gap = self.theme.space(row.space_before);
        Some(RowBands {
            slot_top,
            plate_top: slot_top + gap,
            plate_height: (tops.get(item_index + 1)? - tops.get(item_index)? - gap).max(0.0),
            line_height: self.role_row_height(row.role),
        })
    }

    /// The verb under a point, if the point is on one. Consulted *before* the
    /// row fill, so a press on a verb never also selects the row under it.
    fn action_at(&self, local_x: f32, local_y: f32) -> Option<RowActionIndex> {
        let row_index = self.row_at_local_y(local_y)?;
        let row = self.items.get(row_index)?;
        let bands = self.row_bands(row_index)?;
        self.action_rects(row, bands)
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
        bands: RowBands,
    ) {
        for (action_index, (action, rect)) in row.actions.iter().zip(self.action_rects(row, bands)).enumerate() {
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
            if action_index > 0 {
                items.push(quad(rect.x, rect.y, ROW_RULE_THICKNESS, rect.height, self.theme.edge()));
            }
        }
    }

    /// The width `spans` occupy on one line at `size`: each span measured, plus
    /// the theme's word gap between each pair. `0.0` for an empty run and while
    /// the font's advances are still in flight.
    fn spans_width(&self, spans: &[InkedSpan], size: f32) -> f32 {
        let (Some(metrics), Some(pair_count)) = (self.font_metrics.resolved(), spans.len().checked_sub(1)) else {
            return 0.0;
        };
        #[allow(clippy::cast_precision_loss)] // a trailing run is a few words on one line
        let gaps = pair_count as f32 * self.theme.space(TRAILING_SPAN_GAP_UNITS);
        spans.iter().map(|span| measured_text_width(metrics, &span.text, size)).sum::<f32>() + gaps
    }

    /// The prefix of one row's trailing run that fits `budget`: whole spans
    /// dropped off its **end** until what is left fits.
    ///
    /// A span is a word that means something on its own — `Fire`, `21/20` — so
    /// half of one is worse than none of it. The run gives way by dropping tags
    /// off the end rather than by cutting one mid-word, which keeps the
    /// ellipsis on the leading run the only cut mark a row carries.
    ///
    /// The **head span always stays**, even when it alone is wider than the
    /// budget: the column exists for the fact in it, so a row whose one amount
    /// is wider than the row shows the amount and gives the name nothing,
    /// rather than showing an empty column and a full-width name.
    fn fitted_trailing<'row>(&self, row: &'row VirtualListRow, budget: f32) -> &'row [InkedSpan] {
        let size = self.theme.text_size_pixels(row.role);
        let mut spans = row.trailing.as_slice();
        while spans.len() > 1 && self.spans_width(spans, size) > budget {
            spans = &spans[..spans.len() - 1];
        }
        spans
    }

    /// The widest a trailing column may be: the row's text budget less the verb
    /// block. A run of tags wider than the row it is in is not a column, and it
    /// is what gives way rather than the leading run being squeezed to nothing.
    fn trailing_budget(&self, actions_reserve: f32) -> f32 {
        (self.text_width_budget() - actions_reserve).max(0.0)
    }

    /// One row's trailing run once it has been fitted to `budget`, or `0.0` for
    /// a row without one and while the font's advances are still in flight.
    fn trailing_width(&self, row: &VirtualListRow, budget: f32) -> f32 {
        self.spans_width(self.fitted_trailing(row, budget), self.theme.text_size_pixels(row.role))
    }

    /// The trailing column this window's rows share: the widest trailing run
    /// among the rows **on screen**. One column for the realized window rather
    /// than for the whole vector, because the reader compares what they can
    /// see — and a column sized by an off-screen row would leave a visible gap
    /// nothing stands in. `0.0` when no realized row has a trailing run, which
    /// is the ordinary single-column list.
    fn trailing_column(&self, window: VisibleRowWindow, budget: f32) -> f32 {
        self.items[window.first_index..window.end_exclusive_index]
            .iter()
            .map(|row| self.trailing_width(row, budget))
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
        VirtualListConfig::scroll_track_width(&self.theme)
    }

    /// Where the track stands, or `None` for a frame it cannot stand in.
    ///
    /// The list's own bar is capped at half the frame and refused below a
    /// pixel: a frame too narrow to give the track up would swallow the rows
    /// to draw it. A **host-strip** bar is neither, because it takes nothing
    /// from the rows — it stands in the strip past the frame's right edge,
    /// one gutter clear of it, which is ground the host reserved for it
    /// ([`VirtualListConfig::scroll_strip_width`]).
    fn track_column(&self) -> Option<TrackColumn> {
        let width = self.track_width();
        if self.bar_placement == BarPlacement::HostStrip {
            let left = self.frame.width + self.scroll_bar_gap();
            return (width.is_finite() && left.is_finite()).then_some(TrackColumn { left, width });
        }
        let width = width.min(self.frame.width * 0.5);
        (width.is_finite() && width >= 1.0).then_some(TrackColumn { left: self.frame.width - width, width })
    }

    /// The bar this list stands with right now, or `None` when its vector
    /// fits its viewport.
    fn scroll_bar(&self) -> Option<ScrollBar> {
        scroll_bar(&self.frame, self.track_column()?, self.scroll_extent())
    }

    /// The clear space the bar keeps between itself and the rows: the host's
    /// own [`VirtualListConfig::scroll_bar_gap_units`] in theme metrics.
    fn scroll_bar_gap(&self) -> f32 {
        self.theme.space(self.scroll_bar_gap_units)
    }

    /// What a standing bar takes off a row's width: its track plus the
    /// gutter, and **nothing** when the strip is the host's — the whole point
    /// of that flag is that a value's right edge does not move when the
    /// vector starts to overflow.
    fn bar_reserve_width(&self) -> f32 {
        if self.bar_placement == BarPlacement::HostStrip {
            0.0
        } else {
            self.track_width() + self.scroll_bar_gap()
        }
    }

    /// How much of the frame's right end the bar owns. Zero when no bar
    /// stands, so a list that fits its viewport gives its whole frame to its
    /// rows, and zero for a host-strip bar, which never owned any of it.
    fn bar_gutter_width(&self) -> f32 {
        if self.bar_placement == BarPlacement::HostStrip {
            return 0.0;
        }
        self.scroll_bar().map_or(0.0, |bar| bar.width + self.scroll_bar_gap())
    }

    /// Where the **last** window stands in content space: a viewport short of
    /// the content's end, and `0.0` for content that fits. Zero on the fast
    /// path, whose content is counted in rows rather than pixels.
    fn last_window_top(&self) -> f32 {
        self.row_tops.as_ref().map_or(0.0, |tops| tops.last().copied().unwrap_or(0.0) - self.frame.height)
    }

    /// The topmost row the window can start at: the one past which the rest of
    /// the content no longer fills the viewport. A count on the fast path,
    /// where a viewport is a whole number of rows by construction, and the
    /// **first** row whose top clears a viewport of the content's end once the
    /// offset table stands.
    ///
    /// That last window is rounded **up** (the studio's gap 41a, round-17 note
    /// 1 — "on defense extended stats cannot scroll to bottom"). Rounded down
    /// it started on the last row whose top is at or before the content's end,
    /// so unless the frame happened to be an exact prefix sum of the rows the
    /// window stopped short by up to a row's height and the final statistic
    /// hung below the frame's edge with nothing left to roll. Up, the last row
    /// lands inside the frame and the slack — never more than one row — falls
    /// above the window's start, which is what every scrolling view does with
    /// the end of its content.
    fn max_first_index(&self) -> usize {
        let Some(tops) = &self.row_tops else {
            return self.items.len().saturating_sub(self.visible_row_count);
        };
        let last_top = self.last_window_top();
        if !last_top.is_finite() || last_top <= 0.0 {
            return 0;
        }
        tops.partition_point(|top| *top < last_top).min(self.items.len().saturating_sub(1))
    }

    /// Move the window to `first_index`, clamped. Selection is untouched: a
    /// reader scrolling to look at something has not chosen it.
    fn scroll_to(&mut self, first_index: usize) {
        self.first_index = first_index.min(self.max_first_index());
    }

    /// Scroll by content pixels, carrying the remainder too small to move a
    /// row. Positive moves the window down the vector.
    ///
    /// The window starts on a row's own top either way, so the wheel picks the
    /// row the rolled pixels land in and carries what is left into the next
    /// roll. The carry is bounded by the viewport so that a reader who keeps
    /// rolling at either end of the list does not build up a debt they have to
    /// roll back out.
    #[allow(clippy::cast_possible_truncation)] // the row delta is bounded by the wheel's own pixels
    fn scroll_by_pixels(&mut self, pixels: f32) {
        if !pixels.is_finite() {
            return;
        }
        if self.row_tops.is_none() {
            let Some(row_height) = self.row_height() else {
                return;
            };
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
            return;
        }
        let from = self.scroll_extent().offset;
        let carried = self.wheel_residual_pixels + pixels;
        let next = self.first_index_at_offset(from + carried);
        let travelled = self.content_top(next) - from;
        self.wheel_residual_pixels = (carried - travelled).clamp(-self.frame.height, self.frame.height);
        self.scroll_to(next);
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
        self.scroll_to(self.first_index_at_offset(scroll_offset_at(bar, local_y - grab, self.scroll_extent())));
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
        let height = self.viewport_height();
        let gutter = if self.visible_row_count > 0 && self.items.len() > self.visible_row_count {
            self.bar_reserve_width()
        } else {
            0.0
        };
        let width = self.theme.pad.mul_add(2.0, widest) + gutter;
        (width.is_finite() && height.is_finite()).then_some([width, height])
    }

    /// The whole item vector's height in pixels: the offset table's last sum
    /// once the rows have heights of their own, and the pitch the rows are
    /// actually drawn at ([`Self::row_height`]) by the item count while every
    /// row is one height.
    ///
    /// The **drawn** pitch rather than the theme's, because a fixed-pitch list
    /// divides the frame it was given by the row count it was configured for:
    /// in a frame that is not `theme.row_height × visible_row_count` tall the
    /// rows stand taller or shorter than the theme's pitch and the vector
    /// scrolls through the sum of those. A plate sized from the theme number
    /// would be cut short of the rows it is meant to hold, which is the very
    /// gap this reports to close. The theme's pitch is the fallback for a
    /// frame no row can stand in, where there is nothing drawn to disagree
    /// with.
    ///
    /// This is the scroll extent's `content` said in pixels. The extent counts
    /// **rows** on the fixed-pitch path, because the bar is a ratio either
    /// way; a host drawing a container around the list needs the pixels, so
    /// the one number is stated in both units from the same two branches
    /// rather than re-derived on the host's side out of a mirrored row
    /// arithmetic that drifts (the studio's gap 41).
    ///
    /// `None` for a table whose rows the list has not measured yet — the
    /// offset table missing while some row asks for a height of its own, or
    /// the font's advances still in flight, either of which would answer with
    /// a note wrapped onto a line count it is about to change. A plate sized
    /// from that would resize under the reader a frame later.
    fn content_height(&self) -> Option<f32> {
        let Some(tops) = &self.row_tops else {
            let pitch = self.row_height().unwrap_or(self.theme.row_height);
            #[allow(clippy::cast_precision_loss)] // a row count a reader could scroll cannot lose precision
            return (!self.rows_vary).then_some(pitch * self.items.len() as f32);
        };
        self.font_metrics.resolved()?;
        Some(tops.last().copied().unwrap_or(0.0))
    }

    /// The height the viewport asks for: the configured row count at the one
    /// pitch, and the first that many rows' own heights once they have any.
    ///
    /// Measured from the top of the vector rather than from the realized
    /// window, so the height a slot was sized to does not change under the
    /// reader as they scroll a table of tall and short rows.
    fn viewport_height(&self) -> f32 {
        #[allow(clippy::cast_precision_loss)] // a viewport of rows a reader could scroll cannot lose precision
        let uniform = self.theme.row_height * self.visible_row_count as f32;
        self.row_tops
            .as_ref()
            .map_or(uniform, |tops| tops.get(self.visible_row_count).or_else(|| tops.last()).copied().unwrap_or(0.0))
    }

    /// The widest row in the whole item vector, measured once per change to
    /// the items or the font and cached. A row with a trailing run or a verb on
    /// it is as wide as all of its columns and the gaps between them: a slot
    /// sized from this has to hold the whole row, not just its name.
    ///
    /// A row's **note is not measured**. A note is prose, and prose sized to
    /// its own longest line opens a pane at its ceiling on a sentence and
    /// leaves the table it was supposed to size sitting in a column of empty
    /// plate; the note wraps to whatever width the *rows* ask for. A row's
    /// indent is counted, because that is width the name actually needs.
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
                let trailing = match self.spans_width(&row.trailing, size) {
                    run if run > 0.0 => gap + run,
                    _ => 0.0,
                };
                self.theme.space(row.indent)
                    + measured_text_width(metrics, &row.text, size)
                    + trailing
                    + self.actions_reserve_for(row)
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
        self.ink_at(ink, row.role, selected)
    }

    /// [`Self::run_ink`] for a run set at a role of its own rather than at its
    /// row's — which is a row's note, always a caption whatever the name above
    /// it is set at.
    fn ink_at(&self, ink: TextInk, role: TextRole, selected: bool) -> Rgba {
        let base = match ink {
            TextInk::Inherited if selected => self.theme.selection_text,
            ink => self.theme.text_ink(ink, role),
        };
        self.theme.fill(base, self.state.supporting_theme_state(false))
    }

    /// The hairlines standing between the realized rows of a `ruled` list —
    /// `n - 1` of them for `n` rows, each on the row boundary it divides. A
    /// rule under the last row would underline the list rather than separate
    /// anything, and one above the first would be a second top edge.
    fn rule_items(&self, window: VisibleRowWindow, row_width: f32) -> Vec<WidgetDrawItem> {
        if !self.ruled || window.len() < 2 {
            return Vec::new();
        }
        ((window.first_index + 1)..window.end_exclusive_index)
            .filter_map(|item_index| {
                let bands = self.row_bands(item_index)?;
                Some(quad(0.0, bands.slot_top, row_width, ROW_RULE_THICKNESS, self.theme.outline))
            })
            .collect()
    }

    /// The hairline one row draws to open a block: across the row's own text
    /// budget, at the **top of its space** — the rule first, then the ground,
    /// then the row — so a block boundary reads as a line with air under it
    /// rather than as a line stuck to a name. It spans the budget rather than
    /// the whole row so that it starts and ends where the text does, which is
    /// what tells it apart from the frame's own edge.
    fn rule_above_item(&self, row: &VirtualListRow, bands: RowBands) -> Option<WidgetDrawItem> {
        row.rule_above.then(|| {
            let width = self.text_width_budget();
            quad(self.theme.pad, bands.slot_top, width, ROW_RULE_THICKNESS, self.theme.outline)
        })
    }

    /// One row's note, drawn on the lines under its name: caption size, the
    /// muted ink — which follows the row into `selection_text` when it is
    /// chosen, exactly as a caption-role row's own text does — and set in by
    /// the row's indent and one unit more.
    fn push_row_note(&self, items: &mut Vec<WidgetDrawItem>, row: &VirtualListRow, bands: RowBands, selected: bool) {
        let size = self.theme.text_size_pixels(TextRole::Caption);
        let line_height = self.note_line_height();
        let indent = self.note_indent(row);
        for (line_offset, line) in self.note_lines(row, self.note_budget(row, self.row_width())).into_iter().enumerate()
        {
            #[allow(clippy::cast_precision_loss)] // a note is at most MAX_NOTE_LINES lines
            let line_top = line_height.mul_add(line_offset as f32, bands.plate_top + bands.line_height);
            items.push(WidgetDrawItem::Text {
                x: self.theme.pad + indent,
                y: text_origin_y(line_top, line_height, size),
                font_id: self.theme.font_id,
                text: line,
                size_pixels: size,
                color: self.ink_at(TextInk::Inherited, TextRole::Caption, selected),
                clip: None,
            });
        }
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
        if visible_row_count == 0 || !valid_frame(&self.frame) {
            return Vec::new();
        }

        let row_width = self.row_width();
        let actions_reserve = self.actions_reserve(window);
        let trailing_budget = self.trailing_budget(actions_reserve);
        let trailing_column = self.trailing_column(window, trailing_budget);
        let leading_budget = self.leading_width_budget(trailing_column, actions_reserve);
        let mut items = Vec::with_capacity(visible_row_count.saturating_mul(3).saturating_add(8));
        for (row_offset, item) in self.items[window.first_index..window.end_exclusive_index].iter().enumerate() {
            let item_index = window.first_index + row_offset;
            let Some(bands) = self.row_bands(item_index) else {
                continue;
            };
            let selected = self.selected_index == Some(item_index);
            let hovered = self.hovered_row == Some(item_index);
            // The row's space is ground, not a taller plate: the fill starts
            // below it, so the gap between two blocks is the surface showing
            // through rather than one fat row.
            items.extend(self.rule_above_item(item, bands));
            items.push(quad(0.0, bands.plate_top, row_width, bands.plate_height, self.row_fill(selected, hovered)));

            let indent = self.theme.space(item.indent);
            let size = self.theme.text_size_pixels(item.role);
            items.push(WidgetDrawItem::Text {
                x: self.theme.pad + indent,
                y: text_origin_y(bands.plate_top, bands.line_height, size),
                font_id: self.theme.font_id,
                text: self.fitted_text(&item.text, size, (leading_budget - indent).max(0.0)),
                size_pixels: size,
                color: self.run_ink(item.ink, item, selected),
                clip: None,
            });
            self.push_row_note(&mut items, item, bands, selected);
            // The trailing run is set flush against the row's right pad — or
            // against the verb block when one stands there — so every row's
            // second column ends on one edge. Its spans run left to right from
            // there, each in its own ink, one word gap apart.
            if trailing_column > 0.0
                && let Some(metrics) = self.font_metrics.resolved()
            {
                let fitted = self.fitted_trailing(item, trailing_budget);
                let mut x = row_width - self.theme.pad - actions_reserve - self.spans_width(fitted, size);
                for span in fitted {
                    items.push(WidgetDrawItem::Text {
                        x,
                        y: text_origin_y(bands.plate_top, bands.line_height, size),
                        font_id: self.theme.font_id,
                        text: String::from(span.text.as_str()),
                        size_pixels: size,
                        color: self.run_ink(span.ink, item, selected),
                        clip: None,
                    });
                    x += measured_text_width(metrics, &span.text, size) + self.theme.space(TRAILING_SPAN_GAP_UNITS);
                }
            }
            self.push_row_actions(&mut items, item, item_index, bands);
        }
        items.extend(self.rule_items(window, row_width));
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

/// The width the two text columns of a row `row_width` wide share: the row
/// less one `pad` at each end. Free of the widget because the offset table is
/// built at a width the list does not have yet — the gutter it will give up
/// depends on the heights the table is being built to find.
fn text_budget_of(row_width: f32, pad: f32) -> f32 {
    pad.mul_add(-2.0, row_width).max(0.0)
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
            scroll_bar_gap_units: VirtualListConfig::SCROLL_BAR_GAP_UNITS,
            bar_placement: BarPlacement::InsideFrame,
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
            row_tops: None,
            row_tops_frame: None,
            rows_vary: false,
        }
    }

    /// Resolve a widget's metrics against a table whose every glyph advances
    /// half an em, so a row's width is `chars * size / 2` — exact without
    /// depending on a real font file.
    fn install_test_metrics(widget: &mut VirtualListWidget) {
        widget.font_metrics.take_pending_request();
        widget.font_metrics.accept_reply(Some(CachedFontMetrics::new(&FontMetrics {
            units_per_em: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            line_gap: 0.0,
            default_advance: 500.0,
            advances: Vec::new(),
        })));
    }

    /// The same list with those metrics resolved.
    fn measured_list(item_count: usize, visible_row_count: usize) -> VirtualListWidget {
        let mut widget = list(item_count, visible_row_count, 0);
        install_test_metrics(&mut widget);
        widget
    }

    /// A measured list built the way a host's config builds one — through
    /// `init`'s own mapping — in a frame wide enough that a name has somewhere
    /// to be cut. The fixture for anything whose subject is a config *field*:
    /// assigning the widget field the flag maps to would leave that mapping
    /// untested.
    fn config_list(config: VirtualListConfig) -> VirtualListWidget {
        let mut widget = VirtualListWidget::from_config(config);
        widget.frame = WidgetFrame { x: 10.0, y: 20.0, width: 200.0, height: 120.0 };
        install_test_metrics(&mut widget);
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

    /// Every run one list draws, with where its pen starts and the size it is
    /// set at: `(text, x, y, size_pixels)`.
    fn placed_runs(widget: &VirtualListWidget) -> Vec<(String, f32, f32, f32)> {
        widget
            .draw_items()
            .into_iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Text { text, x, y, size_pixels, .. } => Some((text, x, y, size_pixels)),
                WidgetDrawItem::Quad { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
            })
            .collect()
    }

    /// Every quad one list draws: `(x, y, width, height, color)`.
    fn drawn_quads(widget: &VirtualListWidget) -> Vec<(f32, f32, f32, f32, Rgba)> {
        widget
            .draw_items()
            .into_iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Quad { x, y, width, height, color, .. } => Some((x, y, width, height, color)),
                WidgetDrawItem::Text { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
            })
            .collect()
    }

    /// A measured list of table rows, laid out the way a handler leaves it —
    /// the offset table built, the window settled — in a frame wide enough
    /// that a note has somewhere to wrap.
    fn table_list(items: Vec<VirtualListRow>, visible_row_count: usize) -> VirtualListWidget {
        let mut widget = measured_list(items.len().max(1), visible_row_count);
        widget.frame = WidgetFrame { x: 10.0, y: 20.0, width: 200.0, height: 120.0 };
        widget.selected_index = None;
        widget.rows_vary = rows_vary(&items);
        widget.items = items;
        widget.forget_measurements();
        widget.refresh_row_layout();
        widget
    }

    /// The plate one realized row draws — the quad in one of the four row
    /// fills, in row order.
    fn row_plates(widget: &VirtualListWidget) -> Vec<(f32, f32, f32, f32, Rgba)> {
        let fills = [
            widget.row_fill(false, false),
            widget.row_fill(false, true),
            widget.row_fill(true, false),
            widget.row_fill(true, true),
        ];
        drawn_quads(widget).into_iter().filter(|(_, _, _, _, color)| fills.contains(color)).collect()
    }

    /// A row that says `text` and carries `note` under it.
    fn noted(text: &str, note: &str) -> VirtualListRow {
        VirtualListRow::from(text).with_note(note)
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
            VirtualListRow::from("Astral Plate").with_trailing(vec!["21/20".into()]).with_ink(TextInk::RarityLegendary),
            VirtualListRow::from("Iron Ring").with_trailing(vec!["1".into()]),
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
            VirtualListRow::from("a gem name far too long for this narrow list").with_trailing(vec!["21/20".into()]),
            VirtualListRow::from("short").with_trailing(vec!["1".into()]),
        ];
        widget.forget_measurements();

        let size = widget.theme.label_size_pixels;
        let (column, wide_width, narrow_width) = {
            let metrics = widget.font_metrics.resolved().expect("the test table is installed");
            let wide = measured_text_width(metrics, "21/20", size);
            (wide, wide, measured_text_width(metrics, "1", size))
        };
        let budget = widget.trailing_budget(0.0);
        assert!(narrow_width < wide_width, "the two trailing runs are different widths");
        assert_eq!(widget.trailing_column(widget.window(), budget), column, "the widest of them is the column");

        let runs = drawn_runs(&widget);
        assert_eq!(runs.len(), 4, "two rows, two runs each: {runs:?}");
        assert_eq!(runs[1].1, "21/20");
        assert_eq!(runs[3].1, "1", "the narrow amount is drawn whole, not padded and not cut");
        let right_edge = widget.row_width() - widget.theme.pad;
        assert!((runs[1].0 + wide_width - right_edge).abs() < f32::EPSILON, "the wide amount ends at the right pad");
        assert!((runs[3].0 + narrow_width - right_edge).abs() < f32::EPSILON, "and so does the narrow one");

        assert!(runs[0].1.ends_with(ELLIPSIS), "the name gave way: {:?}", runs[0].1);
        let leading = widget.leading_width_budget(column, 0.0);
        assert_eq!(leading, widget.text_width_budget() - column - widget.theme.space(TRAILING_GAP_UNITS));
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        assert!(measured_text_width(metrics, &runs[0].1, size) <= leading, "and stopped clear of the column");
    }

    #[test]
    fn each_trailing_span_keeps_its_own_ink_and_a_named_one_survives_the_chosen_row() {
        // Tripwire: the owner's round-12 note 6 — "spell tags are all the same
        // colour regardless of tag". Two ways to keep that defect. Join the
        // spans into one run before drawing and every tag comes out in the
        // row's single ink, which is the gap itself. Let the chosen row's
        // `selection_text` win over a span that names an ink and the tags go
        // monochrome on the one row the reader is pointing at, which is the row
        // they are asking about. A span that names no ink still follows the
        // row — that is what keeps a column of amounts one ink down its edge.
        let theme = Theme::DEFAULT;
        let mut widget = measured_list(1, 1);
        widget.frame.width = 400.0;
        widget.items = vec![VirtualListRow::from("Fireball").with_trailing(vec![
            InkedSpan::new("Fire", TextInk::HueWarm),
            InkedSpan::new("Cold", TextInk::HueCool),
            InkedSpan::from("lvl 20"),
        ])];
        widget.selected_index = Some(0);
        widget.forget_measurements();

        let runs = row_runs(&widget);
        assert_eq!(runs.len(), 4, "the name and its three spans: {runs:?}");
        assert_eq!(runs[1], (String::from("Fire"), theme.hue_warm));
        assert_eq!(runs[2], (String::from("Cold"), theme.hue_cool));
        assert_eq!(runs[3], (String::from("lvl 20"), theme.selection_text), "an inkless span follows the row");
    }

    #[test]
    fn a_trailing_run_of_spans_sits_one_word_gap_apart_and_right_aligns_as_a_whole() {
        // Tripwire: three layouts that look right on one row and wrong on the
        // next. Right-align every span against the row's pad and they draw on
        // top of each other. Lay the run out from the left of the shared column
        // and a short run floats away from the edge every other row's run ends
        // on. Drop the word gap and `Fire` `Cold` come out as `FireCold`, which
        // is one word and the reason they are spans at all.
        let mut widget = measured_list(1, 1);
        widget.frame.width = 400.0;
        widget.items =
            vec![VirtualListRow::from("Fireball").with_trailing(vec!["Fire".into(), "Cold".into(), "lvl 20".into()])];
        widget.forget_measurements();

        let size = widget.theme.label_size_pixels;
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        let gap = widget.theme.space(TRAILING_SPAN_GAP_UNITS);
        let runs = drawn_runs(&widget);
        assert_eq!(runs.len(), 4, "the name and its three spans: {runs:?}");

        for pair in runs[1..].windows(2) {
            let advance = measured_text_width(metrics, &pair[0].1, size) + gap;
            assert!((pair[1].0 - (pair[0].0 + advance)).abs() < 1e-3, "the spans are not one word gap apart: {runs:?}");
        }
        let (last_x, last) = runs[3].clone();
        assert!(
            (last_x + measured_text_width(metrics, &last, size) - (widget.row_width() - widget.theme.pad)).abs() < 1e-3,
            "the run as a whole does not end on the row's right pad: {runs:?}",
        );
    }

    #[test]
    fn a_trailing_run_too_wide_for_its_row_drops_whole_spans_off_its_end() {
        // Tripwire: elided the way the leading run is, a run of tags answers
        // `Fire Cold Light…` — half a tag, which names nothing — and drawn
        // unfitted it runs out under the name and off the row's own edge. A
        // span is a word that means something on its own, so the run gives way
        // by whole spans from its end and the leading run's ellipsis stays the
        // one cut mark a row carries.
        let mut widget = measured_list(1, 1);
        widget.items = vec![VirtualListRow::from("Fireball").with_trailing(vec![
            "Fire".into(),
            "Cold".into(),
            "Lightning".into(),
            "Chaos".into(),
        ])];
        widget.forget_measurements();

        let budget = widget.trailing_budget(0.0);
        let fitted = widget.fitted_trailing(&widget.items[0], budget);
        assert!(!fitted.is_empty(), "the head tag stays whatever the budget is");
        assert!(fitted.len() < 4, "this row is too narrow for the whole run: {fitted:?}");
        assert_eq!(
            fitted,
            &widget.items[0].trailing[..fitted.len()],
            "what is left is the run's own head, in order and whole",
        );
        assert!(
            widget.spans_width(fitted, widget.theme.label_size_pixels) <= budget,
            "and it fits the column it was fitted to",
        );

        let runs = drawn_runs(&widget);
        assert_eq!(runs.len(), fitted.len() + 1, "the dropped tags are not drawn at all: {runs:?}");
        assert!(runs[1..].iter().all(|(_, run)| !run.contains(ELLIPSIS)), "no tag was cut mid-word: {runs:?}");
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
        let item_index = widget.window().first_index + row_offset;
        let bands = widget.row_bands(item_index).expect("a realized row stands somewhere");
        widget.action_rects(&widget.items[item_index], bands)
    }

    #[test]
    fn every_row_ends_its_verbs_flush_with_each_other_and_with_the_rows_own_edge() {
        // Tripwire: the owner's round-12 note 1 — "the buttons for the skill
        // remove and skill change aren't flush with each other (touching), and
        // the 'x' button isn't touching the end of the entry". Round 11 read
        // "flush" as one spacing unit between the verbs with the block on the
        // row's right pad, which is what this inverts: nothing between one face
        // and the next, and the last face on the row's own right edge. Three
        // further ways to lose it. Left-align a row's block inside the *shared*
        // column and every row carrying fewer or narrower verbs than the widest
        // one ends short of the edge with a band of slack after it. Right-align
        // against the frame instead of the row and the block slides under the
        // scroll bar's gutter. Keep the pad and the `×` still floats off the
        // end, which is the half of the note the geometry has to answer.
        let mut widget = actioned_list(40, 240.0);
        widget.items[1] = VirtualListRow::from("one verb").with_actions(vec![RowAction::text("Change")]);
        widget.items[2] = VirtualListRow::from("three verbs").with_actions(vec![
            RowAction::text("Change"),
            RowAction::text("Copy"),
            RowAction::danger("x"),
        ]);
        widget.items[3] = VirtualListRow::from("no verbs at all");
        widget.forget_measurements();

        let right_edge = widget.row_width();
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
                    (pair[1].x - (pair[0].x + pair[0].width)).abs() < 1e-3,
                    "row {row_offset} holds its verbs apart instead of flush: {rects:?}",
                );
            }
        }
    }

    #[test]
    fn touching_verbs_are_told_apart_by_one_hairline_on_each_boundary_inside_the_block() {
        // Tripwire: round-12 note 1 makes the faces touch, and the rank a row
        // verb takes is `ButtonEmphasis::Text` — a label and no face at all —
        // so two touching verbs are two labels with nothing between them, which
        // reads as one word. The hairline is what tells them apart. One per
        // boundary *inside* the block and none on its outer edges: a rule at
        // the row's own right edge is a second border, and one before the first
        // verb is a column rule nobody asked for.
        let mut widget = actioned_list(1, 300.0);
        widget.items[0] = VirtualListRow::from("Spark").with_actions(vec![
            RowAction::text("Change"),
            RowAction::text("Copy"),
            RowAction::danger("x"),
        ]);
        widget.forget_measurements();

        let rects = realized_action_rects(&widget, 0);
        let hairlines: Vec<f32> = widget
            .draw_items()
            .into_iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Quad { x, width, color, .. }
                    if width == ROW_RULE_THICKNESS && color == widget.theme.edge() =>
                {
                    Some(x)
                }
                _ => None,
            })
            .collect();

        assert_eq!(hairlines, vec![rects[1].x, rects[2].x], "one hairline per boundary between two verbs");
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
            (rects[1].x + rects[1].width - widget.row_width()).abs() < f32::EPSILON,
            "the verb written last ends on the row's own right edge: {rects:?}",
        );
        assert_eq!(rects[1].x, rects[0].x + rects[0].width, "and the pair touches");

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
        widget.items[0] = VirtualListRow::from("a skill gem with a name far too long for this row")
            .with_trailing(vec!["21/20".into()])
            .with_actions(vec![RowAction::text("Change"), RowAction::danger("x")]);
        widget.forget_measurements();

        let window = widget.window();
        let reserve = widget.actions_reserve(window);
        assert_eq!(
            reserve,
            widget.actions_column(window) + widget.theme.space(ACTION_BLOCK_GAP_UNITS) - widget.theme.pad,
            "the reserve is the shared block plus one gap of clear space, less the pad the block stands in",
        );

        let runs = drawn_runs(&widget);
        let size = widget.theme.label_size_pixels;
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        let (name_x, name) = runs[0].clone();
        assert!(name.ends_with(ELLIPSIS), "the name gave way to the verbs: {name:?}");
        let budget =
            widget.leading_width_budget(widget.trailing_column(window, widget.trailing_budget(reserve)), reserve);
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
        let window = widget.window();
        assert!(widget.rule_items(window, 100.0).is_empty(), "an unruled list draws none");

        widget.ruled = true;
        let rules = widget.rule_items(window, 100.0);
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

        assert!(
            widget.rule_items(VisibleRowWindow { first_index: 0, end_exclusive_index: 1 }, 100.0).is_empty(),
            "one row has nothing to divide",
        );
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
            widget.scroll_bar_gap(),
            "and the gap between the row and the track is one spacing unit",
        );
    }

    /// A list of long-named rows with an amount in the second column, in a
    /// frame wide enough that a name has somewhere to be cut, at the gutter
    /// the host asked for.
    fn gutter_list(scroll_bar_gap_units: u8) -> VirtualListWidget {
        gutter_config_list(VirtualListConfig { scroll_bar_gap_units, ..VirtualListConfig::default() })
    }

    /// The same fixture from a whole config, so a test whose subject is one of
    /// its flags exercises the config → widget hop rather than the field it
    /// lands in.
    fn gutter_config_list(config: VirtualListConfig) -> VirtualListWidget {
        config_list(VirtualListConfig {
            items: (0..200)
                .map(|index| {
                    VirtualListRow::from(format!("a skill gem with a long name {index}"))
                        .with_trailing(vec!["21/20".into()])
                })
                .collect(),
            visible_row_count: 5,
            ..config
        })
    }

    /// The rightmost pen-plus-advance of anything one list draws.
    fn drawn_right_edge(widget: &VirtualListWidget) -> f32 {
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        placed_runs(widget)
            .into_iter()
            .map(|(text, x, _, size)| x + measured_text_width(metrics, &text, size))
            .fold(0.0_f32, f32::max)
    }

    #[test]
    fn a_wider_gutter_shortens_the_rows_rather_than_letting_them_run_under_the_track() {
        // Tripwire: round-17 note 7 — "the scrollbar is still too close to
        // content to the left side". The gutter is the host's now, and the
        // only thing that makes a wider one real is that the rows are laid,
        // filled *and elided* inside what is left of the frame. A gutter that
        // only moved the track would draw the bar further in and leave the
        // names and the amounts exactly where they were, which is under it.
        let narrow = gutter_list(1);
        let wide = gutter_list(3);

        assert_eq!(
            narrow.row_width() - wide.row_width(),
            wide.theme.space(2),
            "the two extra units come out of the row rather than out of nothing",
        );
        assert!(
            drawn_right_edge(&wide) <= wide.row_width() - wide.theme.pad + 1e-3,
            "every run stops on the row's right pad: {} in a row {} wide",
            drawn_right_edge(&wide),
            wide.row_width(),
        );
        assert!(
            drawn_right_edge(&wide) < drawn_right_edge(&narrow),
            "and the amounts moved left with the gutter rather than staying where they were",
        );

        let leading = |widget: &VirtualListWidget| placed_runs(widget).first().expect("a realized row").0.clone();
        assert!(leading(&wide).ends_with(ELLIPSIS), "a name too long for the narrower row is cut: {}", leading(&wide));
        assert!(
            leading(&wide).chars().count() < leading(&narrow).chars().count(),
            "and cut shorter than the same name in the same frame at one unit of gutter",
        );
    }

    #[test]
    fn the_default_gutter_is_the_two_units_a_control_stands_off_a_plate_edge() {
        // Tripwire: the default is what every host that says nothing gets,
        // and it is the number the owner asked for twice (round-14 note 5,
        // round-17 note 7). A config default that fell back to `u8::default()`
        // would give a silent host no gutter at all — the bar drawn flush
        // against the values, which is the bug the field exists to fix.
        assert_eq!(VirtualListConfig::default().scroll_bar_gap_units, 2);
    }

    #[test]
    fn a_host_strip_bar_stands_past_the_frame_and_takes_nothing_off_the_rows() {
        // Tripwire: the studio's gap 42 and round-16 note 3 — "I feel like
        // the scrollbar should EXTEND the panel slightly to exist and be
        // adjacent". A flag that only moved the track would leave the rows
        // laid, filled and elided against a frame a gutter short, so the
        // values would still step left the moment the vector overflowed —
        // the bug the flag exists to remove, and invisible in a capture
        // where the bar happens to stand in reserved ground anyway.
        //
        // Built from the config the host sets rather than by assigning the
        // placement: the flag's whole plumbing is the hop from
        // `host_scroll_strip` to `bar_placement`, and a test that assigned the
        // placement itself would pass with that hop deleted.
        let widget = gutter_config_list(VirtualListConfig { host_scroll_strip: true, ..VirtualListConfig::default() });
        assert_eq!(widget.bar_placement, BarPlacement::HostStrip, "the host's flag put the bar in the host's strip");

        assert_eq!(widget.row_width(), widget.frame.width, "the rows keep the whole frame");
        assert_eq!(widget.bar_gutter_width(), 0.0, "and give up nothing to a bar standing outside it");
        assert_eq!(
            widget.text_width_budget(),
            widget.theme.pad.mul_add(-2.0, widget.frame.width),
            "so a row's text budget is the frame less its two pads and no more",
        );

        let bar = widget.scroll_bar().expect("a vector past its viewport stands a bar");
        assert_eq!(bar.left, widget.frame.width + widget.scroll_bar_gap(), "the track stands one gutter past the edge");
        assert!(drawn_quads(&widget).iter().any(|(x, _, width, _, _)| {
            (*x - bar.left).abs() < f32::EPSILON && (*width - bar.width).abs() < f32::EPSILON
        }));

        let inside = gutter_list(VirtualListConfig::SCROLL_BAR_GAP_UNITS);
        assert!(
            drawn_right_edge(&widget) > drawn_right_edge(&inside),
            "and the amounts stand where they stand on a list that does not overflow at all",
        );
    }

    #[test]
    fn the_reported_strip_is_the_column_the_bar_actually_draws_in() {
        // Tripwire: a host reserves the column from the config's report,
        // before any draw list exists to measure. A report that disagreed
        // with the draw — a gutter counted twice, a track the host guessed at
        // — would put the bar half in the plate beside it or leave a column
        // of ground nothing stands in.
        let theme = Theme::DEFAULT;
        let config = VirtualListConfig { host_scroll_strip: true, ..VirtualListConfig::default() };
        let widget = gutter_config_list(config.clone());

        let bar = widget.scroll_bar().expect("a vector past its viewport stands a bar");
        assert_eq!(config.scroll_strip_width(&theme), bar.left + bar.width - widget.frame.width);
        assert_eq!(
            VirtualListConfig::default().scroll_strip_width(&theme),
            0.0,
            "a list drawing its own bar asks the host for no column at all",
        );
    }

    #[test]
    fn the_last_row_of_a_table_lands_inside_the_frame_however_the_frame_divides_its_rows() {
        // Tripwire: round-17 note 1, verbatim — "On defense extended stats
        // cannot scroll to bottom" (the studio's gap 41a). The window starts
        // on a row's own top, so the last window has to be the first one that
        // reaches the content's end. Chosen as the *last* row starting at or
        // before that end instead, the window stops short by up to a row and
        // the final statistic hangs below the frame's edge with nothing left
        // to roll — invisible on any frame that happens to be an exact prefix
        // sum of its rows, which a plate capped by a pane's height never is.
        let items = alloc::vec![
            noted("Armour", "the share of a hit of the size this fight expects that this takes off it"),
            VirtualListRow::from("Evasion").with_trailing(vec!["1240".into()]),
            noted("Fire resistance", "the lines come to -60%, with no headroom over the maximum at all"),
            VirtualListRow::from("Stun threshold").with_space_before(3).with_rule_above(),
            noted("Block", "nothing on this build blocks, so the chance is the character's own"),
        ];
        let mut widget = table_list(items, 5);
        let tops = widget.row_tops.clone().expect("a table keeps an offset table");

        // A frame that ends half way down the second row's slot: the content's
        // end lands strictly inside a row rather than on one's top edge.
        let half_row = (tops[2] - tops[1]) * 0.5;
        widget.frame = WidgetFrame { height: tops.last().expect("content") - tops[1] - half_row, ..widget.frame };
        widget.refresh_row_layout();
        let tops = widget.row_tops.clone().expect("a table keeps an offset table");
        let last_top = widget.last_window_top();
        assert!(
            last_top > tops[1] && last_top < tops[2],
            "the frame is not an exact prefix sum of the rows: {last_top} between {} and {}",
            tops[1],
            tops[2],
        );

        widget.scroll_to(usize::MAX);
        let last_index = widget.items.len() - 1;
        assert_eq!(widget.window().end_exclusive_index, widget.items.len(), "the last window realizes the last row");
        let bands = widget.row_bands(last_index).expect("the last row is realized at the end of the vector");
        assert!(
            bands.plate_top + bands.plate_height <= widget.frame.height + 1e-3,
            "the last row's bottom is inside the frame: {} of {}",
            bands.plate_top + bands.plate_height,
            widget.frame.height,
        );

        // And the bar reaches the same place: a thumb dragged to the bottom of
        // its track means the end of the content, not the row that happens to
        // start before it.
        widget.first_index = 0;
        let bar = widget.scroll_bar().expect("a table past its frame stands a bar");
        widget.press_scroll_bar(bar, bar.height);
        assert_eq!(widget.first_index, widget.max_first_index(), "the thumb's own end is the list's end");
    }

    #[test]
    fn a_fixed_pitch_list_still_ends_on_its_last_row_exactly() {
        // Tripwire: the fast path divides the frame by the configured row
        // count, so its viewport is a whole number of rows and its last window
        // is the item count less that — rounding it the way a table's is
        // rounded would scroll one row past the end and draw a blank strip
        // under the last row.
        let mut widget = measured_list(200, 5);
        widget.scroll_to(usize::MAX);
        assert_eq!(widget.first_index, 195);
        assert_eq!(widget.window().end_exclusive_index, 200, "and the window ends on the last item");
        let bands = widget.row_bands(199).expect("the last row is realized");
        assert!(
            (bands.plate_top + bands.plate_height - widget.frame.height).abs() < 1e-3,
            "its bottom is the frame's own bottom: {}",
            bands.plate_top + bands.plate_height,
        );
    }

    #[test]
    fn the_content_height_is_the_extent_the_table_scrolls_through_rather_than_a_pitch_by_rows() {
        // Tripwire: the studio's gap 41. A host draws the plate under a list
        // from this number and the list scrolls through the extent, so the
        // two have to be the one number — a content height re-derived as
        // pitch × rows under-measures every table whose rows carry a note or
        // open a block, and the plate is then cut short of its own last rows
        // while the list happily scrolls to them.
        let items = alloc::vec![
            VirtualListRow::from("Armour").with_trailing(vec!["1240".into()]),
            noted(
                "Physical damage mitigated",
                "the share of a hit of the size this fight expects that the armour value above takes off",
            ),
            VirtualListRow::from("Resistances").with_space_before(3).with_rule_above(),
        ];
        let widget = table_list(items, 5);

        assert_eq!(
            widget.content_height(),
            Some(widget.scroll_extent().content),
            "the plate is drawn to the height the list scrolls through",
        );
        assert!(
            widget.scroll_extent().content > widget.theme.row_height * 3.0,
            "and a table of notes and block gaps stands taller than three rows of pitch",
        );

        let plain = measured_list(200, 5);
        assert_eq!(plain.scroll_extent().content, 200.0, "a fixed-pitch list counts its extent in rows");
        assert_eq!(
            plain.content_height(),
            Some(plain.theme.row_height * 200.0),
            "and reports the same content in the pixels a host draws with",
        );

        // A frame the host did not size to the intrinsic: the five rows are
        // drawn to it, so the vector stands taller than the theme's pitch by
        // rows and a plate drawn to that number would be cut short of them.
        let mut off_pitch = measured_list(20, 5);
        off_pitch.frame.height = 200.0;
        assert_eq!(off_pitch.row_height(), Some(40.0), "a taller frame draws its five rows taller");
        assert_eq!(
            off_pitch.content_height(),
            Some(40.0 * 20.0),
            "and the content height follows the pitch the rows are drawn at, not the theme's",
        );

        let mut unwrapped = list(1, 5, 0);
        unwrapped.items = alloc::vec![noted("Armour", "a sentence long enough to wrap onto a second line")];
        unwrapped.rows_vary = true;
        unwrapped.refresh_row_layout();
        assert_eq!(
            unwrapped.content_height(),
            None,
            "a table whose notes have not been wrapped against real advances says nothing rather than a number it is about to change",
        );
    }

    #[test]
    fn a_table_opens_with_the_row_the_host_selected_realized() {
        // Tripwire: `init` has no frame, so it counts the boot window in rows
        // at the one pitch — the only arithmetic there is before anything is
        // measured. A table's rows are not that pitch: a row carrying a note
        // stands taller, so the window that count picks realizes fewer rows
        // than it counted and the selected row falls out of the bottom of it.
        // The list then opens on a table with no highlight anywhere in it and
        // stays that way until the reader scrolls or the host resends the
        // config.
        let mut widget = config_list(VirtualListConfig {
            items: (0..50).map(|index| noted(&format!("stat {index}"), "a sentence under the statistic")).collect(),
            initial_selected_index: Some(40),
            visible_row_count: 5,
            ..VirtualListConfig::default()
        });
        assert_eq!(widget.first_index, 36, "the boot window is five rows of pitch ending on the selection");

        widget.refresh_row_layout();

        let window = widget.window();
        assert!(
            (window.first_index..window.end_exclusive_index).contains(&40),
            "the selected row stands in the realized window {window:?} rather than below it",
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
                + widget.scroll_bar_gap()
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
        assert_eq!(long.bar_gutter_width(), long.track_width() + long.scroll_bar_gap());
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
        let at = |widget: &VirtualListWidget, bar, thumb_top| {
            widget.first_index_at_offset(scroll_offset_at(bar, thumb_top, widget.scroll_extent()))
        };
        assert_eq!(at(&widget, bar, -10.0), 0, "above the track is the top of the vector");
        assert_eq!(at(&widget, bar, bar.travel() + 10.0), 195, "and below it the end");
        for first_index in [0usize, 1, 40, 97, 194, 195] {
            widget.first_index = first_index;
            let drawn = widget.scroll_bar().expect("bar");
            assert_eq!(at(&widget, drawn, drawn.thumb_top), first_index, "round trip at {first_index}");
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

    /// The note the tests wrap: at the caption size on the half-em metric it
    /// is 39 characters, which is wider than the 180-pixel budget a 200-pixel
    /// row leaves a note, so it breaks once and only once.
    const WRAPPING_NOTE: &str = "armour is a function of the hit it meets";

    #[test]
    fn a_note_is_a_second_line_of_its_row_and_the_row_grows_by_the_lines_it_took() {
        // Tripwire: the studio's gap 38. A note pushed into the vector as a
        // row of its own reads as a statistic whose value failed to draw, and
        // a note drawn on a row whose height still came from
        // `frame.height / visible_row_count` would print over the row beneath
        // it. The row has to *grow*, and the growth has to reach the offset
        // table the next row's top is read from.
        let widget = table_list(alloc::vec![noted("Armour", WRAPPING_NOTE), VirtualListRow::from("Evasion")], 5);

        let lines: Vec<String> = placed_runs(&widget)
            .into_iter()
            .filter(|(_, _, _, size)| (*size - widget.theme.caption_size_pixels).abs() < f32::EPSILON)
            .map(|(text, _, _, _)| text)
            .collect();
        assert_eq!(lines.len(), 2, "the note wrapped onto two lines: {lines:?}");
        assert!(lines[0].starts_with("armour is"), "and it broke between words: {lines:?}");
        assert_eq!(lines.concat().replace(' ', ""), WRAPPING_NOTE.replace(' ', ""), "nothing was cut");

        // Body pitch 24, two caption lines at 12 × 1.3: 24 + 31.2.
        let grown = widget.content_top(1) - widget.content_top(0);
        assert!((grown - 55.2).abs() < 1e-3, "the noted row stands {grown} tall");
        assert!(
            (widget.content_top(2) - widget.content_top(1) - 24.0).abs() < 1e-3,
            "and the row under it is the plain body pitch again",
        );

        let plates = row_plates(&widget);
        assert!((plates[1].1 - grown).abs() < 1e-3, "the next row starts below the note, not over it: {plates:?}");
    }

    #[test]
    fn a_note_past_its_cap_ends_on_an_ellipsis_rather_than_growing_the_row_without_end() {
        // Tripwire: a row is an entry in a table, and prose let to wrap
        // forever turns one entry into a paragraph that pushes every other row
        // off the viewport. The cap is three lines and the third says it was
        // cut, which is what stops a note from silently losing its tail.
        let long = "a monster's spells cannot be evaded and nor can a boss attack the game flashes red before it lands";
        let widget = table_list(alloc::vec![noted("Evasion", long)], 5);

        let lines: Vec<String> = placed_runs(&widget)
            .into_iter()
            .filter(|(_, _, _, size)| (*size - widget.theme.caption_size_pixels).abs() < f32::EPSILON)
            .map(|(text, _, _, _)| text)
            .collect();
        assert_eq!(lines.len(), MAX_NOTE_LINES, "three lines and no more: {lines:?}");
        assert!(lines[MAX_NOTE_LINES - 1].ends_with(ELLIPSIS), "and the last says it was cut: {lines:?}");
    }

    #[test]
    fn a_point_inside_a_tall_row_names_that_row_rather_than_the_one_a_pitch_would_name() {
        // Tripwire: every hit the list answers used to be `local_y / pitch`,
        // which names the right row only while every row is one height. At a
        // 24-pixel pitch the point 40 pixels down is row 1; in this list row 0
        // is 55 pixels tall and 40 is still inside it. A press resolving to
        // the wrong row selects the wrong entry, and the same arithmetic backs
        // the reported hover.
        let mut widget = table_list(alloc::vec![noted("Armour", WRAPPING_NOTE), VirtualListRow::from("Evasion")], 5);
        assert_eq!(widget.row_at_local_y(40.0), Some(0), "40 is inside the noted row");
        assert_eq!(widget.row_at_local_y(60.0), Some(1), "and 60 is past it");

        widget.pointer_local = Some((widget.theme.pad, 40.0));
        assert_eq!(widget.pointer_row(), Some(0), "so the hover the host is told about is the row the pointer is on");
    }

    #[test]
    fn the_pointed_row_is_washed_over_the_whole_height_it_actually_has() {
        // Tripwire: the fill is the only mark that says which row the pointer
        // found, and one drawn at the configured pitch would wash the top 24
        // pixels of a 55-pixel entry and leave its note on the plain plate —
        // a row that reads as half-lit, and a hover rect that disagrees with
        // the row the list just reported.
        let mut widget = table_list(alloc::vec![noted("Armour", WRAPPING_NOTE), VirtualListRow::from("Evasion")], 5);
        widget.hovered_row = Some(0);

        let plates = row_plates(&widget);
        let washed = plates.iter().find(|(_, _, _, _, color)| *color == widget.row_fill(false, true));
        let (x, y, width, height, _) = *washed.expect("the pointed row draws the hover wash");
        assert!((x, y, width) == (0.0, 0.0, widget.row_width()), "the wash covers the row across");
        assert!((height - 55.2).abs() < 1e-3, "and down its whole height, note included: {height}");
    }

    #[test]
    fn the_scroll_extent_is_the_sum_of_the_heights_rather_than_a_count_of_rows() {
        // Tripwire: a bar whose thumb is `visible / item_count` says a list of
        // ten short rows and a list of ten two-line rows are the same length,
        // and its travel then lands the reader nowhere near where they
        // pointed. Once rows have heights of their own the extent is pixels.
        let items: Vec<VirtualListRow> = (0..8)
            .map(|index| {
                if index % 2 == 0 {
                    noted(&format!("row {index}"), WRAPPING_NOTE)
                } else {
                    VirtualListRow::from(format!("row {index}"))
                }
            })
            .collect();
        let widget = table_list(items, 5);

        let extent = widget.scroll_extent();
        assert!((extent.content - (4.0f32.mul_add(55.2, 4.0 * 24.0))).abs() < 1e-2, "{extent:?}");
        assert!((extent.viewport - widget.frame.height).abs() < f32::EPSILON, "the viewport is the frame, in pixels");
        assert!(widget.scroll_bar().is_some(), "and content taller than the frame stands a bar");
    }

    #[test]
    fn a_vector_that_asks_for_no_height_of_its_own_keeps_the_pitch_the_frame_divides_into() {
        // Tripwire: the fast path. Every list written before rows had heights
        // draws at `frame.height / visible_row_count` with no table kept, and
        // a change that walked heights for all of them would both re-lay every
        // existing list and spend one `f32` per item on vectors that never
        // asked for it. This pins the geometry and the absence of the table.
        let mut widget = measured_list(9, 5);
        widget.refresh_row_layout();
        assert!(widget.row_tops.is_none(), "a plain vector keeps no offset table");

        let pitch = widget.row_height().expect("a laid-out plain list has one pitch");
        assert!((pitch - widget.frame.height / 5.0).abs() < f32::EPSILON, "which is the frame divided by the count");
        for (offset, plate) in row_plates(&widget).into_iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let expected_y = offset as f32 * pitch;
            assert!((plate.1 - expected_y).abs() < f32::EPSILON, "row {offset} stands at {}", plate.1);
            assert!((plate.3 - pitch).abs() < f32::EPSILON, "row {offset} is one pitch tall");
        }

        let extent = widget.scroll_extent();
        assert_eq!(
            (extent.offset, extent.viewport, extent.content),
            (0.0, 5.0, 9.0),
            "and the bar is still drawn from the three counts",
        );
    }

    #[test]
    fn a_rule_above_stands_at_the_top_of_the_rows_space_with_the_gap_under_it() {
        // Tripwire: `designing-a-screen.md` §4 puts whitespace before a rule,
        // so a block reads as a line and then air and then its heading. A rule
        // drawn against the heading instead — under the gap rather than over
        // it — belongs to the block it just closed, which is the boundary read
        // backwards. It spans the text budget, not the frame, so it is not
        // mistaken for the list's own edge.
        let heading = VirtualListRow::from("Resistances").with_space_before(3).with_rule_above();
        let widget = table_list(alloc::vec![VirtualListRow::from("Armour"), heading], 5);

        let rules: Vec<(f32, f32, f32, f32, Rgba)> =
            drawn_quads(&widget).into_iter().filter(|quad| quad.4 == widget.theme.outline).collect();
        assert_eq!(rules.len(), 1, "one rule, from the one row that asked for it: {rules:?}");
        assert_eq!(
            (rules[0].0, rules[0].1, rules[0].2, rules[0].3),
            (widget.theme.pad, 24.0, widget.text_width_budget(), ROW_RULE_THICKNESS),
            "the rule opens the block at the top of its space, across the text budget",
        );

        let plates = row_plates(&widget);
        assert!(
            (plates[1].1 - (24.0 + widget.theme.space(3))).abs() < 1e-3,
            "and the plate starts below the space, which is ground rather than a taller row: {plates:?}",
        );
    }

    #[test]
    fn an_indent_moves_the_name_and_its_note_and_leaves_the_value_where_it_was() {
        // Tripwire: the studio's gap 39. An indented row is a fact hanging off
        // the one above it, and the signal is the left edge — but a value
        // right-aligns on one column whatever rung its name sits on, because
        // that column is what a reader compares two figures down. Moving the
        // trailing run with the name would ruin the one alignment the table
        // has, and faking the indent with spaces would put it in the text.
        let derived = VirtualListRow::from("Physical damage mitigated")
            .with_indent(2)
            .with_note(WRAPPING_NOTE)
            .with_trailing(alloc::vec!["0%".into()]);
        let widget = table_list(alloc::vec![derived], 5);
        let plain = table_list(alloc::vec![VirtualListRow::from("Armour").with_trailing(alloc::vec!["0%".into()])], 5);

        let runs = placed_runs(&widget);
        let name = runs.iter().find(|(text, _, _, _)| text.starts_with("Physical")).expect("the name");
        let note = runs.iter().find(|(text, _, _, _)| text.starts_with("armour")).expect("the note");
        assert!((name.1 - (widget.theme.pad + widget.theme.space(2))).abs() < f32::EPSILON, "the name steps in");
        assert!((note.1 - (widget.theme.pad + widget.theme.space(3))).abs() < f32::EPSILON, "the note one unit more");

        let value_x = |widget: &VirtualListWidget| {
            placed_runs(widget).into_iter().find(|(text, _, _, _)| text == "0%").expect("the value").1
        };
        assert!((value_x(&widget) - value_x(&plain)).abs() < f32::EPSILON, "and the value column did not move");
    }
}
