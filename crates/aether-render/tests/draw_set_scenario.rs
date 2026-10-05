//! Draw-set harness scenario (ADR-0246 decisions 1 and 2): the
//! `aether.render.{create,update,destroy}_draw_set` family driven
//! end-to-end through an in-process `SubstrateHarness`. The scenario's
//! surface is the registry over the mail path, and what a
//! `destroy_geometry` does to a set that names the geometry; a pass
//! drawing a set is `draw_sets_pass_scenario.rs`.
//!
//! Skipped when no wgpu adapter is available (driverless runners), as
//! the instance-registry scenario is.

use aether_data::{Blob, Kind};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::has_wgpu_adapter;
use aether_render::RenderCapability;
use aether_render::{
    CreateDrawSet, CreateDrawSetResult, CreateGeometry, CreateGeometryResult, CreateInstances, CreateInstancesResult,
    DestroyDrawSet, DestroyGeometry, DrawSpec, IndexRange, InstanceRange, UpdateDrawSet, UpdateDrawSetResult,
    VertexAttribute, VertexFormat,
};

/// One position per vertex: stride 12.
fn vertex_layout() -> Vec<VertexAttribute> {
    vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }]
}

/// One offset per instance: stride 8.
fn instance_layout() -> Vec<VertexAttribute> {
    vec![VertexAttribute { location: 1, format: VertexFormat::Float32x2 }]
}

/// Run one request and decode its reply as `R`. The request comes
/// first so a call site can build it from the harness it then lends.
fn reply<R: Kind>(request: HarnessOp, label: &'static str, harness: &mut SubstrateHarness) -> R {
    harness.execute(vec![(label, request)]).expect("request sequence").reply::<R>(label).expect("decode the reply")
}

/// A draw of the whole triangle over both instance records.
fn draw(geometry_id: u32, instances_id: u32) -> DrawSpec {
    DrawSpec {
        geometry_id,
        indices: IndexRange { first: 0, count: 3 },
        instances_id,
        instances: InstanceRange { first: 0, count: 2 },
    }
}

fn create_set(draws: Vec<DrawSpec>) -> CreateDrawSet {
    CreateDrawSet { vertex_layout: vertex_layout(), instance_layout: instance_layout(), draws }
}

/// One triangle and a two-record instance buffer under the scenario's
/// layouts, as `(geometry_id, instances_id)`.
fn created_buffers(harness: &mut SubstrateHarness) -> (u32, u32) {
    let triangle: Vec<u8> = [0u32, 1, 2].iter().flat_map(|index| index.to_le_bytes()).collect();
    let geometry: CreateGeometryResult = reply(
        HarnessOp::send_and_await_reply(
            &harness.actor_ref::<RenderCapability>(),
            &CreateGeometry {
                layout: vertex_layout(),
                vertices: Blob::from(vec![0u8; 36]),
                indices: Blob::from(triangle),
            },
        ),
        "create_geometry",
        harness,
    );
    let CreateGeometryResult::Ok { geometry_id } = geometry else {
        panic!("create_geometry failed: {geometry:?}");
    };
    let instances: CreateInstancesResult = reply(
        HarnessOp::send_and_await_reply(
            &harness.actor_ref::<RenderCapability>(),
            &CreateInstances { layout: instance_layout(), capacity: 2, records: Blob::from(Vec::new()) },
        ),
        "create_instances",
        harness,
    );
    let CreateInstancesResult::Ok { instances_id } = instances else {
        panic!("create_instances failed: {instances:?}");
    };
    (geometry_id, instances_id)
}

/// The draw-set lifecycle over mail: a valid create replies id 0; a
/// create naming an unknown geometry replies an error and the next
/// accepted set is id 1; a patch replies `Ok`; once `destroy_geometry`
/// has settled, a patch naming that geometry replies unknown id while
/// the set that still holds it is destroyed cleanly. The named bugs: a
/// handler missing from the render actor (a request would never be
/// answered, the tell would never settle), a refused create burning an
/// id over the mail path, and a retired geometry id still nameable by a
/// new draw.
#[test]
fn draw_set_lifecycle_round_trips_over_mail() {
    // The composed render cap is the pumped GPU runtime.
    if !has_wgpu_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let (geometry_id, instances_id) = created_buffers(&mut harness);

    let first: CreateDrawSetResult = reply(
        HarnessOp::send_and_await_reply(
            &harness.actor_ref::<RenderCapability>(),
            &create_set(vec![draw(geometry_id, instances_id)]),
        ),
        "create",
        &mut harness,
    );
    assert!(
        matches!(first, CreateDrawSetResult::Ok { draw_set_id: 0 }),
        "the first accepted draw set must be id 0; got {first:?}",
    );

    let refused: CreateDrawSetResult = reply(
        HarnessOp::send_and_await_reply(
            &harness.actor_ref::<RenderCapability>(),
            &create_set(vec![draw(99, instances_id)]),
        ),
        "create_unknown_geometry",
        &mut harness,
    );
    match refused {
        CreateDrawSetResult::Err { error } => {
            assert!(error.contains("unknown geometry id 99"), "the refusal must name its class; got {error}");
        }
        CreateDrawSetResult::Ok { draw_set_id } => panic!("an unknown geometry must refuse; got set {draw_set_id}"),
    }
    let second: CreateDrawSetResult = reply(
        HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), &create_set(Vec::new())),
        "create_empty",
        &mut harness,
    );
    assert!(
        matches!(second, CreateDrawSetResult::Ok { draw_set_id: 1 }),
        "a refused create must not consume an id; got {second:?}",
    );

    let appended: UpdateDrawSetResult = reply(
        HarnessOp::send_and_await_reply(
            &harness.actor_ref::<RenderCapability>(),
            &UpdateDrawSet { draw_set_id: 0, first: 1, draws: vec![draw(geometry_id, instances_id)] },
        ),
        "append",
        &mut harness,
    );
    assert!(matches!(appended, UpdateDrawSetResult::Ok), "an append at the length must be accepted; got {appended:?}");

    harness
        .execute(vec![(
            "destroy_geometry",
            HarnessOp::send_and_settle(&harness.actor_ref::<RenderCapability>(), &DestroyGeometry { geometry_id }),
        )])
        .expect("destroy_geometry settles");
    let retired: UpdateDrawSetResult = reply(
        HarnessOp::send_and_await_reply(
            &harness.actor_ref::<RenderCapability>(),
            &UpdateDrawSet { draw_set_id: 1, first: 0, draws: vec![draw(geometry_id, instances_id)] },
        ),
        "patch_retired_geometry",
        &mut harness,
    );
    match retired {
        UpdateDrawSetResult::Err { error } => {
            assert!(error.contains("unknown geometry id"), "a retired id must not be nameable; got {error}");
        }
        UpdateDrawSetResult::Ok => panic!("a patch naming a destroyed geometry must refuse"),
    }

    // Set 0 still holds the destroyed geometry; its destroy is the last
    // release. Fire-and-forget, so settlement is the observable.
    harness
        .execute(vec![(
            "destroy_draw_set",
            HarnessOp::send_and_settle(&harness.actor_ref::<RenderCapability>(), &DestroyDrawSet { draw_set_id: 0 }),
        )])
        .expect("destroy_draw_set settles");
}
