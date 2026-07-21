//! Widget-text vertical-alignment regression scenario (issue 2670).
//!
//! The reference `WidgetPanel` routes clicks by row frame, so its glyphs must
//! sit inside the row frame their widget owns. A shared per-widget text-origin
//! expression once mis-used the `aether.text` `Screen`-space convention —
//! treating the line-box top as the baseline — so every glyph sagged about one
//! ascent below its row, uniformly, while every quad stayed put. Nothing
//! rendered text under test (`widget_compositing` pixel-asserts quads only;
//! `widget_set` loads the panel with an empty font path and reads the log
//! ring), so the sag shipped.
//!
//! This scenario loads the panel with a real font (`RobotoMono.ttf`), draws it,
//! and captures with region-scoped `FrameCheck` centroids — one per text row.
//! Each check pins the row's own fill color as the background so only glyph ink
//! lights up, then asserts the in-row glyph centroid sits near the row's
//! vertical center. The centroid over the *exact* row rect is the clean catch:
//! an ascent-sized sag drags the in-row centroid down (or clips it against the
//! row's bottom edge) far past a quarter-row tolerance, while the corrected
//! origin lands it near center. Asserted rows: the fill-free label (keystone),
//! the accent-filled button (a filled row), and the three radio option rows.
//! The slider draws no text; the text field is empty at rest (no glyphs to
//! score) and is left to `widget_set`.
//!
//! Skipped when no wgpu adapter is available or the `aether_kit` wasm has not
//! been pre-built (the shared `require_runtime` gate). CI sets
//! `AETHER_REQUIRE_RUNTIME=1` to turn either skip into a hard failure.

// Integration-test skip diagnostic: emit via stderr so `cargo test` surfaces
// "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Pixel-rect layout constants read clearest as float literals inline, and the
// window rects cast small, non-negative float pixel coords to the u32
// `FrameRect` fields.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
// Row-rect geometry reads clearest as plain `top + n * height` sums; the
// `mul_add` rewrite obscures the layout math for no accuracy that matters to
// a pixel region.
#![allow(clippy::suboptimal_flops)]

use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use std::fs;
use std::path::{Path, PathBuf};

use aether_actor::Addressable;
use aether_data::Kind;
use aether_fs::NamespaceRoots;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::test_helpers::{init_save_sandbox, require_runtime};
use aether_kinds::{
    CaptureFrame, CaptureFrameResult, FrameCheck, FrameCheckResult, FrameRect, FrameReduction, LoadComponent,
    LoadResult, NamedMail, Tick,
};
use aether_kit::{PanelConfig, Theme};
use aether_render::RenderCapability;
use aether_text::{LoadFont, LoadFontResult, TextCapability};

/// Panel origin and stack width (widget-local `(0, 0)` maps to this window
/// point). Chosen so the whole stack fits the capture surface with margin.
const PANEL_X: f32 = 10.0;
const PANEL_Y: f32 = 10.0;
const PANEL_WIDTH: f32 = 200.0;

/// Default-theme layout metrics the panel lays its stack out with — mirrored
/// here so the test computes each row's window rect (`Theme::DEFAULT`).
const ROW_HEIGHT: f32 = 24.0;
const GAP: f32 = 6.0;
/// Radio marker inset: `pad` + `max(row_height * 0.5, 4)`, the local x the
/// marker quad ends by. Region scoping starts past it so the selected row's
/// accent marker never pollutes the glyph mask.
const RADIO_TEXT_INSET: f32 = 24.0;

/// Per-channel tolerance partitioning lit glyph ink from the pinned row
/// background. Far below every glyph-vs-fill gap here (text ink and each fill
/// differ by ~140+ on some channel) yet above interior-fill render rounding.
const LIT_TOLERANCE: u8 = 24;

/// Readback sRGB8 of the three row fills a text row can sit over
/// (`Theme::DEFAULT`, whose colors are the sRGB hex the comments name). An
/// interior fill pixel reads back at its source hex, so these pin the
/// background each row's glyphs light up against.
const SURFACE_SRGB: [u8; 3] = [0x19, 0x1b, 0x15];
const ACCENT_SRGB: [u8; 3] = [0xa8, 0xc9, 0x7a];

/// The full trampoline address the loaded panel registers at (ADR-0099 §4).
fn panel_address() -> String {
    format!("aether.component/{}:panel", aether_component::WasmTrampoline::NAMESPACE)
}

/// The kit's `assets/` dir — where `RobotoMono.ttf` ships, resolved
/// relative to this crate at build time.
fn assets_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets")
}

/// Deterministically load `RobotoMono.ttf` into the shared `aether.text`
/// registry and return its session-scoped `font_id`. Loading it here — rather
/// than letting the panel's `wire` kick off the load — settles the font before
/// any draw, so no capture races the async fs-read + parse.
fn load_font(harness: &mut SubstrateHarness) -> u32 {
    let loaded = harness
        .execute(vec![(
            "font",
            HarnessOp::send_and_await(
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

/// Load the reference `WidgetPanel` (export `aether.kit.widget.panel`) under
/// the name `panel`, with its stack at `(PANEL_X, PANEL_Y)` and its theme
/// pinned to the already-resident `font_id` (empty font path, so the panel
/// does not kick off its own load) — every widget draws text with that font.
fn load_panel(harness: &mut SubstrateHarness, wasm: &[u8], font_id: u32) {
    let config = PanelConfig {
        x: PANEL_X,
        y: PANEL_Y,
        width: PANEL_WIDTH,
        font_namespace: String::new(),
        font_path: String::new(),
        theme: Theme { font_id, ..Theme::DEFAULT },
        children: Vec::new(),
        owns_input: true,
    };
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await(
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
            assert!(name.ends_with(":panel"), "the panel root should register under :panel; got {name}");
        }
        LoadResult::Err { error } => panic!("load WidgetPanel root: {error}"),
    }
}

/// One synthesized frame tick addressed straight to the panel's mailbox.
fn tick_to_panel() -> NamedMail {
    NamedMail { recipient_name: panel_address(), kind_name: Tick::NAME.to_owned(), payload: Vec::new(), count: 1 }
}

/// A region-scoped `Centroid` check over the inclusive window rect
/// `(min_x, min_y)..=(max_x, max_y)`, scored against `background` so only
/// glyph ink lights up.
fn centroid_check(rect: FrameRect, background: [u8; 3]) -> FrameCheck {
    FrameCheck {
        reduction: FrameReduction::Centroid,
        tolerance: LIT_TOLERANCE,
        background: Some(background),
        region: Some(rect),
    }
}

/// A row to assert: its human label, the window y its glyph centroid should
/// land near (its vertical center), and the region-scoped check that scores
/// it.
struct RowCheck {
    name: &'static str,
    center_y: f32,
    check: FrameCheck,
}

/// The per-row region checks, in the layout order the panel stacks its widgets
/// (window coords; stack at `(10, 10)`, row 24, gap 6):
///   `label 10..34   slider 40..64   radio 70..142 (3 rows)`
///   `text 148..172  button 178..202`.
/// The slider draws no text and the text field is empty at rest, so neither is
/// scored; each radio row skips its left marker so the glyph mask is pure text.
fn row_checks() -> Vec<RowCheck> {
    let label_top = PANEL_Y;
    let radio_top = PANEL_Y + 2.0 * (ROW_HEIGHT + GAP);
    let button_top = PANEL_Y + 4.0 * (ROW_HEIGHT + GAP) + 2.0 * ROW_HEIGHT;

    let rect = |min_x: f32, top: f32| FrameRect {
        min_x: min_x as u32,
        min_y: top as u32,
        max_x: (PANEL_X + PANEL_WIDTH) as u32 - 1,
        max_y: (top + ROW_HEIGHT) as u32 - 1,
    };
    let center = |top: f32| top + ROW_HEIGHT * 0.5;

    let mut rows = vec![
        RowCheck {
            name: "label",
            center_y: center(label_top),
            check: centroid_check(rect(PANEL_X, label_top), SURFACE_SRGB),
        },
        RowCheck {
            name: "button",
            center_y: center(button_top),
            check: centroid_check(rect(PANEL_X, button_top), ACCENT_SRGB),
        },
    ];
    for i in 0..3u32 {
        let top = radio_top + i as f32 * ROW_HEIGHT;
        rows.push(RowCheck {
            name: ["radio[0]", "radio[1]", "radio[2]"][i as usize],
            center_y: center(top),
            check: centroid_check(rect(PANEL_X + RADIO_TEXT_INSET, top), SURFACE_SRGB),
        });
    }
    rows
}

/// Every glyph the corrected panel draws sits inside the row frame its widget
/// owns: the in-row glyph centroid lands within a quarter row of the row's
/// vertical center, for the fill-free label, the accent-filled button, and
/// each radio option row alike. Under the one-ascent sag this fix removes, the
/// centroid drags a full ascent below center — well past the tolerance — so
/// this is the tripwire that would have caught the bug.
#[test]
fn panel_glyphs_sit_inside_their_row_frames() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");

    // `assets://` points at the kit assets dir (where the TTF lives);
    // `save://` / `config://` sink into a per-process sandbox tempdir.
    // Composition: GPU captures + kit wasm loads + `aether.text` glyph
    // rasterization (its font fetch rides `aether.fs`, composed from the
    // namespace roots). All mail is addressed directly, so no input fan-out.
    let sandbox = init_save_sandbox("widget-text-alignment");
    let roots = NamespaceRoots { save: sandbox.to_path_buf(), assets: assets_dir(), config: sandbox.to_path_buf() };
    let mut harness = SubstrateHarness::builder()
        .with_render()
        .with_component_host()
        .with_actor::<TextCapability>(())
        .size(240, 220)
        .namespace_roots(roots)
        .build()
        .expect("boot");
    let font_id = load_font(&mut harness);
    load_panel(&mut harness, &wasm, font_id);

    let panel = panel_address();
    // Warm the panel: the first tick spawns + lays out the widget stack and
    // draws it. That first glyph draw only *primes* the atlas — the text cap
    // lazily creates its atlas texture and the `create_texture` reply has to
    // round-trip (the `advance`) before glyphs rasterize into it. The capture
    // then draws once more, so the glyph quads reach the renderer the same
    // frame it reads back.
    harness
        .execute(vec![
            ("spawn", HarnessOp::send_mail(&panel, &Tick)),
            ("prime", HarnessOp::send_mail(&panel, &Tick)),
            ("settle", HarnessOp::advance(2)),
        ])
        .expect("warm-up");

    let rows = row_checks();
    let checks: Vec<FrameCheck> = rows.iter().map(|r| r.check.clone()).collect();
    let captured = harness
        .execute(vec![(
            "snap",
            HarnessOp::send_and_await(
                RenderCapability::NAMESPACE,
                &CaptureFrame { mails: vec![tick_to_panel()], after_mails: Vec::new(), checks, similarity: None },
            ),
        )])
        .expect("capture with region checks");
    let verdict = match captured.reply::<CaptureFrameResult>("snap").expect("decode CaptureFrameResult") {
        CaptureFrameResult::Ok { verdict, .. } => verdict.expect("checks requested → verdict"),
        CaptureFrameResult::Err { error } => panic!("capture failed: {error}"),
    };

    assert_eq!(verdict.results.len(), rows.len(), "one result per requested check");

    // A quarter of the row height: the corrected origin centers glyphs well
    // inside this band, while the removed ascent sag sits a full ascent
    // (~13px at 14px text) below — far outside it.
    let tolerance_y = ROW_HEIGHT * 0.25;
    for (row, result) in rows.iter().zip(&verdict.results) {
        let FrameCheckResult::Centroid { centroid, .. } = result else {
            panic!("row {} scored a non-Centroid result: {result:?}", row.name);
        };
        let centroid = centroid.unwrap_or_else(|| panic!("row {} drew no glyphs (empty mask)", row.name));
        let offset = centroid[1] - row.center_y;
        eprintln!(
            "row {}: centroid_y={:.1} row_center={:.1} offset={:+.1} (tol ±{tolerance_y:.1})",
            row.name, centroid[1], row.center_y, offset,
        );
        assert!(
            offset.abs() <= tolerance_y,
            "row {} glyphs must sit within {tolerance_y:.1}px of the row center {:.1}; \
             centroid landed at y={:.1} (offset {:+.1}) — the one-ascent sag this fix removes",
            row.name,
            row.center_y,
            centroid[1],
            offset,
        );
    }
}
