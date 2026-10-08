//! Overlay draw order between two loaded components (ADR-0248 §4): the
//! renderer lays one actor's screen-space draws over another's by lineage
//! order, which for two root components is the order they were loaded,
//! whatever order their mail reaches it in.
//!
//! Two instances of the `test.ui_widget` fixture draw one `DrawShapes` batch
//! each on the same `Tick`. A stage is fanned out to its subscribers as one
//! burst the scheduler may spread over several workers, so the two draws
//! reach the renderer in an order that can differ from frame to frame; the
//! committed frame lists them in load order on every frame.
//!
//! Skipped when no wgpu adapter is available; a missing pre-built fixture
//! wasm fails (`cargo xtask build-wasm`).

use std::fs;

use aether_data::Kind;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::test_helpers::require_runtime;
use aether_harness_substrate_capture::{RenderHarnessBuilderExt, RenderHarnessExt};
use aether_kinds::LoadComponent;
use aether_test_fixtures_kinds::UiWidgetConfig;

/// Consecutive frames each load order is observed over.
const FRAMES: usize = 16;

/// Boot a harness, load one widget per entry of `shape_counts` in that
/// order, each drawing that many shapes every tick, and return the shape
/// count of every committed shape batch for each of [`FRAMES`] frames.
fn committed_shape_counts(wasm: &[u8], shape_counts: [u32; 2]) -> Vec<Vec<usize>> {
    let mut harness =
        SubstrateHarness::builder().size(64, 48).with_render().with_component_host().build().expect("boot");
    for quad_count in shape_counts {
        let widget = LoadComponent {
            wasm: wasm.to_vec(),
            name: Some(format!("widget-{quad_count}")),
            config: UiWidgetConfig { redraw_each_tick: true, quad_count }.encode_into_bytes(),
            export: Some("test.ui_widget".to_owned()),
        };
        harness.load_any(&widget).expect("the widget loads");
    }

    (0..FRAMES)
        .map(|_| {
            harness.execute(vec![("frame", HarnessOp::advance(1))]).expect("advance one frame");

            harness.committed_shape_snapshot().iter().map(|batch| batch.shapes.len()).collect()
        })
        .collect()
}

/// Catches a lineage-order read with no answer for a real loaded
/// component's stamped sender, which would abort the engine at its first
/// frame, and a commit in `on_frame` that is not handed the read, which
/// would leave the two widgets in whatever order their mail arrived.
#[test]
fn two_components_draw_in_the_order_they_were_loaded_on_every_frame() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read the fixture bundle wasm");

    assert_eq!(
        committed_shape_counts(&wasm, [1, 2]),
        vec![vec![1, 2]; FRAMES],
        "the widget loaded first lies under the one loaded second",
    );
    assert_eq!(
        committed_shape_counts(&wasm, [2, 1]),
        vec![vec![2, 1]; FRAMES],
        "loading them the other way round lays them the other way round",
    );
}
