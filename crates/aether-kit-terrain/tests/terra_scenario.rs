//! Strict typed terra flow through two real wasm components.

use aether_harness_substrate::test_helpers::require_wasm;
use std::fs;
use std::path::Path;

use aether_actor::{Addressable, Kind};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_kit_terrain::mark::{
    Mark, MarkCreate, MarkCreateResult, MarkGeometry, MarkGet, MarkGetResult, MarkRef, MarkUpdate, MarkUpdateResult,
};
use aether_kit_terrain::terra::{
    CreateTerraMark, DeleteTerraSelection, MoveTerraSelection, RelabelTerraSelection, SetTerraSelection,
    TerraCommandResult, TerraConfig, TerraError, TerraQuery, TerraQueryResult, WorldDelta,
};
use aether_kit_terrain::world::WorldPoint;

#[allow(unused_imports)]
use aether_kit_terrain as _;

const MARK_COMPONENT_NAME: &str = "terrain-marks";
const TERRA_COMPONENT_NAME: &str = "terra";

fn component_address(name: &str) -> String {
    format!("aether.component/{}:{name}", aether_component::WasmTrampoline::NAMESPACE)
}

fn load_mark_book(harness: &mut SubstrateHarness, wasm_path: &Path) -> aether_data::MailboxId {
    let loaded = harness
        .execute(vec![(
            "load_mark_book",
            HarnessOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: fs::read(wasm_path).expect("read kit wasm for mark book"),
                    name: Some(MARK_COMPONENT_NAME.to_owned()),
                    config: Vec::new(),
                    export: Some("aether.kit.mark".to_owned()),
                },
            ),
        )])
        .expect("load mark book sequence");
    match loaded.reply::<LoadResult>("load_mark_book").expect("decode mark-book LoadResult") {
        LoadResult::Ok { name, mailbox_id, .. } => {
            assert_eq!(name, component_address(MARK_COMPONENT_NAME));
            mailbox_id
        }
        LoadResult::Err { error } => panic!("load mark book: {error}"),
    }
}

fn load_terra(harness: &mut SubstrateHarness, wasm_path: &Path, mark_book_mailbox: aether_data::MailboxId) {
    let config = TerraConfig { mark_book_mailbox };
    let loaded = harness
        .execute(vec![(
            "load_terra",
            HarnessOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: fs::read(wasm_path).expect("read kit wasm for terra"),
                    name: Some(TERRA_COMPONENT_NAME.to_owned()),
                    config: config.encode_into_bytes(),
                    export: Some("aether.kit.terra".to_owned()),
                },
            ),
        )])
        .expect("load terra sequence");
    match loaded.reply::<LoadResult>("load_terra").expect("decode terra LoadResult") {
        LoadResult::Ok { name, .. } => {
            assert_eq!(name, component_address(TERRA_COMPONENT_NAME));
        }
        LoadResult::Err { error } => panic!("load terra: {error}"),
    }
}

fn create_mark(harness: &mut SubstrateHarness, address: &str, geometry: MarkGeometry, label: &str) -> MarkRef {
    let created = harness
        .execute(vec![(
            "create",
            HarnessOp::send_and_await(address, &MarkCreate { geometry, label: label.to_owned() }),
        )])
        .expect("create mark sequence");
    match created.reply::<MarkCreateResult>("create").expect("decode MarkCreateResult") {
        MarkCreateResult::Created { reference } => reference,
        MarkCreateResult::Rejected { error } => panic!("create mark rejected: {error:?}"),
    }
}

fn get_mark(harness: &mut SubstrateHarness, address: &str, reference: MarkRef) -> Mark {
    let fetched = harness
        .execute(vec![("get", HarnessOp::send_and_await(address, &MarkGet { id: reference.id }))])
        .expect("get mark sequence");
    fetched.reply::<MarkGetResult>("get").expect("decode MarkGetResult").mark.expect("mark exists")
}

struct AppliedCommand {
    selection: Vec<MarkRef>,
    changed: Vec<MarkRef>,
    deleted: Vec<MarkRef>,
}

fn expect_applied(result: TerraCommandResult) -> AppliedCommand {
    match result {
        TerraCommandResult::Applied { selection, changed, deleted } => AppliedCommand { selection, changed, deleted },
        other => panic!("expected applied terra command, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn terra_selection_semantics_and_preflight_run_through_real_wasm() {
    let Some(wasm_path) = require_wasm("aether_kit_terrain") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let mark_mailbox = load_mark_book(&mut harness, &wasm_path);
    load_terra(&mut harness, &wasm_path, mark_mailbox);
    let mark_address = component_address(MARK_COMPONENT_NAME);
    let terra_address = component_address(TERRA_COMPONENT_NAME);

    let created_point = harness
        .execute(vec![(
            "terra_create",
            HarnessOp::send_and_await(
                &terra_address,
                &CreateTerraMark { geometry: MarkGeometry::Point(WorldPoint::new(10, 20)), label: "camp".to_owned() },
            ),
        )])
        .expect("terra create sequence");
    let created_point =
        expect_applied(created_point.reply::<TerraCommandResult>("terra_create").expect("decode terra create result"));
    assert_eq!(created_point.selection, created_point.changed);
    assert_eq!(created_point.changed.len(), 1);
    assert!(created_point.deleted.is_empty());
    let point = created_point.changed[0];
    assert_eq!(
        get_mark(&mut harness, &mark_address, point),
        Mark {
            id: point.id,
            revision: 1,
            geometry: MarkGeometry::Point(WorldPoint::new(10, 20)),
            label: "camp".to_owned(),
        }
    );
    let path = create_mark(
        &mut harness,
        &mark_address,
        MarkGeometry::Path(vec![WorldPoint::new(0, 0), WorldPoint::new(4, 8)]),
        "trail",
    );
    let area = create_mark(
        &mut harness,
        &mark_address,
        MarkGeometry::Area(vec![WorldPoint::new(-4, 0), WorldPoint::new(0, 8), WorldPoint::new(4, 0)]),
        "grove",
    );

    let ordered = vec![area, point, path];
    let selected = harness
        .execute(vec![
            ("set", HarnessOp::send_and_await(&terra_address, &SetTerraSelection { references: ordered.clone() })),
            ("query", HarnessOp::send_and_await(&terra_address, &TerraQuery)),
        ])
        .expect("set and query sequence");
    let set = expect_applied(selected.reply::<TerraCommandResult>("set").expect("decode set result"));
    assert_eq!(set.selection, ordered);
    assert!(set.changed.is_empty());
    assert!(set.deleted.is_empty());
    assert_eq!(
        selected.reply::<TerraQueryResult>("query").expect("decode query result"),
        TerraQueryResult { selection: ordered, busy: false }
    );

    let moved = harness
        .execute(vec![(
            "move",
            HarnessOp::send_and_await(
                &terra_address,
                &MoveTerraSelection { delta: WorldDelta { x_octimeters: 3, z_octimeters: -2 } },
            ),
        )])
        .expect("move sequence");
    let moved = expect_applied(moved.reply::<TerraCommandResult>("move").expect("decode move result"));
    assert_eq!(moved.selection, moved.changed);
    assert_eq!(moved.changed.len(), 3);
    assert!(moved.deleted.is_empty());
    assert_eq!(
        get_mark(&mut harness, &mark_address, moved.changed[0]),
        Mark {
            id: area.id,
            revision: 2,
            geometry: MarkGeometry::Area(vec![WorldPoint::new(-1, -2), WorldPoint::new(3, 6), WorldPoint::new(7, -2),]),
            label: "grove".to_owned(),
        }
    );
    assert_eq!(
        get_mark(&mut harness, &mark_address, moved.changed[1]),
        Mark {
            id: point.id,
            revision: 2,
            geometry: MarkGeometry::Point(WorldPoint::new(13, 18)),
            label: "camp".to_owned(),
        }
    );
    assert_eq!(
        get_mark(&mut harness, &mark_address, moved.changed[2]),
        Mark {
            id: path.id,
            revision: 2,
            geometry: MarkGeometry::Path(vec![WorldPoint::new(3, -2), WorldPoint::new(7, 6),]),
            label: "trail".to_owned(),
        }
    );

    let relabeled = harness
        .execute(vec![(
            "relabel",
            HarnessOp::send_and_await(&terra_address, &RelabelTerraSelection { label: "ridge".to_owned() }),
        )])
        .expect("relabel sequence");
    let relabeled = expect_applied(relabeled.reply::<TerraCommandResult>("relabel").expect("decode relabel result"));
    assert_eq!(relabeled.selection, relabeled.changed);
    assert!(relabeled.deleted.is_empty());
    for reference in &relabeled.selection {
        let mark = get_mark(&mut harness, &mark_address, *reference);
        assert_eq!(mark.reference(), *reference);
        assert_eq!(mark.label, "ridge");
    }
    let before_external: Vec<Mark> =
        relabeled.selection.iter().map(|reference| get_mark(&mut harness, &mark_address, *reference)).collect();
    let stale_index = relabeled.selection.len() - 1;

    let externally_changed = harness
        .execute(vec![(
            "external_update",
            HarnessOp::send_and_await(
                &mark_address,
                &MarkUpdate {
                    id: relabeled.selection[stale_index].id,
                    geometry: None,
                    label: Some("external".to_owned()),
                },
            ),
        )])
        .expect("external update sequence");
    let externally_changed = match externally_changed
        .reply::<MarkUpdateResult>("external_update")
        .expect("decode external MarkUpdateResult")
    {
        MarkUpdateResult::Updated { reference } => reference,
        other => panic!("external update failed: {other:?}"),
    };

    let stale_move = harness
        .execute(vec![(
            "stale_move",
            HarnessOp::send_and_await(
                &terra_address,
                &MoveTerraSelection { delta: WorldDelta { x_octimeters: 100, z_octimeters: 100 } },
            ),
        )])
        .expect("stale move sequence");
    assert_eq!(
        stale_move.reply::<TerraCommandResult>("stale_move").expect("decode stale move result"),
        TerraCommandResult::Rejected {
            selection: relabeled.selection.clone(),
            error: TerraError::StaleReference {
                requested: relabeled.selection[stale_index],
                current: externally_changed,
            },
        }
    );
    for (reference, before) in relabeled.selection[..stale_index].iter().zip(&before_external[..stale_index]) {
        assert_eq!(
            get_mark(&mut harness, &mark_address, *reference),
            *before,
            "complete preflight must perform no eager writes before the final stale mark"
        );
    }
    let changed_after_stale = get_mark(&mut harness, &mark_address, externally_changed);
    assert_eq!(changed_after_stale.reference(), externally_changed);
    assert_eq!(changed_after_stale.geometry, before_external[stale_index].geometry);
    assert_eq!(changed_after_stale.label, "external");

    let fresh_selection = vec![relabeled.selection[0], relabeled.selection[1], externally_changed];
    let deleted = harness
        .execute(vec![
            (
                "set_fresh",
                HarnessOp::send_and_await(&terra_address, &SetTerraSelection { references: fresh_selection.clone() }),
            ),
            ("delete", HarnessOp::send_and_await(&terra_address, &DeleteTerraSelection)),
            ("query", HarnessOp::send_and_await(&terra_address, &TerraQuery)),
        ])
        .expect("fresh selection and delete sequence");
    assert_eq!(
        expect_applied(deleted.reply::<TerraCommandResult>("set_fresh").expect("decode fresh set result")).selection,
        fresh_selection
    );
    let deleted_result = expect_applied(deleted.reply::<TerraCommandResult>("delete").expect("decode delete result"));
    assert!(deleted_result.selection.is_empty());
    assert!(deleted_result.changed.is_empty());
    assert_eq!(deleted_result.deleted, fresh_selection);
    assert_eq!(
        deleted.reply::<TerraQueryResult>("query").expect("decode final query result"),
        TerraQueryResult { selection: Vec::new(), busy: false }
    );
}
