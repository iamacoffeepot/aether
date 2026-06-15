//! Mesh-viewer scenario tests. Each test boots a `TestBench`, loads
//! this crate's own wasm artifact (built separately for
//! `wasm32-unknown-unknown`), seeds a fixture `.dsl` / `.obj` file
//! into the substrate's `save://` namespace, and drives the component
//! through `aether.mesh.load` to verify the load → parse → render
//! pipeline end-to-end.
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The component's wasm hasn't been built — tests read
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_mesh_viewer.wasm`
//!   and skip with an `eprintln!` when both paths are absent. CI
//!   builds the wasm before invoking `cargo test`.
//!
//! All boot-time mechanics (wgpu probe, wasm locator, skip-or-panic
//! gate, `save://` sandbox) live in
//! `aether_substrate_bundle::test_bench::test_helpers` (issues 460 +
//! 821). Per issue 464, the sandbox is plumbed via
//! `TestBench::builder().namespace_roots(...)` rather than env-var
//! mutation.

use aether_kinds::{
    CorridorEdge, CorridorGraph, CorridorNode, EdgeKind, LoadComponent, LoadResult, MeshLoadResult,
    ScalarField, TrajectoryEndReason, TrajectoryLog, TrajectorySampleEntry, TrajectorySet,
};
use aether_mesh_viewer::{CorridorLoadResult, LoadCorridor, LoadMesh, Scrub};
use aether_substrate_bundle::test_bench::{
    BenchOp, TestBench,
    test_helpers::{init_save_sandbox, require_runtime, test_namespace_roots, write_fixture},
};
use aether_substrate_bundle::visual::{decode_png, differs_from_background, mean_absolute_error};

// Force linkage of `aether-mesh-viewer`'s `inventory::submit!`
// `KindDescriptor` entries into this test binary. Cargo treats
// integration tests as separate crates that link against the test
// target's host rlib, but the linker strips inventory submits for
// kinds the test code doesn't statically reference.
#[allow(unused_imports)]
use aether_mesh_viewer as _;
use std::fs;
use std::path::Path;

/// User-facing component name passed to `LoadComponent`.
const COMPONENT_NAME: &str = "mv";

/// Full mailbox address the substrate registers for the loaded
/// component (issue 634 Phase 4 PR 1). Mail to the bare
/// `COMPONENT_NAME` warn-drops as unknown — agents address the
/// trampoline by its full `aether.embedded:NAME` form,
/// which is what `LoadResult.name` returns. Built from
/// The `/`-rendered lineage a loaded component registers at (ADR-0099
/// §4): the component host `aether.component` `/`-joined to the
/// trampoline node — exactly what `LoadResult.name` reports.
fn component_address() -> String {
    use aether_actor::Actor;
    format!(
        "aether.component/{}:{}",
        aether_capabilities::WasmTrampoline::NAMESPACE,
        COMPONENT_NAME,
    )
}

const BOX_DSL: &[u8] = b"(box 1 1 1 :color 0)\n";
const QUAD_OBJ: &[u8] = b"\
v -0.5 -0.5 0
v  0.5 -0.5 0
v  0.5  0.5 0
v -0.5  0.5 0
f 1 2 3 4
";
const BAD_DSL: &[u8] = b"(box not-a-number 1 1)\n";

/// Load this crate's pre-built wasm into the bench and await
/// `LoadResult`. Panics on load failure so the calling test surfaces
/// the error message rather than wedging on a missing subscription.
fn load_viewer(bench: &mut TestBench, wasm_path: &Path) {
    let wasm = fs::read(wasm_path).expect("read mesh-viewer wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(COMPONENT_NAME.to_owned()),
                    config: Vec::new(),
                    export: None,
                },
            ),
        )])
        .expect("load sequence");
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

/// Assert that `aether.draw_triangle` was observed at least once.
/// Surfaces the observed-kinds list on failure so a typo or missing
/// subscription is debuggable.
fn assert_draw_triangle_observed(bench: &TestBench) {
    let observed = bench.count_observed("aether.draw_triangle");
    assert!(
        observed >= 1,
        "expected ≥1 aether.draw_triangle observed; got {observed}; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// Smoke test: load a `.dsl` box → triangles flow to the render sink
/// every tick → the captured frame contains pixels that diverge from
/// the chassis clear color. Validates the entire DSL load path: the
/// IO sink read, `aether-mesh`'s parser+mesher, the wireframe outline
/// emission, and the per-tick render-sink replay.
#[test]
fn dsl_box_loads_and_renders() {
    let Some(wasm_path) = require_runtime("aether_mesh_viewer") else {
        return;
    };
    let sandbox = init_save_sandbox("mesh-viewer");
    let path = write_fixture("dsl_box.dsl", BOX_DSL);

    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut bench, &wasm_path);

    // Priming tick triggers the load; the read reply lands on a later
    // tick, so a handful of post-load ticks populate the cache and
    // emit several render-sink frames before the capture.
    let result = bench
        .execute(vec![
            ("prime", BenchOp::advance(1)),
            (
                "load_mesh",
                BenchOp::send_mail(
                    component_address(),
                    &LoadMesh {
                        namespace: "save".to_owned(),
                        path,
                    },
                ),
            ),
            ("post", BenchOp::advance(5)),
            ("snap", BenchOp::capture()),
        ])
        .expect("prime + load + advance + capture");

    let png = result.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    assert_draw_triangle_observed(&bench);
    differs_from_background(&img, 5).expect("captured frame should diverge from clear color");
}

/// Issue 1868 render-path smoke: a `.field` load decodes a postcard
/// `ScalarField`, surface-nets it, and replays the triangles to
/// `aether.render` every frame — `aether.draw_triangle` is observed.
/// Asserts mesh structure (triangles flow), not pixels, per the GPU-test
/// split (the test bench has no GPU-capture host for structural checks).
#[test]
fn field_loads_and_renders() {
    let Some(wasm_path) = require_runtime("aether_mesh_viewer") else {
        return;
    };
    let sandbox = init_save_sandbox("mesh-viewer");
    // A small filled box with an interior empty pocket — a non-trivial
    // boundary surface (an outer shell plus a cavity).
    let (w, h, t) = (4u32, 4u32, 4u32);
    let (wz, hz, tz) = (w as usize, h as usize, t as usize);
    let mut values = vec![1u32; wz * hz * tz];
    values[2 * hz * wz + 2 * wz + 2] = 0; // carve an interior empty cell
    let field = ScalarField {
        width: w,
        height: h,
        ticks: t,
        values,
    };
    let bytes = postcard::to_allocvec(&field).expect("encode ScalarField fixture");
    let path = write_fixture("reach.field", &bytes);

    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut bench, &wasm_path);

    bench
        .execute(vec![
            ("prime", BenchOp::advance(1)),
            (
                "load_mesh",
                BenchOp::send_mail(
                    component_address(),
                    &LoadMesh {
                        namespace: "save".to_owned(),
                        path,
                    },
                ),
            ),
            ("post", BenchOp::advance(5)),
        ])
        .expect("prime + load + advance");

    assert_draw_triangle_observed(&bench);
}

/// Issue 1870 render-path smoke: load a `.field` solid then a `.paths`
/// overlay; both replay to `aether.render` every frame, and the observed
/// `aether.draw_triangle` count rises after the overlay load (the path
/// ribbons add geometry on top of the solid). Asserts geometry flow, not
/// pixels, per the GPU-test split.
#[test]
fn field_then_paths_overlay_renders() {
    let Some(wasm_path) = require_runtime("aether_mesh_viewer") else {
        return;
    };
    let sandbox = init_save_sandbox("mesh-viewer");

    // A small solid field so the overlay has a volume to thread.
    let (w, h, t) = (4u32, 4u32, 4u32);
    let field = ScalarField {
        width: w,
        height: h,
        ticks: t,
        values: vec![1u32; (w * h * t) as usize],
    };
    let field_bytes = postcard::to_allocvec(&field).expect("encode ScalarField fixture");
    let field_path = write_fixture("reach.field", &field_bytes);

    // Two paths threading the volume; they share the first grid step so
    // the traffic ramp has something to colour.
    let make_log = |seed: u64, cells: &[(u32, u32, u32)]| TrajectoryLog {
        seed,
        samples: cells
            .iter()
            .map(|&(tick, x, y)| TrajectorySampleEntry {
                tick,
                x,
                y,
                value: 0,
            })
            .collect(),
        end_reason: TrajectoryEndReason::Completed,
    };
    let set = TrajectorySet {
        logs: vec![
            make_log(1, &[(0, 0, 0), (1, 1, 0), (2, 2, 0), (3, 3, 0)]),
            make_log(2, &[(0, 0, 0), (1, 1, 0), (2, 2, 1), (3, 3, 1)]),
        ],
    };
    let paths_bytes = postcard::to_allocvec(&set).expect("encode TrajectorySet fixture");
    let paths_path = write_fixture("herd.paths", &paths_bytes);

    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut bench, &wasm_path);

    // Load the solid and advance; record the solid-only draw count.
    bench
        .execute(vec![
            ("prime", BenchOp::advance(1)),
            (
                "load_field",
                BenchOp::send_mail(
                    component_address(),
                    &LoadMesh {
                        namespace: "save".to_owned(),
                        path: field_path,
                    },
                ),
            ),
            ("post_field", BenchOp::advance(5)),
        ])
        .expect("prime + load field + advance");
    let solid_only = bench.count_observed("aether.draw_triangle");
    assert!(
        solid_only >= 1,
        "the solid should flow before the overlay loads; got {solid_only}",
    );

    // Load the path overlay and advance again; the per-frame draw count
    // now includes the ribbon geometry, so the total observed count rises.
    bench
        .execute(vec![
            (
                "load_paths",
                BenchOp::send_mail(
                    component_address(),
                    &LoadMesh {
                        namespace: "save".to_owned(),
                        path: paths_path,
                    },
                ),
            ),
            ("post_paths", BenchOp::advance(5)),
        ])
        .expect("load paths + advance");
    let with_overlay = bench.count_observed("aether.draw_triangle");
    assert!(
        with_overlay > solid_only,
        "the overlay load should raise the observed draw-triangle count: \
         solid-only {solid_only}, with overlay {with_overlay}",
    );
}

/// `.obj` parser smoke. The OBJ path doesn't go through `aether-mesh`'s
/// parser — it's a built-in fan-triangulation parser inside the
/// component — so this test guards against the OBJ branch silently
/// regressing while the DSL branch keeps working.
#[test]
fn obj_quad_loads_and_renders() {
    let Some(wasm_path) = require_runtime("aether_mesh_viewer") else {
        return;
    };
    let sandbox = init_save_sandbox("mesh-viewer");
    let path = write_fixture("obj_quad.obj", QUAD_OBJ);

    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut bench, &wasm_path);

    let result = bench
        .execute(vec![
            ("prime", BenchOp::advance(1)),
            (
                "load_mesh",
                BenchOp::send_mail(
                    component_address(),
                    &LoadMesh {
                        namespace: "save".to_owned(),
                        path,
                    },
                ),
            ),
            ("post", BenchOp::advance(5)),
            ("snap", BenchOp::capture()),
        ])
        .expect("prime + load + advance + capture");

    let png = result.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    assert_draw_triangle_observed(&bench);
    differs_from_background(&img, 5).expect("captured frame should diverge from clear color");
}

/// Parse-failure resilience: a known-bad DSL after a known-good DSL
/// must keep the previous mesh visible — the component's contract is
/// "partial parse / mesh failure leaves the previous mesh intact."
/// Loads a good box, advances until triangles flow, loads the bad
/// DSL, advances again, and verifies the frame still diverges from
/// the clear color.
#[test]
fn parse_failure_keeps_prior_mesh() {
    let Some(wasm_path) = require_runtime("aether_mesh_viewer") else {
        return;
    };
    let sandbox = init_save_sandbox("mesh-viewer");
    let good = write_fixture("good.dsl", BOX_DSL);
    let bad = write_fixture("bad.dsl", BAD_DSL);

    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut bench, &wasm_path);

    bench
        .execute(vec![
            ("prime", BenchOp::advance(1)),
            (
                "load_good",
                BenchOp::send_mail(
                    component_address(),
                    &LoadMesh {
                        namespace: "save".to_owned(),
                        path: good,
                    },
                ),
            ),
            ("post_good", BenchOp::advance(5)),
        ])
        .expect("prime + good load");

    // Baseline: the good mesh is publishing.
    assert_draw_triangle_observed(&bench);

    // Now hand the viewer something it can't parse, then capture. The
    // cached triangle list should be intact — the frame still has
    // non-clear-color geometry.
    let result = bench
        .execute(vec![
            (
                "load_bad",
                BenchOp::send_mail(
                    component_address(),
                    &LoadMesh {
                        namespace: "save".to_owned(),
                        path: bad,
                    },
                ),
            ),
            ("post_bad", BenchOp::advance(5)),
            ("snap", BenchOp::capture()),
        ])
        .expect("bad load + capture");
    let png = result.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    differs_from_background(&img, 5)
        .expect("cached mesh should remain visible after parse failure");
}

/// Issue 964 acceptance: a good-DSL load replies `aether.mesh.load_result`
/// with `ok: true`, no `error`, and no `warnings`, echoing the request's
/// `namespace` + `path`. `send_and_await` blocks through the async
/// `aether.fs.read` round-trip until the structured reply lands, so the
/// reply is the wire signal a harness reads instead of inferring success
/// from rendered geometry.
#[test]
fn good_dsl_load_replies_ok() {
    let Some(wasm_path) = require_runtime("aether_mesh_viewer") else {
        return;
    };
    let sandbox = init_save_sandbox("mesh-viewer");
    let path = write_fixture("reply_good.dsl", BOX_DSL);

    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut bench, &wasm_path);

    let result = bench
        .execute(vec![(
            "load_mesh",
            BenchOp::send_and_await(
                component_address(),
                &LoadMesh {
                    namespace: "save".to_owned(),
                    path: path.clone(),
                },
            ),
        )])
        .expect("load + reply");

    let reply = result
        .reply::<MeshLoadResult>("load_mesh")
        .expect("decode MeshLoadResult");
    assert!(reply.ok, "good DSL should load: {:?}", reply.error);
    assert!(reply.error.is_none(), "good load carries no error");
    assert!(
        reply.warnings.is_empty(),
        "good load carries no warnings; got {:?}",
        reply.warnings,
    );
    assert_eq!(reply.namespace, "save", "reply echoes request namespace");
    assert_eq!(reply.path, path, "reply echoes request path");
}

/// Issue 964 acceptance: a bad-DSL load replies `aether.mesh.load_result`
/// with `ok: false` and `error.is_some()`. The prior cache (none here)
/// is untouched; the failure surfaces on the wire rather than only in
/// `engine_logs`.
#[test]
fn bad_dsl_load_replies_err() {
    let Some(wasm_path) = require_runtime("aether_mesh_viewer") else {
        return;
    };
    let sandbox = init_save_sandbox("mesh-viewer");
    let path = write_fixture("reply_bad.dsl", BAD_DSL);

    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut bench, &wasm_path);

    let result = bench
        .execute(vec![(
            "load_mesh",
            BenchOp::send_and_await(
                component_address(),
                &LoadMesh {
                    namespace: "save".to_owned(),
                    path: path.clone(),
                },
            ),
        )])
        .expect("load + reply");

    let reply = result
        .reply::<MeshLoadResult>("load_mesh")
        .expect("decode MeshLoadResult");
    assert!(!reply.ok, "bad DSL should not load");
    assert!(reply.error.is_some(), "bad load carries a failure reason");
    assert_eq!(reply.namespace, "save", "reply echoes request namespace");
    assert_eq!(reply.path, path, "reply echoes request path");
}

/// A small corridor-graph fixture (issue 1869) whose two ticks have
/// visibly different component layouts: tick 0 is a single component that
/// splits into two distinct lineages at tick 1. Scrubbing between the two
/// ticks re-addresses the datum, so the rendered slices — and the captured
/// frames — differ.
fn split_corridor_bytes() -> Vec<u8> {
    let node = |tick, component, cell_count| CorridorNode {
        tick,
        component,
        cell_count,
        min_cost: 0,
    };
    let flow = |from, to, overlap_width| CorridorEdge {
        from,
        to,
        kind: EdgeKind::Flow,
        price: 0,
        overlap_width,
    };
    let graph = CorridorGraph {
        // tick 0: one big component; tick 1: two branches.
        nodes: vec![node(0, 0, 16), node(1, 0, 8), node(1, 1, 8)],
        edges: vec![flow(0, 1, 5), flow(0, 2, 5)],
    };
    postcard::to_allocvec(&graph).expect("encode CorridorGraph fixture")
}

/// Issue 1869 acceptance: load a fixture `CorridorGraph`, scrub to two
/// distinct ticks, capture each, and assert the frames differ — the scrub
/// re-addresses the per-tick datum — while `aether.draw_triangle` is
/// observed throughout. Proves the scrubbable corridor datum end-to-end:
/// ingest, the per-tick node buckets, the scrub cursor, and the per-tick
/// render emit.
#[test]
fn corridor_scrub_re_addresses_the_datum() {
    let Some(wasm_path) = require_runtime("aether_mesh_viewer") else {
        return;
    };
    let sandbox = init_save_sandbox("mesh-viewer");
    let path = write_fixture("corridor.graph", &split_corridor_bytes());

    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");
    load_viewer(&mut bench, &wasm_path);

    // Load the corridor graph and await the structured result.
    let loaded = bench
        .execute(vec![
            ("prime", BenchOp::advance(1)),
            (
                "load_corridor",
                BenchOp::send_and_await(
                    component_address(),
                    &LoadCorridor {
                        namespace: "save".to_owned(),
                        path: path.clone(),
                    },
                ),
            ),
        ])
        .expect("prime + corridor load");
    let reply = loaded
        .reply::<CorridorLoadResult>("load_corridor")
        .expect("decode CorridorLoadResult");
    assert!(reply.ok, "corridor load should succeed: {:?}", reply.error);
    assert_eq!(reply.path, path, "reply echoes request path");

    // Scrub to tick 0, advance to emit several render frames, capture.
    let frame0 = bench
        .execute(vec![
            (
                "scrub0",
                BenchOp::send_mail(component_address(), &Scrub { tick: 0 }),
            ),
            ("post0", BenchOp::advance(5)),
            ("snap0", BenchOp::capture()),
        ])
        .expect("scrub 0 + capture");
    let png0 = frame0.captured("snap0").expect("snap0 ran");
    let img0 = decode_png(png0).expect("decode tick-0 capture");

    // Scrub to tick 1 (the split), advance, capture again.
    let frame1 = bench
        .execute(vec![
            (
                "scrub1",
                BenchOp::send_mail(component_address(), &Scrub { tick: 1 }),
            ),
            ("post1", BenchOp::advance(5)),
            ("snap1", BenchOp::capture()),
        ])
        .expect("scrub 1 + capture");
    let png1 = frame1.captured("snap1").expect("snap1 ran");
    let img1 = decode_png(png1).expect("decode tick-1 capture");

    assert_draw_triangle_observed(&bench);
    // Each frame carries geometry (diverges from the clear color)...
    differs_from_background(&img0, 5).expect("tick-0 frame has corridor geometry");
    differs_from_background(&img1, 5).expect("tick-1 frame has corridor geometry");
    // ...and the two scrubbed frames differ from each other — the scrub
    // re-addressed the datum rather than re-emitting the same slice.
    let mae = mean_absolute_error(&img0, &img1).expect("same-size frames compare");
    assert!(
        mae > 0.0,
        "scrubbing to a different tick must change the rendered frame (mae = {mae})",
    );
}
