//! The demo's bring-up reaches the viewer and draws the framed subject.
//!
//! Boots a rendering `SubstrateHarness` with the component host, writes
//! `lamp_post.dsl` into the sandbox's `assets` root, and loads the demo's four
//! components in manifest order at their default names: the kit camera, the
//! kit camera controller with the checked-in `controller.json` (encoded by the
//! same encoder the depot build and the boot manifest use), the kit mesh
//! viewer, then [`Demo`]. The demo's own log proves its load reached the viewer
//! and succeeded; the captured frame's coverage proves the controller seed
//! framed the subject rather than leaving it the speck the compiled baseline
//! pose gives.

use std::fs;
use std::ops::RangeInclusive;

use aether_actor::Addressable;
use aether_demo::Demo;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{
    init_save_sandbox, require_runtime, test_namespace_roots, write_fixture,
};
use aether_harness_substrate_capture::visual::{background_top_left, coverage, decode_png};
use aether_kinds::{LoadComponent, LogTail, LogTailResult};
use aether_kit::camera::CameraComponent;
use aether_kit::camera::controller::CameraController;
use aether_kit::mesh::MeshViewer;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;

/// The subject the demo loads, from the same examples directory the depot
/// packages as its asset tree.
const LAMP_POST_DSL: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../aether-mesh/examples/lamp_post.dsl"));

/// The checked-in controller init-config the demo ships.
const CONTROLLER_JSON: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/controller.json"));

/// The fraction of the frame the framed lamp post covers. Tuned against the
/// capture: the checked-in seed covers about 1.4% (the post is thin), the
/// compiled baseline pose (12 units back, 63° overhead) about 0.2%, so the
/// floor sits between the two; the ceiling, five times the framed value,
/// catches a seed that puts the eye in or against the subject.
const COVERAGE_BAND: RangeInclusive<f32> = 0.007..=0.07;

/// Load the export `R` at its default name, the name a declared dependency
/// looks for.
fn load<R: Addressable>(harness: &mut SubstrateHarness, wasm: &[u8], config: Vec<u8>) -> aether_actor::ActorRef<R> {
    harness
        .load::<R>(LoadComponent { wasm: wasm.to_vec(), name: None, config, export: None })
        .unwrap_or_else(|error| panic!("load {}: {error}", R::NAMESPACE))
        .0
}

#[test]
fn demo_loads_the_subject_and_the_seed_frames_it() {
    let Some(kit_path) = require_runtime("aether_kit") else {
        return;
    };
    let Some(demo_path) = require_runtime("aether_demo") else {
        return;
    };
    let (kit, demo_wasm) = (fs::read(kit_path).expect("read kit wasm"), fs::read(demo_path).expect("read demo wasm"));
    let sandbox = init_save_sandbox("demo-scenario");
    write_fixture("lamp_post.dsl", LAMP_POST_DSL);
    let mut harness = SubstrateHarness::builder()
        .size(WIDTH, HEIGHT)
        .namespace_roots(test_namespace_roots(sandbox))
        .with_render()
        .with_component_host()
        .build()
        .expect("boot");

    let controller_config =
        aether_chassis::encode_config_json(&kit, Some(CameraController::NAMESPACE), CONTROLLER_JSON)
            .expect("controller.json encodes against the controller's config schema");
    load::<CameraComponent>(&mut harness, &kit, Vec::new());
    load::<CameraController>(&mut harness, &kit, controller_config);
    load::<MeshViewer>(&mut harness, &kit, Vec::new());
    let demo = load::<Demo>(&mut harness, &demo_wasm, Vec::new());

    let loaded = LogTail { max: 0, min_level: None, since: None, contains: Some("subject loaded".to_owned()) };
    let result = harness
        .execute(vec![
            (
                "loaded",
                HarnessOp::poll_until(
                    demo.erase(),
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
        "the framed lamp post should cover {COVERAGE_BAND:?} of the frame; it covers {covered}",
    );
}
