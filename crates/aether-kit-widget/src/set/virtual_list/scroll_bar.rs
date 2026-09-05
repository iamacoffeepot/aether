//! The bar: where its track stands, how long its thumb is, what a drag on it
//! means, and how the pair draws.
//!
//! The bar holds no scroll state of its own. Its length is the visible share
//! of the vector and its position is where the reader is, both derived from
//! the one window the list already had, so a wheel, a drag and a keyboard
//! reveal all move it by moving that window.

use crate::set::quad;
use crate::set::virtual_list::scroll::ScrollSpan;
use crate::set::virtual_list::{VirtualListWidget, valid_frame};
use crate::theme::ThemeState;
use crate::{VirtualListConfig, WidgetDrawItem, WidgetFrame};

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
pub(super) struct ScrollBar {
    /// The track's left edge in widget-local pixels.
    pub(super) left: f32,
    pub(super) width: f32,
    pub(super) height: f32,
    thumb_top: f32,
    thumb_height: f32,
}

impl ScrollBar {
    pub(super) fn contains(self, local_x: f32, local_y: f32) -> bool {
        local_x >= self.left && local_x < self.left + self.width && local_y >= 0.0 && local_y < self.height
    }

    fn thumb_contains(self, local_y: f32) -> bool {
        local_y >= self.thumb_top && local_y < self.thumb_top + self.thumb_height
    }

    /// How far the thumb can travel: the track less the thumb itself. Zero for
    /// a thumb that fills its track, which is a list that does not overflow.
    pub(super) fn travel(self) -> f32 {
        (self.height - self.thumb_height).max(0.0)
    }
}

/// Where a list's scroll bar stands: in a gutter cut out of the list's own
/// frame, or in a strip the host reserved past the frame's right edge
/// ([`VirtualListConfig::host_scroll_strip`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BarPlacement {
    InsideFrame,
    HostStrip,
}

impl BarPlacement {
    pub(super) fn of(host_scroll_strip: bool) -> Self {
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
    pub(super) left: f32,
    pub(super) width: f32,
}

/// The bar a list standing at `span` draws in `track`, or `None` when there
/// is nothing to say: a vector that fits its viewport, or an unlaid-out frame.
fn scroll_bar(frame: &WidgetFrame, track: TrackColumn, span: ScrollSpan) -> Option<ScrollBar> {
    if !valid_frame(frame) || span.viewport <= 0.0 || span.content <= span.viewport {
        return None;
    }
    let width = track.width;
    let height = frame.height;
    let share = span.viewport / span.content;
    let thumb_height = (height * share).max(width * MIN_THUMB_RATIO).min(height);
    let progress = (span.offset / span.travel()).clamp(0.0, 1.0);
    Some(ScrollBar { left: track.left, width, height, thumb_top: progress * (height - thumb_height), thumb_height })
}

/// The scroll offset a thumb whose top stands at `thumb_top` means, in
/// `span`'s own unit — the inverse of the `progress` [`scroll_bar`] draws
/// with, so a drag and the bar it moves cannot disagree about where the reader
/// is.
fn scroll_offset_at(bar: ScrollBar, thumb_top: f32, span: ScrollSpan) -> f32 {
    let travel = bar.travel();
    if travel <= 0.0 || !thumb_top.is_finite() {
        return 0.0;
    }
    (thumb_top / travel).clamp(0.0, 1.0) * span.travel()
}

impl VirtualListWidget {
    /// The track's configured width — a metric, not a measurement, so it
    /// scales with a theme scaled for a dense display.
    pub(super) fn track_width(&self) -> f32 {
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
    pub(super) fn scroll_bar(&self) -> Option<ScrollBar> {
        scroll_bar(&self.frame, self.track_column()?, self.scroll_span())
    }

    /// The clear space the bar keeps between itself and the rows: the host's
    /// own [`VirtualListConfig::scroll_bar_gap_units`] in theme metrics.
    pub(super) fn scroll_bar_gap(&self) -> f32 {
        self.theme.space(self.scroll_bar_gap_units)
    }

    /// What a standing bar takes off a row's width: its track plus the
    /// gutter, and **nothing** when the strip is the host's — the whole point
    /// of that flag is that a value's right edge does not move when the
    /// vector starts to overflow.
    pub(super) fn bar_reserve_width(&self) -> f32 {
        if self.bar_placement == BarPlacement::HostStrip {
            0.0
        } else {
            self.track_width() + self.scroll_bar_gap()
        }
    }

    /// How much of the frame's right end the bar owns. Zero when no bar
    /// stands, so a list that fits its viewport gives its whole frame to its
    /// rows, and zero for a host-strip bar, which never owned any of it.
    pub(super) fn bar_gutter_width(&self) -> f32 {
        if self.bar_placement == BarPlacement::HostStrip {
            return 0.0;
        }
        self.scroll_bar().map_or(0.0, |bar| bar.width + self.scroll_bar_gap())
    }

    /// Take the thumb at `local_y`, from the point on it the pointer grabbed —
    /// or, for a press on the bare track, from its middle, so the press
    /// carries the reader to where they pointed.
    pub(super) fn press_scroll_bar(&mut self, bar: ScrollBar, local_y: f32) {
        self.thumb_grab_pixels = Some(if bar.thumb_contains(local_y) {
            local_y - bar.thumb_top
        } else {
            bar.thumb_height * 0.5
        });
        self.drag_thumb(local_y);
    }

    /// Move the window to wherever a live thumb drag now points.
    pub(super) fn drag_thumb(&mut self, local_y: f32) {
        let (Some(grab), Some(bar)) = (self.thumb_grab_pixels, self.scroll_bar()) else {
            return;
        };
        self.scroll_to(self.first_index_at_offset(scroll_offset_at(bar, local_y - grab, self.scroll_span())));
    }

    /// The bar's own draw: the track in the outline role, the thumb in the
    /// muted-text one so it reads as a mark on the list rather than as a
    /// control to press. Both are existing style roles — a scroll bar is not
    /// a new colour in the theme.
    pub(super) fn scroll_bar_items(&self, bar: ScrollBar) -> [WidgetDrawItem; 2] {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set::ELLIPSIS;
    use crate::set::measured_text_width;
    use crate::set::virtual_list::fixture::{
        drawn_quads, drawn_right_edge, gutter_config_list, gutter_list, list, measured_list, placed_runs,
    };
    use crate::theme::Theme;
    use crate::{VirtualListRow, WidgetDrawItem};
    use alloc::format;

    #[test]
    fn a_row_stops_the_hosts_gutter_short_of_the_bar_standing_beside_it() {
        // Tripwire: round-5 note 8 — "the scrollbar has no padding with the
        // inner content to the left so it just draws over it". A row laid out
        // across the whole frame runs under the track, so the bar prints on
        // top of the row's fill and, for a long enough name, its text. The
        // gutter is the track plus the host's own `scroll_bar_gap_units` —
        // two by default — and both the fill and the elision budget stop at
        // it.
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
            "and the gap between the row and the track is the host's gutter",
        );
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
            widget.first_index_at_offset(scroll_offset_at(bar, thumb_top, widget.scroll_span()))
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
}
