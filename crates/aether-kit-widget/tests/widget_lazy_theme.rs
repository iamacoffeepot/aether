//! Live `SetTheme` / `LoadFontResult` updates that beat the first panel Tick
//! or scroll Collect must still restyle spawned children (issue 5539).
//!
//! `WidgetPanel` fans style across an empty child list until the first Tick
//! spawns it; `ScrollWidget` used to drop the same mail until its first
//! Collect. These scenarios send the update first, then capture the first
//! Collect, and read the child's committed draw payload — a filled button
//! plate tint, or resident glyph quads — so a dropped restyle cannot hide
//! behind retained pending state.
//!
//! Skipped when no wgpu adapter is available or the `aether_kit_widget` wasm
//! has not been pre-built (the shared `require_runtime` gate). CI sets
//! `AETHER_REQUIRE_RUNTIME=1` to turn either skip into a hard failure.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]

use aether_harness_substrate_capture::{RenderHarnessBuilderExt, RenderHarnessExt};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use aether_actor::Addressable;
use aether_data::Kind;
use aether_fs::NamespaceRoots;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::test_helpers::{init_save_sandbox, require_runtime};
use aether_kinds::{ClipRect, LoadComponent, LoadResult, NamedMail, Tick};
use aether_kit_widget::{
    ButtonConfig, LabelConfig, PanelConfig, ScrollConfig, ScrollExtent, ScrollOffset, SetTheme, Theme, WidgetChildSpec,
    WidgetKind,
};
use aether_math::Rgba;
use aether_render::{DrawTexturedQuads, WHITE_TEXTURE_ID};
use aether_text::{LoadFont, LoadFontResult, TextCapability};

const PANEL_X: f32 = 10.0;
const PANEL_Y: f32 = 10.0;
const PANEL_WIDTH: f32 = 200.0;
const RESTYLE_RED: Rgba = Rgba::new(0.90, 0.05, 0.05, 1.0);
const RESTYLE_BLUE: Rgba = Rgba::new(0.05, 0.05, 0.90, 1.0);

fn panel_address() -> String {
    format!("aether.component/{}:panel", aether_component::WasmTrampoline::NAMESPACE)
}

fn tick_to_panel() -> NamedMail {
    NamedMail {
        recipient_name: panel_address(),
        kind_name: Tick::NAME.to_owned(),
        payload: Tick::default().encode_into_bytes(),
        count: 1,
    }
}

fn row_height() -> f32 {
    Theme::DEFAULT.row_height
}

fn row_clip(y: f32) -> ClipRect {
    ClipRect { x: PANEL_X, y, width: PANEL_WIDTH, height: row_height() }
}

fn accent_theme(accent: Rgba) -> Theme {
    Theme { accent, ..Theme::DEFAULT }
}

fn filled_button(subname: &str, theme: Theme) -> WidgetChildSpec {
    WidgetChildSpec {
        subname: subname.to_owned(),
        kind: WidgetKind::Button,
        origin: [0.0, 0.0],
        clip: None,
        config: ButtonConfig { label: subname.to_owned(), theme, ..ButtonConfig::default() }.encode_into_bytes(),
    }
}

fn scroll_child(
    subname: &str,
    viewport: ScrollExtent,
    content_extent: ScrollExtent,
    content: WidgetChildSpec,
) -> WidgetChildSpec {
    WidgetChildSpec {
        subname: subname.to_owned(),
        kind: WidgetKind::Scroll,
        origin: [0.0, 0.0],
        clip: None,
        config: ScrollConfig {
            viewport_extent: viewport,
            content_extent,
            initial_offset: ScrollOffset::default(),
            content,
        }
        .encode_into_bytes(),
    }
}

fn solid_for<'a>(snapshot: &'a [DrawTexturedQuads], clip: &ClipRect) -> &'a DrawTexturedQuads {
    snapshot
        .iter()
        .find(|batch| batch.texture_id == WHITE_TEXTURE_ID && batch.clip.as_ref() == Some(clip))
        .unwrap_or_else(|| panic!("missing solid batch for {clip:?}; snapshot: {snapshot:?}"))
}

fn assert_row_fill(snapshot: &[DrawTexturedQuads], clip: &ClipRect, color: Rgba, message: &str) {
    let batch = solid_for(snapshot, clip);
    assert!(
        batch.quads.iter().any(|quad| quad.tint == color),
        "{message}; wanted {color:?} in {clip:?}; batch: {batch:?}",
    );
}

fn assert_row_lacks_fill(snapshot: &[DrawTexturedQuads], clip: &ClipRect, color: Rgba, message: &str) {
    let batch = solid_for(snapshot, clip);
    assert!(
        batch.quads.iter().all(|quad| quad.tint != color),
        "{message}; did not want {color:?} in {clip:?}; batch: {batch:?}",
    );
}

fn load_panel(harness: &mut SubstrateHarness, wasm: &[u8], children: Vec<WidgetChildSpec>) {
    let config = PanelConfig {
        x: PANEL_X,
        y: PANEL_Y,
        width: PANEL_WIDTH,
        font_namespace: String::new(),
        font_path: String::new(),
        theme: Theme::DEFAULT,
        children,
        owns_input: true,
    };
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some("panel".to_owned()),
                    config: config.encode_into_bytes(),
                    export: Some("aether.kit.widget.panel".to_owned()),
                },
            ),
        )])
        .expect("load WidgetPanel");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => {
            assert!(name.ends_with(":panel"), "the panel root should register under :panel; got {name}");
        }
        LoadResult::Err { error } => panic!("load WidgetPanel: {error}"),
    }
}

fn color_bench() -> SubstrateHarness {
    SubstrateHarness::builder()
        .size(240, 80)
        .with_render()
        .with_component_host()
        .build()
        .expect("boot")
}

fn capture_first_collect(harness: &mut SubstrateHarness) {
    harness
        .execute(vec![("first_collect", HarnessOp::capture_with_mails(vec![tick_to_panel()], Vec::new()))])
        .expect("capture the first Collect");
}

fn send_theme(harness: &mut SubstrateHarness, theme: Theme) {
    harness
        .execute(vec![("set_theme", HarnessOp::send_and_settle(&panel_address(), &SetTheme { theme }))])
        .expect("send SetTheme before the first Tick");
}

fn assets_dir() -> PathBuf {
    match env::current_dir().map(|current| current.join("assets")) {
        Ok(dir) if dir.is_dir() => dir,
        _ => Path::new(env!("CARGO_MANIFEST_DIR")).join("assets"),
    }
}

fn load_font(harness: &mut SubstrateHarness) -> u32 {
    let loaded = harness
        .execute(vec![(
            "font",
            HarnessOp::send_and_await_reply(
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

/// **Bug class: restyle before spawn is dropped.** `SetTheme` before the first
/// Tick updates the root and fans to no children; the first Collect then draws
/// each child's config theme. The first filled-button plate must already be
/// the restyle accent.
#[test]
fn early_set_theme_restyles_the_first_collect_plate() {
    let Some(wasm_path) = require_runtime("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = color_bench();
    load_panel(&mut harness, &wasm, vec![filled_button("apply", Theme::DEFAULT)]);
    send_theme(&mut harness, accent_theme(RESTYLE_RED));
    capture_first_collect(&mut harness);

    let snapshot = harness.committed_overlay_snapshot();
    let clip = row_clip(PANEL_Y);
    assert_row_fill(
        &snapshot,
        &clip,
        RESTYLE_RED,
        "the first Collect must draw the restyle accent, not the config theme",
    );
    assert_row_lacks_fill(
        &snapshot,
        &clip,
        Theme::DEFAULT.accent,
        "a dropped restyle would keep the default accent plate",
    );
}

/// **Bug class: later restyles collapse to the first.** Two `SetTheme` mails
/// before spawn must latest-win: the first Collect plate is the second accent.
#[test]
fn early_theme_updates_latest_win_on_the_first_collect() {
    let Some(wasm_path) = require_runtime("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = color_bench();
    load_panel(&mut harness, &wasm, vec![filled_button("apply", Theme::DEFAULT)]);
    send_theme(&mut harness, accent_theme(RESTYLE_RED));
    send_theme(&mut harness, accent_theme(RESTYLE_BLUE));
    capture_first_collect(&mut harness);

    let snapshot = harness.committed_overlay_snapshot();
    let clip = row_clip(PANEL_Y);
    assert_row_fill(
        &snapshot,
        &clip,
        RESTYLE_BLUE,
        "the first Collect must draw the latest restyle, not an earlier one",
    );
    assert_row_lacks_fill(
        &snapshot,
        &clip,
        RESTYLE_RED,
        "retaining every update without latest-wins would keep the first restyle",
    );
}

/// **Bug class: spawn overlays the root theme with no live update.** Explicit
/// per-child config themes must survive the first Collect when no `SetTheme`
/// or `LoadFontResult` has arrived.
#[test]
fn first_collect_keeps_explicit_child_themes_when_nothing_was_fanned() {
    let Some(wasm_path) = require_runtime("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = color_bench();
    load_panel(
        &mut harness,
        &wasm,
        vec![
            filled_button("red", accent_theme(RESTYLE_RED)),
            filled_button("blue", accent_theme(RESTYLE_BLUE)),
        ],
    );
    capture_first_collect(&mut harness);

    let snapshot = harness.committed_overlay_snapshot();
    let first = row_clip(PANEL_Y);
    let second = row_clip(PANEL_Y + row_height() + Theme::DEFAULT.gap);
    assert_row_fill(
        &snapshot,
        &first,
        RESTYLE_RED,
        "the first child must keep its config accent when no root restyle arrived",
    );
    assert_row_fill(
        &snapshot,
        &second,
        RESTYLE_BLUE,
        "the second child must keep its own config accent rather than a fanned root theme",
    );
    assert_row_lacks_fill(
        &snapshot,
        &first,
        Theme::DEFAULT.accent,
        "unconditionally fanning the panel theme at spawn would paint the default accent",
    );
    assert_row_lacks_fill(
        &snapshot,
        &second,
        Theme::DEFAULT.accent,
        "unconditionally fanning the panel theme at spawn would paint both children the default accent",
    );
}

/// **Bug class: nested scroll drops the restyle until after first Collect.**
/// Outer → inner `ScrollWidget` must forward the retained theme onto the
/// content button before that first Collect, so the viewport batch already
/// carries the restyle accent.
#[test]
fn nested_scroll_forwards_early_theme_before_the_first_collect() {
    let Some(wasm_path) = require_runtime("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let viewport = ScrollExtent { width_pixels: PANEL_WIDTH, height_pixels: row_height() };
    let inner = scroll_child("inner", viewport, viewport, filled_button("apply", Theme::DEFAULT));
    let outer = scroll_child("outer", viewport, viewport, inner);

    let mut harness = color_bench();
    load_panel(&mut harness, &wasm, vec![outer]);
    send_theme(&mut harness, accent_theme(RESTYLE_RED));
    capture_first_collect(&mut harness);

    let snapshot = harness.committed_overlay_snapshot();
    let clip = row_clip(PANEL_Y);
    assert_row_fill(
        &snapshot,
        &clip,
        RESTYLE_RED,
        "nested scroll must replay the restyle onto the content button before the first Collect",
    );
    assert_row_lacks_fill(
        &snapshot,
        &clip,
        Theme::DEFAULT.accent,
        "a dropped nested relay would keep the config accent inside the viewport",
    );
}

/// **Bug class: a resolved font id that beats the first Tick never reaches
/// text.** The panel stamps `LoadFontResult` and used to fan it to no children;
/// the first Collect then draws with the config placeholder, so `aether.text`
/// warn-drops the run. Resident glyph quads after that result are the proof
/// the resolved id was replayed.
#[test]
fn early_load_font_result_draws_glyphs_on_the_first_collect_path() {
    let Some(wasm_path) = require_runtime("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let sandbox = init_save_sandbox("widget-lazy-theme");
    let roots = NamespaceRoots { save: sandbox.to_path_buf(), assets: assets_dir(), config: sandbox.to_path_buf() };
    let mut harness = SubstrateHarness::builder()
        .size(240, 80)
        .with_render()
        .with_component_host()
        .with_actor::<TextCapability>(())
        .namespace_roots(roots)
        .build()
        .expect("boot");

    let font_id = load_font(&mut harness);
    let label = WidgetChildSpec {
        subname: "title".to_owned(),
        kind: WidgetKind::Label,
        origin: [0.0, 0.0],
        clip: None,
        config: LabelConfig {
            text: "Early font".to_owned(),
            theme: Theme { font_id: u32::MAX, ..Theme::DEFAULT },
            ..LabelConfig::default()
        }
        .encode_into_bytes(),
    };
    load_panel(&mut harness, &wasm, vec![label]);

    let panel = panel_address();
    harness
        .execute(vec![(
            "font_result",
            HarnessOp::send_and_settle(
                &panel,
                &LoadFontResult::Ok { font_id, name: "RobotoMono".to_owned(), resident_bytes: 1 },
            ),
        )])
        .expect("deliver LoadFontResult before the first Tick");
    harness
        .execute(vec![
            ("spawn", HarnessOp::send_and_settle(&panel, &Tick::default())),
            ("prime", HarnessOp::send_and_settle(&panel, &Tick::default())),
            ("settle", HarnessOp::advance(2)),
            ("capture", HarnessOp::capture_with_mails(vec![tick_to_panel()], Vec::new())),
        ])
        .expect("spawn after the font result and capture glyphs");

    let clip = row_clip(PANEL_Y);
    let snapshot = harness.committed_overlay_snapshot();
    let glyph_batches: Vec<_> = snapshot
        .iter()
        .filter(|batch| batch.texture_id != WHITE_TEXTURE_ID && batch.clip.as_ref() == Some(&clip))
        .collect();
    assert!(
        glyph_batches.iter().any(|batch| !batch.quads.is_empty()),
        "the resolved font id must reach the label before Collect so glyphs rasterize; snapshot: {snapshot:?}",
    );
}
