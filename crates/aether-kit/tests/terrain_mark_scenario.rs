//! Terrain picking and MarkBook-projected overlays through the real kit wasm.

use aether_substrate_bundle::FullBenchExt;
use std::f32::consts::FRAC_PI_3;
use std::fs;
use std::path::Path;

use aether_actor::Addressable;
use aether_data::Kind;
use aether_kinds::{
    DescribeComponent, DescribeComponentResult, FrameCheck, FrameCheckResult, FrameReduction, LoadComponent,
    LoadResult, NamedMail, Render,
};
use aether_kit::mark::{
    MarkCreate, MarkCreateResult, MarkDelete, MarkDeleteResult, MarkGeometry, MarkRef, MarkUpdate, MarkUpdateResult,
};
use aether_kit::world::{
    CELLS_PER_CHUNK_AREA, Chunk, ChunkPos, Material, PickTerrain, PickTerrainResult, SUBCELLS_PER_CELL, SetCellHeights,
    SetChunk, SetMarkOverlaySelection, SetMarkOverlaySelectionResult, SetMarkOverlayVisibility,
    SetMarkOverlayVisibilityResult, TerrainRay, WaterPlane, World, WorldDirection, WorldLoad, WorldPoint,
    WorldPositionMeters,
};
use aether_math::{Mat4, Vec3};
use aether_render::ViewProjection;
use aether_substrate_bench::{BenchOp, SubstrateBench};
use aether_substrate_bench_capture::visual::{Rect, decode_png, run_checks, target_color_stats};
use aether_substrate_bench_capture::{
    ArtifactGuard,
    test_helpers::{init_save_sandbox, require_runtime, test_namespace_roots},
};

#[allow(unused_imports)]
use aether_kit as _;

const MARK_COMPONENT_NAME: &str = "aether.kit.mark";
const WORLD_COMPONENT_NAME: &str = "world";
const WIDTH: u32 = 192;
const HEIGHT: u32 = 192;
const WIDTH_F32: f32 = 192.0;
const HEIGHT_F32: f32 = 192.0;
const NORMAL_SRGB: [u8; 3] = [50, 220, 235];
const SELECTED_SRGB: [u8; 3] = [255, 190, 48];
const COLOR_TOLERANCE: u8 = 24;
const STONE_SRGB: [u8; 3] = [140, 140, 148];
const POINT_REGION: Rect = Rect { min_x: 58, min_y: 58, max_x: 78, max_y: 78 };
const PATH_REGION: Rect = Rect { min_x: 88, min_y: 50, max_x: 123, max_y: 67 };
const AREA_REGION: Rect = Rect { min_x: 126, min_y: 69, max_x: 163, max_y: 104 };

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

fn load_export(bench: &mut SubstrateBench, wasm_path: &Path, export: &str, name: &str) {
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: fs::read(wasm_path).expect("read kit wasm"),
                    name: Some(name.to_owned()),
                    config: Vec::new(),
                    export: Some(export.to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name: loaded, .. } => assert_eq!(loaded, component_address(name)),
        LoadResult::Err { error } => panic!("load {export}: {error}"),
    }
}

fn top_down_view_projection() -> ViewProjection {
    let eye = Vec3::new(4.0, 10.0, 4.0);
    let target = Vec3::new(4.0, 0.0, 4.0);
    let view = Mat4::look_at_rh(eye, target, Vec3::new(0.0, 0.0, -1.0));
    let projection = Mat4::orthographic_rh(-5.0, 5.0, -5.0, 5.0, 0.1, 100.0);
    ViewProjection { view_proj: (projection * view).to_cols_array() }
}

fn terrain_ray_from_screen_pixel(eye: Vec3, target: Vec3, pixel_x: f32, pixel_y: f32) -> TerrainRay {
    let ndc_x = (pixel_x / WIDTH_F32).mul_add(2.0, -1.0);
    let ndc_y = (pixel_y / HEIGHT_F32).mul_add(-2.0, 1.0);
    let forward = (target - eye).normalize();
    let right = forward.cross(Vec3::Y).normalize();
    let up = right.cross(forward);
    let tangent = (FRAC_PI_3 * 0.5).tan();
    let direction = forward + right * (ndc_x * tangent) + up * (ndc_y * tangent);
    TerrainRay {
        origin: WorldPositionMeters { x_meters: eye.x, y_meters: eye.y, z_meters: eye.z },
        direction: WorldDirection { x_unitless: direction.x, y_unitless: direction.y, z_unitless: direction.z },
        max_distance_meters: 10.0,
    }
}

fn capture(bench: &mut SubstrateBench, world: &str, label: &str) -> Vec<u8> {
    let captured = bench
        .execute(vec![(
            label,
            BenchOp::capture_with_mails(
                vec![envelope("aether.render", &top_down_view_projection()), envelope(world, &Render)],
                Vec::new(),
            ),
        )])
        .expect("capture mark overlay");
    captured.captured(label).expect("capture bytes").to_vec()
}

fn create_mark(bench: &mut SubstrateBench, marks: &str, label: &str, geometry: MarkGeometry) -> MarkRef {
    let created = bench
        .execute(vec![(label, BenchOp::send_and_await(marks, &MarkCreate { geometry, label: label.to_owned() }))])
        .expect("create mark");
    match created.reply::<MarkCreateResult>(label).expect("decode MarkCreateResult") {
        MarkCreateResult::Created { reference } => reference,
        MarkCreateResult::Rejected { error } => panic!("create {label}: {error:?}"),
    }
}

fn overlay_checks() -> Vec<FrameCheck> {
    [POINT_REGION, PATH_REGION, AREA_REGION]
        .into_iter()
        .map(|region| FrameCheck {
            reduction: FrameReduction::Coverage,
            tolerance: 20,
            background: Some(STONE_SRGB),
            region: Some(region.into()),
        })
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)]
fn terrain_pick_and_revisioned_mark_overlays_render_through_real_wasm() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let sandbox = init_save_sandbox("kit-terrain-mark");
    let water_world_path = "terrain-mark-water.world";
    let water_cell_index = 7 * 16;
    let mut authored_water = World::new();
    authored_water.insert_water_plane(1, WaterPlane { level_octimeters: 384 });
    let mut water_chunk = Chunk::empty();
    water_chunk.underlay[water_cell_index] = Material::Water;
    water_chunk.height[water_cell_index] = -256;
    water_chunk.water_plane[water_cell_index] = 1;
    authored_water.insert_chunk(ChunkPos { x: 0, z: 0 }, water_chunk);
    fs::write(sandbox.join(water_world_path), authored_water.to_bytes()).expect("write authored water world fixture");

    let mut bench = SubstrateBench::builder()
        .full()
        .size(WIDTH, HEIGHT)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_export(&mut bench, &wasm_path, "aether.kit.mark", MARK_COMPONENT_NAME);
    load_export(&mut bench, &wasm_path, "aether.kit.world", WORLD_COMPONENT_NAME);
    let marks = component_address(MARK_COMPONENT_NAME);
    let world = component_address(WORLD_COMPONENT_NAME);

    bench
        .execute(vec![(
            "load-water-world",
            BenchOp::send_mail(&world, &WorldLoad { namespace: "save".to_owned(), path: water_world_path.to_owned() }),
        )])
        .expect("load authored water world");
    let water_ray = TerrainRay {
        origin: WorldPositionMeters { x_meters: 0.5, y_meters: 5.0, z_meters: 7.5 },
        direction: WorldDirection { x_unitless: 0.0, y_unitless: -1.0, z_unitless: 0.0 },
        max_distance_meters: 10.0,
    };
    let mut water_hit = None;
    for _ in 0..16 {
        let picked = bench
            .execute(vec![("pick-water", BenchOp::send_and_await(&world, &PickTerrain { ray: water_ray }))])
            .expect("pick authored water plane");
        if let PickTerrainResult::Hit { hit } =
            picked.reply::<PickTerrainResult>("pick-water").expect("decode water PickTerrainResult")
        {
            water_hit = Some(hit);
            break;
        }
    }
    let water_hit = water_hit.expect("registered water-plane world settles within bounded polls");
    assert_eq!(water_hit.surface.cell.x, 0);
    assert_eq!(water_hit.surface.cell.z, 7);
    assert_eq!(water_hit.surface.mark_point, WorldPoint::new(128, 1920));
    assert!(
        (water_hit.position.y_meters - 1.5).abs() < 0.001,
        "pick resolves the registered water plane rather than its -1m lakebed: {water_hit:?}",
    );

    let described = bench
        .execute(vec![(
            "describe-world",
            BenchOp::send_and_await("aether.component", &DescribeComponent { name: world.clone() }),
        )])
        .expect("describe loaded world component");
    let capabilities =
        match described.reply::<DescribeComponentResult>("describe-world").expect("decode DescribeComponentResult") {
            DescribeComponentResult::Ok { capabilities } => capabilities,
            DescribeComponentResult::Err { error } => panic!("describe world: {error}"),
        };
    let handler_names: Vec<_> = capabilities.handlers.iter().map(|handler| handler.name.as_str()).collect();
    for expected in [
        "aether.kit.world.pick_terrain",
        "aether.kit.world.set_mark_overlay_visibility",
        "aether.kit.world.set_mark_overlay_selection",
    ] {
        assert!(handler_names.contains(&expected), "world advertises {expected}: {handler_names:?}");
    }
    for mark_crud in [
        "aether.kit.mark.create",
        "aether.kit.mark.update",
        "aether.kit.mark.delete",
        "aether.kit.mark.get",
        "aether.kit.mark.list",
    ] {
        assert!(!handler_names.contains(&mark_crud), "mark CRUD remains exclusive to MarkBook: {handler_names:?}");
    }

    let mut underlay = vec![Material::Stone.to_u8(); CELLS_PER_CHUNK_AREA];
    underlay[water_cell_index] = Material::Water.to_u8();
    let mut height = vec![0; CELLS_PER_CHUNK_AREA];
    height[water_cell_index] = -256;
    let mut water_plane = vec![0; CELLS_PER_CHUNK_AREA];
    water_plane[water_cell_index] = 1;

    bench
        .execute(vec![
            (
                "chunk",
                BenchOp::send_mail(
                    &world,
                    &SetChunk {
                        chunk_x: 0,
                        chunk_z: 0,
                        underlay,
                        underlay_points: Vec::new(),
                        height_points: Vec::new(),
                        overlay: Vec::new(),
                        overlay_mask: Vec::new(),
                        height,
                        region: Vec::new(),
                        water_plane,
                        smoothing: Vec::new(),
                    },
                ),
            ),
            (
                "relief",
                BenchOp::send_mail(&world, &SetCellHeights { x: 2, z: 2, deltas: vec![128; SUBCELLS_PER_CELL] }),
            ),
        ])
        .expect("author non-flat terrain");

    let pick_eye = Vec3::new(2.5, 5.0, 5.5);
    let pick_target = Vec3::new(2.5, 0.5, 2.5);
    let picked = bench
        .execute(vec![(
            "pick",
            BenchOp::send_and_await(
                &world,
                &PickTerrain {
                    ray: terrain_ray_from_screen_pixel(pick_eye, pick_target, WIDTH_F32 * 0.5, HEIGHT_F32 * 0.5),
                },
            ),
        )])
        .expect("pick raised terrain");
    let hit = match picked.reply::<PickTerrainResult>("pick").expect("decode PickTerrainResult") {
        PickTerrainResult::Hit { hit } => hit,
        other => panic!("expected raised terrain hit, got {other:?}"),
    };
    assert!((hit.position.y_meters - 0.5).abs() < 0.001, "pick follows authored relief rather than flat y=0: {hit:?}");
    assert_eq!(hit.surface.mark_point, WorldPoint::new(640, 640));

    let point = create_mark(&mut bench, &marks, "point", MarkGeometry::Point(hit.surface.mark_point));
    let path = create_mark(
        &mut bench,
        &marks,
        "path",
        MarkGeometry::Path(vec![WorldPoint::new(1024, 512), WorldPoint::new(1280, 512)]),
    );
    let area = create_mark(
        &mut bench,
        &marks,
        "area",
        MarkGeometry::Area(vec![WorldPoint::new(1536, 768), WorldPoint::new(1792, 768), WorldPoint::new(1664, 1024)]),
    );

    let enabled = bench
        .execute(vec![("enable", BenchOp::send_and_await(&world, &SetMarkOverlayVisibility { visible: true }))])
        .expect("enable overlay");
    assert_eq!(
        enabled.reply::<SetMarkOverlayVisibilityResult>("enable").expect("decode visibility result"),
        SetMarkOverlayVisibilityResult { visible: true, synchronized: false }
    );
    let mut synchronized_result = None;
    for _ in 0..16 {
        let synchronized = bench
            .execute(vec![("sync", BenchOp::send_and_await(&world, &SetMarkOverlayVisibility { visible: true }))])
            .expect("poll synchronized overlay");
        let result = synchronized
            .reply::<SetMarkOverlayVisibilityResult>("sync")
            .expect("decode synchronized visibility result");
        if result.synchronized {
            synchronized_result = Some(result);
            break;
        }
    }
    assert_eq!(
        synchronized_result,
        Some(SetMarkOverlayVisibilityResult { visible: true, synchronized: true }),
        "the correlated MarkList reply settles within a bounded poll",
    );
    let selected = bench
        .execute(vec![("select", BenchOp::send_and_await(&world, &SetMarkOverlaySelection { selected: Some(point) }))])
        .expect("select exact point revision");
    assert_eq!(
        selected.reply::<SetMarkOverlaySelectionResult>("select").expect("decode selection result"),
        SetMarkOverlaySelectionResult::Selected { reference: point }
    );

    let before_png = capture(&mut bench, &world, "before");
    let before_image = decode_png(&before_png).expect("decode initial overlay");
    assert_eq!((before_image.width, before_image.height), (WIDTH, HEIGHT));
    let checks = overlay_checks();
    let verdict = run_checks(before_image.rgba.clone(), before_image.width, before_image.height, &checks);
    let _before_guard = ArtifactGuard::arm("terrain_mark_overlay_before", before_png, checks, verdict.results.clone())
        .with_expectation("selected point is amber; path and closed area are cyan and terrain-anchored");
    for result in &verdict.results {
        match result {
            FrameCheckResult::Coverage { fraction, .. } => assert!(
                *fraction > 0.005,
                "each bounded point/path/area region contains visible overlay pixels: {result:?}",
            ),
            other => panic!("expected coverage result, got {other:?}"),
        }
    }
    let selected_before = target_color_stats(&before_image, SELECTED_SRGB, COLOR_TOLERANCE, Some(POINT_REGION));
    let path_before = target_color_stats(&before_image, NORMAL_SRGB, COLOR_TOLERANCE, Some(PATH_REGION));
    let area_before = target_color_stats(&before_image, NORMAL_SRGB, COLOR_TOLERANCE, Some(AREA_REGION));
    assert!(selected_before.matching >= 4, "selected point: {selected_before:?}");
    assert!(path_before.matching >= 4, "path ribbon: {path_before:?}");
    assert!(area_before.matching >= 4, "area perimeter: {area_before:?}");

    let mutated = bench
        .execute(vec![
            (
                "update",
                BenchOp::send_and_await(
                    &marks,
                    &MarkUpdate { id: point.id, geometry: None, label: Some("updated".to_owned()) },
                ),
            ),
            ("delete", BenchOp::send_and_await(&marks, &MarkDelete { id: area.id })),
        ])
        .expect("update point and delete area");
    let point_v2 = MarkRef { id: point.id, revision: 2 };
    assert_eq!(
        mutated.reply::<MarkUpdateResult>("update").expect("decode MarkUpdateResult"),
        MarkUpdateResult::Updated { reference: point_v2 }
    );
    assert_eq!(
        mutated.reply::<MarkDeleteResult>("delete").expect("decode MarkDeleteResult"),
        MarkDeleteResult::Deleted { reference: area }
    );

    bench
        .execute(vec![("refresh", BenchOp::send_and_await(&world, &SetMarkOverlayVisibility { visible: true }))])
        .expect("refresh mutated MarkBook projection");
    let mut stale_result = None;
    for _ in 0..16 {
        let stale = bench
            .execute(vec![(
                "stale",
                BenchOp::send_and_await(&world, &SetMarkOverlaySelection { selected: Some(point) }),
            )])
            .expect("poll old selected revision");
        let result = stale.reply::<SetMarkOverlaySelectionResult>("stale").expect("decode stale selection result");
        if matches!(result, SetMarkOverlaySelectionResult::Stale { .. }) {
            stale_result = Some(result);
            break;
        }
    }
    assert_eq!(
        stale_result,
        Some(SetMarkOverlaySelectionResult::Stale { requested: point, current: point_v2 }),
        "the refreshed projection classifies the old revision within a bounded poll",
    );

    let after_png = capture(&mut bench, &world, "after");
    let after_image = decode_png(&after_png).expect("decode refreshed overlay");
    let checks = overlay_checks();
    let verdict = run_checks(after_image.rgba.clone(), after_image.width, after_image.height, &checks);
    let _after_guard = ArtifactGuard::arm("terrain_mark_overlay_after", after_png, checks, verdict.results)
        .with_expectation("updated point remains as ordinary cyan, path remains, deleted area disappears");
    let selected_after = target_color_stats(&after_image, SELECTED_SRGB, COLOR_TOLERANCE, Some(POINT_REGION));
    let point_after = target_color_stats(&after_image, NORMAL_SRGB, COLOR_TOLERANCE, Some(POINT_REGION));
    let path_after = target_color_stats(&after_image, NORMAL_SRGB, COLOR_TOLERANCE, Some(PATH_REGION));
    let area_after = target_color_stats(&after_image, NORMAL_SRGB, COLOR_TOLERANCE, Some(AREA_REGION));
    assert_eq!(selected_after.matching, 0, "revision advance clears the amber highlight: {selected_after:?}");
    assert!(point_after.matching >= 4, "updated point stays visible: {point_after:?}");
    assert!(path_after.matching >= 4, "unmodified path stays visible: {path_after:?}");
    assert_eq!(area_after.matching, 0, "atomic refresh removes deleted area geometry: {area_after:?}");
    assert_ne!(path, area, "separate MarkBook allocations stay distinct");
}
