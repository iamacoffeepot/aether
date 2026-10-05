//! Instance-registry harness scenario (ADR-0246 decision 3): the
//! `aether.render.{create,update,destroy}_instances` family driven
//! end-to-end through an in-process `SubstrateHarness`. Nothing draws
//! from an instance buffer yet — the draw-set stage is its first
//! reader — so the scenario's surface is the registry over the mail
//! path.
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

use aether_data::Blob;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::has_wgpu_adapter;
use aether_render::RenderCapability;
use aether_render::{
    CreateInstances, CreateInstancesResult, DestroyInstances, UpdateInstances, VertexAttribute, VertexFormat,
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

/// One offset per instance: stride 12.
fn offset_layout() -> Vec<VertexAttribute> {
    vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }]
}

fn create_reply(harness: &mut SubstrateHarness, label: &'static str, mail: &CreateInstances) -> CreateInstancesResult {
    harness
        .execute(vec![(label, HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), mail))])
        .expect("create_instances sequence")
        .reply::<CreateInstancesResult>(label)
        .expect("decode CreateInstancesResult")
}

fn created_id(harness: &mut SubstrateHarness, label: &'static str, mail: &CreateInstances) -> u32 {
    match create_reply(harness, label, mail) {
        CreateInstancesResult::Ok { instances_id } => instances_id,
        CreateInstancesResult::Err { error } => panic!("create_instances ({label}) failed: {error}"),
    }
}

/// The instance-buffer lifecycle over mail: a valid create replies id
/// 0, an off-stride create replies an error naming its class and
/// consumes no id, an update, a destroy and an update against the
/// destroyed id all settle, and the next create replies id 1. The named
/// bugs: a handler missing from the render actor (the create would
/// never be answered, a tell would never settle), a refused create
/// burning an id over the mail path, and a destroyed id being handed
/// out again.
#[test]
fn instances_lifecycle_round_trips_over_mail() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    // Four records of capacity, the first two supplied.
    let first = created_id(
        &mut harness,
        "create",
        &CreateInstances { layout: offset_layout(), capacity: 4, records: Blob::from(vec![0u8; 24]) },
    );
    assert_eq!(first, 0, "the first accepted instance buffer must be id 0");

    // 23 bytes over the 12-byte stride.
    let refused = create_reply(
        &mut harness,
        "create_off_stride",
        &CreateInstances { layout: offset_layout(), capacity: 4, records: Blob::from(vec![0u8; 23]) },
    );
    match refused {
        CreateInstancesResult::Err { error } => {
            assert!(error.contains("stride"), "the off-stride create must name its class; got {error}");
        }
        CreateInstancesResult::Ok { instances_id } => {
            panic!("the off-stride create must refuse; got id {instances_id}")
        }
    }

    // Each is fire-and-forget, so settlement is the observable: a
    // wedged or crashed handler would never settle.
    harness
        .execute(vec![
            (
                "update",
                HarnessOp::send_and_settle(
                    &harness.actor_ref::<RenderCapability>(),
                    &UpdateInstances { instances_id: first, first: 2, records: Blob::from(vec![1u8; 24]) },
                ),
            ),
            (
                "destroy",
                HarnessOp::send_and_settle(
                    &harness.actor_ref::<RenderCapability>(),
                    &DestroyInstances { instances_id: first },
                ),
            ),
            (
                "update_after_destroy",
                HarnessOp::send_and_settle(
                    &harness.actor_ref::<RenderCapability>(),
                    &UpdateInstances { instances_id: first, first: 0, records: Blob::from(vec![1u8; 12]) },
                ),
            ),
        ])
        .expect("update, destroy, and post-destroy update all settle");

    let second = created_id(
        &mut harness,
        "create_after_destroy",
        &CreateInstances { layout: offset_layout(), capacity: 1, records: Blob::from(Vec::new()) },
    );
    assert_eq!(second, 1, "ids stay dense over accepted creates and a destroyed id is not recycled");
}
