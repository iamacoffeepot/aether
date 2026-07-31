//! Console render + activation acceptance tests.
//!
//! These tests load the real `aether.kit.console` actor into a `SubstrateHarness`,
//! drive its typed input path, and assert rendered output through frame
//! reductions. The desktop driver has a unit tripwire for the physical
//! `Backquote` mapping; this suite proves the engine key code actually opens
//! the console overlay and reaches the render sink.
//!
//! Skipped when no wgpu adapter is available or the `aether_kit_commons` wasm has not
//! been pre-built. CI sets `AETHER_REQUIRE_RUNTIME=1` to make either skip a
//! hard failure.

// Integration-test skip diagnostic: emit via stderr so `cargo test` surfaces
// "skipping: ..." alongside `test ... ok`.
#![allow(clippy::print_stderr)]

use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use std::fs;
use std::path::{Path, PathBuf};

use aether_actor::Addressable;
use aether_data::{Kind, MailboxId};
use aether_fs::NamespaceRoots;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::test_helpers::{init_save_sandbox, require_runtime};
use aether_kinds::keycode::KEY_BACKQUOTE;
use aether_kinds::{
    CaptureFrame, CaptureFrameResult, FrameCheck, FrameCheckResult, FrameRect, FrameReduction, Key, LoadComponent,
    LoadResult, NamedMail, Tick, WindowId, WindowSize,
};
use aether_kit_commons::{ConsoleCommandOutput, ConsoleConfig};
use aether_kit_widget::{EditorConfig, EditorKeyChord, EditorRegionRect, RegionInputLanes, RegionSpec};
use aether_render::RenderCapability;
use aether_text::TextCapability;

const WINDOW_WIDTH: u32 = 320;
const WINDOW_HEIGHT: u32 = 200;
const TEST_WINDOW_ID: WindowId = WindowId(1);
const CLEAR_SRGB: [u8; 3] = [63, 75, 97];
const PARTITION_TOLERANCE: u8 = 8;

fn console_address() -> String {
    format!("aether.component/{}:console", aether_component::WasmTrampoline::NAMESPACE)
}

fn assets_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets")
}

fn build_bench() -> SubstrateHarness {
    build_bench_with_assets(assets_dir(), "console-render-interaction")
}

fn build_bench_without_assets_root() -> SubstrateHarness {
    let sandbox = init_save_sandbox("console-render-interaction-no-assets");
    let assets = sandbox.join("empty-assets");
    fs::create_dir_all(&assets).expect("create empty assets root");
    build_bench_with_assets(assets, "console-render-interaction-no-assets")
}

/// Composition: GPU captures + kit wasm loads; the console rasterizes its
/// overlay text through `aether.text` (fonts fetched via `aether.fs`, which
/// composes from the namespace roots); one scenario drives the toggle key
/// through the harness's synthetic window fan-out.
fn build_bench_with_assets(assets: PathBuf, sandbox_name: &str) -> SubstrateHarness {
    let sandbox = init_save_sandbox(sandbox_name);
    let roots = NamespaceRoots { save: sandbox.to_path_buf(), assets, config: sandbox.to_path_buf() };
    SubstrateHarness::builder()
        .with_render()
        .with_component_host()
        .with_actor::<TextCapability>(())
        .size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .namespace_roots(roots)
        .build()
        .expect("boot")
}

fn envelope<K: Kind>(recipient: &str, mail: &K) -> NamedMail {
    NamedMail {
        recipient_name: recipient.to_owned(),
        kind_name: K::NAME.to_owned(),
        payload: mail.encode_into_bytes(),
        count: 1,
    }
}

fn load_console(harness: &mut SubstrateHarness, wasm: &[u8]) -> MailboxId {
    load_console_with_config(harness, wasm, &ConsoleConfig::default())
}

fn load_console_with_config(harness: &mut SubstrateHarness, wasm: &[u8], config: &ConsoleConfig) -> MailboxId {
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some("console".to_owned()),
                    config: config.encode_into_bytes(),
                    export: Some("aether.kit.console".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, name, .. } => {
            assert!(name.ends_with(":console"), "console should register under :console; got {name}");
            mailbox_id
        }
        LoadResult::Err { error } => panic!("load console: {error}"),
    }
}

fn load_editor_shell(harness: &mut SubstrateHarness, wasm: &[u8], target: MailboxId) {
    let config = EditorConfig {
        regions: vec![RegionSpec {
            name: "console".to_owned(),
            rect: EditorRegionRect { x_pixels: 0.0, y_pixels: 0.0, width_pixels: 320.0, height_pixels: 200.0 },
            target,
            keyboard_focus_eligible: true,
            input_lanes: RegionInputLanes::ALL,
            activation_chord: Some(EditorKeyChord {
                key_code: KEY_BACKQUOTE,
                shift: false,
                ctrl: false,
                alt: false,
                meta: false,
            }),
        }],
    };
    let loaded = harness
        .execute(vec![(
            "load-editor",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some("editor".to_owned()),
                    config: config.encode_into_bytes(),
                    export: Some("aether.kit.widget.editor".to_owned()),
                },
            ),
        )])
        .expect("load editor shell");
    match loaded.reply::<LoadResult>("load-editor").expect("decode editor LoadResult") {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load editor shell: {error}"),
    }
}

fn top_band() -> FrameRect {
    FrameRect { min_x: 0, min_y: 0, max_x: WINDOW_WIDTH - 1, max_y: 96 }
}

fn top_band_coverage(harness: &mut SubstrateHarness, label: &'static str) -> f32 {
    coverage_in_region(harness, label, top_band(), CLEAR_SRGB)
}

fn history_text_band() -> FrameRect {
    FrameRect { min_x: 8, min_y: 20, max_x: WINDOW_WIDTH - 8, max_y: 72 }
}

fn coverage_in_region(
    harness: &mut SubstrateHarness,
    label: &'static str,
    region: FrameRect,
    background: [u8; 3],
) -> f32 {
    let captured = harness
        .execute(vec![(
            label,
            HarnessOp::send_and_await_reply(
                RenderCapability::NAMESPACE,
                &CaptureFrame {
                    window: None,
                    mails: vec![envelope(&console_address(), &Tick)],
                    after_mails: Vec::new(),
                    checks: vec![FrameCheck {
                        reduction: FrameReduction::Coverage,
                        tolerance: PARTITION_TOLERANCE,
                        background: Some(background),
                        region: Some(region),
                    }],
                    similarity: None,
                },
            ),
        )])
        .expect("capture sequence");
    let result = match captured.reply::<CaptureFrameResult>(label).expect("decode CaptureFrameResult") {
        CaptureFrameResult::Ok { verdict, .. } => {
            let verdict = verdict.expect("checks requested");
            verdict.results.into_iter().next().expect("one check result")
        }
        CaptureFrameResult::Err { error } => panic!("capture failed: {error}"),
    };
    match result {
        FrameCheckResult::Coverage { fraction, .. } => fraction,
        other => panic!("expected Coverage result; got {other:?}"),
    }
}

fn history_text_differs_from_panel(harness: &mut SubstrateHarness, label: &'static str) -> bool {
    let captured = harness
        .execute(vec![(
            label,
            HarnessOp::send_and_await_reply(
                RenderCapability::NAMESPACE,
                &CaptureFrame {
                    window: None,
                    mails: vec![envelope(&console_address(), &Tick)],
                    after_mails: Vec::new(),
                    checks: vec![FrameCheck {
                        reduction: FrameReduction::DiffersFromBackground,
                        tolerance: PARTITION_TOLERANCE,
                        background: None,
                        region: Some(history_text_band()),
                    }],
                    similarity: None,
                },
            ),
        )])
        .expect("capture sequence");
    let result = match captured.reply::<CaptureFrameResult>(label).expect("decode CaptureFrameResult") {
        CaptureFrameResult::Ok { verdict, .. } => {
            let verdict = verdict.expect("checks requested");
            verdict.results.into_iter().next().expect("one check result")
        }
        CaptureFrameResult::Err { error } => panic!("capture failed: {error}"),
    };
    match result {
        FrameCheckResult::DiffersFromBackground { passed, .. } => passed,
        other => panic!("expected DiffersFromBackground result; got {other:?}"),
    }
}

#[test]
fn backquote_key_opens_console_overlay() {
    let Some(wasm_path) = require_runtime("aether_kit_commons") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = build_bench();
    load_console(&mut harness, &wasm);

    harness
        .execute(vec![(
            "size",
            HarnessOp::send_and_settle(
                console_address(),
                &WindowSize { window: TEST_WINDOW_ID, width: WINDOW_WIDTH, height: WINDOW_HEIGHT },
            ),
        )])
        .expect("window size");

    let closed = top_band_coverage(&mut harness, "closed");
    assert!(closed < 0.01, "closed console should leave the top band at clear color; coverage={closed:.3}");

    harness
        .execute(vec![(
            "toggle",
            HarnessOp::send_and_settle(console_address(), &Key { window: TEST_WINDOW_ID, code: KEY_BACKQUOTE }),
        )])
        .expect("toggle key");

    let open = top_band_coverage(&mut harness, "open");
    assert!(open > 0.90, "backquote should open the console and cover the top band; coverage={open:.3}");
}

#[test]
fn editor_shell_exclusively_forwards_console_input_while_window_size_stays_direct() {
    let Some(wasm_path) = require_runtime("aether_kit_commons") else {
        return;
    };
    // The `EditorShell` arbiter now ships in the `aether-kit-widget` wasm
    // (iamacoffeepot/aether#3950); the console still ships in the kit wasm.
    let Some(widget_wasm_path) = require_runtime("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let widget_wasm = fs::read(&widget_wasm_path).expect("read kit-widget wasm");
    let mut harness = build_bench();
    let console =
        load_console_with_config(&mut harness, &wasm, &ConsoleConfig { owns_input: false, ..ConsoleConfig::default() });

    harness
        .execute(vec![
            (
                "size-direct-fanout",
                HarnessOp::window_event(
                    TEST_WINDOW_ID,
                    &WindowSize { window: TEST_WINDOW_ID, width: WINDOW_WIDTH, height: WINDOW_HEIGHT },
                ),
            ),
            (
                "unrouted-toggle",
                HarnessOp::window_event(TEST_WINDOW_ID, &Key { window: TEST_WINDOW_ID, code: KEY_BACKQUOTE }),
            ),
        ])
        .expect("direct fanout before editor shell");
    let closed = top_band_coverage(&mut harness, "owns-input-disabled");
    assert!(closed < 0.01, "console must not self-subscribe to interactive input; coverage={closed:.3}");

    load_editor_shell(&mut harness, &widget_wasm, console);
    harness
        .execute(vec![(
            "routed-toggle",
            HarnessOp::window_event(TEST_WINDOW_ID, &Key { window: TEST_WINDOW_ID, code: KEY_BACKQUOTE }),
        )])
        .expect("toggle through editor shell");
    let open = top_band_coverage(&mut harness, "editor-routed");
    assert!(open > 0.90, "editor shell should forward backquote exactly once; coverage={open:.3}");
}

#[test]
fn markdown_command_output_renders_into_history_band() {
    let Some(wasm_path) = require_runtime("aether_kit_commons") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = build_bench_without_assets_root();
    load_console(&mut harness, &wasm);

    harness
        .execute(vec![
            (
                "size",
                HarnessOp::send_and_settle(
                    console_address(),
                    &WindowSize { window: TEST_WINDOW_ID, width: WINDOW_WIDTH, height: WINDOW_HEIGHT },
                ),
            ),
            ("settle", HarnessOp::advance(8)),
            (
                "toggle",
                HarnessOp::send_and_settle(console_address(), &Key { window: TEST_WINDOW_ID, code: KEY_BACKQUOTE }),
            ),
        ])
        .expect("open console");

    let empty = history_text_differs_from_panel(&mut harness, "empty-history");
    assert!(!empty, "empty history band should match the panel background");

    harness
        .execute(vec![(
            "markdown-output",
            HarnessOp::send_and_settle(
                console_address(),
                &ConsoleCommandOutput {
                    command: String::from("diagnostics"),
                    lines: vec![String::from("## Heading"), String::from("- [x] `code` [link](target)")],
                    error: false,
                },
            ),
        )])
        .expect("send markdown output");

    let rendered = history_text_differs_from_panel(&mut harness, "rendered-history");
    assert!(rendered, "markdown output should add visible text/background pixels to the history band");
}

#[test]
fn configured_font_override_renders_into_history_band() {
    let Some(wasm_path) = require_runtime("aether_kit_commons") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let config = ConsoleConfig {
        font_namespace: String::from("assets"),
        font_path: String::from("fonts/RobotoMono.ttf"),
        ..ConsoleConfig::default()
    };
    let mut harness = build_bench();
    load_console_with_config(&mut harness, &wasm, &config);

    harness
        .execute(vec![
            (
                "size",
                HarnessOp::send_and_settle(
                    console_address(),
                    &WindowSize { window: TEST_WINDOW_ID, width: WINDOW_WIDTH, height: WINDOW_HEIGHT },
                ),
            ),
            ("settle", HarnessOp::advance(8)),
            (
                "toggle",
                HarnessOp::send_and_settle(console_address(), &Key { window: TEST_WINDOW_ID, code: KEY_BACKQUOTE }),
            ),
            (
                "output",
                HarnessOp::send_and_settle(
                    console_address(),
                    &ConsoleCommandOutput {
                        command: String::from("override"),
                        lines: vec![String::from("## Override"), String::from("- [x] `font` rendered")],
                        error: false,
                    },
                ),
            ),
        ])
        .expect("open console with configured font");

    let rendered = history_text_differs_from_panel(&mut harness, "override-rendered-history");
    assert!(rendered, "configured font override should render visible text into the history band");
}
