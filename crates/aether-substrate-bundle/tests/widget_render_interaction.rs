//! Widget render + interaction acceptance suite (issue 2674).
//!
//! The widget tier already has two `TestBench` scenarios, and between them they
//! leave the *rendered placement* of the real widgets — driven by real
//! synthetic input — untested. `widget_compositing` pixel-asserts abstract
//! colored quads with no real widget, no font, and no input; `widget_set`
//! drives real input but loads the panel fontless and reads value-up events off
//! the log ring (a proof that committed values *reach* the panel, never *where*
//! anything lands). `widget_text_alignment` renders one shared glyph-origin
//! expression's vertical centering per row.
//!
//! This suite renders the reference `WidgetPanel` with the vendored
//! `RobotoMono.ttf`, drives it through the real `aether.input` fan-out, and
//! asserts per-widget placement + behavior through issue 2673's region-scoped
//! `FrameCheck` verdict path — each check pins its own surface background so the
//! accent fills / near-white glyphs form the lit mask, and reads the reduction
//! in absolute frame coordinates. Every scenario names the distinct
//! placement-or-behavior bug class it catches and does not re-assert what the
//! three sibling scenarios already prove.
//!
//! Skipped when no wgpu adapter is available or the `aether_kit` wasm has not
//! been pre-built (the shared `require_runtime` gate). CI sets
//! `AETHER_REQUIRE_RUNTIME=1` to turn either skip into a hard failure. Rendered
//! output can only be asserted on the GPU path, so this is correctly `TestBench`
//! (`FleetBench` is headless).

// Integration-test skip diagnostic: emit via stderr so `cargo test` surfaces
// "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Pixel-rect layout constants read clearest as float literals inline, and the
// window rects cast small, non-negative float pixel coords to the u32
// `FrameRect` fields.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
// Row-rect geometry reads clearest as plain `top + n * height` sums; the
// `mul_add` rewrite obscures the layout math for no accuracy that matters to a
// pixel region.
#![allow(clippy::suboptimal_flops)]

use std::fs;
use std::path::{Path, PathBuf};

use aether_actor::Addressable;
use aether_capabilities::RenderCapability;
use aether_capabilities::fs::NamespaceRoots;
use aether_capabilities::render::{DrawTexturedQuads, WHITE_TEXTURE_ID};
use aether_capabilities::text::{FontMetricsRequest, FontMetricsResult, FontRef, LoadFont, LoadFontResult};
use aether_data::Kind;
use aether_kinds::keycode::{KEY_BACKSPACE, KEY_ENTER, KEY_RIGHT, KEY_TAB};
use aether_kinds::mouse_button::LEFT;
use aether_kinds::{
    CachedFontMetrics, CaptureFrame, CaptureFrameResult, ClipRect, FrameCheck, FrameCheckResult, FrameRect,
    FrameReduction, FrameVerdict, ImePreedit, Key, LoadComponent, LoadResult, LogTailResult, Modifiers, MouseButton,
    MouseButtonRelease, MouseMove, NamedMail, TextInput, Tick,
};
use aether_kit::{
    ButtonConfig, PanelConfig, SetWidgetState, SliderConfig, Theme, ThemeState, WidgetChildSpec, WidgetControlState,
    WidgetKind, WidgetValidation,
};
use aether_substrate_bundle::test_bench::{
    BenchOp, TestBench,
    test_helpers::{init_save_sandbox, require_runtime},
};

/// Panel origin and stack width (widget-local `(0, 0)` maps to this window
/// point), matching `widget_set` / `widget_text_alignment`.
const PANEL_X: f32 = 10.0;
const PANEL_Y: f32 = 10.0;
const PANEL_WIDTH: f32 = 200.0;

/// Default-theme layout metrics the panel lays its stack out with — mirrored
/// here so a theme-metric change surfaces as a placement-region shift rather
/// than a silently-stale constant (`Theme::DEFAULT`).
const ROW_HEIGHT: f32 = 24.0;
const GAP: f32 = 6.0;
const PAD: f32 = 8.0;
/// Focus/selection ring + border thickness (`push_border` argument at every
/// widget draw site).
const BORDER: f32 = 2.0;

/// Capture surface. Wide enough that the ~192px-tall panel sits with a margin
/// of untouched clear color on the right and bottom (scenario 1's containment
/// sanity scores that margin).
const WINDOW_WIDTH: u32 = 240;
const WINDOW_HEIGHT: u32 = 220;

/// Readback sRGB8 of the theme colors a region scores against (`Theme::DEFAULT`,
/// whose colors are the sRGB hex their source comments name). An interior fill
/// pixel reads back at its source hex, so these pin the background each mask
/// lights up against.
const SURFACE_SRGB: [u8; 3] = [0x19, 0x1b, 0x15];
const SURFACE_RAISED_SRGB: [u8; 3] = [0x20, 0x23, 0x1b];
const ACCENT_SRGB: [u8; 3] = [0xa8, 0xc9, 0x7a];
/// Readback sRGB8 of the render target's clear color (`wgpu::Color` linear
/// `0.05, 0.07, 0.12`, sRGB-encoded at store) — the color outside the panel
/// backdrop, so scenario 1 can assert nothing bled past the stack.
const CLEAR_SRGB: [u8; 3] = [63, 75, 97];

/// Per-channel tolerance partitioning a lit mask from its pinned background.
/// Sits in the ~150+ sRGB gap between accent (`~205`) or glyphs (`~230`) and
/// the dark surfaces (`~40`), yet above interior-fill readback rounding — so
/// the mask is identical across GPU vendors (±1 LSB noise cannot flip it).
const PARTITION_TOLERANCE: u8 = 24;

/// Readback sRGB8 of the un-pressed accent *fill* interior — the linear accent
/// composited over the surface and sRGB-encoded at store reads back a few steps
/// brighter than the source hex (`ACCENT_SRGB`), so scenario 5 pins this
/// measured value rather than the source. Pressing composites a 12%-black
/// overlay in linear space, darkening the fill ~7–12 sRGB per channel off this
/// pin — the pressed mask.
const ACCENT_FILL_SRGB: [u8; 3] = [176, 207, 131];
/// Tolerance for scenario 5's fill-darkening probe: above the un-pressed fill's
/// (essentially zero) self-noise, well below the ~7–12 sRGB press darkening —
/// so the un-pressed fill scores empty and the pressed fill scores full.
const DARKEN_TOLERANCE: u8 = 6;

/// The full trampoline address the loaded panel registers at (ADR-0099 §4).
fn panel_address() -> String {
    format!("aether.component/{}:panel", aether_capabilities::WasmTrampoline::NAMESPACE,)
}

fn child_address(subname: &str) -> String {
    format!("{}/{}:{}", panel_address(), aether_capabilities::WasmTrampoline::NAMESPACE, subname,)
}

/// The bundle's `assets/` dir — where `RobotoMono.ttf` ships, resolved relative
/// to this crate at build time.
fn assets_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets")
}

/// Boot a chassis whose `assets://` points at the bundle assets dir (the TTF)
/// and whose `save://` / `config://` sink into a per-process sandbox tempdir.
fn build_bench() -> TestBench {
    let sandbox = init_save_sandbox("widget-render-interaction");
    let roots = NamespaceRoots { save: sandbox.to_path_buf(), assets: assets_dir(), config: sandbox.to_path_buf() };
    TestBench::builder().size(WINDOW_WIDTH, WINDOW_HEIGHT).namespace_roots(roots).build().expect("boot")
}

/// Deterministically load `RobotoMono.ttf` into the shared `aether.text`
/// registry and return its session-scoped `font_id`. Loading it here — rather
/// than letting the panel's `wire` kick off the load — settles the font before
/// any draw, so no capture races the async fs-read + parse.
fn load_font(bench: &mut TestBench) -> u32 {
    let loaded = bench
        .execute(vec![(
            "font",
            BenchOp::send_and_await(
                "aether.text",
                &LoadFont { namespace: "assets".to_owned(), path: "fonts/RobotoMono.ttf".to_owned() },
            ),
        )])
        .expect("load_font sequence");
    match loaded.reply::<LoadFontResult>("font").expect("decode LoadFontResult") {
        LoadFontResult::Ok { font_id, .. } => font_id,
        LoadFontResult::Err { error, .. } => panic!("load RobotoMono: {error}"),
    }
}

/// Grab `RobotoMono`'s resolved metric table (the same one the field measures
/// against) so the test can compute expected pixel boundaries for pointer
/// placement and caret geometry.
fn load_metrics(bench: &mut TestBench, font_id: u32) -> CachedFontMetrics {
    let got = bench
        .execute(vec![(
            "metrics",
            BenchOp::send_and_await("aether.text", &FontMetricsRequest { font: FontRef::Id(font_id) }),
        )])
        .expect("font_metrics sequence");
    match got.reply::<FontMetricsResult>("metrics").expect("decode FontMetricsResult") {
        FontMetricsResult::Ok { metrics } => CachedFontMetrics::new(&metrics),
        FontMetricsResult::Err { error } => panic!("grab RobotoMono metrics: {error}"),
    }
}

/// Load the reference `WidgetPanel` (export `aether.kit.widget.panel`) under the
/// name `panel`, its stack at `(PANEL_X, PANEL_Y)` `PANEL_WIDTH` wide and its
/// theme pinned to the already-resident `font_id` (empty font path, so the
/// panel does not kick off its own load). Every widget draws text with that
/// font.
fn load_panel(bench: &mut TestBench, wasm: &[u8], font_id: u32) {
    load_panel_with_children(bench, wasm, font_id, Vec::new());
}

fn load_panel_with_children(bench: &mut TestBench, wasm: &[u8], font_id: u32, children: Vec<WidgetChildSpec>) {
    let config = PanelConfig {
        x: PANEL_X,
        y: PANEL_Y,
        width: PANEL_WIDTH,
        font_namespace: String::new(),
        font_path: String::new(),
        theme: Theme { font_id, ..Theme::DEFAULT },
        children,
    };
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some("panel".to_owned()),
                    config: config.encode_into_bytes(),
                    export: Some("aether.kit.widget.panel".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => {
            assert!(name.ends_with(":panel"), "the panel root should register under :panel; got {name}",)
        }
        LoadResult::Err { error } => panic!("load WidgetPanel root: {error}"),
    }
}

/// Load font + panel and warm the render path: the first tick spawns + lays out
/// the widget stack, the second primes the glyph atlas (the text cap lazily
/// creates its atlas texture, whose `create_texture` reply has to round-trip
/// before glyphs rasterize into it), and the `advance` settles that
/// round-trip — so the first real capture draws with glyphs resident.
fn boot_panel(bench: &mut TestBench, wasm: &[u8]) {
    let font_id = load_font(bench);
    load_panel(bench, wasm, font_id);
    warm_panel(bench);
}

fn boot_panel_with_children(bench: &mut TestBench, wasm: &[u8], children: Vec<WidgetChildSpec>) {
    let font_id = load_font(bench);
    load_panel_with_children(bench, wasm, font_id, children);
    warm_panel(bench);
}

fn warm_panel(bench: &mut TestBench) {
    let panel = panel_address();
    bench
        .execute(vec![
            ("spawn", BenchOp::send_mail(&panel, &Tick)),
            ("prime", BenchOp::send_mail(&panel, &Tick)),
            ("settle", BenchOp::advance(2)),
        ])
        .expect("warm-up");
}

/// One synthesized frame tick addressed straight to the panel's mailbox — sent
/// as a capture's `mails` so the panel redraws with its current widget state the
/// same frame the substrate reads back.
fn tick_to_panel() -> NamedMail {
    NamedMail { recipient_name: panel_address(), kind_name: Tick::NAME.to_owned(), payload: Vec::new(), count: 1 }
}

/// A left mouse-button press at `(x, y)`.
fn press(x: f32, y: f32) -> MouseButton {
    MouseButton { button: LEFT, x, y }
}

/// A left mouse-button release at `(x, y)`.
fn release(x: f32, y: f32) -> MouseButtonRelease {
    MouseButtonRelease { button: LEFT, x, y }
}

fn button_child(subname: &str, label: &str, state: WidgetControlState) -> WidgetChildSpec {
    WidgetChildSpec {
        subname: subname.to_owned(),
        kind: WidgetKind::Button,
        origin: [0.0, 0.0],
        clip: None,
        config: ButtonConfig { label: label.to_owned(), theme: Theme::DEFAULT, state }.encode_into_bytes(),
    }
}

fn slider_child(subname: &str, state: WidgetControlState) -> WidgetChildSpec {
    WidgetChildSpec {
        subname: subname.to_owned(),
        kind: WidgetKind::Slider,
        origin: [0.0, 0.0],
        clip: None,
        config: SliderConfig { min: 0.0, max: 1.0, step: 0.1, initial: 0.5, theme: Theme::DEFAULT, state }
            .encode_into_bytes(),
    }
}

fn control_state_children() -> Vec<WidgetChildSpec> {
    let hidden = WidgetControlState { visible: false, ..WidgetControlState::default() };
    let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
    let invalid = WidgetControlState {
        validation: WidgetValidation::Error { message: "outside range".to_owned() },
        ..WidgetControlState::default()
    };
    vec![
        button_child("hidden", "Hidden", hidden),
        button_child("disabled", "Disabled", disabled),
        slider_child("value", invalid),
        button_child("hover", "Hover", WidgetControlState::default()),
    ]
}

fn row_clip(y: f32) -> ClipRect {
    ClipRect { x: PANEL_X, y, width: PANEL_WIDTH, height: ROW_HEIGHT }
}

fn solid_for<'a>(snapshot: &'a [DrawTexturedQuads], clip: &ClipRect) -> &'a DrawTexturedQuads {
    snapshot
        .iter()
        .find(|batch| batch.texture_id == WHITE_TEXTURE_ID && batch.clip.as_ref() == Some(clip))
        .unwrap_or_else(|| panic!("missing solid batch for {clip:?}; snapshot: {snapshot:?}"))
}

fn assert_initial_control_snapshot(snapshot: &[DrawTexturedQuads], slider_y: f32, hover_y: f32) {
    let hidden_clip = row_clip(PANEL_Y);
    assert!(
        snapshot.iter().all(|batch| batch.clip.as_ref() != Some(&hidden_clip)),
        "the hidden first child retains its slot but contributes no solid or glyph batch",
    );

    let disabled_batch = solid_for(snapshot, &row_clip(PANEL_Y + ROW_HEIGHT + GAP));
    assert_eq!(
        disabled_batch.quads[0].tint,
        Theme::DEFAULT.fill(Theme::DEFAULT.accent, ThemeState::Disabled),
        "disabled button fill uses the shared disabled role",
    );

    let value_batch = solid_for(snapshot, &row_clip(slider_y));
    assert_eq!(value_batch.quads.len(), 10, "track + fill + two four-quad outlines");
    assert!(
        value_batch.quads[2..6].iter().all(|quad| quad.tint == Theme::DEFAULT.error),
        "the outer validation ring uses the error role",
    );
    assert!(
        value_batch.quads[6..10].iter().all(|quad| quad.tint == Theme::DEFAULT.accent),
        "the inset focus ring remains visible after validation",
    );
    assert_eq!(value_batch.quads[2].y, slider_y);
    assert_eq!(value_batch.quads[6].y, slider_y + BORDER);

    let hover_batch = solid_for(snapshot, &row_clip(hover_y));
    assert_eq!(
        hover_batch.quads[0].tint,
        Theme::DEFAULT.fill(Theme::DEFAULT.accent, ThemeState::Hover),
        "the final sibling owns hover after lost-before-gained delivery",
    );
}

fn assert_updated_control_snapshot(snapshot: &[DrawTexturedQuads], slider_y: f32, hover_y: f32) {
    assert!(
        !solid_for(snapshot, &row_clip(PANEL_Y)).quads.is_empty(),
        "runtime visible=true restores drawing in the retained first slot",
    );
    assert_eq!(
        solid_for(snapshot, &row_clip(slider_y)).quads[2].tint,
        Theme::DEFAULT.warning,
        "runtime validation changes the outer role without resetting the slider",
    );
    assert_eq!(
        solid_for(snapshot, &row_clip(hover_y)).quads[0].tint,
        Theme::DEFAULT.accent,
        "child→empty hover emits HoverLost and restores the normal fill",
    );
}

fn assert_stationary_hover_survives_focus_traversal(bench: &mut TestBench, panel: &str, hover_y: f32) {
    // Focus is independent from hover: Tab to the hovered button and away
    // again without moving the pointer. The button must remain hovered because
    // only a root-issued HoverLost may clear that fact.
    bench
        .execute(vec![
            ("focus_hovered_button", BenchOp::send_mail(panel, &Key { code: KEY_TAB })),
            ("focus_away_without_motion", BenchOp::send_mail(panel, &Key { code: KEY_TAB })),
            ("capture_stationary_hover", BenchOp::capture_with_mails(vec![tick_to_panel()], Vec::new())),
        ])
        .expect("stationary hover survives focus traversal");
    assert_eq!(
        solid_for(&bench.committed_overlay_snapshot(), &row_clip(hover_y)).quads[0].tint,
        Theme::DEFAULT.fill(Theme::DEFAULT.accent, ThemeState::Hover),
        "Tab focus changes must not clear root-owned hover while the pointer stays still",
    );
}

/// A half-open window band `[min_x, max_x) × [min_y, max_y)` as an inclusive
/// `FrameRect` (the region primitive scores an inclusive extent).
fn rect(min_x: f32, min_y: f32, max_x: f32, max_y: f32) -> FrameRect {
    FrameRect { min_x: min_x as u32, min_y: min_y as u32, max_x: max_x as u32 - 1, max_y: max_y as u32 - 1 }
}

/// A region-scoped check of one reduction over `region`, scored against
/// `background` at `tolerance`.
fn check(reduction: FrameReduction, region: FrameRect, background: [u8; 3], tolerance: u8) -> FrameCheck {
    FrameCheck { reduction, tolerance, background: Some(background), region: Some(region) }
}

/// Capture one frame (redrawing the panel via a `Tick` in `mails`) with the
/// requested region checks and return the settled verdict, in check order.
fn capture(bench: &mut TestBench, checks: Vec<FrameCheck>) -> FrameVerdict {
    let captured = bench
        .execute(vec![(
            "snap",
            BenchOp::send_and_await(
                RenderCapability::NAMESPACE,
                &CaptureFrame { mails: vec![tick_to_panel()], after_mails: Vec::new(), checks, similarity: None },
            ),
        )])
        .expect("capture with region checks");
    match captured.reply::<CaptureFrameResult>("snap").expect("decode CaptureFrameResult") {
        CaptureFrameResult::Ok { verdict, .. } => verdict.expect("checks requested → verdict"),
        CaptureFrameResult::Err { error } => panic!("capture failed: {error}"),
    }
}

/// The `Coverage.fraction` of one result (panics on a variant mismatch).
fn coverage(result: &FrameCheckResult) -> f32 {
    match result {
        FrameCheckResult::Coverage { fraction, .. } => *fraction,
        other => panic!("expected a Coverage result; got {other:?}"),
    }
}

/// The `BoundingBox.rect` of one result — `None` when the mask was empty.
fn bounding_box(result: &FrameCheckResult) -> Option<FrameRect> {
    match result {
        FrameCheckResult::BoundingBox { rect, .. } => *rect,
        other => panic!("expected a BoundingBox result; got {other:?}"),
    }
}

/// Every log message in the panel's ring, oldest first — the value-up
/// observation surface (`widget_set`'s idiom).
fn panel_log_messages(bench: &mut TestBench) -> Vec<String> {
    match bench.log_tail(&panel_address(), None) {
        LogTailResult::Ok { entries, .. } => entries.into_iter().map(|e| e.message).collect(),
        LogTailResult::Err { error } => panic!("log_tail on the panel failed: {error}"),
    }
}

/// The window rect of stack row `index` (0 = label), height `rows` row-heights.
fn row_band(index: usize, rows: f32) -> (f32, f32) {
    // Rows before `index` each contribute a row-height plus a gap; the radio
    // group is the sole multi-row widget but it still occupies one gap after
    // it, so the running top is a simple `sum of (height + gap)`.
    let heights = [1.0, 1.0, 3.0, 1.0, 1.0];
    let mut top = PANEL_Y;
    for h in heights.iter().take(index) {
        top += h * ROW_HEIGHT + GAP;
    }
    (top, top + rows * ROW_HEIGHT)
}

const LABEL_ROW: usize = 0;
const SLIDER_ROW: usize = 1;
const RADIO_ROW: usize = 2;
const TEXT_ROW: usize = 3;
const BUTTON_ROW: usize = 4;

/// One text-bearing row to assert glyph containment over: its human name, the
/// window `x` its glyph mask starts at (past a selected radio row's marker), the
/// window `y` its row frame starts at, and the surface its glyphs light up
/// against.
struct TextRow {
    name: &'static str,
    glyph_left: f32,
    top: f32,
    background: [u8; 3],
}

/// The text-bearing rows scenario 1 scores, in panel order: the fill-free label,
/// the accent-filled button, and the three radio option labels (the slider draws
/// no text in v1 and the text field is empty at rest).
fn text_rows() -> Vec<TextRow> {
    // The selected radio row draws its accent marker at `x ∈ [pad, pad +
    // marker]`; start each radio row's glyph region past it so the marker never
    // joins the glyph mask (`marker = max(row_height / 2, 4) = 12`).
    let radio_marker_end = PANEL_X + PAD + (ROW_HEIGHT * 0.5).max(4.0);
    let (label_top, _) = row_band(LABEL_ROW, 1.0);
    let (radio_top, _) = row_band(RADIO_ROW, 3.0);
    let (button_top, _) = row_band(BUTTON_ROW, 1.0);

    let mut rows = vec![
        TextRow { name: "label", glyph_left: PANEL_X, top: label_top, background: SURFACE_SRGB },
        TextRow { name: "button", glyph_left: PANEL_X, top: button_top, background: ACCENT_SRGB },
    ];
    for i in 0..3usize {
        rows.push(TextRow {
            name: ["radio[0]", "radio[1]", "radio[2]"][i],
            glyph_left: radio_marker_end,
            top: radio_top + i as f32 * ROW_HEIGHT,
            background: SURFACE_SRGB,
        });
    }
    rows
}

/// The row-clamped region already bounds the glyph mask to the row frame, so the
/// honest placement signals are: the glyph *top* sits in the row's upper half (a
/// #2670-class downward sag — ~one full ascent — drops it past center and
/// fails), the mask stays above the row's bottom edge, and the glyph *span*
/// stays within the row's horizontal frame (the text-x off-framing this scenario
/// adds over the vertical-only sibling checks).
fn assert_row_contained(row: &TextRow, result: &FrameCheckResult) {
    let bbox = bounding_box(result).unwrap_or_else(|| panic!("row {} drew no glyphs (empty mask)", row.name));
    let (min_x, min_y, max_x, max_y) = (bbox.min_x as f32, bbox.min_y as f32, bbox.max_x as f32, bbox.max_y as f32);
    let (top, bottom) = (row.top, row.top + ROW_HEIGHT);
    let right = PANEL_X + PANEL_WIDTH;
    eprintln!(
        "row {}: glyph bbox x[{min_x:.0}..{max_x:.0}] y[{min_y:.0}..{max_y:.0}] \
         inside row y[{top:.0}..{bottom:.0}]",
        row.name,
    );
    assert!(
        min_y <= top + ROW_HEIGHT * 0.5,
        "row {} glyphs must start in the row's upper half (top {top:.0}, center {:.0}); \
         the mask began at y={min_y:.0} — the per-widget text-y sag class",
        row.name,
        top + ROW_HEIGHT * 0.5,
    );
    assert!(
        max_y <= bottom,
        "row {} glyphs must stay above the row's bottom edge {bottom:.0}; the mask reached \
         y={max_y:.0}",
        row.name,
    );
    assert!(
        min_x >= row.glyph_left - 1.0 && max_x <= right - (BORDER + 1.0),
        "row {} glyphs must sit inside the row's horizontal frame [{:.0}..{right:.0}]; \
         bbox x was [{min_x:.0}..{max_x:.0}] — the per-widget text-x off-framing class",
        row.name,
        row.glyph_left,
    );
}

/// **Bug class: a per-widget-type text-placement regression.** The screen-space
/// text-origin expression is copied into each widget's own draw site
/// (`label.rs`, `radio.rs`, `button.rs`), so a regression in any single copy
/// off-frames only that widget's text — the #2670 sag, but spread across every
/// text-bearing widget rather than the single shared centering `widget_set` /
/// `widget_text_alignment` cover. This scenario asserts the full glyph
/// *bounding box* (x and y) sits inside each owning row for the label, all three
/// radio option labels, and the button, plus a panel-level containment sanity
/// (chrome present inside the backdrop, clear color untouched beside it). The
/// slider draws no text in v1, and the text field is empty at rest, so neither
/// is scored here.
#[test]
fn panel_renders_every_text_row_inside_its_frame() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = build_bench();
    boot_panel(&mut bench, &wasm);

    let rows = text_rows();
    let right = PANEL_X + PANEL_WIDTH;
    let (button_top, _) = row_band(BUTTON_ROW, 1.0);
    let checks: Vec<FrameCheck> = rows
        .iter()
        .map(|r| {
            check(
                FrameReduction::BoundingBox,
                rect(r.glyph_left, r.top, right, r.top + ROW_HEIGHT),
                r.background,
                PARTITION_TOLERANCE,
            )
        })
        // Panel-level sanity: chrome present over the backdrop, and the clear
        // color untouched in the right margin beside it.
        .chain([
            check(
                FrameReduction::Coverage,
                rect(PANEL_X, PANEL_Y, right, button_top + ROW_HEIGHT),
                SURFACE_SRGB,
                PARTITION_TOLERANCE,
            ),
            check(
                FrameReduction::Coverage,
                rect(right + 4.0, PANEL_Y, WINDOW_WIDTH as f32, button_top + ROW_HEIGHT),
                CLEAR_SRGB,
                PARTITION_TOLERANCE,
            ),
        ])
        .collect();

    let verdict = capture(&mut bench, checks);
    assert_eq!(verdict.results.len(), rows.len() + 2, "one result per requested check",);

    for (row, result) in rows.iter().zip(&verdict.results) {
        assert_row_contained(row, result);
    }

    let chrome = coverage(&verdict.results[rows.len()]);
    let outside = coverage(&verdict.results[rows.len() + 1]);
    eprintln!("panel chrome coverage {chrome:.3}, outside-panel coverage {outside:.3}");
    assert!(
        chrome > 0.1 && chrome < 0.98,
        "the panel backdrop should carry chrome (glyphs + accent fills + markers) \
         yet leave surface visible; coverage was {chrome:.3}",
    );
    assert!(
        outside < 0.02,
        "the clear color beside the panel should be untouched; coverage over the \
         right margin was {outside:.3}",
    );
}

/// **Bug class: the slider fill quad drawn at the wrong fraction of the track**
/// (fill-fraction math or track-relative origin wrong) — a render bug
/// `widget_set` cannot see, since it reads only the committed value off the log
/// ring. This scenario drives the same press-move-release drag `widget_set`
/// does (to `x = 160` over the 200px stack → value `191/255 ≈ 0.749`), then
/// region-scopes the accent fill's right edge over the slider track band and
/// asserts the rendered fill fraction ≈ 0.749. It does *not* re-read the
/// committed value (that is `widget_set`'s proof).
#[test]
fn slider_drag_renders_fill_at_track_fraction() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = build_bench();
    boot_panel(&mut bench, &wasm);

    let panel = panel_address();
    bench
        .execute(vec![
            ("drag_press", BenchOp::send_mail(&panel, &press(110.0, 52.0))),
            ("drag_move", BenchOp::send_mail(&panel, &MouseMove { x: 160.0, y: 52.0 })),
            ("drag_release", BenchOp::send_mail(&panel, &release(160.0, 52.0))),
        ])
        .expect("slider drag");

    // The track fills the row width and sits vertically centered at
    // `height * 0.35` tall (`slider.rs on_collect`). Score the fill's right
    // edge over the track band, inset past the left/right focus-ring borders so
    // only the accent fill (not the ring) bounds the mask.
    let (slider_top, _) = row_band(SLIDER_ROW, 1.0);
    let track_height = (ROW_HEIGHT * 0.35).clamp(4.0, ROW_HEIGHT);
    let track_top = slider_top + (ROW_HEIGHT - track_height) * 0.5;
    let region = rect(PANEL_X + BORDER + 1.0, track_top, PANEL_X + PANEL_WIDTH - BORDER, track_top + track_height);
    let verdict =
        capture(&mut bench, vec![check(FrameReduction::BoundingBox, region, SURFACE_RAISED_SRGB, PARTITION_TOLERANCE)]);

    let bbox = bounding_box(&verdict.results[0]).expect("the drag should render a non-empty fill");
    // The fill starts at the track's left (`local x = 0`); its right edge is the
    // fraction of the 200px track that the committed value covers.
    let fill_right = bbox.max_x as f32 + 1.0;
    let fraction = (fill_right - PANEL_X) / PANEL_WIDTH;
    let expected = 191.0 / 255.0;
    eprintln!("slider fill right edge x={fill_right:.0} → fraction {fraction:.3} (expected {expected:.3})",);
    assert!(
        (fraction - expected).abs() <= 0.05,
        "the rendered slider fill should reach {expected:.3} of the track; it reached \
         {fraction:.3} (right edge x={fill_right:.0}) — the fill-fraction render class",
    );
}

/// **Bug class: the selected radio marker quad rendered in the wrong option
/// row** (marker-row mapping wrong) — the marker not following the selection.
/// `widget_set` proves the selection *index* reaches the panel (log ring
/// `index=2`); this asserts the accent marker is *drawn* in that row and absent
/// from the others. It does not re-read the index.
#[test]
fn radio_click_moves_marker_into_clicked_row() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = build_bench();
    boot_panel(&mut bench, &wasm);

    // The panel seeds the radio at index 0; click the third option row (y in
    // [118, 142)) to move the selection to index 2.
    let panel = panel_address();
    bench
        .execute(vec![
            ("radio_press", BenchOp::send_mail(&panel, &press(30.0, 125.0))),
            ("radio_release", BenchOp::send_mail(&panel, &release(30.0, 125.0))),
        ])
        .expect("radio click");

    // The marker is a `marker × marker` accent quad at local `x ∈ [pad, pad +
    // marker]`, vertically centered in each option row. Score it per row against
    // the raised surface an *un*selected marker paints, so only the selected
    // (accent) marker lights up.
    let (radio_top, _) = row_band(RADIO_ROW, 3.0);
    let marker = (ROW_HEIGHT * 0.5).max(4.0);
    let marker_left = PANEL_X + PAD;
    let checks: Vec<FrameCheck> = (0..3usize)
        .map(|i| {
            let row_top = radio_top + i as f32 * ROW_HEIGHT;
            let marker_top = row_top + (ROW_HEIGHT - marker) * 0.5;
            check(
                FrameReduction::Coverage,
                rect(marker_left, marker_top, marker_left + marker + 1.0, marker_top + marker),
                SURFACE_RAISED_SRGB,
                PARTITION_TOLERANCE,
            )
        })
        .collect();
    let verdict = capture(&mut bench, checks);

    let fractions: Vec<f32> = verdict.results.iter().map(coverage).collect();
    eprintln!("radio marker coverage per row: [0]={:.3} [1]={:.3} [2]={:.3}", fractions[0], fractions[1], fractions[2],);
    assert!(fractions[2] > 0.5, "the accent marker should fill the clicked row 2; coverage was {:.3}", fractions[2],);
    for i in [0usize, 1] {
        assert!(
            fractions[i] < 0.1,
            "row {i} must carry no accent marker after selecting row 2; coverage was {:.3} \
             — the marker-follows-selection class",
            fractions[i],
        );
    }
}

/// **Bug class: an editing key silently neutralized at the widget tier,
/// end-to-end** — the #2671 Backspace regression. #2671's fix is a pure gate
/// unit test upstream and its plan defers the widget-tier round-trip here.
/// `widget_set` types "hi" + Enter with no backspace. This focuses the field,
/// types "hix", captures the glyph mask width, sends Backspace, captures again
/// (the mask must shrink by ≈ one advance), then commits and reads the trimmed
/// string off the log ring — two honest surfaces, the rendered shrink and the
/// committed value.
#[test]
fn text_field_backspace_shrinks_glyphs_and_commits_trimmed() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = build_bench();
    boot_panel(&mut bench, &wasm);

    // The field's glyph region: its interior, inset past the focus-ring border
    // (the field draws a border + caret while focused, both accent; the inset
    // drops the border, and the caret trails the glyphs so it shrinks with
    // them). Scored against the field's raised-surface fill so glyph + caret ink
    // is the mask.
    let (text_top, _) = row_band(TEXT_ROW, 1.0);
    let field_region = rect(
        PANEL_X + BORDER + 1.0,
        text_top + BORDER + 1.0,
        PANEL_X + PANEL_WIDTH - BORDER,
        text_top + ROW_HEIGHT - BORDER,
    );
    let glyph_check =
        || vec![check(FrameReduction::BoundingBox, field_region, SURFACE_RAISED_SRGB, PARTITION_TOLERANCE)];

    let panel = panel_address();
    // Focus the field and type; a follow-up tick + advance rasterizes any new
    // glyph ('x' is unseen) into the atlas before the measuring capture.
    bench
        .execute(vec![
            ("focus", BenchOp::send_mail(&panel, &press(50.0, text_top + 10.0))),
            ("focus_up", BenchOp::send_mail(&panel, &release(50.0, text_top + 10.0))),
            ("type", BenchOp::send_mail(&panel, &TextInput { text: "hix".to_owned() })),
            ("rasterize", BenchOp::send_mail(&panel, &Tick)),
            ("settle", BenchOp::advance(2)),
        ])
        .expect("focus + type");

    let typed = capture(&mut bench, glyph_check());
    let typed_box = bounding_box(&typed.results[0]).expect("typed text should render glyphs");
    let typed_right = typed_box.max_x as f32;

    bench.execute(vec![("backspace", BenchOp::send_mail(&panel, &Key { code: KEY_BACKSPACE }))]).expect("backspace");

    let deleted = capture(&mut bench, glyph_check());
    let deleted_right =
        bounding_box(&deleted.results[0]).expect("the field still shows \"hi\" after one backspace").max_x as f32;

    let shrink = typed_right - deleted_right;
    eprintln!(
        "text glyph right edge: typed x={typed_right:.0} → after backspace x={deleted_right:.0} \
         (shrink {shrink:.0}px)",
    );
    // One RobotoMono advance at 14px is ~8px; assert a real monotone decrease
    // (a full advance minus rasterization slack), robust to the approximate
    // caret width the field lays out with. A neutralized backspace leaves the
    // mask unchanged (shrink ≈ 0) and fails here.
    assert!(
        shrink >= 3.0,
        "one backspace must shrink the field's glyph mask by ≈ one advance; the right \
         edge moved {shrink:.0}px (typed x={typed_right:.0}, after x={deleted_right:.0}) \
         — the neutralized-editing-key class",
    );

    bench.execute(vec![("commit", BenchOp::send_mail(&panel, &Key { code: KEY_ENTER }))]).expect("commit");

    let log = panel_log_messages(&mut bench);
    let joined = log.join("\n");
    assert!(
        log.iter().any(|m| m.contains("widget text committed") && m.contains("text=hi")),
        "after typing \"hix\", one backspace, and Enter the committed string must be \"hi\"; \
         log was:\n{joined}",
    );
}

/// **Bug class: the armed/pressed button render not appearing on press, or the
/// press event never firing.** The button is absent from both `widget_set`'s
/// input session and `widget_compositing`, so this is the only scenario that
/// exercises it. Capture the button fill un-pressed, press within it (holding),
/// capture again — the accent fill reads darker (the `pressed_overlay` darkens
/// accent ~12%) — then release within the rect and read the click off the log
/// ring.
#[test]
fn button_press_renders_pressed_state_and_reports_click() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = build_bench();
    boot_panel(&mut bench, &wasm);

    // Score a glyph-free strip of the button fill (its "Apply" label is
    // left-aligned at `x = pad`, so the right portion is pure accent), inset
    // past the focus ring the press raises. Un-pressed it matches the pinned
    // accent (coverage ≈ 0); pressed it darkens off the pin (coverage ≈ 1).
    let (button_top, _) = row_band(BUTTON_ROW, 1.0);
    let fill_region = rect(
        PANEL_X + PANEL_WIDTH * 0.6,
        button_top + BORDER + 1.0,
        PANEL_X + PANEL_WIDTH - BORDER,
        button_top + ROW_HEIGHT - BORDER,
    );
    let fill_check = || vec![check(FrameReduction::Coverage, fill_region, ACCENT_FILL_SRGB, DARKEN_TOLERANCE)];

    let baseline_cov = coverage(&capture(&mut bench, fill_check()).results[0]);

    let panel = panel_address();
    let button_x = PANEL_X + PANEL_WIDTH * 0.5;
    let button_y = button_top + ROW_HEIGHT * 0.5;
    bench.execute(vec![("press", BenchOp::send_mail(&panel, &press(button_x, button_y)))]).expect("button press");

    let pressed_cov = coverage(&capture(&mut bench, fill_check()).results[0]);
    eprintln!("button fill coverage-off-accent: un-pressed {baseline_cov:.3} → pressed {pressed_cov:.3}",);
    assert!(
        baseline_cov < 0.1,
        "the un-pressed button fill should match its accent color (coverage off-accent ≈ 0); \
         it was {baseline_cov:.3}",
    );
    assert!(
        pressed_cov > 0.7,
        "pressing the button should darken its fill off the accent pin; coverage rose only to \
         {pressed_cov:.3} — the pressed-state-not-rendered class",
    );

    bench.execute(vec![("release", BenchOp::send_mail(&panel, &release(button_x, button_y)))]).expect("button release");

    let log = panel_log_messages(&mut bench);
    let joined = log.join("\n");
    assert!(
        log.iter().any(|m| m.contains("widget button clicked")),
        "a press then release inside the button should fire a click; log was:\n{joined}",
    );
}

/// **Bug class: the focus ring rendered on the wrong widget, or not moving as
/// Tab cycles focus.** `widget_set` proves Tab *moves* focus (Tab + Down routes
/// to the radio); this asserts the accent ring is *drawn* around the focused
/// widget and follows Tab. Tab once (→ slider) and score the ring over the
/// slider's top edge; Tab again (→ radio) and score it present over the radio's
/// top edge and gone from the slider's.
#[test]
fn focus_ring_follows_tab() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = build_bench();
    boot_panel(&mut bench, &wasm);

    // The ring is a 2px accent border; probe the top edge band of a widget's
    // frame, where only the ring can light up (the slider's fill sits lower in
    // the track, the radio's markers lower in their rows), inset past the
    // left/right borders. Scored against the panel surface behind the edge.
    let (slider_top, _) = row_band(SLIDER_ROW, 1.0);
    let (radio_top, _) = row_band(RADIO_ROW, 3.0);
    let top_edge = |top: f32| rect(PANEL_X + BORDER + 1.0, top, PANEL_X + PANEL_WIDTH - BORDER, top + BORDER);
    let slider_edge = || vec![check(FrameReduction::Coverage, top_edge(slider_top), SURFACE_SRGB, PARTITION_TOLERANCE)];

    let panel = panel_address();
    // Tab from no focus lands on the first focusable widget — the slider.
    bench.execute(vec![("tab1", BenchOp::send_mail(&panel, &Key { code: KEY_TAB }))]).expect("first tab");
    let on_slider = coverage(&capture(&mut bench, slider_edge()).results[0]);

    // Tab again advances focus to the radio group.
    bench.execute(vec![("tab2", BenchOp::send_mail(&panel, &Key { code: KEY_TAB }))]).expect("second tab");
    let verdict = capture(
        &mut bench,
        vec![
            check(FrameReduction::Coverage, top_edge(slider_top), SURFACE_SRGB, PARTITION_TOLERANCE),
            check(FrameReduction::Coverage, top_edge(radio_top), SURFACE_SRGB, PARTITION_TOLERANCE),
        ],
    );
    let slider_after = coverage(&verdict.results[0]);
    let radio_after = coverage(&verdict.results[1]);
    eprintln!(
        "focus ring coverage: slider(tab1)={on_slider:.3} → slider(tab2)={slider_after:.3}, \
         radio(tab2)={radio_after:.3}",
    );
    assert!(
        on_slider > 0.5,
        "the first Tab should ring the slider; ring coverage over its top edge was {on_slider:.3}",
    );
    assert!(
        radio_after > 0.5,
        "the second Tab should ring the radio group; ring coverage over its top edge was \
         {radio_after:.3}",
    );
    assert!(
        slider_after < 0.1,
        "the ring must leave the slider when focus advances; coverage over its top edge stayed \
         {slider_after:.3} — the ring-follows-focus class",
    );
}

/// **Bug class: measured selection / composition / caret geometry drifting from
/// the edited UTF-8 value.** The measured click is deliberately chosen where
/// the warm-up approximation resolves to the *next* character, so the final
/// committed value proves the field installed its metric table. Tight positive
/// and neighboring exclusion regions then pin the first selected cell, the full
/// non-collapsed IME cursor span, the preedit underline at the replaced
/// selection (not the old trailing position), and the final measured caret.
#[test]
#[allow(clippy::too_many_lines)] // one cohesive place → select → compose → commit acceptance run
fn text_field_selection_and_ime_render_measured_bands_and_commit() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = build_bench();

    let font_id = load_font(&mut bench);
    let metrics = load_metrics(&mut bench, font_id);
    load_panel(&mut bench, &wasm, font_id);
    let panel = panel_address();
    // Warm up: spawn + prime the atlas, and give the field's own single-flight
    // metrics request the extra ticks it needs to round-trip and install before
    // the measured interactions.
    bench
        .execute(vec![
            ("spawn", BenchOp::send_mail(&panel, &Tick)),
            ("prime", BenchOp::send_mail(&panel, &Tick)),
            ("settle", BenchOp::advance(4)),
        ])
        .expect("warm-up");

    let size = Theme::DEFAULT.value_size_pixels;
    let (text_top, _) = row_band(TEXT_ROW, 1.0);
    let content_x = PANEL_X + PAD;
    let typed_text = "abécd";

    // Focus the field, type a value with a multibyte scalar, and rasterize it.
    bench
        .execute(vec![
            ("focus", BenchOp::send_mail(&panel, &press(50.0, text_top + 10.0))),
            ("focus_up", BenchOp::send_mail(&panel, &release(50.0, text_top + 10.0))),
            ("type", BenchOp::send_mail(&panel, &TextInput { text: typed_text.to_owned() })),
            ("rasterize", BenchOp::send_mail(&panel, &Tick)),
            ("settle", BenchOp::advance(2)),
        ])
        .expect("focus + type");

    // Click after "abé" (char index 3, byte 4). Roboto Mono advances 0.6em,
    // while the bounded warm-up fallback uses 0.5em: at this boundary the
    // fallback rounds to char index 4. Pin that fixture property so this
    // scenario cannot silently stop discriminating exact from approximate hit
    // testing after a font or theme change.
    let measured_boundary = metrics.caret_x(typed_text, 3, size);
    let fallback_advance = (size * 0.5).max(1.0);
    let fallback_index = ((measured_boundary / fallback_advance + 0.5) as usize).min(typed_text.chars().count());
    assert_eq!(fallback_index, 4, "the measured click fixture must differ from the 0.5em fallback");
    let boundary_x = content_x + measured_boundary;
    let after_two_x = content_x + metrics.caret_x(typed_text, 2, size);
    let after_four_x = content_x + metrics.caret_x(typed_text, 4, size);
    let after_five_x = content_x + metrics.caret_x(typed_text, 5, size);
    let mark_top = text_top + PAD + 1.0;
    let mark_bottom = text_top + ROW_HEIGHT - PAD - 1.0;
    let selected_first_cell = rect(boundary_x + 1.0, mark_top, after_four_x - 1.0, mark_bottom);
    let selection_exclusion = rect(after_two_x + 1.0, mark_top, boundary_x - 1.0, mark_bottom);
    let second_selected_cell = rect(after_four_x + 1.0, mark_top, after_five_x - 1.0, mark_bottom);
    let cell_checks = || {
        [selected_first_cell, selection_exclusion, second_selected_cell]
            .into_iter()
            .map(|region| check(FrameReduction::Coverage, region, SURFACE_RAISED_SRGB, PARTITION_TOLERANCE))
            .collect()
    };
    let typed_cells = capture(&mut bench, cell_checks());
    let typed_first = coverage(&typed_cells.results[0]);
    let typed_exclusion = coverage(&typed_cells.results[1]);

    let click_y = text_top + 10.0;
    // Shift-extend two characters right to select "cd". If the click takes the
    // approximate path it starts after `c` and selects only `d`, so the first
    // expected cell and final committed value both fail.
    bench
        .execute(vec![
            ("place", BenchOp::send_mail(&panel, &press(boundary_x, click_y))),
            ("place_up", BenchOp::send_mail(&panel, &release(boundary_x, click_y))),
            ("shift", BenchOp::send_mail(&panel, &Modifiers { shift: true, ..Modifiers::default() })),
            ("extend1", BenchOp::send_mail(&panel, &Key { code: KEY_RIGHT })),
            ("extend2", BenchOp::send_mail(&panel, &Key { code: KEY_RIGHT })),
        ])
        .expect("measured place + Shift-extend");

    let selected_cells = capture(&mut bench, cell_checks());
    let selected_first = coverage(&selected_cells.results[0]);
    let selected_exclusion_coverage = coverage(&selected_cells.results[1]);
    eprintln!(
        "selection first-cell coverage: typed {typed_first:.3} → selected {selected_first:.3}; \
         neighbor {typed_exclusion:.3} → {selected_exclusion_coverage:.3}",
    );
    assert!(
        selected_first > 0.75 && selected_first > typed_first + 0.25,
        "the first measured selection cell (`c`) must be accent-filled; resting coverage was \
         {typed_first:.3}, selected was {selected_first:.3}",
    );
    assert!(
        (selected_exclusion_coverage - typed_exclusion).abs() < 0.08,
        "the preceding `é` cell must remain outside the selection; coverage changed from \
         {typed_exclusion:.3} to {selected_exclusion_coverage:.3}",
    );

    // Compose `üx` over `cd`, with a non-collapsed byte span selecting the
    // first, two-byte `ü`. The cursor-span band must fill only the first preedit
    // cell, while the whole two-cell preedit receives an underline at this
    // measured selection position.
    bench
        .execute(vec![
            (
                "preedit",
                BenchOp::send_mail(
                    &panel,
                    &ImePreedit { text: "üx".to_owned(), cursor_begin: Some(0), cursor_end: Some(2) },
                ),
            ),
            ("rasterize", BenchOp::send_mail(&panel, &Tick)),
            ("settle", BenchOp::advance(2)),
        ])
        .expect("ime preedit");

    let underline_y = text_top + (ROW_HEIGHT - size) * 0.5 + size;
    let preedit_underline = rect(boundary_x + 1.0, underline_y, after_five_x - 1.0, underline_y + 2.0);
    let underline_exclusion =
        rect(after_five_x + 1.0, underline_y, after_five_x + (after_four_x - boundary_x) - 1.0, underline_y + 2.0);
    let composition = capture(
        &mut bench,
        vec![
            check(FrameReduction::Coverage, selected_first_cell, SURFACE_RAISED_SRGB, PARTITION_TOLERANCE),
            check(FrameReduction::Coverage, second_selected_cell, SURFACE_RAISED_SRGB, PARTITION_TOLERANCE),
            check(FrameReduction::Coverage, preedit_underline, SURFACE_RAISED_SRGB, PARTITION_TOLERANCE),
            check(FrameReduction::Coverage, underline_exclusion, SURFACE_RAISED_SRGB, PARTITION_TOLERANCE),
        ],
    );
    let cursor_span = coverage(&composition.results[0]);
    let cursor_exclusion = coverage(&composition.results[1]);
    let underline = coverage(&composition.results[2]);
    let underline_after = coverage(&composition.results[3]);
    eprintln!(
        "composition coverage: cursor-span {cursor_span:.3}, next-cell {cursor_exclusion:.3}, \
         underline {underline:.3}, after-underline {underline_after:.3}",
    );
    assert!(
        cursor_span > 0.75 && cursor_span > cursor_exclusion + 0.25,
        "the full IME cursor selection must fill the first preedit cell only; span coverage was \
         {cursor_span:.3}, neighboring cell was {cursor_exclusion:.3}",
    );
    assert!(
        underline > 0.2 && underline > underline_after + 0.15,
        "the preedit underline must occupy its two measured cells at the replaced selection; \
         coverage there was {underline:.3}, after the preedit was {underline_after:.3}",
    );

    // Commit replacement text (clears the composition and replaces `cd`), then
    // measure the caret at the end of the non-ASCII result `abéZ`.
    bench
        .execute(vec![
            ("commit_text", BenchOp::send_mail(&panel, &TextInput { text: "Z".to_owned() })),
            ("rasterize", BenchOp::send_mail(&panel, &Tick)),
            ("settle", BenchOp::advance(2)),
        ])
        .expect("commit replacement text");

    let final_text = "abéZ";
    let final_caret_x = content_x + metrics.caret_x(final_text, 4, size);
    let final_caret = rect(final_caret_x - 1.0, text_top + PAD, final_caret_x + 2.0, text_top + ROW_HEIGHT - PAD);
    let final_caret_exclusion =
        rect(final_caret_x + 3.0, text_top + PAD, final_caret_x + 6.0, text_top + ROW_HEIGHT - PAD);
    let final_verdict = capture(
        &mut bench,
        vec![
            check(FrameReduction::Coverage, final_caret, SURFACE_RAISED_SRGB, PARTITION_TOLERANCE),
            check(FrameReduction::Coverage, final_caret_exclusion, SURFACE_RAISED_SRGB, PARTITION_TOLERANCE),
        ],
    );
    let caret_coverage = coverage(&final_verdict.results[0]);
    let caret_exclusion = coverage(&final_verdict.results[1]);
    eprintln!(
        "final caret coverage: expected {caret_coverage:.3}, neighboring exclusion \
         {caret_exclusion:.3}",
    );
    assert!(
        caret_coverage > 0.2 && caret_coverage > caret_exclusion + 0.2,
        "the final measured caret must occupy its tight expected band; coverage there was \
         {caret_coverage:.3}, neighboring coverage was {caret_exclusion:.3}",
    );

    bench.execute(vec![("commit", BenchOp::send_mail(&panel, &Key { code: KEY_ENTER }))]).expect("commit");

    let log = panel_log_messages(&mut bench);
    let joined = log.join("\n");
    assert!(
        log.iter().any(|m| m.contains("widget text committed") && m.contains("text=abéZ")),
        "clicking after `abé`, extending over `cd`, and committing `Z` must leave the non-ASCII \
         value `abéZ`; log was:\n{joined}",
    );
}

/// Exact overlay batches prove the shared state contract without raster
/// ambiguity: hidden slots are absent, disabled/hover fills use the expected
/// derived roles, and validation plus focus remain two ordered outlines.
#[test]
fn control_state_drives_exact_overlay_batches_and_runtime_updates() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = build_bench();

    boot_panel_with_children(&mut bench, &wasm, control_state_children());

    let panel = panel_address();
    let slider_y = PANEL_Y + (ROW_HEIGHT + GAP) * 2.0;
    let hover_y = PANEL_Y + (ROW_HEIGHT + GAP) * 3.0;
    bench
        .execute(vec![
            // Focus skips hidden + disabled and lands on the invalid slider.
            ("focus", BenchOp::send_mail(&panel, &Key { code: KEY_TAB })),
            // Exercise sibling→sibling hover before settling on the button.
            (
                "hover_slider",
                BenchOp::send_mail(&panel, &MouseMove { x: PANEL_X + 20.0, y: slider_y + ROW_HEIGHT * 0.5 }),
            ),
            (
                "hover_button",
                BenchOp::send_mail(&panel, &MouseMove { x: PANEL_X + 20.0, y: hover_y + ROW_HEIGHT * 0.5 }),
            ),
            ("capture", BenchOp::capture_with_mails(vec![tick_to_panel()], Vec::new())),
        ])
        .expect("state snapshot");

    assert_initial_control_snapshot(&bench.committed_overlay_snapshot(), slider_y, hover_y);
    assert_stationary_hover_survives_focus_traversal(&mut bench, &panel, hover_y);

    // Moving to empty clears hover; runtime mail reveals the hidden slot and
    // changes the slider's validation role without changing either value.
    let warning = WidgetControlState {
        validation: WidgetValidation::Warning { message: "check value".to_owned() },
        ..WidgetControlState::default()
    };
    bench
        .execute(vec![
            (
                "hover_empty",
                BenchOp::send_mail(&panel, &MouseMove { x: WINDOW_WIDTH as f32 - 2.0, y: WINDOW_HEIGHT as f32 - 2.0 }),
            ),
            (
                "show_hidden",
                BenchOp::send_mail(child_address("hidden"), &SetWidgetState { state: WidgetControlState::default() }),
            ),
            ("warn_value", BenchOp::send_mail(child_address("value"), &SetWidgetState { state: warning })),
            ("capture_updated", BenchOp::capture_with_mails(vec![tick_to_panel()], Vec::new())),
        ])
        .expect("runtime state update snapshot");

    assert_updated_control_snapshot(&bench.committed_overlay_snapshot(), slider_y, hover_y);
}

#[test]
fn resident_label_glyphs_forward_the_exact_parent_row_clip() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = build_bench();
    boot_panel(&mut bench, &wasm);

    bench
        .execute(vec![("snap", BenchOp::capture_with_mails(vec![tick_to_panel()], Vec::new()))])
        .expect("capture resident clipped glyphs");
    let expected = ClipRect { x: PANEL_X, y: PANEL_Y, width: PANEL_WIDTH, height: ROW_HEIGHT };
    let snapshot = bench.committed_overlay_snapshot();
    let label_batches: Vec<_> = snapshot
        .iter()
        .filter(|batch| batch.texture_id != WHITE_TEXTURE_ID && batch.clip.as_ref() == Some(&expected))
        .collect();
    assert!(
        !label_batches.is_empty(),
        "resident label glyphs should retain the panel-derived framebuffer clip; snapshot: {snapshot:?}",
    );
    assert!(
        label_batches.iter().all(|batch| !batch.quads.is_empty()),
        "the matched atlas-backed batches carry resident glyph geometry",
    );
    for quad in label_batches.iter().flat_map(|batch| &batch.quads) {
        assert!(
            quad.x >= expected.x
                && quad.y >= expected.y
                && quad.x + quad.width <= expected.x + expected.width
                && quad.y + quad.height <= expected.y + expected.height,
            "label glyph {quad:?} should lie inside its forwarded row clip {expected:?}",
        );
    }
}
