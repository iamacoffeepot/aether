use super::super::terrain::*;
#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;

use aether_data::{EnumVariant, NamedField, SchemaCell};
use std::collections::{HashMap, VecDeque};
use std::slice::from_ref;
use std::sync::{Arc, Mutex};

fn field(name: &'static str, ty: SchemaType) -> NamedField {
    NamedField { name: name.into(), ty }
}

fn record(fields: Vec<NamedField>) -> SchemaType {
    SchemaType::Struct { fields: fields.into(), repr_c: false }
}

fn cell(schema: SchemaType) -> SchemaCell {
    SchemaCell::Owned(Box::new(schema))
}

fn mark_id_schema() -> SchemaType {
    record(vec![field("0", SchemaType::Scalar(Primitive::U32))])
}

fn point_schema() -> SchemaType {
    record(vec![
        field("x_octimeters", SchemaType::Scalar(Primitive::I32)),
        field("z_octimeters", SchemaType::Scalar(Primitive::I32)),
    ])
}

fn mark_ref_schema() -> SchemaType {
    record(vec![field("id", mark_id_schema()), field("revision", SchemaType::Scalar(Primitive::U32))])
}

fn mark_geometry_schema() -> SchemaType {
    SchemaType::Enum {
        variants: vec![
            EnumVariant::Tuple { name: "Point".into(), discriminant: 0, fields: vec![point_schema()].into() },
            EnumVariant::Tuple {
                name: "Path".into(),
                discriminant: 1,
                fields: vec![SchemaType::Vec(cell(point_schema()))].into(),
            },
            EnumVariant::Tuple {
                name: "Area".into(),
                discriminant: 2,
                fields: vec![SchemaType::Vec(cell(point_schema()))].into(),
            },
        ]
        .into(),
    }
}

fn mark_schema() -> SchemaType {
    record(vec![
        field("id", mark_id_schema()),
        field("revision", SchemaType::Scalar(Primitive::U32)),
        field("geometry", mark_geometry_schema()),
        field("label", SchemaType::String),
    ])
}

fn mark_get_descriptors() -> [KindDescriptor; 2] {
    [
        descriptor("aether.kit.mark.get", record(vec![field("id", mark_id_schema())])),
        descriptor("aether.kit.mark.get_result", record(vec![field("mark", SchemaType::Option(cell(mark_schema())))])),
    ]
}

fn operator_result_descriptor() -> KindDescriptor {
    let chunk = record(vec![
        field("chunk_x", SchemaType::Scalar(Primitive::I32)),
        field("chunk_z", SchemaType::Scalar(Primitive::I32)),
    ]);
    let stats = record(vec![
        field("steps_run", SchemaType::Scalar(Primitive::U32)),
        field("subcells_written", SchemaType::Scalar(Primitive::U32)),
        field("touched_chunks", SchemaType::Vec(cell(chunk))),
    ]);
    let error = SchemaType::Enum {
        variants: vec![
            EnumVariant::Struct {
                name: "InvalidParameters".into(),
                discriminant: 0,
                fields: vec![field("reason", SchemaType::String)].into(),
            },
            EnumVariant::Unit { name: "StepBudgetExhausted".into(), discriminant: 1 },
            EnumVariant::Unit { name: "SubcellBudgetExhausted".into(), discriminant: 2 },
        ]
        .into(),
    };
    descriptor(
        "aether.kit.world.operator_result",
        SchemaType::Enum {
            variants: vec![
                EnumVariant::Struct {
                    name: "Applied".into(),
                    discriminant: 0,
                    fields: vec![field("source", mark_ref_schema()), field("stats", stats.clone())].into(),
                },
                EnumVariant::Struct {
                    name: "Failed".into(),
                    discriminant: 1,
                    fields: vec![field("source", mark_ref_schema()), field("error", error), field("stats", stats)]
                        .into(),
                },
            ]
            .into(),
        },
    )
}

fn proposal_result_descriptor() -> KindDescriptor {
    let proposal_id = record(vec![field("value", SchemaType::Scalar(Primitive::U64))]);
    let operation_result = SchemaType::Enum {
        variants: vec![
            EnumVariant::Unit { name: "Mutation".into(), discriminant: 0 },
            EnumVariant::Struct {
                name: "Operator".into(),
                discriminant: 1,
                fields: vec![field("result", operator_result_descriptor().schema)].into(),
            },
        ]
        .into(),
    };
    let chunk = record(vec![
        field("chunk_x", SchemaType::Scalar(Primitive::I32)),
        field("chunk_z", SchemaType::Scalar(Primitive::I32)),
    ]);
    let bounds = record(vec![
        field("min_x_meters", SchemaType::Scalar(Primitive::F32)),
        field("min_y_meters", SchemaType::Scalar(Primitive::F32)),
        field("min_z_meters", SchemaType::Scalar(Primitive::F32)),
        field("max_x_meters", SchemaType::Scalar(Primitive::F32)),
        field("max_y_meters", SchemaType::Scalar(Primitive::F32)),
        field("max_z_meters", SchemaType::Scalar(Primitive::F32)),
    ]);
    let digest = record(vec![
        field("touched_chunks", SchemaType::Vec(cell(chunk))),
        field("triangle_count", SchemaType::Scalar(Primitive::U64)),
        field("changed_geometry_bounds", SchemaType::Option(cell(bounds))),
    ]);
    let error = SchemaType::Enum {
        variants: vec![
            EnumVariant::Unit { name: "ProposalIdExhausted".into(), discriminant: 0 },
            EnumVariant::Unit { name: "StagedProposalLimitReached".into(), discriminant: 1 },
            EnumVariant::Struct {
                name: "NoTouchedChunks".into(),
                discriminant: 2,
                fields: vec![field("operation_result", operation_result.clone())].into(),
            },
            EnumVariant::Struct {
                name: "UnknownProposal".into(),
                discriminant: 3,
                fields: vec![field("proposal_id", proposal_id.clone())].into(),
            },
            EnumVariant::Struct {
                name: "StaleProposal".into(),
                discriminant: 4,
                fields: vec![
                    field("proposal_id", proposal_id.clone()),
                    field("proposed_at_revision", SchemaType::Scalar(Primitive::U64)),
                    field("committed_revision", SchemaType::Scalar(Primitive::U64)),
                ]
                .into(),
            },
        ]
        .into(),
    };
    descriptor(
        "aether.kit.world.proposal_result",
        SchemaType::Enum {
            variants: vec![
                EnumVariant::Struct {
                    name: "Staged".into(),
                    discriminant: 0,
                    fields: vec![
                        field("proposal_id", proposal_id.clone()),
                        field("operation_result", operation_result),
                        field("digest", digest.clone()),
                    ]
                    .into(),
                },
                EnumVariant::Struct {
                    name: "Committed".into(),
                    discriminant: 1,
                    fields: vec![field("proposal_id", proposal_id.clone()), field("digest", digest.clone())].into(),
                },
                EnumVariant::Struct {
                    name: "Discarded".into(),
                    discriminant: 2,
                    fields: vec![field("proposal_id", proposal_id.clone())].into(),
                },
                EnumVariant::Struct {
                    name: "PreviewSet".into(),
                    discriminant: 3,
                    fields: vec![
                        field("active_proposal_id", SchemaType::Option(cell(proposal_id))),
                        field("digest", SchemaType::Option(cell(digest))),
                    ]
                    .into(),
                },
                EnumVariant::Struct {
                    name: "Rejected".into(),
                    discriminant: 4,
                    fields: vec![field("error", error)].into(),
                },
            ]
            .into(),
        },
    )
}

fn brush_request_descriptor() -> KindDescriptor {
    descriptor(
        "aether.kit.world.apply_brush",
        record(vec![
            field("source", mark_ref_schema()),
            field("path", SchemaType::Vec(cell(point_schema()))),
            field(
                "brush",
                record(vec![
                    field("radius_octimeters", SchemaType::Scalar(Primitive::U32)),
                    field("spacing_octimeters", SchemaType::Scalar(Primitive::U32)),
                    field("material", SchemaType::Scalar(Primitive::U8)),
                ]),
            ),
            field(
                "budget",
                record(vec![
                    field("max_steps", SchemaType::Scalar(Primitive::U32)),
                    field("max_subcells", SchemaType::Scalar(Primitive::U32)),
                ]),
            ),
        ]),
    )
}

fn automaton_request_descriptor() -> KindDescriptor {
    descriptor(
        "aether.kit.world.run_automaton",
        record(vec![
            field("source", mark_ref_schema()),
            field(
                "seed",
                record(vec![
                    field("cell_x", SchemaType::Scalar(Primitive::I32)),
                    field("cell_z", SchemaType::Scalar(Primitive::I32)),
                ]),
            ),
            field(
                "rule",
                SchemaType::Enum {
                    variants: vec![EnumVariant::Struct {
                        name: "Grow".into(),
                        discriminant: 0,
                        fields: vec![
                            field("material", SchemaType::Scalar(Primitive::U8)),
                            field("generations", SchemaType::Scalar(Primitive::U32)),
                        ]
                        .into(),
                    }]
                    .into(),
                },
            ),
            field(
                "budget",
                record(vec![
                    field("max_steps", SchemaType::Scalar(Primitive::U32)),
                    field("max_subcells", SchemaType::Scalar(Primitive::U32)),
                ]),
            ),
        ]),
    )
}

#[allow(clippy::needless_pass_by_value)] // Fixture values are intentionally inline at call sites.
fn mark_result(descriptor: &KindDescriptor, id: u32, revision: u32, geometry: serde_json::Value) -> TerrainRouteReply {
    TerrainRouteReply {
        events: vec![encoded_event(
            descriptor,
            serde_json::json!({
                "mark": { "id": { "0": id }, "revision": revision, "geometry": geometry, "label": "source" }
            }),
        )],
        settle: true,
    }
}

fn descriptor(name: &str, schema: SchemaType) -> KindDescriptor {
    KindDescriptor { name: name.to_owned(), schema }
}

fn descriptor_wire(descriptor: &KindDescriptor) -> KindDescriptorWire {
    KindDescriptorWire {
        id: KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema)),
        name: descriptor.name.clone(),
        schema_wire: wire::to_vec(&descriptor.schema).expect("dynamic test schema wire-encodes"),
    }
}

fn inventory(descriptors: &[KindDescriptor]) -> ListKindsResult {
    ListKindsResult { kinds: descriptors.iter().map(descriptor_wire).collect() }
}

#[allow(clippy::needless_pass_by_value)] // Fixture values are intentionally inline at call sites.
fn encoded_event(descriptor: &KindDescriptor, value: serde_json::Value) -> TerrainReplyEvent {
    TerrainReplyEvent {
        kind: KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema)),
        payload: aether_codec::encode_schema(&value, &descriptor.schema).expect("fixture result schema-encodes"),
    }
}

fn reply_envelope(kind: KindId, payload: Vec<u8>) -> MailEnvelope {
    MailEnvelope {
        to: MailboxAddress { engine: None, mailbox: MailboxId::NONE },
        from: None,
        kind,
        correlation_id: None,
        payload,
    }
}

fn reference(id: u32, revision: u32) -> MarkRef {
    MarkRef { id: MarkId { value: id }, revision }
}

fn point(x_octimeters: i32, z_octimeters: i32) -> WorldPoint {
    WorldPoint { x_octimeters, z_octimeters }
}

#[test]
fn terrain_dtos_use_named_records_and_all_discriminators() {
    let mark: MarkGeometry = serde_json::from_value(serde_json::json!({
        "path": { "points": [{ "x_octimeters": 1, "z_octimeters": 2 }] }
    }))
    .expect("task Path geometry deserializes");
    assert_eq!(mark, MarkGeometry::Path { points: vec![point(1, 2)] });

    let mark_ops = [
        serde_json::json!({ "create": { "geometry": { "point": { "point": { "x_octimeters": 1, "z_octimeters": 2 } } }, "label": "p" } }),
        serde_json::json!({ "update": { "id": { "value": 1 }, "geometry": null, "label": "renamed" } }),
        serde_json::json!({ "delete": { "id": { "value": 1 } } }),
        serde_json::json!({ "get": { "id": { "value": 1 } } }),
        serde_json::json!("list"),
    ];
    for operation in mark_ops {
        serde_json::from_value::<TerrainMarksOperation>(operation).expect("every mark discriminator deserializes");
    }

    let editor_ops = [
        serde_json::json!({ "set_selection": { "references": [{ "id": { "value": 1 }, "revision": 2 }] } }),
        serde_json::json!({ "toggle_selection": { "reference": { "id": { "value": 1 }, "revision": 2 } } }),
        serde_json::json!("clear_selection"),
        serde_json::json!({ "create_mark": { "geometry": { "area": { "points": [] } }, "label": "a" } }),
        serde_json::json!({ "move_selection": { "delta": { "x_octimeters": -1, "z_octimeters": 3 } } }),
        serde_json::json!({ "relabel_selection": { "label": "b" } }),
        serde_json::json!("delete_selection"),
        serde_json::json!("query"),
    ];
    for operation in editor_ops {
        serde_json::from_value::<TerrainEditorOperation>(operation).expect("every editor discriminator deserializes");
    }

    let proposal_ops = [
        serde_json::json!({ "set_chunk": { "chunk_x": 0, "chunk_z": 0, "underlay": [], "underlay_points": [], "height_points": [], "overlay": [], "overlay_mask": [], "height": [], "region": [], "water_plane": [], "smoothing": [] } }),
        serde_json::json!({ "set_cell_points": { "x": 0, "z": 0, "points": [] } }),
        serde_json::json!({ "set_cell_heights": { "x": 0, "z": 0, "deltas": [] } }),
        serde_json::json!({ "stamp_polygon": { "points": [], "material": 1 } }),
        serde_json::json!({ "stamp_disc": { "center": { "x_octimeters": 0, "z_octimeters": 0 }, "radius_octimeters": 1, "material": 1 } }),
        serde_json::json!({ "stamp_hexagon": { "center": { "x_octimeters": 0, "z_octimeters": 0 }, "radius_octimeters": 1, "material": 1 } }),
        serde_json::json!({ "apply_brush": { "mark_book_mailbox": "marks", "source": { "id": { "value": 1 }, "revision": 1 }, "geometry": "source_mark", "brush": { "radius_octimeters": 1, "spacing_octimeters": 1, "material": 1 }, "budget": { "max_steps": 1, "max_subcells": 1 } } }),
        serde_json::json!({ "run_automaton": { "mark_book_mailbox": "marks", "source": { "id": { "value": 1 }, "revision": 1 }, "geometry": { "explicit": { "seed": { "cell_x": 0, "cell_z": 0 } } }, "rule": { "grow": { "material": 1, "generations": 0 } }, "budget": { "max_steps": 1, "max_subcells": 1 } } }),
    ];
    for operation in proposal_ops {
        serde_json::from_value::<TerrainProposalOperation>(operation)
            .expect("every proposal discriminator deserializes");
    }

    let schema = serde_json::to_string(&schemars::schema_for!(ApplyTerrainBrushArgs)).expect("schema serializes");
    for named_field in [
        "x_octimeters",
        "z_octimeters",
        "max_steps",
        "max_subcells",
        "radius_octimeters",
        "spacing_octimeters",
        "value",
    ] {
        assert!(schema.contains(named_field), "terrain schema contains named field {named_field}");
    }
}

#[test]
fn terrain_vocabulary_source_has_no_positional_domain_stand_ins() {
    let source = include_str!("../../args.rs");
    let start = source.find("pub struct MarkId").expect("terrain vocabulary start exists");
    let end = source.find("pub struct MarkMissingData").expect("terrain vocabulary end exists");
    let terrain = &source[start..end];
    assert!(!terrain.contains("pub struct MarkId("));
    assert!(!terrain.contains("[i32; 2]"));
    assert!(!terrain.contains("[u32; 2]"));
    assert!(!terrain.contains("(MarkId, u32)"));
}

#[test]
fn mark_and_editor_operations_choose_exact_kinds_and_wire_params() {
    let mark_cases = [
        (
            TerrainMarksOperation::Create { geometry: MarkGeometry::Point { point: point(1, 2) }, label: "p".into() },
            "aether.kit.mark.create",
            "aether.kit.mark.create_result",
            false,
        ),
        (
            TerrainMarksOperation::Update {
                id: MarkId { value: 7 },
                geometry: Some(MarkGeometry::Path { points: vec![point(3, 4)] }),
                label: None,
            },
            "aether.kit.mark.update",
            "aether.kit.mark.update_result",
            false,
        ),
        (
            TerrainMarksOperation::Delete { id: MarkId { value: 7 } },
            "aether.kit.mark.delete",
            "aether.kit.mark.delete_result",
            false,
        ),
        (
            TerrainMarksOperation::Get { id: MarkId { value: 7 } },
            "aether.kit.mark.get",
            "aether.kit.mark.get_result",
            false,
        ),
        (TerrainMarksOperation::List, "aether.kit.mark.list", "aether.kit.mark.list_result", true),
    ];
    for (operation, request_kind, reply_kind, fieldless) in mark_cases {
        let call = mark_kind_call(operation);
        assert_eq!(call.request_kind, request_kind);
        assert_eq!(call.reply_kind, reply_kind);
        assert_eq!(call.params.is_none(), fieldless);
    }
    assert_eq!(
        mark_kind_call(TerrainMarksOperation::Get { id: MarkId { value: 7 } }).params,
        Some(serde_json::json!({ "id": { "0": 7 } })),
    );

    let editor_cases = [
        (TerrainEditorOperation::SetSelection { references: vec![reference(1, 2)] }, "set_selection", false),
        (TerrainEditorOperation::ToggleSelection { reference: reference(1, 2) }, "toggle_selection", false),
        (TerrainEditorOperation::ClearSelection, "clear_selection", true),
        (
            TerrainEditorOperation::CreateMark {
                geometry: MarkGeometry::Point { point: point(1, 2) },
                label: "p".into(),
            },
            "create_mark",
            false,
        ),
        (
            TerrainEditorOperation::MoveSelection { delta: WorldDelta { x_octimeters: 1, z_octimeters: 2 } },
            "move_selection",
            false,
        ),
        (TerrainEditorOperation::RelabelSelection { label: "x".into() }, "relabel_selection", false),
        (TerrainEditorOperation::DeleteSelection, "delete_selection", true),
        (TerrainEditorOperation::Query, "query", true),
    ];
    for (operation, suffix, fieldless) in editor_cases {
        let call = editor_kind_call(operation);
        assert_eq!(call.request_kind, format!("aether.kit.terra.{suffix}"));
        assert_eq!(
            call.reply_kind,
            if suffix == "query" {
                "aether.kit.terra.query_result"
            } else {
                "aether.kit.terra.command_result"
            }
        );
        assert_eq!(call.params.is_none(), fieldless);
    }
}

#[test]
fn proposal_operations_convert_to_exact_live_request_variants() {
    let mutations = [
        (
            TerrainProposalOperation::SetChunk {
                chunk_x: 1,
                chunk_z: 2,
                underlay: vec![1],
                underlay_points: vec![2],
                height_points: vec![-3],
                overlay: vec![4],
                overlay_mask: vec![5],
                height: vec![6],
                region: vec![7],
                water_plane: vec![8],
                smoothing: vec![9],
            },
            "SetChunk",
        ),
        (TerrainProposalOperation::SetCellPoints { x: 1, z: 2, points: vec![3] }, "SetCellPoints"),
        (TerrainProposalOperation::SetCellHeights { x: 1, z: 2, deltas: vec![-3] }, "SetCellHeights"),
        (TerrainProposalOperation::StampPolygon { points: vec![point(1, 2)], material: 3 }, "StampPolygon"),
        (TerrainProposalOperation::StampDisc { center: point(1, 2), radius_octimeters: 3, material: 4 }, "StampDisc"),
        (
            TerrainProposalOperation::StampHexagon { center: point(1, 2), radius_octimeters: 3, material: 4 },
            "StampHexagon",
        ),
    ];
    for (operation, variant) in mutations {
        let live = live_mutation_proposal_operation(&operation).expect("mutation converts synchronously");
        assert_eq!(live.as_object().and_then(|object| object.keys().next()).map(String::as_str), Some(variant));
        assert!(live[variant]["request"].is_object());
    }
    assert_eq!(
        live_mutation_proposal_operation(&TerrainProposalOperation::SetChunk {
            chunk_x: 1,
            chunk_z: 2,
            underlay: vec![1],
            underlay_points: vec![2],
            height_points: vec![-3],
            overlay: vec![4],
            overlay_mask: vec![5],
            height: vec![6],
            region: vec![7],
            water_plane: vec![8],
            smoothing: vec![9],
        })
        .expect("SetChunk converts")["SetChunk"]["request"],
        serde_json::json!({
            "chunk_x": 1, "chunk_z": 2, "underlay": [1], "underlay_points": [2],
            "height_points": [-3], "overlay": [4], "overlay_mask": [5], "height": [6],
            "region": [7], "water_plane": [8], "smoothing": [9]
        }),
    );

    let request = serde_json::json!({ "source": { "id": { "0": 1 }, "revision": 1 } });
    assert_eq!(
        live_operator_proposal_operation("ApplyBrush", request.clone()),
        serde_json::json!({ "ApplyBrush": { "request": request } }),
    );
    assert_eq!(
        live_operator_proposal_operation("RunAutomaton", request.clone()),
        serde_json::json!({ "RunAutomaton": { "request": request } }),
    );
}

#[test]
fn proposal_lifecycle_ids_and_preview_null_keep_named_records() {
    let proposal_id = ProposalId { value: 42 };
    assert_eq!(live_proposal_id_params(proposal_id), serde_json::json!({ "proposal_id": { "value": 42 } }));
    assert_eq!(live_proposal_preview_params(Some(proposal_id)), serde_json::json!({ "proposal_id": { "value": 42 } }),);
    assert_eq!(live_proposal_preview_params(None), serde_json::json!({ "proposal_id": null }));
}

#[test]
fn recursive_mark_id_normalization_covers_results_selections_and_errors() {
    let result = serde_json::json!({
        "PartiallyApplied": {
            "selection": [{ "id": { "0": 1 }, "revision": 2 }],
            "changed": [{ "id": { "0": 3 }, "revision": 4 }],
            "deleted": [],
            "error": { "RevisionRace": {
                "expected": { "id": { "0": 5 }, "revision": 6 },
                "observed": { "id": { "0": 5 }, "revision": 7 }
            } }
        }
    });
    let normalized = normalize_mark_ids(result);
    assert_eq!(normalized["PartiallyApplied"]["selection"][0]["id"], serde_json::json!({ "value": 1 }));
    assert_eq!(normalized["PartiallyApplied"]["changed"][0]["id"], serde_json::json!({ "value": 3 }));
    assert_eq!(
        normalized["PartiallyApplied"]["error"]["RevisionRace"]["observed"]["id"],
        serde_json::json!({ "value": 5 }),
    );
}

#[test]
fn point_to_cell_matches_negative_safe_kit_boundaries() {
    for (octimeters, expected) in [(-1, -1), (-256, -1), (-257, -2), (0, 0), (255, 0), (256, 1)] {
        let cell = point_to_operator_cell(point(octimeters, octimeters));
        assert_eq!((cell.cell_x, cell.cell_z), (expected, expected));
    }
}

#[test]
fn source_mark_geometry_is_strict_for_both_operator_families() {
    let source = reference(1, 1);
    let path_mark = ResolvedMark { reference: source, geometry: MarkGeometry::Path { points: vec![point(1, 2)] } };
    let point_mark = ResolvedMark { reference: source, geometry: MarkGeometry::Point { point: point(-1, -257) } };
    let area_mark = ResolvedMark {
        reference: source,
        geometry: MarkGeometry::Area { points: vec![point(1, 2), point(3, 4), point(5, 6)] },
    };
    assert_eq!(brush_path(BrushGeometry::SourceMark, &path_mark).expect("Path is a brush path"), vec![point(1, 2)]);
    for (mark, actual) in [(&point_mark, "point"), (&area_mark, "area")] {
        let error = brush_path(BrushGeometry::SourceMark, mark).expect_err("non-Path brush source is rejected");
        assert_eq!(error.data.as_ref().and_then(|data| data["expected"].as_str()), Some("path"));
        assert_eq!(error.data.as_ref().and_then(|data| data["actual"].as_str()), Some(actual));
    }
    assert_eq!(
        automaton_seed(&AutomatonGeometry::SourceMark, &point_mark).expect("Point is an automaton seed"),
        serde_json::json!({ "cell_x": -1, "cell_z": -2 }),
    );
    for (mark, actual) in [(&path_mark, "path"), (&area_mark, "area")] {
        let error =
            automaton_seed(&AutomatonGeometry::SourceMark, mark).expect_err("non-Point automaton source is rejected");
        assert_eq!(error.data.as_ref().and_then(|data| data["expected"].as_str()), Some("point"));
        assert_eq!(error.data.as_ref().and_then(|data| data["actual"].as_str()), Some(actual));
    }
}

#[tokio::test]
async fn dynamic_relay_refreshes_routes_settles_decodes_and_normalizes() {
    let request =
        descriptor("aether.kit.test.request", record(vec![field("value", SchemaType::Scalar(Primitive::U32))]));
    let response = descriptor(
        "aether.kit.test.result",
        record(vec![field("status", SchemaType::String), field("id", mark_id_schema())]),
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let replies = Arc::new(Mutex::new(VecDeque::from([TerrainRouteReply {
        events: vec![encoded_event(&response, serde_json::json!({ "status": "ok", "id": { "0": 7 } }))],
        settle: true,
    }])));
    let Ok((_chassis, port)) = try_boot_hub_with_terrain_route_loopback(
        inventory(&[request.clone(), response.clone()]),
        Arc::clone(&calls),
        replies,
    ) else {
        return;
    };
    let mcp = connect_mcp(port);
    let engine_id = Uuid::from_u128(0x2933).to_string();
    let mailbox = "aether.component/aether.embedded:terrain";

    let result = call_terrain_kind(
        &mcp,
        &engine_id,
        mailbox,
        &request.name,
        Some(serde_json::json!({ "value": 9 })),
        &response.name,
    )
    .await
    .expect("settled dynamic terrain relay succeeds");
    assert_eq!(result, serde_json::json!({ "status": "ok", "id": { "value": 7 } }));

    let calls = calls.lock().expect("calls mutex");
    assert_eq!(calls.len(), 2, "one inventory refresh precedes one task request");
    assert_eq!(calls[0].kind, ListKinds::ID);
    assert_eq!(calls[1].mailbox, mailbox_id_from_path(mailbox));
    assert_eq!(calls[1].kind, KindId(kind_id_from_parts(&request.name, &request.schema)));
    assert_eq!(
        aether_codec::decode_schema(&calls[1].payload, &request.schema).expect("request decodes"),
        serde_json::json!({ "value": 9 }),
    );
    drop(calls);
}

fn brush_args(engine_id: &str, geometry: BrushGeometry) -> ApplyTerrainBrushArgs {
    ApplyTerrainBrushArgs {
        engine_id: engine_id.to_owned(),
        world_mailbox: "aether.component/aether.embedded:world".into(),
        mark_book_mailbox: "aether.component/aether.embedded:marks".into(),
        source: reference(1, 1),
        geometry,
        brush: BrushParameters { radius_octimeters: 64, spacing_octimeters: 32, material: 3 },
        budget: OperatorBudget { max_steps: 8, max_subcells: 4096 },
    }
}

#[tokio::test]
async fn operator_source_preflight_rejects_without_world_dispatch() {
    let [mark_get, mark_get_result] = mark_get_descriptors();
    let brush = brush_request_descriptor();
    let operator_result = operator_result_descriptor();
    let replies = Arc::new(Mutex::new(VecDeque::from([
        TerrainRouteReply {
            events: vec![encoded_event(&mark_get_result, serde_json::json!({ "mark": null }))],
            settle: true,
        },
        mark_result(&mark_get_result, 1, 2, serde_json::json!({ "Point": point(0, 0) })),
        mark_result(&mark_get_result, 1, 1, serde_json::json!({ "Point": point(0, 0) })),
        mark_result(&mark_get_result, 2, 1, serde_json::json!({ "Path": [point(0, 0)] })),
    ])));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let Ok((_chassis, port)) = try_boot_hub_with_terrain_route_loopback(
        inventory(&[mark_get, mark_get_result, brush, operator_result]),
        Arc::clone(&calls),
        replies,
    ) else {
        return;
    };
    let mcp = connect_mcp(port);
    let engine_id = Uuid::from_u128(0x0002_9331).to_string();

    let missing =
        apply_terrain_brush(&mcp, brush_args(&engine_id, BrushGeometry::Explicit { path: vec![point(1, 2)] }))
            .await
            .expect_err("explicit geometry still preflights a missing source");
    assert_eq!(missing.data.as_ref().and_then(|data| data["code"].as_str()), Some("mark_missing"));

    let stale = apply_terrain_brush(&mcp, brush_args(&engine_id, BrushGeometry::SourceMark))
        .await
        .expect_err("newer revision is stale");
    assert_eq!(stale.data.as_ref().and_then(|data| data["code"].as_str()), Some("stale_mark_reference"));
    assert_eq!(stale.data.as_ref().map(|data| &data["current"]), Some(&serde_json::json!(reference(1, 2))));

    let wrong = apply_terrain_brush(&mcp, brush_args(&engine_id, BrushGeometry::SourceMark))
        .await
        .expect_err("Point cannot become a brush path");
    assert_eq!(wrong.data.as_ref().and_then(|data| data["code"].as_str()), Some("wrong_mark_geometry"));
    assert_eq!(wrong.data.as_ref().and_then(|data| data["actual"].as_str()), Some("point"));

    let protocol = apply_terrain_brush(&mcp, brush_args(&engine_id, BrushGeometry::SourceMark))
        .await
        .expect_err("returned id mismatch is a protocol error");
    assert!(protocol.data.is_none());
    assert!(protocol.message.contains("protocol error"));

    let calls = calls.lock().expect("calls mutex");
    assert_eq!(calls.len(), 5, "one inventory refresh plus four MarkGet calls and no world call");
    assert!(
        calls
            .iter()
            .skip(1)
            .all(|call| call.kind
                == KindId(kind_id_from_parts("aether.kit.mark.get", &mark_get_descriptors()[0].schema,)))
    );
    drop(calls);
}

#[tokio::test]
async fn brush_source_mark_builds_exact_payload_and_projects_operator_result() {
    let [mark_get, mark_get_result] = mark_get_descriptors();
    let brush = brush_request_descriptor();
    let operator_result = operator_result_descriptor();
    let replies = Arc::new(Mutex::new(VecDeque::from([
        mark_result(&mark_get_result, 1, 1, serde_json::json!({ "Path": [point(-1, -257), point(256, 512)] })),
        TerrainRouteReply {
            events: vec![encoded_event(
                &operator_result,
                serde_json::json!({
                    "Applied": {
                        "source": { "id": { "0": 1 }, "revision": 1 },
                        "stats": { "steps_run": 2, "subcells_written": 32, "touched_chunks": [{ "chunk_x": -1, "chunk_z": 0 }] }
                    }
                }),
            )],
            settle: true,
        },
    ])));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let Ok((_chassis, port)) = try_boot_hub_with_terrain_route_loopback(
        inventory(&[mark_get, mark_get_result, brush.clone(), operator_result]),
        Arc::clone(&calls),
        replies,
    ) else {
        return;
    };
    let mcp = connect_mcp(port);
    let engine_id = Uuid::from_u128(0x0002_9332).to_string();
    let output = apply_terrain_brush(&mcp, brush_args(&engine_id, BrushGeometry::SourceMark))
        .await
        .expect("exact source revision applies");
    let output: serde_json::Value = serde_json::from_str(&output).expect("operator result is JSON");
    assert_eq!(output["Applied"]["source"]["id"], serde_json::json!({ "value": 1 }));
    assert_eq!(output["Applied"]["stats"]["steps_run"], 2);

    let calls = calls.lock().expect("calls mutex");
    assert_eq!(calls.len(), 3, "inventory, MarkGet, then world brush");
    assert_eq!(calls[2].mailbox, mailbox_id_from_path("aether.component/aether.embedded:world"));
    assert_eq!(
        aether_codec::decode_schema(&calls[2].payload, &brush.schema).expect("brush payload decodes"),
        serde_json::json!({
            "source": { "id": { "0": 1 }, "revision": 1 },
            "path": [point(-1, -257), point(256, 512)],
            "brush": { "radius_octimeters": 64, "spacing_octimeters": 32, "material": 3 },
            "budget": { "max_steps": 8, "max_subcells": 4096 }
        }),
    );
    drop(calls);
}

#[tokio::test]
async fn automaton_source_mark_builds_exact_payload_and_projects_failed_result() {
    let [mark_get, mark_get_result] = mark_get_descriptors();
    let automaton = automaton_request_descriptor();
    let operator_result = operator_result_descriptor();
    let replies = Arc::new(Mutex::new(VecDeque::from([
        mark_result(&mark_get_result, 1, 1, serde_json::json!({ "Point": point(-257, -1) })),
        TerrainRouteReply {
            events: vec![encoded_event(
                &operator_result,
                serde_json::json!({
                    "Failed": {
                        "source": { "id": { "0": 1 }, "revision": 1 },
                        "error": "StepBudgetExhausted",
                        "stats": { "steps_run": 1, "subcells_written": 256, "touched_chunks": [{ "chunk_x": -1, "chunk_z": -1 }] }
                    }
                }),
            )],
            settle: true,
        },
    ])));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let Ok((_chassis, port)) = try_boot_hub_with_terrain_route_loopback(
        inventory(&[mark_get, mark_get_result, automaton.clone(), operator_result]),
        Arc::clone(&calls),
        replies,
    ) else {
        return;
    };
    let mcp = connect_mcp(port);
    let engine_id = Uuid::from_u128(0x0002_9333).to_string();
    let output = run_terrain_automaton(
        &mcp,
        RunTerrainAutomatonArgs {
            engine_id,
            world_mailbox: "aether.component/aether.embedded:world".into(),
            mark_book_mailbox: "aether.component/aether.embedded:marks".into(),
            source: reference(1, 1),
            geometry: AutomatonGeometry::SourceMark,
            rule: AutomatonRule::Grow { material: 4, generations: 3 },
            budget: OperatorBudget { max_steps: 1, max_subcells: 256 },
        },
    )
    .await
    .expect("domain Failed remains successful tool output");
    let output: serde_json::Value = serde_json::from_str(&output).expect("operator result is JSON");
    assert_eq!(output["Failed"]["error"], "StepBudgetExhausted");
    assert_eq!(output["Failed"]["source"]["id"], serde_json::json!({ "value": 1 }));

    let calls = calls.lock().expect("calls mutex");
    assert_eq!(calls.len(), 3, "inventory, MarkGet, then world automaton");
    assert_eq!(
        aether_codec::decode_schema(&calls[2].payload, &automaton.schema).expect("automaton payload decodes"),
        serde_json::json!({
            "source": { "id": { "0": 1 }, "revision": 1 },
            "seed": { "cell_x": -2, "cell_z": -1 },
            "rule": { "Grow": { "material": 4, "generations": 3 } },
            "budget": { "max_steps": 1, "max_subcells": 256 }
        }),
    );
    drop(calls);
}

#[test]
fn terrain_reply_requires_one_expected_decodable_event_and_no_timeout() {
    let expected = descriptor("aether.kit.test.result", record(vec![field("status", SchemaType::String)]));
    let expected_id = KindId(kind_id_from_parts(&expected.name, &expected.schema));
    let payload = aether_codec::encode_schema(&serde_json::json!({ "status": "ok" }), &expected.schema)
        .expect("response encodes");
    let event = reply_envelope(expected_id, payload.clone());
    let kinds = HashMap::from([(expected.name.clone(), expected.clone())]);

    assert!(decode_terrain_reply(from_ref(&event), true, &expected, &kinds).is_err());
    assert!(decode_terrain_reply(&[], false, &expected, &kinds).is_err());
    assert!(decode_terrain_reply(&[event.clone(), event], false, &expected, &kinds).is_err());
    assert!(decode_terrain_reply(&[reply_envelope(KindId(99), payload)], false, &expected, &kinds).is_err());
    assert!(decode_terrain_reply(&[reply_envelope(expected_id, vec![0xff])], false, &expected, &kinds).is_err());
}

#[test]
fn proposal_results_preserve_full_lifecycle_and_capacity_rejection() {
    let descriptor = proposal_result_descriptor();
    let id = KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema));
    let kinds = HashMap::from([(descriptor.name.clone(), descriptor.clone())]);
    let decode = |value: serde_json::Value| {
        let payload = aether_codec::encode_schema(&value, &descriptor.schema).expect("proposal result encodes");
        decode_terrain_reply(&[reply_envelope(id, payload)], false, &descriptor, &kinds)
            .expect("proposal result is ordinary domain output")
    };

    let capacity = decode(serde_json::json!({
        "Rejected": { "error": "StagedProposalLimitReached" }
    }));
    assert_eq!(capacity, serde_json::json!({ "Rejected": { "error": "StagedProposalLimitReached" } }));

    for error in [
        serde_json::json!("ProposalIdExhausted"),
        serde_json::json!({ "UnknownProposal": { "proposal_id": { "value": 99 } } }),
        serde_json::json!({ "StaleProposal": { "proposal_id": { "value": 2 }, "proposed_at_revision": 1, "committed_revision": 2 } }),
        serde_json::json!({ "NoTouchedChunks": { "operation_result": "Mutation" } }),
    ] {
        let projected = decode(serde_json::json!({ "Rejected": { "error": error.clone() } }));
        assert_eq!(projected["Rejected"]["error"], error);
    }

    let discarded = decode(serde_json::json!({ "Discarded": { "proposal_id": { "value": 1 } } }));
    assert_eq!(discarded["Discarded"]["proposal_id"]["value"], 1);
    let staged = decode(serde_json::json!({
        "Staged": {
            "proposal_id": { "value": 65 },
            "operation_result": "Mutation",
            "digest": { "touched_chunks": [{ "chunk_x": 0, "chunk_z": 0 }], "triangle_count": 1, "changed_geometry_bounds": null }
        }
    }));
    assert_eq!(staged["Staged"]["proposal_id"]["value"], 65, "capacity rejection consumed no id");

    let committed = decode(serde_json::json!({
        "Committed": {
            "proposal_id": { "value": 65 },
            "digest": { "touched_chunks": [], "triangle_count": 0, "changed_geometry_bounds": null }
        }
    }));
    assert_eq!(committed["Committed"]["proposal_id"]["value"], 65);
    let preview_clear = decode(serde_json::json!({
        "PreviewSet": { "active_proposal_id": null, "digest": null }
    }));
    assert!(preview_clear["PreviewSet"]["active_proposal_id"].is_null());
}

#[test]
fn terrain_router_discovers_exact_eight_tools_with_loaded_mailbox_guidance() {
    let expected = [
        "apply_terrain_brush",
        "commit_terrain_proposal",
        "discard_terrain_proposal",
        "propose_terrain_edit",
        "run_terrain_automaton",
        "set_terrain_proposal_preview",
        "terrain_editor",
        "terrain_marks",
    ];
    let mut terrain_tools: Vec<_> =
        Mcp::tool_router().list_all().into_iter().filter(|tool| expected.contains(&tool.name.as_ref())).collect();
    terrain_tools.sort_by(|left, right| left.name.cmp(&right.name));
    assert_eq!(terrain_tools.iter().map(|tool| tool.name.as_ref()).collect::<Vec<_>>(), expected);
    for tool in terrain_tools {
        let description = tool.description.as_deref().expect("terrain tool has a description");
        assert!(description.contains("LoadResult.name"), "{} names the loaded mailbox contract", tool.name);
        assert!(description.contains("load_component"), "{} points to discovery", tool.name);
        let schema = serde_json::to_string(&tool.input_schema).expect("input schema serializes");
        assert!(schema.contains("engine_id"));
        assert!(schema.contains("mailbox"));
    }
}

#[test]
fn complete_terrain_result_uses_generic_forced_spill_guard() {
    let dir = std_env::temp_dir().join(format!("aether-terrain-spill-{}", process::id()));
    std_fs::create_dir_all(&dir).expect("create spill scratch");
    let body = serde_json::to_string(&serde_json::json!({
        "Staged": {
            "proposal_id": { "value": 65 },
            "operation_result": "Mutation",
            "digest": { "touched_chunks": [{ "chunk_x": 0, "chunk_z": 0 }], "triangle_count": 99, "changed_geometry_bounds": null }
        }
    }))
    .expect("terrain result serializes");
    let rendered = spill_oversized_response_in("propose_terrain_edit", body.clone(), 1, &dir);
    let spill: serde_json::Value = serde_json::from_str(&rendered).expect("spill reference decodes");
    let file = spill["file"].as_str().expect("spill file is named");
    assert_eq!(std_fs::read_to_string(file).expect("spill exists"), body);
    std_fs::remove_file(file).ok();
    std_fs::remove_dir_all(&dir).ok();
}
