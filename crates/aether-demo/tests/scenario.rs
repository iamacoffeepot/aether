//! The checked-in boot manifest's bring-up reaches the viewer and draws the
//! framed subject.
//!
//! Boots a rendering `SubstrateHarness` with the component host, roots its
//! `assets` namespace at `crates/aether-mesh/examples` (the tree the depot
//! packages, so the test names no subject of its own), opens the `main`
//! window the desktop chassis opens, and loads every entry of
//! `demo.boot.json` in manifest order under its manifest `name`, as the
//! chassis boot does: each `wasm` is the pre-built artifact of the same file
//! stem, and each `config_json` is encoded against its export's schema by the
//! encoder the chassis boot uses. So a stale export name, key or wasm stem, a
//! config path that names no live camera, or a config file that no longer
//! encodes fails here. The demo's own log proves its load reached the viewer
//! and succeeded; the captured frame's coverage is tuned to the pose in
//! `camera.json`, which reaches the GPU only when the camera learned its
//! window's size and the renderer followed it.

use std::fs;
use std::ops::RangeInclusive;
use std::path::Path;

use aether_actor::{ActorRef, Addressable};
use aether_demo::Demo;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{init_save_sandbox, require_runtime, test_namespace_roots};
use aether_harness_substrate_capture::visual::{background_top_left, coverage, decode_png};
use aether_kinds::{LoadComponent, LogTail, LogTailResult};
use aether_window::{
    CreateWindow, CreateWindowResult, WindowCapability, WindowMode, WindowPresentation, WindowSizeRequest, WindowSpec,
};
use serde_json::Value;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;

/// The fraction of the frame the framed subject covers. Tuned against the
/// capture: the checked-in pose frames the teapot at about 13%. The floor, a
/// third of that, sits above what a pose further back leaves; the ceiling,
/// three times it, catches a pose that puts the eye in or against the
/// subject.
const COVERAGE_BAND: RangeInclusive<f32> = 0.04..=0.40;

/// The repository root: this crate lives at `crates/aether-demo`.
fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).expect("workspace root")
}

/// One boot-manifest entry as the chassis boot loads it: the pre-built
/// artifact of its `wasm` file stem, its export, and its `config_json`
/// encoded against that export's schema.
struct ManifestEntry {
    wasm: Vec<u8>,
    export: String,
    name: Option<String>,
    config: Vec<u8>,
}

/// Every entry of the checked-in `demo.boot.json`, in manifest order, or
/// `None` when a pre-built artifact is missing.
fn manifest_entries() -> Option<Vec<ManifestEntry>> {
    let checked_in =
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("demo.boot.json")).expect("read demo.boot.json");
    let manifest: Value = serde_json::from_slice(&checked_in).expect("parse demo.boot.json");

    let mut entries = Vec::new();
    for entry in manifest["components"].as_array().expect("a components array") {
        let stem = Path::new(entry["wasm"].as_str().expect("a wasm path"))
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("a wasm file stem");
        let wasm = fs::read(require_runtime(stem)?).expect("read the component wasm");
        let export = entry["export"].as_str().expect("an export name").to_owned();
        let name = entry.get("name").and_then(Value::as_str).map(str::to_owned);
        let config = entry.get("config_json").and_then(Value::as_str).map_or_else(Vec::new, |config| {
            let json = fs::read_to_string(workspace_root().join(config)).expect("read the config json");
            aether_chassis::encode_config_json(&wasm, Some(&export), &json)
                .unwrap_or_else(|error| panic!("{config} encodes against {export}'s config schema: {error:?}"))
        });
        entries.push(ManifestEntry { wasm, export, name, config });
    }
    Some(entries)
}

/// Open the window the desktop chassis opens at boot, at the capture's size:
/// the demo's camera follows `main` and publishes nothing until it has
/// learned that window's size.
fn open_main_window(harness: &mut SubstrateHarness) {
    let spec = WindowSpec {
        name: "main".to_owned(),
        title: "aether".to_owned(),
        mode: WindowMode::Windowed,
        size: Some(WindowSizeRequest { width: WIDTH, height: HEIGHT }),
        presentation: WindowPresentation::Display,
    };
    let opened = harness
        .execute(vec![(
            "open",
            HarnessOp::send_and_await_reply(&harness.actor_ref::<WindowCapability>(), &CreateWindow { spec }),
        )])
        .expect("the window manager answers the create");

    let reply = opened.reply::<CreateWindowResult>("open").expect("decode CreateWindowResult");
    assert!(matches!(reply, CreateWindowResult::Ok { .. }), "the main window opens: {reply:?}");
}

#[test]
fn demo_loads_the_subject_and_the_camera_frames_it() {
    let Some(entries) = manifest_entries() else {
        return;
    };
    let mut roots = test_namespace_roots(init_save_sandbox("demo-scenario"));
    roots.assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../aether-mesh/examples");
    let mut harness = SubstrateHarness::builder()
        .size(WIDTH, HEIGHT)
        .namespace_roots(roots)
        .with_render()
        .with_component_host()
        .build()
        .expect("boot");
    open_main_window(&mut harness);

    let mut demo: Option<ActorRef<Demo>> = None;
    for ManifestEntry { wasm, export, name, config } in entries {
        if export == Demo::NAMESPACE {
            let loaded = harness
                .load::<Demo>(LoadComponent { wasm, name, config, export: Some(export.clone()) })
                .unwrap_or_else(|error| panic!("load {export}: {error}"));
            demo = Some(loaded);
        } else {
            harness
                .load_any(&LoadComponent { wasm, name, config, export: Some(export.clone()) })
                .unwrap_or_else(|error| panic!("load {export}: {error}"));
        }
    }
    let demo = demo.expect("the manifest boots the demo");

    let loaded = LogTail { max: 0, min_level: None, since: None, contains: Some("subject loaded".to_owned()) };
    let result = harness
        .execute(vec![
            (
                "loaded",
                HarnessOp::poll_until(
                    &demo,
                    &loaded,
                    |reply: &LogTailResult| matches!(reply, LogTailResult::Ok { entries, .. } if !entries.is_empty()),
                ),
            ),
            ("frames", HarnessOp::advance(5)),
            ("snap", HarnessOp::capture()),
        ])
        .expect("the demo logs its subject loaded, then the frame captures");

    let image = decode_png(result.captured("snap").expect("snap step ran")).expect("decode capture png");
    let covered = coverage(&image, background_top_left(&image), 5);
    assert!(
        COVERAGE_BAND.contains(&covered),
        "the framed subject should cover {COVERAGE_BAND:?} of the frame; it covers {covered}",
    );
}
