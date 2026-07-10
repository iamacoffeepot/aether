//! World-stamp acceptance coverage through the real wasm component and
//! `TestBench` render path. The host tests pin scalar area math and chunk-border
//! remeshing; this test proves a compact `stamp_hexagon` mail reaches the
//! handler and produces the expected smooth scalar-contour silhouette.
//!
//! Skipped when no wgpu adapter is available or the `aether_kit` wasm has not
//! been pre-built. CI sets `AETHER_REQUIRE_RUNTIME=1` so either condition is a
//! hard failure there.

use std::fs;
use std::path::Path;

use aether_actor::Addressable;
use aether_capabilities::render::ViewProjection;
use aether_data::Kind;
use aether_kinds::{LoadComponent, LoadResult, NamedMail, Render};
use aether_kit::mark::{MarkId, MarkRef};
use aether_kit::world::{
    ApplyBrush, AutomatonRule, BrushParameters, Material, OperatorBudget, OperatorCell,
    OperatorChunk, OperatorError, OperatorResult, OperatorStats, RunAutomaton, StampHexagon,
    WorldPoint,
};
use aether_math::{Mat4, Vec3};
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::require_runtime};
use aether_substrate_bundle::visual::{
    background_top_left, bounding_box, centroid, coverage, decode_png,
};

const COMPONENT_NAME: &str = "world";
const WIDTH: u32 = 128;
const HEIGHT: u32 = 128;
const FRAME_CENTER: f32 = 64.0;

fn component_address() -> String {
    format!(
        "aether.component/{}:{COMPONENT_NAME}",
        aether_capabilities::WasmTrampoline::NAMESPACE,
    )
}

fn envelope<K: Kind>(recipient: &str, mail: &K) -> NamedMail {
    NamedMail {
        recipient_name: recipient.to_owned(),
        kind_name: K::NAME.to_owned(),
        payload: mail.encode_into_bytes(),
        count: 1,
    }
}

fn load_world(bench: &mut TestBench, wasm_path: &Path) {
    let wasm = fs::read(wasm_path).expect("read kit wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(COMPONENT_NAME.to_owned()),
                    config: Vec::new(),
                    export: Some("aether.kit.world".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { name, .. } => assert_eq!(name, component_address()),
        LoadResult::Err { error } => panic!("load world: {error}"),
    }
}

fn top_down_view_projection(center_x: f32, center_z: f32, extent: f32) -> ViewProjection {
    let eye = Vec3::new(center_x, 10.0, center_z);
    let target = Vec3::new(center_x, 0.0, center_z);
    let view = Mat4::look_at_rh(eye, target, Vec3::new(0.0, 0.0, -1.0));
    let projection = Mat4::orthographic_rh(-extent, extent, -extent, extent, 0.1, 100.0);
    ViewProjection {
        view_proj: (projection * view).to_cols_array(),
    }
}

#[test]
fn stamp_hexagon_renders_a_smooth_centered_silhouette() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let mut bench = TestBench::start_with_size(WIDTH, HEIGHT).expect("boot");
    load_world(&mut bench, &wasm_path);
    let world = component_address();

    // Eight meters from the origin keeps the shape in chunk (0,0). This
    // non-lattice radius is shown close enough that one subcell spans several
    // pixels, making a binary staircase distinguishable from scalar coverage.
    bench
        .execute(vec![(
            "stamp",
            BenchOp::send_mail(
                world.as_str(),
                &StampHexagon {
                    center: WorldPoint::new(2048, 2048),
                    radius_octimeters: 300,
                    material: Material::Stone.to_u8(),
                },
            ),
        )])
        .expect("stamp hexagon");

    let captured = bench
        .execute(vec![(
            "capture",
            BenchOp::capture_with_mails(
                vec![
                    envelope("aether.render", &top_down_view_projection(8.0, 8.0, 1.5)),
                    envelope(world.as_str(), &Render),
                ],
                Vec::new(),
            ),
        )])
        .expect("capture stamped world");
    let image = decode_png(captured.captured("capture").expect("capture bytes"))
        .expect("decode capture png");
    let background = background_top_left(&image);
    let fraction = coverage(&image, background, 5);
    assert!(
        (0.30..0.52).contains(&fraction),
        "regular hexagon should occupy a centered, bounded fraction of the frame; got {fraction}",
    );

    let (center_x, center_y) = centroid(&image, background, 5).expect("hexagon centroid");
    assert!(
        (center_x - FRAME_CENTER).abs() < 5.0 && (center_y - FRAME_CENTER).abs() < 5.0,
        "hexagon centroid ({center_x}, {center_y}) should be near frame center",
    );
    let bounds = bounding_box(&image, background, 5).expect("hexagon bounds");
    let is_lit = |x: u32, y: u32| {
        let offset = ((y * image.width + x) * 4) as usize;
        image.rgba[offset..offset + 3]
            .iter()
            .zip(background)
            .any(|(actual, clear)| actual.abs_diff(clear) > 5)
    };
    let left_edges: Vec<_> = (bounds.min_y..=bounds.max_y)
        .filter_map(|y| (bounds.min_x..=bounds.max_x).find(|&x| is_lit(x, y)))
        .collect();
    let mut longest_flat_run = 1usize;
    let mut flat_run = 1usize;
    for pair in left_edges.windows(2) {
        if pair[0] == pair[1] {
            flat_run += 1;
            longest_flat_run = longest_flat_run.max(flat_run);
        } else {
            flat_run = 1;
        }
    }
    assert!(
        longest_flat_run <= 4,
        "scalar coverage should move the zoomed diagonal contour continuously; a binary-coverage \
         mutation produces long subcell stair steps (longest flat run: {longest_flat_run}, \
         bounds: {bounds:?})",
    );
    let drawn_width = bounds.max_x - bounds.min_x + 1;
    let drawn_height = bounds.max_y - bounds.min_y + 1;
    assert!(
        drawn_width > 85 && drawn_height > 70,
        "hexagon silhouette should have broad smooth extents; got {drawn_width}x{drawn_height}",
    );
}

#[test]
#[allow(clippy::too_many_lines)] // one cohesive brush + automaton + partial-budget acceptance proof
fn bounded_terrain_operators_reply_with_the_rendered_partial_world() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let mut bench = TestBench::start_with_size(WIDTH, HEIGHT).expect("boot");
    load_world(&mut bench, &wasm_path);
    let world = component_address();
    let brush_source = MarkRef {
        id: MarkId::new(11),
        revision: 4,
    };

    let brushed = bench
        .execute(vec![(
            "brush",
            BenchOp::send_and_await(
                world.as_str(),
                &ApplyBrush {
                    source: brush_source,
                    path: vec![WorldPoint::new(576, 1088), WorldPoint::new(1088, 1088)],
                    brush: BrushParameters {
                        radius_octimeters: 64,
                        spacing_octimeters: 256,
                        material: Material::Stone.to_u8(),
                    },
                    budget: OperatorBudget {
                        max_steps: 3,
                        max_subcells: 180,
                    },
                },
            ),
        )])
        .expect("apply reference brush");
    assert_eq!(
        brushed
            .reply::<OperatorResult>("brush")
            .expect("decode brush result"),
        OperatorResult::Applied {
            source: brush_source,
            stats: OperatorStats {
                steps_run: 3,
                subcells_written: 180,
                touched_chunks: vec![OperatorChunk {
                    chunk_x: 0,
                    chunk_z: 0,
                }],
            },
        },
    );

    let brush_capture = bench
        .execute(vec![(
            "brush_capture",
            BenchOp::capture_with_mails(
                vec![
                    envelope("aether.render", &top_down_view_projection(3.25, 4.25, 1.5)),
                    envelope(world.as_str(), &Render),
                ],
                Vec::new(),
            ),
        )])
        .expect("capture brushed world");
    let brush_image = decode_png(
        brush_capture
            .captured("brush_capture")
            .expect("brush capture bytes"),
    )
    .expect("decode brush capture");
    let brush_background = background_top_left(&brush_image);
    let brush_fraction = coverage(&brush_image, brush_background, 5);
    assert!(
        (0.03..0.15).contains(&brush_fraction),
        "three bounded brush discs should render as a visible path; got {brush_fraction}",
    );

    let automaton_source = MarkRef {
        id: MarkId::new(12),
        revision: 2,
    };
    let grown = bench
        .execute(vec![(
            "automaton",
            BenchOp::send_and_await(
                world.as_str(),
                &RunAutomaton {
                    source: automaton_source,
                    seed: OperatorCell {
                        cell_x: 8,
                        cell_z: 8,
                    },
                    rule: AutomatonRule::Grow {
                        material: Material::Grass.to_u8(),
                        generations: 1,
                    },
                    budget: OperatorBudget {
                        max_steps: 5,
                        max_subcells: 1_280,
                    },
                },
            ),
        )])
        .expect("run reference automaton");
    assert_eq!(
        grown
            .reply::<OperatorResult>("automaton")
            .expect("decode automaton result"),
        OperatorResult::Applied {
            source: automaton_source,
            stats: OperatorStats {
                steps_run: 5,
                subcells_written: 1_280,
                touched_chunks: vec![OperatorChunk {
                    chunk_x: 0,
                    chunk_z: 0,
                }],
            },
        },
    );

    let automaton_capture = bench
        .execute(vec![(
            "automaton_capture",
            BenchOp::capture_with_mails(
                vec![
                    envelope("aether.render", &top_down_view_projection(8.5, 8.5, 2.5)),
                    envelope(world.as_str(), &Render),
                ],
                Vec::new(),
            ),
        )])
        .expect("capture automaton world");
    let automaton_image = decode_png(
        automaton_capture
            .captured("automaton_capture")
            .expect("automaton capture bytes"),
    )
    .expect("decode automaton capture");
    let automaton_background = background_top_left(&automaton_image);
    let automaton_fraction = coverage(&automaton_image, automaton_background, 5);
    assert!(
        (0.08..0.40).contains(&automaton_fraction),
        "the five-cell reference growth should occupy a bounded cross; got {automaton_fraction}",
    );

    let limited_source = MarkRef {
        id: MarkId::new(13),
        revision: 9,
    };
    let limited = bench
        .execute(vec![(
            "limited",
            BenchOp::send_and_await(
                world.as_str(),
                &RunAutomaton {
                    source: limited_source,
                    seed: OperatorCell {
                        cell_x: 12,
                        cell_z: 8,
                    },
                    rule: AutomatonRule::Grow {
                        material: Material::Sand.to_u8(),
                        generations: 1,
                    },
                    budget: OperatorBudget {
                        max_steps: 5,
                        max_subcells: 512,
                    },
                },
            ),
        )])
        .expect("run budget-limited automaton");
    assert_eq!(
        limited
            .reply::<OperatorResult>("limited")
            .expect("decode limited result"),
        OperatorResult::Failed {
            source: limited_source,
            error: OperatorError::SubcellBudgetExhausted,
            stats: OperatorStats {
                steps_run: 2,
                subcells_written: 512,
                touched_chunks: vec![OperatorChunk {
                    chunk_x: 0,
                    chunk_z: 0,
                }],
            },
        },
    );

    let limited_capture = bench
        .execute(vec![(
            "limited_capture",
            BenchOp::capture_with_mails(
                vec![
                    envelope("aether.render", &top_down_view_projection(13.0, 8.5, 2.0)),
                    envelope(world.as_str(), &Render),
                ],
                Vec::new(),
            ),
        )])
        .expect("capture limited automaton world");
    let limited_image = decode_png(
        limited_capture
            .captured("limited_capture")
            .expect("limited capture bytes"),
    )
    .expect("decode limited capture");
    let limited_background = background_top_left(&limited_image);
    let limited_fraction = coverage(&limited_image, limited_background, 5);
    assert!(
        (0.04..0.30).contains(&limited_fraction),
        "the two accepted cells should render while the third stays absent; got {limited_fraction}",
    );
    let limited_bounds = bounding_box(&limited_image, limited_background, 5)
        .expect("limited automaton rendered bounds");
    let limited_width = limited_bounds.max_x - limited_bounds.min_x + 1;
    let limited_height = limited_bounds.max_y - limited_bounds.min_y + 1;
    assert!(
        limited_width > limited_height + limited_height / 2,
        "the rendered prefix should be exactly the two horizontal seed/east cells, not the rejected north cell; bounds: {limited_bounds:?}",
    );
}
