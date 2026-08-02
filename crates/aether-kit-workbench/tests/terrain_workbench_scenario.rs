//! Full terrain annotation workbench flow through the real workbench wasm.

use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use std::f32::consts::FRAC_PI_3;
use std::fs;
use std::path::Path;

use aether_actor::Addressable;
use aether_component::WasmTrampoline;
use aether_data::{Kind, MailboxId};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::ArtifactGuard;
use aether_harness_substrate_capture::test_helpers::{envelope, require_runtime};
use aether_harness_substrate_capture::visual::{Rect, decode_png, run_checks, target_color_stats};
use aether_kinds::keycode::{KEY_A, KEY_ENTER, KEY_UP};
use aether_kinds::{
    FrameCheck, FrameCheckResult, FrameReduction, Key, LoadComponent, LoadResult, Modifiers, MouseButton,
    MouseButtonRelease, Render, TextInput, Tick, WindowId,
};
use aether_kit_commons::console::ConsoleConfig;
use aether_kit_terrain::mark::{Mark, MarkGeometry, MarkGet, MarkGetResult};
use aether_kit_terrain::terra::{TerraConfig, TerraQuery, TerraQueryResult};
use aether_kit_terrain::world::{
    AutomatonRule, BrushParameters, CELLS_PER_CHUNK_AREA, Material, OperatorBudget, ProposalError, SUBCELLS_PER_CELL,
    SetCellHeights, SetChunk, WorldPositionMeters,
};
use aether_kit_widget::EditorRegionRect;
use aether_kit_widget::theme::Theme;
use aether_kit_workbench::{
    WorkbenchCamera, WorkbenchConfig, WorkbenchFailure, WorkbenchInitialSettings, WorkbenchLayout, WorkbenchMarkMode,
    WorkbenchOperator, WorkbenchPanelSettings, WorkbenchQuery, WorkbenchQueryResult,
};
use aether_text::{LoadFontBytes, LoadFontResult, TextCapability};

const MARK_COMPONENT_NAME: &str = "aether.kit.mark";
const WORLD_COMPONENT_NAME: &str = "world";
const TERRA_COMPONENT_NAME: &str = "terra";
const WORKBENCH_COMPONENT_NAME: &str = "workbench";
const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const TEST_WINDOW_ID: WindowId = WindowId(1);
const SELECTED_SRGB: [u8; 3] = [255, 190, 48];
const STONE_SRGB: [u8; 3] = [140, 140, 148];
const COLOR_TOLERANCE: u8 = 28;
const VIEWPORT_REGION: Rect = Rect { min_x: 180, min_y: 0, max_x: 639, max_y: 359 };
const AUTHORED_REGION: Rect = Rect { min_x: 300, min_y: 90, max_x: 500, max_y: 270 };

fn component_address(name: &str) -> String {
    format!("aether.component/{}:{name}", WasmTrampoline::NAMESPACE)
}

fn child_address(parent: &str, subname: &str) -> String {
    format!("{parent}/{}:{subname}", WasmTrampoline::NAMESPACE)
}

fn load_export(
    harness: &mut SubstrateHarness,
    wasm_path: &Path,
    export: &str,
    name: &str,
    config: Vec<u8>,
) -> MailboxId {
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm: fs::read(wasm_path).expect("read kit wasm"),
                    name: Some(name.to_owned()),
                    config,
                    export: Some(export.to_owned()),
                    replica: None,
                },
            ),
        )])
        .expect("load component sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name: loaded_name, mailbox_id, .. } => {
            assert_eq!(loaded_name, component_address(name));
            mailbox_id
        }
        LoadResult::Err { error } => panic!("load {export}: {error}"),
    }
}

fn load_panel_font(harness: &mut SubstrateHarness) -> u32 {
    let loaded = harness
        .execute(vec![(
            "font",
            HarnessOp::send_and_await_reply(
                "aether.text",
                &LoadFontBytes {
                    name: "terrain-workbench.ttf".to_owned(),
                    bytes: include_bytes!("../assets/fonts/SourceCodePro-Regular.ttf").to_vec(),
                },
            ),
        )])
        .expect("load workbench panel font");
    match loaded.reply::<LoadFontResult>("font").expect("decode LoadFontResult") {
        LoadFontResult::Ok { font_id, .. } => font_id,
        LoadFontResult::Err { error, .. } => panic!("load workbench panel font: {error}"),
    }
}

fn click(harness: &mut SubstrateHarness, x: f32, y: f32) {
    let press = MouseButton { window: TEST_WINDOW_ID, button: 0, x, y };
    let release = MouseButtonRelease { window: TEST_WINDOW_ID, button: 0, x, y };
    harness
        .execute(vec![
            ("press", HarnessOp::window_event(TEST_WINDOW_ID, &press)),
            ("release", HarnessOp::window_event(TEST_WINDOW_ID, &release)),
        ])
        .expect("raw pointer click");
    harness.execute(vec![("settle", HarnessOp::advance(3))]).expect("settle raw pointer click");
}

fn send_input<K: Kind>(harness: &mut SubstrateHarness, mail: &K) {
    harness.execute(vec![("window-input", HarnessOp::window_event(TEST_WINDOW_ID, mail))]).expect("raw input mail");
}

fn query_workbench(harness: &mut SubstrateHarness, workbench: &str) -> WorkbenchQueryResult {
    harness
        .execute(vec![("query", HarnessOp::send_and_await_reply(workbench, &WorkbenchQuery))])
        .expect("query workbench")
        .reply::<WorkbenchQueryResult>("query")
        .expect("decode WorkbenchQueryResult")
}

fn wait_for_idle(harness: &mut SubstrateHarness, workbench: &str) -> WorkbenchQueryResult {
    for _ in 0..16 {
        let result = query_workbench(harness, workbench);
        if !result.busy {
            return result;
        }
        harness.execute(vec![("settle", HarnessOp::advance(1))]).expect("advance workbench");
    }
    panic!("workbench did not settle within sixteen frames")
}

fn capture(harness: &mut SubstrateHarness, workbench: &str, world: &str, label: &'static str) -> Vec<u8> {
    let viewport = child_address(workbench, "viewport");
    let panel = child_address(workbench, "tools");
    let console = child_address(workbench, "console");
    let captured = harness
        .execute(vec![(
            label,
            HarnessOp::capture_with_mails(
                vec![
                    envelope(&viewport, &Render),
                    envelope(world, &Render),
                    envelope(&panel, &Tick),
                    envelope(&console, &Tick),
                ],
                Vec::new(),
            ),
        )])
        .expect("capture workbench frame");
    captured.captured(label).expect("captured PNG").to_vec()
}

fn region_rgba(png: &[u8], region: Rect) -> Vec<u8> {
    let image = decode_png(png).expect("decode region PNG");
    let mut bytes = Vec::new();
    for y in region.min_y..=region.max_y {
        for x in region.min_x..=region.max_x {
            let offset = ((y * image.width + x) * 4) as usize;
            bytes.extend_from_slice(&image.rgba[offset..offset + 4]);
        }
    }
    bytes
}

fn differing_pixels(left_png: &[u8], right_png: &[u8], region: Rect) -> usize {
    let left = region_rgba(left_png, region);
    let right = region_rgba(right_png, region);
    left.chunks_exact(4).zip(right.chunks_exact(4)).filter(|(left, right)| left != right).count()
}

fn replace_numeric_with_zero(harness: &mut SubstrateHarness) {
    click(harness, 90.0, 102.0);
    send_input(harness, &Modifiers { window: TEST_WINDOW_ID, ctrl: true, ..Modifiers::default() });
    send_input(harness, &Key { window: TEST_WINDOW_ID, code: KEY_A });
    send_input(harness, &TextInput { window: TEST_WINDOW_ID, text: "0".to_owned() });
    send_input(harness, &Modifiers { window: TEST_WINDOW_ID, ..Modifiers::default() });
    send_input(harness, &Key { window: TEST_WINDOW_ID, code: KEY_ENTER });
}

#[test]
#[allow(clippy::too_many_lines)]
fn terrain_annotation_workbench_runs_the_full_raw_input_proposal_loop() {
    // The mark / world / terra exports live in the `aether-kit-terrain` wasm and
    // the workbench export in this crate's own `aether-kit-workbench` wasm, so
    // this scenario loads both.
    let Some(terrain_wasm_path) = require_runtime("aether_kit_terrain") else {
        return;
    };
    let Some(workbench_wasm_path) = require_runtime("aether_kit_workbench") else {
        return;
    };
    let mut harness = SubstrateHarness::builder()
        .size(WIDTH, HEIGHT)
        .with_render()
        .with_component_host()
        .with_actor::<TextCapability>(())
        .build()
        .expect("boot SubstrateHarness");
    let mark_book_mailbox =
        load_export(&mut harness, &terrain_wasm_path, "aether.kit.mark", MARK_COMPONENT_NAME, Vec::new());
    let world_mailbox =
        load_export(&mut harness, &terrain_wasm_path, "aether.kit.world", WORLD_COMPONENT_NAME, Vec::new());
    let terra_mailbox = load_export(
        &mut harness,
        &terrain_wasm_path,
        "aether.kit.terra",
        TERRA_COMPONENT_NAME,
        TerraConfig { mark_book_mailbox }.encode_into_bytes(),
    );
    let panel_font_id = load_panel_font(&mut harness);
    let workbench_config = WorkbenchConfig {
        mark_book_mailbox,
        terra_mailbox,
        world_mailbox,
        layout: WorkbenchLayout {
            tools: EditorRegionRect { x_pixels: 0.0, y_pixels: 0.0, width_pixels: 180.0, height_pixels: 480.0 },
            viewport: EditorRegionRect { x_pixels: 180.0, y_pixels: 0.0, width_pixels: 460.0, height_pixels: 360.0 },
            console: EditorRegionRect { x_pixels: 180.0, y_pixels: 360.0, width_pixels: 460.0, height_pixels: 120.0 },
        },
        camera: WorkbenchCamera {
            eye: WorldPositionMeters { x_meters: 4.0, y_meters: 10.0, z_meters: 4.0 },
            target: WorldPositionMeters { x_meters: 4.0, y_meters: 0.0, z_meters: 4.0 },
            vertical_field_of_view_radians: FRAC_PI_3,
            near_clip_meters: 0.1,
            far_clip_meters: 100.0,
            maximum_pick_distance_meters: 24.0,
        },
        panel: WorkbenchPanelSettings {
            font_namespace: String::new(),
            font_path: String::new(),
            theme: Theme { font_id: panel_font_id, ..Theme::default() },
        },
        console: ConsoleConfig { panel_height: 120.0, owns_input: false, ..ConsoleConfig::default() },
        initial: WorkbenchInitialSettings {
            mark_mode: WorkbenchMarkMode::Path,
            operator: WorkbenchOperator::Brush,
            brush: BrushParameters { radius_octimeters: 96, spacing_octimeters: 96, material: Material::Grass.to_u8() },
            automaton: AutomatonRule::Grow { material: Material::Grass.to_u8(), generations: 1 },
            budget: OperatorBudget { max_steps: 32, max_subcells: 8192 },
        },
    };
    load_export(
        &mut harness,
        &workbench_wasm_path,
        "aether.kit.workbench",
        WORKBENCH_COMPONENT_NAME,
        workbench_config.encode_into_bytes(),
    );

    let marks = component_address(MARK_COMPONENT_NAME);
    let world = component_address(WORLD_COMPONENT_NAME);
    let terra = component_address(TERRA_COMPONENT_NAME);
    let workbench = component_address(WORKBENCH_COMPONENT_NAME);

    harness
        .execute(vec![
            (
                "chunk",
                HarnessOp::send_and_settle(
                    &world,
                    &SetChunk {
                        chunk_x: 0,
                        chunk_z: 0,
                        underlay: vec![Material::Stone.to_u8(); CELLS_PER_CHUNK_AREA],
                        underlay_points: Vec::new(),
                        height_points: Vec::new(),
                        overlay: Vec::new(),
                        overlay_mask: Vec::new(),
                        height: Vec::new(),
                        region: Vec::new(),
                        water_plane: Vec::new(),
                        smoothing: Vec::new(),
                    },
                ),
            ),
            (
                "relief",
                HarnessOp::send_and_settle(
                    &world,
                    &SetCellHeights { x: 4, z: 4, deltas: vec![128; SUBCELLS_PER_CELL] },
                ),
            ),
            ("activate", HarnessOp::advance(3)),
        ])
        .expect("seed non-flat terrain and activate workbench");

    click(&mut harness, 90.0, 42.0);
    send_input(&mut harness, &TextInput { window: TEST_WINDOW_ID, text: "ridge instruction".to_owned() });
    send_input(&mut harness, &Key { window: TEST_WINDOW_ID, code: KEY_ENTER });
    assert_eq!(query_workbench(&mut harness, &workbench).draft.instruction, "ridge instruction");

    click(&mut harness, 90.0, 102.0);
    send_input(&mut harness, &Key { window: TEST_WINDOW_ID, code: KEY_UP });
    assert_eq!(query_workbench(&mut harness, &workbench).draft.brush.radius_octimeters, 97);

    click(&mut harness, 380.0, 180.0);
    click(&mut harness, 440.0, 180.0);
    let drafted = wait_for_idle(&mut harness, &workbench);
    assert_eq!(drafted.draft.points.len(), 2);
    click(&mut harness, 90.0, 252.0);
    let authored = wait_for_idle(&mut harness, &workbench);
    assert_eq!(authored.selection.len(), 1);
    assert!(authored.draft.points.is_empty());
    let selected = authored.selection[0];

    let terra_state = harness
        .execute(vec![("terra", HarnessOp::send_and_await_reply(&terra, &TerraQuery))])
        .expect("query terra")
        .reply::<TerraQueryResult>("terra")
        .expect("decode TerraQueryResult");
    assert_eq!(terra_state.selection, authored.selection);
    let mark = harness
        .execute(vec![("mark", HarnessOp::send_and_await_reply(&marks, &MarkGet { id: selected.id }))])
        .expect("get selected mark")
        .reply::<MarkGetResult>("mark")
        .expect("decode MarkGetResult")
        .mark
        .expect("selected mark exists");
    assert_eq!(mark.reference(), selected);
    assert_eq!(mark.label, "ridge instruction");
    assert!(matches!(&mark.geometry, MarkGeometry::Path(points) if points.len() == 2));

    let selected_png = capture(&mut harness, &workbench, &world, "selected_overlay");
    let selected_image = decode_png(&selected_png).expect("decode selected overlay frame");
    let checks = vec![FrameCheck {
        reduction: FrameReduction::Coverage,
        tolerance: 20,
        background: Some(STONE_SRGB),
        region: Some(AUTHORED_REGION.into()),
    }];
    let verdict = run_checks(selected_image.rgba.clone(), selected_image.width, selected_image.height, &checks);
    let _overlay_guard =
        ArtifactGuard::arm("terrain_workbench_selected_overlay", selected_png.clone(), checks, verdict.results.clone())
            .with_expectation("tool panel and terrain viewport render with the selected instructed path overlay");
    assert!(matches!(
        verdict.results.as_slice(),
        [FrameCheckResult::Coverage { fraction, .. }] if *fraction > 0.001
    ));
    assert!(
        target_color_stats(&selected_image, SELECTED_SRGB, COLOR_TOLERANCE, Some(AUTHORED_REGION)).matching >= 4,
        "selected path is visible in the bounded authored region",
    );

    click(&mut harness, 90.0, 282.0);
    let staged = wait_for_idle(&mut harness, &workbench);
    assert!(staged.proposal.is_some());
    let baseline_png = capture(&mut harness, &workbench, &world, "committed_baseline");
    assert_eq!(
        region_rgba(&baseline_png, VIEWPORT_REGION),
        region_rgba(&selected_png, VIEWPORT_REGION),
        "staging alone leaves committed terrain and the selected overlay unchanged",
    );

    click(&mut harness, 90.0, 312.0);
    let preview_state = wait_for_idle(&mut harness, &workbench);
    assert!(preview_state.proposal.as_ref().is_some_and(|proposal| proposal.preview_active));
    let preview_png = capture(&mut harness, &workbench, &world, "discard_preview");
    let preview_image = decode_png(&preview_png).expect("decode discard preview frame");
    let preview_checks = vec![FrameCheck {
        reduction: FrameReduction::Coverage,
        tolerance: 20,
        background: Some(STONE_SRGB),
        region: Some(AUTHORED_REGION.into()),
    }];
    let preview_verdict =
        run_checks(preview_image.rgba.clone(), preview_image.width, preview_image.height, &preview_checks);
    let _preview_guard = ArtifactGuard::arm(
        "terrain_workbench_bounded_preview",
        preview_png.clone(),
        preview_checks,
        preview_verdict.results,
    )
    .with_reference_png(baseline_png.clone())
    .with_expectation("the staged brush changes terrain only inside the bounded authored region");
    assert!(differing_pixels(&baseline_png, &preview_png, AUTHORED_REGION) > 12);
    assert!(
        differing_pixels(&baseline_png, &preview_png, VIEWPORT_REGION)
            < ((VIEWPORT_REGION.max_x - VIEWPORT_REGION.min_x + 1)
                * (VIEWPORT_REGION.max_y - VIEWPORT_REGION.min_y + 1)) as usize
                / 3,
        "proposal preview remains a bounded terrain change",
    );

    click(&mut harness, 90.0, 372.0);
    assert!(wait_for_idle(&mut harness, &workbench).proposal.is_none());
    let discarded_png = capture(&mut harness, &workbench, &world, "discarded");
    assert_eq!(region_rgba(&discarded_png, VIEWPORT_REGION), region_rgba(&baseline_png, VIEWPORT_REGION));

    click(&mut harness, 90.0, 282.0);
    assert!(wait_for_idle(&mut harness, &workbench).proposal.is_some());
    click(&mut harness, 90.0, 312.0);
    assert!(wait_for_idle(&mut harness, &workbench).proposal.as_ref().is_some_and(|proposal| proposal.preview_active));
    let accepted_preview_png = capture(&mut harness, &workbench, &world, "accepted_preview");
    click(&mut harness, 90.0, 342.0);
    assert!(wait_for_idle(&mut harness, &workbench).proposal.is_none());
    let committed_png = capture(&mut harness, &workbench, &world, "accepted_commit");
    let identity_checks = vec![FrameCheck {
        reduction: FrameReduction::Coverage,
        tolerance: 5,
        background: None,
        region: Some(VIEWPORT_REGION.into()),
    }];
    let committed_image = decode_png(&committed_png).expect("decode committed workbench frame");
    let identity_verdict =
        run_checks(committed_image.rgba.clone(), committed_image.width, committed_image.height, &identity_checks);
    let _identity_guard = ArtifactGuard::arm(
        "terrain_workbench_preview_commit_identity",
        committed_png.clone(),
        identity_checks,
        identity_verdict.results,
    )
    .with_reference_png(accepted_preview_png.clone())
    .with_expectation("accepted terrain pixels are byte-identical to the staged preview in the viewport");
    assert_eq!(region_rgba(&committed_png, VIEWPORT_REGION), region_rgba(&accepted_preview_png, VIEWPORT_REGION));

    replace_numeric_with_zero(&mut harness);
    assert_eq!(query_workbench(&mut harness, &workbench).draft.brush.radius_octimeters, 0);
    click(&mut harness, 90.0, 282.0);
    let rejected = wait_for_idle(&mut harness, &workbench);
    assert!(rejected.proposal.is_none(), "a rejected no-touch operation enables no preview or accept candidate");
    assert!(matches!(
        rejected.failure,
        Some(WorkbenchFailure::Proposal { error: ProposalError::NoTouchedChunks { .. } })
    ));

    let Mark { label, .. } = mark;
    assert_eq!(label, "ridge instruction");
}
