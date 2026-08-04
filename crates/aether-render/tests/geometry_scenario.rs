//! Geometry-registry harness scenario (ADR-0171, issue #4384): the
//! `aether.render.{create,update,destroy}_geometry` family driven
//! end-to-end through an in-process `SubstrateHarness`, pinning the
//! reply shapes over the mail path. No consumer executes against the
//! registry yet — the draw-pass stage is the next slice — so the
//! scenario's surface is registry behavior only.
//!
//! Skipped when no wgpu adapter is available (driverless runners);
//! `AETHER_REQUIRE_RUNTIME=1` (CI) flips the skip into a hard panic.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]

use std::env;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::has_wgpu_adapter;
use aether_render::{
    CreateGeometry, CreateGeometryResult, DestroyGeometry, UpdateGeometry, VertexAttribute, VertexFormat,
};

/// Skip (or panic under `AETHER_REQUIRE_RUNTIME`) when no wgpu adapter
/// is available — the composed render cap is the pumped GPU runtime.
fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

/// The minimal layout: one position attribute, stride 12.
fn position_layout() -> Vec<VertexAttribute> {
    vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }]
}

fn indices_bytes(indices: &[u32]) -> Vec<u8> {
    indices.iter().flat_map(|index| index.to_le_bytes()).collect()
}

fn create_reply(harness: &mut SubstrateHarness, label: &'static str, mail: &CreateGeometry) -> CreateGeometryResult {
    harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", mail))])
        .expect("create_geometry sequence")
        .reply::<CreateGeometryResult>(label)
        .expect("decode CreateGeometryResult")
}

fn created_id(harness: &mut SubstrateHarness, label: &'static str, mail: &CreateGeometry) -> u32 {
    match create_reply(harness, label, mail) {
        CreateGeometryResult::Ok { geometry_id } => geometry_id,
        CreateGeometryResult::Err { reason } => panic!("create_geometry ({label}) failed: {reason}"),
    }
}

/// ADR-0171 registry lifecycle over mail: a valid create replies
/// `Ok { geometry_id: 0 }`, an invalid create replies `Err { reason }`
/// naming its validation class and consumes no id, the fire-and-forget
/// update and destroy settle without wedging the pumped actor (including
/// an update against the destroyed id, which warn-drops), and a
/// subsequent create replies `Ok { geometry_id: 1 }` — session-scoped
/// ids stay dense over accepted creates and a destroyed id is never
/// recycled. The named bugs: the reply enum's wire shape drifting from
/// the pinned `Ok`/`Err` arms, a rejected create burning an id, a
/// lifecycle mail crashing or hanging the render actor (settlement would
/// never come), and destroy leaving the id allocatable again.
#[test]
fn geometry_lifecycle_round_trips_over_mail() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    // Three position-only vertices, one triangle.
    let first = created_id(
        &mut harness,
        "create",
        &CreateGeometry { layout: position_layout(), vertices: vec![0u8; 36], indices: indices_bytes(&[0, 1, 2]) },
    );
    assert_eq!(first, 0, "the first accepted geometry must be id 0");

    // 35 bytes over the 12-byte position stride: rejected with the
    // stride class, and no id is consumed.
    let rejected = create_reply(
        &mut harness,
        "create_off_stride",
        &CreateGeometry { layout: position_layout(), vertices: vec![0u8; 35], indices: Vec::new() },
    );
    match rejected {
        CreateGeometryResult::Err { reason } => {
            assert!(reason.contains("stride"), "the off-stride create must name its class; got {reason}");
        }
        CreateGeometryResult::Ok { geometry_id } => panic!("the off-stride create must reject; got id {geometry_id}"),
    }

    // In-place replacement resizes the content (four vertices, two
    // triangles), then destroy releases the id; an update against the
    // destroyed id warn-drops. Each is fire-and-forget, so settlement is
    // the observable: a wedged or crashed handler would never settle.
    harness
        .execute(vec![
            (
                "update",
                HarnessOp::send_and_settle(
                    "aether.render",
                    &UpdateGeometry {
                        geometry_id: first,
                        vertices: vec![0u8; 48],
                        indices: indices_bytes(&[0, 1, 2, 0, 2, 3]),
                    },
                ),
            ),
            ("destroy", HarnessOp::send_and_settle("aether.render", &DestroyGeometry { geometry_id: first })),
            (
                "update_after_destroy",
                HarnessOp::send_and_settle(
                    "aether.render",
                    &UpdateGeometry { geometry_id: first, vertices: vec![0u8; 12], indices: indices_bytes(&[0]) },
                ),
            ),
        ])
        .expect("update, destroy, and post-destroy update all settle");

    let second = created_id(
        &mut harness,
        "create_after_destroy",
        &CreateGeometry { layout: position_layout(), vertices: vec![0u8; 12], indices: indices_bytes(&[0]) },
    );
    assert_eq!(second, 1, "ids stay dense over accepted creates and a destroyed id is not recycled");
}
