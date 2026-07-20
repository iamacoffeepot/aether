//! Terrain-mark CRUD and hot-swap coverage through the real wasm component.
//!
//! The scenario selects the non-entry `aether.kit.mark` actor, exercises its
//! typed request/reply surface, and replaces the component at the same mailbox
//! with the same wasm. The final allocation proves both live marks and the
//! deleted-id watermark crossed the lifecycle boundary.

use std::fs;
use std::path::Path;

use aether_actor::Addressable;
use aether_kinds::{LoadComponent, LoadResult, ReplaceComponent, ReplaceResult};
use aether_kit::mark::{
    Mark, MarkCreate, MarkCreateResult, MarkDelete, MarkDeleteResult, MarkGeometry, MarkGet, MarkGetResult, MarkId,
    MarkList, MarkListResult, MarkRef, MarkUpdate, MarkUpdateResult,
};
use aether_kit::world::WorldPoint;
use aether_substrate_bundle::substrate_bench::{BenchOp, SubstrateBench, test_helpers::require_runtime};

// Retain all of the kit's native inventory submissions in this integration
// test binary, matching the other component scenarios.
#[allow(unused_imports)]
use aether_kit as _;

const COMPONENT_NAME: &str = "marks";

fn component_address() -> String {
    format!("aether.component/{}:{COMPONENT_NAME}", aether_component::WasmTrampoline::NAMESPACE)
}

fn load_mark_book(bench: &mut SubstrateBench, wasm_path: &Path) -> aether_data::MailboxId {
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: fs::read(wasm_path).expect("read kit wasm"),
                    name: Some(COMPONENT_NAME.to_owned()),
                    config: Vec::new(),
                    export: Some("aether.kit.mark".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, mailbox_id, .. } => {
            assert_eq!(name, component_address());
            mailbox_id
        }
        LoadResult::Err { error } => panic!("load mark book: {error}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)] // one cohesive CRUD → replace → allocation-watermark proof
fn mark_crud_and_allocation_survive_component_replace() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let mut bench = SubstrateBench::start_with_size(64, 48).expect("boot");
    let mailbox_id = load_mark_book(&mut bench, &wasm_path);
    let address = component_address();

    let created = bench
        .execute(vec![
            (
                "create_point",
                BenchOp::send_and_await(
                    address.as_str(),
                    &MarkCreate { geometry: MarkGeometry::Point(WorldPoint::new(128, 256)), label: "camp".to_owned() },
                ),
            ),
            (
                "create_path",
                BenchOp::send_and_await(
                    address.as_str(),
                    &MarkCreate {
                        geometry: MarkGeometry::Path(vec![WorldPoint::new(0, 0), WorldPoint::new(256, 256)]),
                        label: "trail".to_owned(),
                    },
                ),
            ),
        ])
        .expect("create sequence");
    let point_ref = match created.reply::<MarkCreateResult>("create_point").expect("decode point MarkCreateResult") {
        MarkCreateResult::Created { reference } => reference,
        MarkCreateResult::Rejected { error } => panic!("create point rejected: {error:?}"),
    };
    let path_ref = match created.reply::<MarkCreateResult>("create_path").expect("decode path MarkCreateResult") {
        MarkCreateResult::Created { reference } => reference,
        MarkCreateResult::Rejected { error } => panic!("create path rejected: {error:?}"),
    };
    assert_eq!(point_ref, MarkRef { id: MarkId::new(1), revision: 1 });
    assert_eq!(path_ref, MarkRef { id: MarkId::new(2), revision: 1 });

    let updated_geometry =
        MarkGeometry::Area(vec![WorldPoint::new(64, 64), WorldPoint::new(192, 64), WorldPoint::new(128, 192)]);
    let changed = bench
        .execute(vec![
            (
                "update",
                BenchOp::send_and_await(
                    address.as_str(),
                    &MarkUpdate {
                        id: point_ref.id,
                        geometry: Some(updated_geometry.clone()),
                        label: Some("rally point".to_owned()),
                    },
                ),
            ),
            ("get", BenchOp::send_and_await(address.as_str(), &MarkGet { id: point_ref.id })),
            ("list", BenchOp::send_and_await(address.as_str(), &MarkList)),
            ("delete", BenchOp::send_and_await(address.as_str(), &MarkDelete { id: path_ref.id })),
            ("get_deleted", BenchOp::send_and_await(address.as_str(), &MarkGet { id: path_ref.id })),
        ])
        .expect("update, read, and delete sequence");
    assert_eq!(
        changed.reply::<MarkUpdateResult>("update").expect("decode MarkUpdateResult"),
        MarkUpdateResult::Updated { reference: MarkRef { id: point_ref.id, revision: 2 } }
    );
    let expected_mark =
        Mark { id: point_ref.id, revision: 2, geometry: updated_geometry, label: "rally point".to_owned() };
    assert_eq!(
        changed.reply::<MarkGetResult>("get").expect("decode MarkGetResult"),
        MarkGetResult { mark: Some(expected_mark.clone()) }
    );
    assert_eq!(
        changed
            .reply::<MarkListResult>("list")
            .expect("decode MarkListResult")
            .marks
            .iter()
            .map(|mark| mark.id)
            .collect::<Vec<_>>(),
        vec![point_ref.id, path_ref.id],
        "list order must follow stable ids"
    );
    assert_eq!(
        changed.reply::<MarkDeleteResult>("delete").expect("decode MarkDeleteResult"),
        MarkDeleteResult::Deleted { reference: path_ref }
    );
    assert_eq!(
        changed.reply::<MarkGetResult>("get_deleted").expect("decode deleted MarkGetResult"),
        MarkGetResult { mark: None }
    );

    let replaced = bench
        .execute(vec![(
            "replace",
            BenchOp::send_and_await(
                "aether.component",
                &ReplaceComponent {
                    mailbox_id,
                    wasm: fs::read(&wasm_path).expect("re-read kit wasm"),
                    drain_timeout_ms: None,
                    config: Vec::new(),
                    export: None,
                },
            ),
        )])
        .expect("replace sequence");
    match replaced.reply::<ReplaceResult>("replace").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace mark book: {error}"),
    }

    let after = bench
        .execute(vec![
            ("get", BenchOp::send_and_await(address.as_str(), &MarkGet { id: point_ref.id })),
            ("list", BenchOp::send_and_await(address.as_str(), &MarkList)),
            (
                "create",
                BenchOp::send_and_await(
                    address.as_str(),
                    &MarkCreate {
                        geometry: MarkGeometry::Point(WorldPoint::new(512, 768)),
                        label: "lookout".to_owned(),
                    },
                ),
            ),
        ])
        .expect("post-replace sequence");
    assert_eq!(
        after.reply::<MarkGetResult>("get").expect("decode post-replace MarkGetResult"),
        MarkGetResult { mark: Some(expected_mark.clone()) }
    );
    assert_eq!(
        after.reply::<MarkListResult>("list").expect("decode post-replace MarkListResult"),
        MarkListResult { marks: vec![expected_mark] }
    );
    assert_eq!(
        after.reply::<MarkCreateResult>("create").expect("decode post-replace MarkCreateResult"),
        MarkCreateResult::Created { reference: MarkRef { id: MarkId::new(3), revision: 1 } },
        "replacement must retain the next-id watermark above the deleted id"
    );
}
