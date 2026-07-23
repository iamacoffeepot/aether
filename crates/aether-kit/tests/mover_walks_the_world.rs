//! Acceptance: a held key walks the mover across the flat world base (issue
//! 2867).
//!
//! Loads `WorldView` and `WorldMover`, authors a grass chunk with a one-meter
//! north/south height break, and gives that region a sand-colored cliff face.
//! The mover owns the follow camera, so walking north keeps its marker centered
//! while the world-anchored cliff scrolls down the frame. Target-color geometry
//! proves that the intended cliff moved; bounded whole-frame MAE is supporting
//! evidence. Releasing W and advancing again proves the held-input state clears
//! and the camera stops.
//!
//! Skipped when no wgpu adapter is available or the `aether_kit` wasm has not
//! been pre-built. CI sets `AETHER_REQUIRE_RUNTIME=1`, turning either skip into
//! a hard failure.

// Integration-test diagnostics intentionally surface alongside a failing test.
#![allow(clippy::print_stderr)]

use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use std::fs;

use aether_actor::Addressable;
use aether_data::{Kind, MailboxId};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::ArtifactGuard;
use aether_harness_substrate_capture::test_helpers::require_runtime;
use aether_harness_substrate_capture::visual::{
    ColorRegionStats, Rect, decode_png, mean_absolute_error, target_color_stats,
};
use aether_input::InputCapability;
use aether_kinds::keycode::KEY_W;
use aether_kinds::{
    CaptureFrame, CaptureFrameResult, FrameCheck, FrameCheckResult, FrameReduction, Key, KeyRelease, LoadComponent,
    LoadResult, NamedMail, Render, WindowSize,
};
use aether_kit::world::{Material, SetChunk, SetRegion};
use aether_kit::{MoverConfig, MoverTeleport};
use aether_kit_widget::{EditorConfig, EditorKeyChord, EditorRegionRect, RegionInputLanes, RegionSpec};

const WINDOW_WIDTH: u32 = 128;
const WINDOW_HEIGHT: u32 = 96;
const CHUNK_EDGE: usize = 16;
const CHUNK_AREA: usize = CHUNK_EDGE * CHUNK_EDGE;
const HEIGHT_BREAK_ROW: usize = 8;
const CLIFF_HEIGHT_OCTIMETERS: i32 = 256;
const REGION_ID: u32 = 1;

/// `SubstrateHarness` readback RGB for the renderer's clear color.
const CLEAR_SRGB: [u8; 3] = [63, 75, 97];
/// The built-in Sand style's documented sRGB design value.
const SAND_CLIFF_SRGB: [u8; 3] = [217, 199, 140];
/// Covers the style's square-law linearization and sRGB-target conversion,
/// plus edge rasterization differences across adapters.
const CLIFF_COLOR_TOLERANCE: u8 = 20;

/// The central band containing the authored z=8 cliff. It excludes the sand
/// skirts at the chunk's north and south Void boundaries.
const CLIFF_OBSERVATION: Rect = Rect { min_x: 4, min_y: 16, max_x: 123, max_y: 58 };
/// A grass-only strip below the moving cliff and above the chunk's south edge.
/// A full-screen sand clear or an unrelated large sand patch would light this
/// strip and fail the exclusion oracle.
const CLIFF_EXCLUSION: Rect = Rect { min_x: 36, min_y: 47, max_x: 91, max_y: 57 };

struct SceneCapture {
    png: Vec<u8>,
    checks: Vec<FrameCheck>,
    results: Vec<FrameCheckResult>,
}

fn component_address(name: &str) -> String {
    format!("aether.component/{}:{name}", aether_component::WasmTrampoline::NAMESPACE)
}

fn envelope<K: Kind>(recipient: &str, mail: &K) -> NamedMail {
    NamedMail {
        recipient_name: recipient.to_owned(),
        kind_name: K::NAME.to_owned(),
        payload: mail.encode_into_bytes(),
        count: 1,
    }
}

fn load_kit_export(harness: &mut SubstrateHarness, wasm: &[u8], export: &str, name: &str) -> MailboxId {
    load_kit_export_with_config(harness, wasm, export, name, Vec::new())
}

fn load_kit_export_with_config(
    harness: &mut SubstrateHarness,
    wasm: &[u8],
    export: &str,
    name: &str,
    config: Vec<u8>,
) -> MailboxId {
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some(name.to_owned()),
                    config,
                    export: Some(export.to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, name: address, .. } => {
            assert!(
                address.ends_with(&format!(":{name}")),
                "export {export} should register under :{name}; got {address}",
            );
            mailbox_id
        }
        LoadResult::Err { error } => panic!("load {export}: {error}"),
    }
}

fn load_mover_editor_shell(harness: &mut SubstrateHarness, wasm: &[u8], mover: MailboxId) {
    let lanes = RegionInputLanes { key_press: true, key_release: true, ..RegionInputLanes::default() };
    let config = EditorConfig {
        regions: vec![RegionSpec {
            name: "world".to_owned(),
            rect: EditorRegionRect { x_pixels: 0.0, y_pixels: 0.0, width_pixels: 128.0, height_pixels: 96.0 },
            target: mover,
            keyboard_focus_eligible: true,
            input_lanes: lanes,
            activation_chord: Some(EditorKeyChord {
                key_code: KEY_W,
                shift: false,
                ctrl: false,
                alt: false,
                meta: false,
            }),
        }],
    };
    let _editor =
        load_kit_export_with_config(harness, wasm, "aether.kit.widget.editor", "editor", config.encode_into_bytes());
}

/// Chunk 0 is one grass region. Rows north of z=8 sit one meter above the
/// southern rows, producing a visible sand wall on the exact height-break
/// line while keeping both horizontal caps the same grass color.
fn height_break_chunk() -> SetChunk {
    let mut height = vec![0; CHUNK_AREA];
    let mut region = vec![0; CHUNK_AREA];
    for z in 0..CHUNK_EDGE {
        for x in 0..CHUNK_EDGE {
            let index = z * CHUNK_EDGE + x;
            height[index] = if z < HEIGHT_BREAK_ROW {
                CLIFF_HEIGHT_OCTIMETERS
            } else {
                0
            };
            region[index] = REGION_ID;
        }
    }
    SetChunk {
        chunk_x: 0,
        chunk_z: 0,
        // The region default supplies the grass fabric deliberately.
        underlay: Vec::new(),
        underlay_points: Vec::new(),
        height_points: Vec::new(),
        overlay: Vec::new(),
        overlay_mask: Vec::new(),
        height,
        region,
        water_plane: Vec::new(),
        smoothing: Vec::new(),
    }
}

fn scene_checks() -> Vec<FrameCheck> {
    [FrameReduction::Coverage, FrameReduction::Centroid, FrameReduction::BoundingBox]
        .into_iter()
        .map(|reduction| FrameCheck { reduction, tolerance: 5, background: Some(CLEAR_SRGB), region: None })
        .collect()
}

/// Capture the mover camera/marker and world into one frame, asking the
/// substrate for basic silhouette checks so `ArtifactGuard` can preserve the
/// exact frame, masks, and measurements when a visual assertion fails.
fn capture_scene(harness: &mut SubstrateHarness, mover: &str, world: &str, label: &'static str) -> SceneCapture {
    let checks = scene_checks();
    let captured = harness
        .execute(vec![(
            label,
            HarnessOp::send_and_await(
                "aether.render",
                &CaptureFrame {
                    mails: vec![envelope(mover, &Render), envelope(world, &Render)],
                    after_mails: Vec::new(),
                    checks: checks.clone(),
                    similarity: None,
                },
            ),
        )])
        .expect("capture sequence");
    match captured.reply::<CaptureFrameResult>(label).expect("decode CaptureFrameResult") {
        CaptureFrameResult::Ok { png, verdict: Some(verdict), .. } => {
            SceneCapture { png, checks, results: verdict.results }
        }
        CaptureFrameResult::Ok { verdict: None, .. } => {
            panic!("capture with checks omitted its verdict")
        }
        CaptureFrameResult::Err { error } => panic!("capture frame: {error}"),
    }
}

#[allow(clippy::cast_precision_loss)]
fn assert_cliff_shape(label: &str, stats: ColorRegionStats, exclusion: ColorRegionStats) {
    assert!(stats.sampled > 0 && stats.matching >= 32, "{label} should contain a sampled sand cliff; stats={stats:?}");
    assert!(
        (0.005..0.20).contains(&stats.fraction),
        "{label} sand cliff should be bounded, neither absent nor a full-screen target; \
         stats={stats:?}",
    );
    assert!(
        exclusion.sampled > 0 && exclusion.fraction < 0.01,
        "{label} neighboring grass strip should exclude the sand target; \
         exclusion={exclusion:?}",
    );

    let centroid = stats.centroid.expect("a visible cliff has a centroid");
    assert!(
        (40.0..88.0).contains(&centroid.x)
            && (CLIFF_OBSERVATION.min_y as f32..CLIFF_OBSERVATION.max_y as f32).contains(&centroid.y),
        "{label} sand centroid should stay centered inside the cliff observation region; \
         centroid={centroid:?} stats={stats:?}",
    );

    let bounds = stats.bounding_box.expect("a visible cliff has a bounding box");
    let width = bounds.max_x - bounds.min_x + 1;
    let height = bounds.max_y - bounds.min_y + 1;
    assert!(
        bounds.min_x > CLIFF_OBSERVATION.min_x
            && bounds.max_x < CLIFF_OBSERVATION.max_x
            && bounds.min_y > CLIFF_OBSERVATION.min_y
            && bounds.max_y < CLIFF_OBSERVATION.max_y
            && (60..=118).contains(&width)
            && (1..=14).contains(&height),
        "{label} sand target should be a wide, shallow interior cliff face; \
         bounds={bounds:?} width={width} height={height} stats={stats:?}",
    );
}

#[test]
fn mover_opts_out_of_interactive_fanout_but_moves_when_the_editor_routes_input() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    // The `EditorShell` arbiter now ships in the `aether-kit-widget` wasm
    // (iamacoffeepot/aether#3950); world + mover still ship in the kit wasm.
    let Some(widget_wasm_path) = require_runtime("aether_kit_widget") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let widget_wasm = fs::read(&widget_wasm_path).expect("read kit-widget wasm");
    let mut harness = SubstrateHarness::builder()
        .size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .with_render()
        .with_component_host()
        .with_actor::<InputCapability>(())
        .build()
        .expect("boot");
    let world = component_address("world");
    let mover_address = component_address("mover");
    let _world_mailbox = load_kit_export(&mut harness, &wasm, "aether.kit.world", "world");
    let mover_mailbox = load_kit_export_with_config(
        &mut harness,
        &wasm,
        "aether.kit.mover",
        "mover",
        MoverConfig { owns_input: false }.encode_into_bytes(),
    );

    harness
        .execute(vec![
            (
                "region",
                HarnessOp::send_mail(
                    world.as_str(),
                    &SetRegion {
                        region_id: REGION_ID,
                        name: "meadow".to_owned(),
                        default_material: Material::Grass.to_u8(),
                        cliff_material: Material::Sand.to_u8(),
                    },
                ),
            ),
            ("chunk", HarnessOp::send_mail(world.as_str(), &height_break_chunk())),
            (
                "retained-window-size",
                HarnessOp::send_mail("aether.input", &WindowSize { width: WINDOW_WIDTH, height: WINDOW_HEIGHT }),
            ),
            ("place", HarnessOp::send_mail(mover_address.as_str(), &MoverTeleport { cell_x: 8, cell_z: 12 })),
            ("settle", HarnessOp::advance(2)),
        ])
        .expect("author world and settle opted-out mover");

    let before = capture_scene(&mut harness, &mover_address, &world, "opt-out-before");
    harness
        .execute(vec![
            ("unrouted-w", HarnessOp::send_mail("aether.input", &Key { code: KEY_W })),
            ("unrouted-advance", HarnessOp::advance(32)),
        ])
        .expect("unrouted input window");
    let blocked = capture_scene(&mut harness, &mover_address, &world, "opt-out-blocked");

    load_mover_editor_shell(&mut harness, &widget_wasm, mover_mailbox);
    harness
        .execute(vec![
            ("routed-w", HarnessOp::send_mail("aether.input", &Key { code: KEY_W })),
            ("routed-advance", HarnessOp::advance(32)),
            ("routed-release", HarnessOp::send_mail("aether.input", &KeyRelease { code: KEY_W })),
        ])
        .expect("editor-routed input window");
    let routed = capture_scene(&mut harness, &mover_address, &world, "editor-routed");

    let before_image = decode_png(&before.png).expect("decode before png");
    let blocked_image = decode_png(&blocked.png).expect("decode blocked png");
    let routed_image = decode_png(&routed.png).expect("decode routed png");
    let blocked_mae = mean_absolute_error(&blocked_image, &before_image).expect("same-size blocked captures");
    let routed_mae = mean_absolute_error(&routed_image, &blocked_image).expect("same-size routed captures");
    assert!(
        blocked_mae < 0.005,
        "owns_input=false must prevent the mover from self-subscribing to W; blocked MAE={blocked_mae:.4}",
    );
    assert!(
        (0.002..0.35).contains(&routed_mae),
        "the editor shell must forward W to the opted-out mover; routed MAE={routed_mae:.4}",
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn held_w_walks_the_mover_past_the_flat_world_cliff_and_release_stops_it() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder()
        .size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .with_render()
        .with_component_host()
        .with_actor::<InputCapability>(())
        .build()
        .expect("boot");

    let world = component_address("world");
    let mover = component_address("mover");
    load_kit_export(&mut harness, &wasm, "aether.kit.world", "world");
    load_kit_export(&mut harness, &wasm, "aether.kit.mover", "mover");

    harness
        .execute(vec![
            (
                "region",
                HarnessOp::send_mail(
                    world.as_str(),
                    &SetRegion {
                        region_id: REGION_ID,
                        name: "meadow".to_owned(),
                        default_material: Material::Grass.to_u8(),
                        cliff_material: Material::Sand.to_u8(),
                    },
                ),
            ),
            ("chunk", HarnessOp::send_mail(world.as_str(), &height_break_chunk())),
            (
                "aspect",
                HarnessOp::send_mail(mover.as_str(), &WindowSize { width: WINDOW_WIDTH, height: WINDOW_HEIGHT }),
            ),
            ("place", HarnessOp::send_mail(mover.as_str(), &MoverTeleport { cell_x: 8, cell_z: 12 })),
            // Let both actors finish wiring before input begins.
            ("settle", HarnessOp::advance(2)),
        ])
        .expect("author height break + settle actors");

    let before = capture_scene(&mut harness, &mover, &world, "before");

    // A cell is 256 octimeters and the mover advances 8 per tick. Ninety-six
    // ticks walk exactly three cells north, bringing the z=8 wall toward the
    // follow camera without crossing it.
    harness
        .execute(vec![
            ("press_w", HarnessOp::send_mail(mover.as_str(), &Key { code: KEY_W })),
            ("walk", HarnessOp::advance(96)),
        ])
        .expect("hold W + walk north");
    let walked_capture = capture_scene(&mut harness, &mover, &world, "moved");

    // Release at a cell center, settle the release, then leave a full cell's
    // worth of ticks between captures. If held-W state did not clear, the
    // second stopped capture would scroll by another cell.
    harness
        .execute(vec![
            ("release_w", HarnessOp::send_mail(mover.as_str(), &KeyRelease { code: KEY_W })),
            ("settle_release", HarnessOp::advance(4)),
        ])
        .expect("release W + settle");
    let stopped_first = capture_scene(&mut harness, &mover, &world, "stopped_first");
    harness.execute(vec![("idle", HarnessOp::advance(32))]).expect("idle for one cell cadence");
    let stopped_second = capture_scene(&mut harness, &mover, &world, "stopped_second");

    // Decode each capture exactly once, then derive every oracle from those
    // four images.
    let before_image = decode_png(&before.png).expect("decode before png");
    let moved_image = decode_png(&walked_capture.png).expect("decode moved png");
    let stopped_first_image = decode_png(&stopped_first.png).expect("decode first stopped png");
    let stopped_second_image = decode_png(&stopped_second.png).expect("decode second stopped png");

    let before_stats =
        target_color_stats(&before_image, SAND_CLIFF_SRGB, CLIFF_COLOR_TOLERANCE, Some(CLIFF_OBSERVATION));
    let moved_stats = target_color_stats(&moved_image, SAND_CLIFF_SRGB, CLIFF_COLOR_TOLERANCE, Some(CLIFF_OBSERVATION));
    let before_exclusion =
        target_color_stats(&before_image, SAND_CLIFF_SRGB, CLIFF_COLOR_TOLERANCE, Some(CLIFF_EXCLUSION));
    let moved_exclusion =
        target_color_stats(&moved_image, SAND_CLIFF_SRGB, CLIFF_COLOR_TOLERANCE, Some(CLIFF_EXCLUSION));
    let walk_mae = mean_absolute_error(&moved_image, &before_image).expect("same-size walk captures");
    let stopped_mae =
        mean_absolute_error(&stopped_second_image, &stopped_first_image).expect("same-size stopped captures");

    eprintln!(
        "sand cliff: before={before_stats:?} moved={moved_stats:?} \
         before_exclusion={before_exclusion:?} moved_exclusion={moved_exclusion:?} \
         walk_mae={walk_mae:.4} stopped_mae={stopped_mae:.4}",
    );

    let expectation = format!(
        "sand cliff remains bounded and shifts down after walking north; \
         before={before_stats:?}; moved={moved_stats:?}; \
         before_exclusion={before_exclusion:?}; moved_exclusion={moved_exclusion:?}; \
         walk_mae={walk_mae:.4}; stopped_mae={stopped_mae:.4}"
    );
    let before_reference = before.png.clone();
    let stopped_reference = stopped_first.png.clone();
    let _before_guard = ArtifactGuard::arm("mover_world_before", before.png, before.checks, before.results)
        .with_expectation(expectation.clone());
    let _moved_guard =
        ArtifactGuard::arm("mover_world_moved", walked_capture.png, walked_capture.checks, walked_capture.results)
            .with_expectation(expectation.clone())
            .with_reference_png(before_reference);
    let _stopped_first_guard =
        ArtifactGuard::arm("mover_world_stopped_first", stopped_first.png, stopped_first.checks, stopped_first.results)
            .with_expectation(expectation.clone());
    let _stopped_second_guard = ArtifactGuard::arm(
        "mover_world_stopped_second",
        stopped_second.png,
        stopped_second.checks,
        stopped_second.results,
    )
    .with_expectation(expectation)
    .with_reference_png(stopped_reference);

    assert_cliff_shape("before", before_stats, before_exclusion);
    assert_cliff_shape("moved", moved_stats, moved_exclusion);

    let before_centroid = before_stats.centroid.expect("before cliff centroid");
    let moved_centroid = moved_stats.centroid.expect("moved cliff centroid");
    let centroid_shift_y = moved_centroid.y - before_centroid.y;
    assert!(
        (6.0..24.0).contains(&centroid_shift_y) && (moved_centroid.x - before_centroid.x).abs() < 5.0,
        "walking north should scroll the symmetric cliff down without lateral drift; \
         before={before_centroid:?} moved={moved_centroid:?} \
         shift_y={centroid_shift_y:.2}",
    );

    let before_bounds = before_stats.bounding_box.expect("before cliff bounds");
    let moved_bounds = moved_stats.bounding_box.expect("moved cliff bounds");
    assert!(
        moved_bounds.min_y >= before_bounds.min_y + 6 && moved_bounds.max_y >= before_bounds.max_y + 6,
        "the sand cliff's whole vertical extent should shift down after the walk; \
         before={before_bounds:?} moved={moved_bounds:?}",
    );

    assert!(
        (0.002..0.35).contains(&walk_mae),
        "the flat world should move without becoming an unrelated frame; \
         walk MAE={walk_mae:.4}, expected 0.002..0.35",
    );
    assert!(
        stopped_mae < 0.005,
        "releasing W should freeze the follow-camera pose across 32 more ticks; \
         stopped MAE={stopped_mae:.4}, expected < 0.005",
    );
}
