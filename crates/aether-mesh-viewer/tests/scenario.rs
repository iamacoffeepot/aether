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

use aether_data::Kind;
use aether_kinds::{LoadComponent, LoadResult, Tick};
use aether_mesh_viewer::LoadMesh;
use aether_substrate_bundle::test_bench::{
    TestBench,
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

/// User-facing component name passed to `LoadComponent`.
const COMPONENT_NAME: &str = "mv";

/// Full mailbox address the substrate registers for the loaded
/// component (issue 634 Phase 4 PR 1). Mail to the bare
/// `COMPONENT_NAME` warn-drops as unknown — agents address the
/// trampoline by its full `aether.component.trampoline:NAME` form,
/// which is what `LoadResult.name` returns. Built from
/// `WasmTrampoline::NAMESPACE` — the cap-owned single source of truth
/// post issue 654.
fn component_address() -> String {
    use aether_actor::Actor;
    format!(
        "{}:{}",
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
fn load_viewer(bench: &mut TestBench, wasm_path: &std::path::Path) {
    let wasm = std::fs::read(wasm_path).expect("read mesh-viewer wasm");
    let result: LoadResult = bench
        .send_and_await_reply(
            "aether.component",
            &LoadComponent {
                wasm,
                name: Some(COMPONENT_NAME.to_owned()),
            },
        )
        .expect("await load_component reply");
    match result {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

/// Direct `aether.tick` to the loaded viewer so the next `capture`
/// sees fresh render-sink emissions.
///
/// Background: `TestBench::capture` runs its frame with
/// `dispatch_tick=false` (capture is a state snapshot, not a tick
/// advance). The render sink's vert buffer is consumed-and-replaced
/// every frame, so a component that emits geometry only on `on_tick`
/// paints nothing during the capture frame even though the previous
/// `advance` ticked it. Sending `Tick` directly to the component's
/// mailbox right before `capture` queues a tick that drains alongside
/// the capture request, populating the buffer before the offscreen
/// render reads it.
fn nudge_tick(bench: &mut TestBench) {
    // `Tick` is a unit struct on the cast wire shape (no Serialize),
    // so it can't go through `send_mail::<K>` (postcard-only). Use
    // the bytes path with the kind's own `encode_into_bytes` helper,
    // which the derive emits per-shape.
    bench
        .send_bytes(&component_address(), Tick::ID, Tick.encode_into_bytes())
        .expect("send tick");
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

    // First tick triggers the load; the read reply lands on a later
    // tick. A handful of ticks ensures the cache is populated and
    // several render-sink emissions land.
    bench.advance(1).expect("priming advance");
    bench
        .send_mail(
            &component_address(),
            &LoadMesh {
                namespace: "save".to_owned(),
                path,
            },
        )
        .expect("send mesh.load");
    bench.advance(5).expect("post-load advance");
    nudge_tick(&mut bench);

    let png = bench.capture().expect("capture");
    let img = decode_png(&png).expect("decode capture png");
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

    bench.advance(1).expect("priming advance");
    bench
        .send_mail(
            &component_address(),
            &LoadMesh {
                namespace: "save".to_owned(),
                path,
            },
        )
        .expect("send mesh.load");
    bench.advance(5).expect("post-load advance");
    nudge_tick(&mut bench);

    let png = bench.capture().expect("capture");
    let img = decode_png(&png).expect("decode capture png");
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

    bench.advance(1).expect("priming advance");
    bench
        .send_mail(
            &component_address(),
            &LoadMesh {
                namespace: "save".to_owned(),
                path: good,
            },
        )
        .expect("send good mesh.load");
    bench.advance(5).expect("post-good-load advance");

    // Baseline: the good mesh is publishing.
    assert_draw_triangle_observed(&bench);

    // Now hand the viewer something it can't parse.
    bench
        .send_mail(
            &component_address(),
            &LoadMesh {
                namespace: "save".to_owned(),
                path: bad,
            },
        )
        .expect("send bad mesh.load");
    bench.advance(5).expect("post-bad-load advance");
    nudge_tick(&mut bench);

    // The cached triangle list should be intact — the captured frame
    // still has non-clear-color geometry.
    let png = bench.capture().expect("capture");
    let img = decode_png(&png).expect("decode capture png");
    differs_from_background(&img, 5)
        .expect("cached mesh should remain visible after parse failure");
}
