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

use aether_kinds::{LoadComponent, LoadResult, MeshLoadResult};
use aether_mesh_viewer::LoadMesh;
use aether_substrate_bundle::test_bench::{
    BenchOp, TestBench,
    test_helpers::{init_save_sandbox, require_runtime, test_namespace_roots, write_fixture},
    visual::{decode_png, differs_from_background},
};

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
/// trampoline by its full `aether.component.trampoline:NAME` form,
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
