//! Acceptance: a small marching-squares case atlas renders as one isolated,
//! scored contact sheet (issue 2949).
//!
//! The fixture builder places case-local mutations at the center of fixed
//! grid slots. Each slot owns two empty cells on every side, matching the
//! mesher's bounded two-cell apron, so one case cannot feed another case's
//! scored region. The starter atlas exercises an empty case, an all-inside
//! single-material case, a two-material window, and a height-break cliff.
//! A fixed top-down orthographic projection frames the whole grid without a
//! camera component or font dependency; grid order plus the opt-in legend is
//! the stable labelling surface.
//!
//! Skipped when no wgpu adapter is available or the `aether_kit` wasm has not
//! been pre-built. CI sets `AETHER_REQUIRE_RUNTIME=1`, turning either skip
//! into a hard failure.

// Integration-test skip diagnostics intentionally surface beside test output.
#![allow(clippy::print_stderr)]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::iter::once;
use std::path::{Path, PathBuf};

use aether_actor::Addressable;
use aether_data::{Kind, MailboxId};
use aether_kinds::{
    CaptureFrame, CaptureFrameResult, FrameCheck, FrameCheckResult, FrameRect, FrameReduction, FrameVerdict,
    LoadComponent, LoadResult, NamedMail, Render,
};
use aether_kit::world::{
    CELLS_PER_CHUNK, CELLS_PER_CHUNK_AREA, CellPos, ChunkPos, Material, SUBCELLS_PER_CELL, SUBCELLS_PER_CELL_EDGE,
    SetCellHeights, SetCellPoints, SetChunk, SetRegion, WorldPoint,
};
use aether_math::{Mat4, Vec3};
use aether_render::{RenderCapability, ViewProjection};
use aether_substrate_bundle::substrate_bench::{ArtifactGuard, BenchOp, SubstrateBench, test_helpers::require_runtime};

const WINDOW_WIDTH: u32 = 320;
const WINDOW_HEIGHT: u32 = 320;
const GRID_COLUMNS: usize = 2;
const OCTIMETERS_PER_CELL: i32 = 256;

/// Re-grounded to `world/mesher/constants.rs`: the contour reader is capped
/// at two cells (`2 * SUB`) outside the case being marched.
const SUB: i32 = SUBCELLS_PER_CELL_EDGE.cast_signed();
const MAX_APRON_SUBCELLS: i32 = 2 * SUB;
const GUTTER_CELLS: i32 = MAX_APRON_SUBCELLS / SUB;
const SLOT_EDGE_CELLS: i32 = GUTTER_CELLS * 2 + 1;
const _: () = assert!(GUTTER_CELLS >= 2, "each slot must reserve at least two empty gutter cells per side");

const CLEAR_SRGB: [u8; 3] = [63, 75, 97];
const CLIFF_REGION_ID: u32 = 1;

#[derive(Clone)]
enum AtlasMutation {
    CellPoints {
        offset: CellPos,
        points: Vec<u8>,
    },
    CellHeights {
        offset: CellPos,
        deltas: Vec<i16>,
    },
    /// A case-local base-cell write. The fixture compiler groups these into
    /// real `SetChunk` payloads so region membership remains on the ordinary
    /// world-mail path without forcing case authors to hand-build 256 cells.
    CellBase {
        offset: CellPos,
        underlay: Material,
        height_octimeters: i32,
        region_id: u32,
    },
    Region(SetRegion),
}

#[derive(Clone)]
struct AtlasCaseSpec {
    name: &'static str,
    mutations: Vec<AtlasMutation>,
}

#[derive(Clone, Copy)]
struct AtlasGridPlacement {
    case_index: usize,
    slot_column: usize,
    slot_row: usize,
    anchor_cell: CellPos,
    anchor_world: WorldPoint,
    chunk: ChunkPos,
}

#[derive(Clone, Copy)]
struct AtlasWorldBounds {
    min: WorldPoint,
    max: WorldPoint,
}

struct AtlasFixture {
    cases: Vec<AtlasCaseSpec>,
    placements: Vec<AtlasGridPlacement>,
    columns: usize,
    rows: usize,
}

struct AtlasChunkDraft {
    position: ChunkPos,
    underlay: Vec<u8>,
    height: Vec<i32>,
    region: Vec<u32>,
}

impl AtlasChunkDraft {
    fn new(position: ChunkPos) -> Self {
        Self {
            position,
            underlay: vec![Material::Void.to_u8(); CELLS_PER_CHUNK_AREA],
            height: vec![0; CELLS_PER_CHUNK_AREA],
            region: vec![0; CELLS_PER_CHUNK_AREA],
        }
    }

    fn set_cell(&mut self, cell: CellPos, underlay: Material, height_octimeters: i32, region_id: u32) {
        assert_eq!(cell.chunk(), self.position, "chunk draft must contain its authored cell");
        let local_x = usize::try_from(cell.x.rem_euclid(CELLS_PER_CHUNK)).expect("local x is non-negative");
        let local_z = usize::try_from(cell.z.rem_euclid(CELLS_PER_CHUNK)).expect("local z is non-negative");
        let chunk_edge = usize::try_from(CELLS_PER_CHUNK).expect("chunk edge is positive");
        let index = local_z * chunk_edge + local_x;
        self.underlay[index] = underlay.to_u8();
        self.height[index] = height_octimeters;
        self.region[index] = region_id;
    }

    fn into_mail(self) -> SetChunk {
        SetChunk {
            chunk_x: self.position.x,
            chunk_z: self.position.z,
            underlay: self.underlay,
            underlay_points: Vec::new(),
            height_points: Vec::new(),
            overlay: Vec::new(),
            overlay_mask: Vec::new(),
            height: self.height,
            region: self.region,
            water_plane: Vec::new(),
            smoothing: Vec::new(),
        }
    }
}

impl AtlasFixture {
    fn new(cases: Vec<AtlasCaseSpec>) -> Self {
        assert!(!cases.is_empty(), "an atlas needs at least one named case");

        let rows = cases.len().div_ceil(GRID_COLUMNS);
        let placements = cases
            .iter()
            .enumerate()
            .map(|(case_index, _)| {
                let slot_column = case_index % GRID_COLUMNS;
                let slot_row = case_index / GRID_COLUMNS;
                let anchor_cell = CellPos {
                    x: i32::try_from(slot_column).expect("atlas column fits i32") * SLOT_EDGE_CELLS + GUTTER_CELLS,
                    z: i32::try_from(slot_row).expect("atlas row fits i32") * SLOT_EDGE_CELLS + GUTTER_CELLS,
                };
                let anchor_world = WorldPoint::new(
                    anchor_cell.x * OCTIMETERS_PER_CELL + OCTIMETERS_PER_CELL / 2,
                    anchor_cell.z * OCTIMETERS_PER_CELL + OCTIMETERS_PER_CELL / 2,
                );
                AtlasGridPlacement {
                    case_index,
                    slot_column,
                    slot_row,
                    anchor_cell,
                    anchor_world,
                    chunk: anchor_cell.chunk(),
                }
            })
            .collect();

        Self { cases, placements, columns: GRID_COLUMNS, rows }
    }

    fn bounds(&self) -> AtlasWorldBounds {
        AtlasWorldBounds {
            min: WorldPoint::new(0, 0),
            max: WorldPoint::new(
                i32::try_from(self.columns).expect("atlas columns fit i32") * SLOT_EDGE_CELLS * OCTIMETERS_PER_CELL,
                i32::try_from(self.rows).expect("atlas rows fit i32") * SLOT_EDGE_CELLS * OCTIMETERS_PER_CELL,
            ),
        }
    }

    fn placement(&self, case_index: usize) -> AtlasGridPlacement {
        self.placements[case_index]
    }

    fn apply_all(&self, bench: &mut SubstrateBench, world: &str) {
        self.apply_cases(bench, world, 0..self.cases.len());
    }

    fn apply_case(&self, bench: &mut SubstrateBench, world: &str, case_index: usize) {
        self.apply_cases(bench, world, once(case_index));
    }

    fn apply_cases(&self, bench: &mut SubstrateBench, world: &str, selected: impl IntoIterator<Item = usize>) {
        let selected: Vec<usize> = selected.into_iter().collect();
        let mut chunks = BTreeMap::<ChunkPos, AtlasChunkDraft>::new();

        for &case_index in &selected {
            let placement = self.placement(case_index);
            for mutation in &self.cases[case_index].mutations {
                if let AtlasMutation::CellBase { offset, underlay, height_octimeters, region_id } = mutation {
                    let cell = offset_cell(placement.anchor_cell, *offset);
                    chunks.entry(cell.chunk()).or_insert_with(|| AtlasChunkDraft::new(cell.chunk())).set_cell(
                        cell,
                        *underlay,
                        *height_octimeters,
                        *region_id,
                    );
                }
            }
        }

        for &case_index in &selected {
            for mutation in &self.cases[case_index].mutations {
                if let AtlasMutation::Region(region) = mutation {
                    bench
                        .execute(vec![("set_region", BenchOp::send_mail(world, region))])
                        .expect("register atlas region");
                }
            }
        }
        for chunk in chunks.into_values() {
            bench
                .execute(vec![("set_chunk", BenchOp::send_mail(world, &chunk.into_mail()))])
                .expect("author atlas base chunk");
        }

        for case_index in selected {
            let placement = self.placement(case_index);
            for mutation in &self.cases[case_index].mutations {
                match mutation {
                    AtlasMutation::CellPoints { offset, points } => {
                        let cell = offset_cell(placement.anchor_cell, *offset);
                        bench
                            .execute(vec![(
                                "set_cell_points",
                                BenchOp::send_mail(
                                    world,
                                    &SetCellPoints { x: cell.x, z: cell.z, points: points.clone() },
                                ),
                            )])
                            .expect("author atlas material points");
                    }
                    AtlasMutation::CellHeights { offset, deltas } => {
                        let cell = offset_cell(placement.anchor_cell, *offset);
                        bench
                            .execute(vec![(
                                "set_cell_heights",
                                BenchOp::send_mail(
                                    world,
                                    &SetCellHeights { x: cell.x, z: cell.z, deltas: deltas.clone() },
                                ),
                            )])
                            .expect("author atlas height points");
                    }
                    AtlasMutation::CellBase { .. } | AtlasMutation::Region(_) => {}
                }
            }
        }
    }

    fn slot_frame_rect(&self, placement: AtlasGridPlacement) -> FrameRect {
        let columns = u32::try_from(self.columns).expect("atlas columns fit u32");
        let rows = u32::try_from(self.rows).expect("atlas rows fit u32");
        let column = u32::try_from(placement.slot_column).expect("slot column fits u32");
        let row = u32::try_from(placement.slot_row).expect("slot row fits u32");
        FrameRect {
            min_x: column * WINDOW_WIDTH / columns,
            min_y: row * WINDOW_HEIGHT / rows,
            max_x: (column + 1) * WINDOW_WIDTH / columns - 1,
            max_y: (row + 1) * WINDOW_HEIGHT / rows - 1,
        }
    }

    fn legend(&self) -> String {
        let mut lines =
            vec![format!("world-case-atlas columns={} rows={} gutter_cells={GUTTER_CELLS}", self.columns, self.rows)];
        for placement in &self.placements {
            let case = &self.cases[placement.case_index];
            lines.push(format!(
                "row={} column={} name={} anchor_cell=({}, {}) anchor_octimeters=({}, {}) chunk=({}, {})",
                placement.slot_row,
                placement.slot_column,
                case.name,
                placement.anchor_cell.x,
                placement.anchor_cell.z,
                placement.anchor_world.x_octimeters,
                placement.anchor_world.z_octimeters,
                placement.chunk.x,
                placement.chunk.z,
            ));
        }
        lines.push(String::new());
        lines.join("\n")
    }
}

struct AtlasOracle {
    case_name: &'static str,
    region: FrameRect,
    coverage_index: usize,
    bounds_index: usize,
}

struct GuardedCapture {
    png: Vec<u8>,
    verdict: FrameVerdict,
    _artifacts: ArtifactGuard,
}

fn offset_cell(anchor: CellPos, offset: CellPos) -> CellPos {
    CellPos { x: anchor.x + offset.x, z: anchor.z + offset.z }
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

fn load_kit_export(bench: &mut SubstrateBench, wasm: &[u8], export: &str, name: &str) -> MailboxId {
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some(name.to_owned()),
                    config: Vec::new(),
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

fn single_label_case() -> AtlasCaseSpec {
    AtlasCaseSpec {
        name: "single_label_all_inside",
        mutations: vec![AtlasMutation::CellPoints {
            offset: CellPos { x: 0, z: 0 },
            points: vec![Material::Grass.to_u8(); SUBCELLS_PER_CELL],
        }],
    }
}

fn two_label_case() -> AtlasCaseSpec {
    let edge = usize::try_from(SUBCELLS_PER_CELL_EDGE).expect("subcell edge fits usize");
    let mut points = Vec::with_capacity(SUBCELLS_PER_CELL);
    for z in 0..edge {
        for x in 0..edge {
            let material = if x + z < edge {
                Material::Grass
            } else {
                Material::Stone
            };
            points.push(material.to_u8());
        }
    }
    AtlasCaseSpec {
        name: "two_label_window",
        mutations: vec![AtlasMutation::CellPoints { offset: CellPos { x: 0, z: 0 }, points }],
    }
}

fn cliff_case() -> AtlasCaseSpec {
    let edge = usize::try_from(SUBCELLS_PER_CELL_EDGE).expect("subcell edge fits usize");
    let mut deltas = Vec::with_capacity(SUBCELLS_PER_CELL);
    for z in 0..edge {
        for _ in 0..edge {
            deltas.push(if z < edge / 2 {
                256
            } else {
                0
            });
        }
    }
    AtlasCaseSpec {
        name: "cliff_height_break",
        mutations: vec![
            AtlasMutation::Region(SetRegion {
                region_id: CLIFF_REGION_ID,
                name: "atlas-cliff".to_owned(),
                default_material: Material::Grass.to_u8(),
                cliff_material: Material::Sand.to_u8(),
            }),
            AtlasMutation::CellBase {
                offset: CellPos { x: 0, z: 0 },
                underlay: Material::Void,
                height_octimeters: 0,
                region_id: CLIFF_REGION_ID,
            },
            AtlasMutation::CellHeights { offset: CellPos { x: 0, z: 0 }, deltas },
        ],
    }
}

fn starter_cases() -> Vec<AtlasCaseSpec> {
    vec![AtlasCaseSpec { name: "empty", mutations: Vec::new() }, single_label_case(), two_label_case(), cliff_case()]
}

#[allow(clippy::cast_precision_loss)] // The fixture bounds are small exact multiples of one 256-octimeter cell.
fn atlas_view_projection(bounds: AtlasWorldBounds) -> ViewProjection {
    let min_x = bounds.min.x_octimeters as f32 / OCTIMETERS_PER_CELL as f32;
    let min_z = bounds.min.z_octimeters as f32 / OCTIMETERS_PER_CELL as f32;
    let max_x = bounds.max.x_octimeters as f32 / OCTIMETERS_PER_CELL as f32;
    let max_z = bounds.max.z_octimeters as f32 / OCTIMETERS_PER_CELL as f32;
    let center_x = (min_x + max_x) * 0.5;
    let center_z = (min_z + max_z) * 0.5;
    let projection = Mat4::orthographic_rh(
        -(max_x - min_x) * 0.5,
        (max_x - min_x) * 0.5,
        -(max_z - min_z) * 0.5,
        (max_z - min_z) * 0.5,
        0.1,
        64.0,
    );
    let view = Mat4::look_at_rh(
        Vec3::new(center_x, 32.0, center_z),
        Vec3::new(center_x, 0.0, center_z),
        Vec3::new(0.0, 0.0, -1.0),
    );
    ViewProjection { view_proj: (projection * view).to_cols_array() }
}

fn case_checks(fixture: &AtlasFixture) -> (Vec<FrameCheck>, Vec<AtlasOracle>) {
    let mut checks = Vec::new();
    let mut oracles = Vec::new();
    for placement in &fixture.placements {
        let case = &fixture.cases[placement.case_index];
        if case.mutations.is_empty() {
            continue;
        }
        let region = fixture.slot_frame_rect(*placement);
        let coverage_index = checks.len();
        checks.push(FrameCheck {
            reduction: FrameReduction::Coverage,
            tolerance: 5,
            background: Some(CLEAR_SRGB),
            region: Some(region),
        });
        let bounds_index = checks.len();
        checks.push(FrameCheck {
            reduction: FrameReduction::BoundingBox,
            tolerance: 5,
            background: Some(CLEAR_SRGB),
            region: Some(region),
        });
        oracles.push(AtlasOracle { case_name: case.name, region, coverage_index, bounds_index });
    }
    (checks, oracles)
}

fn checks_for_region(region: FrameRect) -> Vec<FrameCheck> {
    [FrameReduction::Coverage, FrameReduction::BoundingBox]
        .into_iter()
        .map(|reduction| FrameCheck { reduction, tolerance: 5, background: Some(CLEAR_SRGB), region: Some(region) })
        .collect()
}

fn capture_guarded(
    bench: &mut SubstrateBench,
    world: &str,
    view_projection: ViewProjection,
    id: &str,
    expectation: &str,
    checks: Vec<FrameCheck>,
) -> GuardedCapture {
    let captured = bench
        .execute(vec![(
            "capture",
            BenchOp::send_and_await(
                RenderCapability::NAMESPACE,
                &CaptureFrame {
                    mails: vec![envelope(RenderCapability::NAMESPACE, &view_projection), envelope(world, &Render)],
                    after_mails: Vec::new(),
                    checks: checks.clone(),
                    similarity: None,
                },
            ),
        )])
        .expect("capture atlas frame");
    match captured.reply::<CaptureFrameResult>("capture").expect("decode CaptureFrameResult") {
        CaptureFrameResult::Ok { png, verdict: Some(verdict), .. } => {
            let artifacts =
                ArtifactGuard::arm(id, png.clone(), checks, verdict.results.clone()).with_expectation(expectation);
            GuardedCapture { png, verdict, _artifacts: artifacts }
        }
        CaptureFrameResult::Ok { verdict: None, .. } => {
            panic!("atlas capture requested checks but returned no verdict")
        }
        CaptureFrameResult::Err { error } => panic!("capture atlas frame: {error}"),
    }
}

fn assert_case_oracle(oracle: &AtlasOracle, verdict: &FrameVerdict) {
    let coverage = match &verdict.results[oracle.coverage_index] {
        FrameCheckResult::Coverage { fraction, .. } => *fraction,
        other => panic!("{} expected coverage result; got {other:?}", oracle.case_name),
    };
    assert!(
        (0.01..0.15).contains(&coverage),
        "{} should occupy a bounded fraction of its isolated slot; coverage={coverage:.4}",
        oracle.case_name,
    );

    let bounds = match &verdict.results[oracle.bounds_index] {
        FrameCheckResult::BoundingBox { rect: Some(rect), .. } => *rect,
        FrameCheckResult::BoundingBox { rect: None, .. } => {
            panic!("{} should have a visible bounding box", oracle.case_name)
        }
        other => panic!("{} expected bounding-box result; got {other:?}", oracle.case_name),
    };
    let region_width = oracle.region.max_x - oracle.region.min_x + 1;
    let region_height = oracle.region.max_y - oracle.region.min_y + 1;
    let inset_x = region_width / 4;
    let inset_y = region_height / 4;
    let width = bounds.max_x - bounds.min_x + 1;
    let height = bounds.max_y - bounds.min_y + 1;
    assert!(
        bounds.min_x >= oracle.region.min_x + inset_x
            && bounds.max_x <= oracle.region.max_x - inset_x
            && bounds.min_y >= oracle.region.min_y + inset_y
            && bounds.max_y <= oracle.region.max_y - inset_y
            && (8..=region_width / 2).contains(&width)
            && (8..=region_height / 2).contains(&height),
        "{} should stay well inside its gutter-isolated slot; region={:?} bounds={bounds:?} size={width}x{height}",
        oracle.case_name,
        oracle.region,
    );
}

#[allow(clippy::disallowed_methods)] // test-only: explicit demo gate and Cargo target override, not runtime config.
fn export_demo_if_requested(png: &[u8], legend: &str) {
    if env::var_os("AETHER_ATLAS_DEMO").is_none() {
        return;
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root reachable from CARGO_MANIFEST_DIR");
    let target_root = env::var_os("CARGO_TARGET_DIR").map_or_else(|| workspace.join("target"), PathBuf::from);
    let output = target_root.join("world-case-atlas");
    fs::create_dir_all(&output).expect("create world-case-atlas demo directory");
    fs::write(output.join("contact-sheet.png"), png).expect("write world-case-atlas contact sheet");
    fs::write(output.join("legend.txt"), legend).expect("write world-case-atlas legend");
}

#[test]
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
fn starter_case_atlas_is_scored_isolated_and_demo_exportable() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");

    let fixture = AtlasFixture::new(starter_cases());
    let world = component_address("world");
    let mut bench = SubstrateBench::builder().size(WINDOW_WIDTH, WINDOW_HEIGHT).build().expect("boot atlas bench");
    load_kit_export(&mut bench, &wasm, "aether.kit.world", "world");
    fixture.apply_all(&mut bench, &world);
    bench.execute(vec![("settle", BenchOp::advance(2))]).expect("settle atlas remesh");

    let (checks, oracles) = case_checks(&fixture);
    let expectation = format!(
        "every non-empty starter case stays visible and bounded inside its slot; {}",
        fixture.legend().trim_end()
    );
    let contact = capture_guarded(
        &mut bench,
        &world,
        atlas_view_projection(fixture.bounds()),
        "world_case_atlas_contact_sheet",
        &expectation,
        checks,
    );
    for oracle in &oracles {
        assert_case_oracle(oracle, &contact.verdict);
    }

    // Isolation tripwire: capture the focal case before and after a case five
    // cells away is populated. Its region reductions must remain byte-for-byte
    // identical; shrinking the gutter below the mesher apron breaks this.
    let isolation_fixture = AtlasFixture::new(vec![single_label_case(), two_label_case()]);
    let isolation_world = component_address("isolation-world");
    let mut isolation_bench =
        SubstrateBench::builder().size(WINDOW_WIDTH, WINDOW_HEIGHT).build().expect("boot isolation bench");
    load_kit_export(&mut isolation_bench, &wasm, "aether.kit.world", "isolation-world");
    isolation_fixture.apply_case(&mut isolation_bench, &isolation_world, 0);
    isolation_bench.execute(vec![("settle", BenchOp::advance(2))]).expect("settle isolated case");

    let focal_region = isolation_fixture.slot_frame_rect(isolation_fixture.placement(0));
    let isolation_view = atlas_view_projection(isolation_fixture.bounds());
    let isolated = capture_guarded(
        &mut isolation_bench,
        &isolation_world,
        isolation_view,
        "world_case_atlas_isolated",
        "the focal case before its neighbor is populated",
        checks_for_region(focal_region),
    );
    isolation_fixture.apply_case(&mut isolation_bench, &isolation_world, 1);
    isolation_bench.execute(vec![("settle", BenchOp::advance(2))]).expect("settle populated neighbor");
    let neighboring = capture_guarded(
        &mut isolation_bench,
        &isolation_world,
        isolation_view,
        "world_case_atlas_neighboring",
        "the focal case remains unchanged after its gutter-separated neighbor is populated",
        checks_for_region(focal_region),
    );
    assert_eq!(
        isolated.verdict.results, neighboring.verdict.results,
        "a case's scored observation must not change when the neighboring slot is populated",
    );

    export_demo_if_requested(&contact.png, &fixture.legend());
}
