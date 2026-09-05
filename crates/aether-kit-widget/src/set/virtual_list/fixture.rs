//! The fixtures every submodule's tests are built from: a list at a known
//! frame, a half-em metrics table so a row's width is exact without a font
//! file on disk, and the extractors that read a widget's draw list back as
//! runs, quads and plates.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use aether_kinds::{CachedFontMetrics, FontMetrics};
use aether_math::Rgba;

use crate::set::measured_text_width;
use crate::set::virtual_list::VirtualListWidget;
use crate::set::virtual_list::actions::ActionRect;
use crate::set::virtual_list::rows::rows_vary;
use crate::set::virtual_list::scroll_bar::BarPlacement;
use crate::state::InteractionState;
use crate::text_edit::FontMetricsAdapter;
use crate::theme::Theme;
use crate::{RowAction, VirtualListConfig, VirtualListRow, WidgetControlState, WidgetDrawItem, WidgetFrame};

pub(super) fn list(item_count: usize, visible_row_count: usize, selected_index: usize) -> VirtualListWidget {
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
pub(super) fn measured_list(item_count: usize, visible_row_count: usize) -> VirtualListWidget {
    let mut widget = list(item_count, visible_row_count, 0);
    install_test_metrics(&mut widget);
    widget
}

/// A measured list built the way a host's config builds one — through
/// `init`'s own mapping — in a frame wide enough that a name has somewhere
/// to be cut. The fixture for anything whose subject is a config *field*:
/// assigning the widget field the flag maps to would leave that mapping
/// untested.
pub(super) fn config_list(config: VirtualListConfig) -> VirtualListWidget {
    let mut widget = VirtualListWidget::from_config(config);
    widget.frame = WidgetFrame { x: 10.0, y: 20.0, width: 200.0, height: 120.0 };
    install_test_metrics(&mut widget);
    widget
}

pub(super) fn row_text(widget: &VirtualListWidget) -> Vec<String> {
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
pub(super) fn row_runs(widget: &VirtualListWidget) -> Vec<(String, Rgba)> {
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
pub(super) fn placed_runs(widget: &VirtualListWidget) -> Vec<(String, f32, f32, f32)> {
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
pub(super) fn drawn_quads(widget: &VirtualListWidget) -> Vec<(f32, f32, f32, f32, Rgba)> {
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
pub(super) fn table_list(items: Vec<VirtualListRow>, visible_row_count: usize) -> VirtualListWidget {
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
pub(super) fn row_plates(widget: &VirtualListWidget) -> Vec<(f32, f32, f32, f32, Rgba)> {
    let fills = [
        widget.row_fill(false, false),
        widget.row_fill(false, true),
        widget.row_fill(true, false),
        widget.row_fill(true, true),
    ];
    drawn_quads(widget).into_iter().filter(|(_, _, _, _, color)| fills.contains(color)).collect()
}

/// A row that says `text` and carries `note` under it.
pub(super) fn noted(text: &str, note: &str) -> VirtualListRow {
    VirtualListRow::from(text).with_note(note)
}

/// The drawn runs of a list as `(x, text)`, in draw order — a row's
/// leading run then its trailing one.
pub(super) fn drawn_runs(widget: &VirtualListWidget) -> Vec<(f32, String)> {
    widget
        .draw_items()
        .into_iter()
        .filter_map(|item| match item {
            WidgetDrawItem::Text { x, text, .. } => Some((x, text)),
            WidgetDrawItem::Quad { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
        })
        .collect()
}

/// A measured list whose every row carries the owner's pair of verbs —
/// `[Change] [x]`, the second destructive — on a frame wide enough to hold
/// a name beside them.
pub(super) fn actioned_list(item_count: usize, frame_width: f32) -> VirtualListWidget {
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
pub(super) fn row_middle_y(widget: &VirtualListWidget, row_offset: usize) -> f32 {
    let row_height = widget.row_height().expect("a laid-out list has a row height");
    #[allow(clippy::cast_precision_loss)]
    let top = row_offset as f32 * row_height;
    row_height.mul_add(0.5, top)
}

/// The rects the verbs of the `row_offset`-th realized row stand at.
pub(super) fn realized_action_rects(widget: &VirtualListWidget, row_offset: usize) -> Vec<ActionRect> {
    let item_index = widget.window().first_index + row_offset;
    let bands = widget.row_bands(item_index).expect("a realized row stands somewhere");
    widget.action_rects(&widget.items[item_index], bands)
}

/// A list of long-named rows with an amount in the second column, in a
/// frame wide enough that a name has somewhere to be cut, at the gutter
/// the host asked for.
pub(super) fn gutter_list(scroll_bar_gap_units: u8) -> VirtualListWidget {
    gutter_config_list(VirtualListConfig { scroll_bar_gap_units, ..VirtualListConfig::default() })
}

/// The same fixture from a whole config, so a test whose subject is one of
/// its flags exercises the config → widget hop rather than the field it
/// lands in.
pub(super) fn gutter_config_list(config: VirtualListConfig) -> VirtualListWidget {
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
pub(super) fn drawn_right_edge(widget: &VirtualListWidget) -> f32 {
    let metrics = widget.font_metrics.resolved().expect("the test table is installed");
    placed_runs(widget)
        .into_iter()
        .map(|(text, x, _, size)| x + measured_text_width(metrics, &text, size))
        .fold(0.0_f32, f32::max)
}

/// The note the tests wrap: at the caption size on the half-em metric it
/// is 39 characters, which is wider than the 180-pixel budget a 200-pixel
/// row leaves a note, so it breaks once and only once.
pub(super) const WRAPPING_NOTE: &str = "armour is a function of the hit it meets";
