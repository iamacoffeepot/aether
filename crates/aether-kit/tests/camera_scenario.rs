//! Camera component lifecycle scenario. It boots a `TestBench`, loads
//! `aether-kit`'s wasm artifact (built separately for
//! `wasm32-unknown-unknown`) selecting the non-entry `camera` export
//! (ADR-0096), drives the `CameraComponent` through its
//! `aether.kit.camera.*` mail surface, and asserts mail-flow / render
//! survivability via direct `TestBench` assertions (post-issue-821:
//! the `aether-scenario` Script/Step vocabulary retired in favour of
//! calling the bench methods directly).
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The component's wasm hasn't been built — tests read
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_kit.wasm`
//!   and skip with an `eprintln!` when both paths are absent. CI
//!   builds the wasm before invoking `cargo test`.
//!
//! All boot-time mechanics (wgpu probe, wasm locator, skip-or-panic
//! gate) live in `aether_substrate_bundle::test_bench::test_helpers`
//! (issues 460 + 821).

use aether_capabilities::render::ViewProjection;
use aether_data::Kind;
use aether_kinds::{LoadComponent, LoadResult};
use aether_kit::camera::CameraDestroy;
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::require_runtime};
use aether_substrate_bundle::visual::{decode_png, not_all_black};

// Force linkage of `aether-kit`'s `inventory::submit!` `KindDescriptor`
// entries into this test binary. Cargo treats integration tests as
// separate crates that link against the test target's host rlib, but
// the linker strips inventory submits for kinds the test code doesn't
// statically reference. Without this anchor, `count_observed` against
// the camera-published kinds (and `send_mail::<CameraDestroy>`) would
// still resolve, but other inventory-collected metadata wouldn't —
// keep the anchor for parity with the other component scenario files.
#[allow(unused_imports)]
use aether_kit as _;
use std::fs;
use std::path::Path;

/// Component name passed to `LoadComponent`; [`component_address`]
/// derives the loaded trampoline's full address from it.
const COMPONENT_NAME: &str = "cam";

/// The `/`-rendered lineage a loaded component registers at (ADR-0099
/// §4): the component host `aether.component` `/`-joined to the
/// trampoline node — exactly what `LoadResult.name` reports.
fn component_address() -> String {
    use aether_actor::Addressable;
    format!("aether.component/{}:{}", aether_capabilities::WasmTrampoline::NAMESPACE, COMPONENT_NAME)
}

/// Load `aether-kit`'s pre-built wasm into the bench, selecting the
/// `camera` export (ADR-0096; the kit is defaultless per ADR-0138, so
/// the export selector is required), and await `LoadResult`. Panics on load failure so
/// the calling test surfaces the error message rather than wedging on
/// a missing subscription.
fn load_camera(bench: &mut TestBench, wasm_path: &Path) {
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
                    export: Some("aether.kit.camera".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

#[test]
fn camera_component_lifecycle() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_camera(&mut bench, &wasm_path);

    // The frozen default camera still publishes after initialization.
    bench.execute(vec![("advance", BenchOp::advance(5))]).expect("advance");
    let initialized = bench.count_observed(ViewProjection::NAME);
    assert!(
        initialized >= 1,
        "expected ≥1 aether.view_projection after initialization; got {initialized}; observed kinds: {:?}",
        bench.observed_kinds(),
    );

    // Let destruction and any already in-flight publication settle before
    // taking the cumulative observation baseline.
    bench
        .execute(vec![
            ("destroy", BenchOp::send_mail(component_address(), &CameraDestroy { name: "main".to_owned() })),
            ("settle", BenchOp::advance(2)),
        ])
        .expect("destroy + settle");
    let post_destroy = bench.count_observed(ViewProjection::NAME);

    let result = bench
        .execute(vec![("post_destroy", BenchOp::advance(5)), ("snap", BenchOp::capture())])
        .expect("post-destroy advance + capture");
    let after_window = bench.count_observed(ViewProjection::NAME);
    assert_eq!(
        after_window, post_destroy,
        "aether.view_projection count increased after destroying main: baseline {post_destroy}, after window {after_window}",
    );

    let png = result.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    not_all_black(&img).expect("frame should not be all black after camera destroy");
}
