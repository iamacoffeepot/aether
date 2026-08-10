use std::collections::{BTreeMap, HashMap, VecDeque};

use super::super::contracts::{
    ConfigContract, ContractIdentity, ContractSnapshot, HandlerContract, ReplyContractSnapshot, descriptor,
    diff_contracts,
};
use super::super::test_support::{
    TerrainReplyEvent, TerrainRouteReply, boot_hub, boot_hub_with_route_loopback, connect_mcp,
    try_boot_hub_with_terrain_route_loopback,
};
use super::super::*;
use crate::args::{CompareComponentContractsArgs, ComponentContractSubject};
use aether_data::{EngineId, KindId, Uuid, canonical::kind_id_from_parts, mailbox_id_from_path, tagged_id, wire};
use aether_kinds::{ComponentCapabilities, DescribeComponent, DescribeComponentResult, KindDescriptorWire};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

fn identity(engine_id: &str, lineage: &str) -> ContractIdentity {
    ContractIdentity {
        engine_id: engine_id.to_owned(),
        canonical_lineage: lineage.to_owned(),
        mailbox_id: "mbx-0000000000000000".to_owned(),
    }
}

fn handler(schema: SchemaType) -> HandlerContract {
    HandlerContract {
        input_schema: schema,
        reply: ReplyContractSnapshot { class: "none".to_owned(), id: None, name: None, schema: None },
    }
}

fn snapshot() -> ContractSnapshot {
    ContractSnapshot {
        identity: identity("00000000-0000-0000-0000-000000000001", "aether.component/aether.embedded:baseline"),
        handlers: BTreeMap::from([("aether.test.keep".to_owned(), handler(SchemaType::String))]),
        config: None,
        fallback: false,
    }
}

#[test]
fn equal_contracts_are_compatible_and_serialization_is_stable() {
    let baseline = snapshot();
    let candidate = ContractSnapshot {
        identity: identity("00000000-0000-0000-0000-000000000002", "aether.component/aether.embedded:candidate"),
        ..baseline.clone()
    };
    let first = serde_json::to_vec(&diff_contracts(baseline.clone(), candidate.clone())).expect("response serializes");
    let second = serde_json::to_vec(&diff_contracts(baseline, candidate)).expect("response serializes");
    assert_eq!(first, second, "normalized BTreeMap diff serializes byte-stably");
    let response: serde_json::Value = serde_json::from_slice(&first).expect("response is JSON");
    assert_eq!(response["compatible"], true);
    assert!(response["additions"].as_array().is_some_and(Vec::is_empty));
    assert!(response["removals"].as_array().is_some_and(Vec::is_empty));
    assert!(response["changes"].as_array().is_some_and(Vec::is_empty));
}

#[test]
fn additions_are_additive_but_all_declared_breaking_categories_are_incompatible() {
    let mut baseline = snapshot();
    baseline.handlers.insert(
        "aether.test.reply".to_owned(),
        HandlerContract {
            input_schema: SchemaType::Bool,
            reply: ReplyContractSnapshot {
                class: "one".to_owned(),
                id: Some("knd-old".to_owned()),
                name: Some("aether.test.reply_old".to_owned()),
                schema: Some(SchemaType::String),
            },
        },
    );
    baseline.handlers.insert("aether.test.removed".to_owned(), handler(SchemaType::Unit));
    baseline.config = Some(ConfigContract {
        id: "knd-config".to_owned(),
        name: "aether.test.config".to_owned(),
        schema: SchemaType::String,
    });
    baseline.fallback = true;

    let mut candidate = snapshot();
    candidate.identity = identity("00000000-0000-0000-0000-000000000002", "aether.component/aether.embedded:candidate");
    candidate.handlers.insert("aether.test.keep".to_owned(), handler(SchemaType::Bool));
    candidate.handlers.insert(
        "aether.test.reply".to_owned(),
        HandlerContract {
            input_schema: SchemaType::Bool,
            reply: ReplyContractSnapshot {
                class: "multi".to_owned(),
                id: Some("knd-new".to_owned()),
                name: Some("aether.test.reply_new".to_owned()),
                schema: Some(SchemaType::Bytes),
            },
        },
    );
    candidate.handlers.insert("aether.test.added".to_owned(), handler(SchemaType::Unit));
    candidate.config = Some(ConfigContract {
        id: "knd-config-new".to_owned(),
        name: "aether.test.config".to_owned(),
        schema: SchemaType::Bytes,
    });

    let response = diff_contracts(baseline, candidate);
    assert!(!response.compatible);
    assert_eq!(response.baseline.engine_id, "00000000-0000-0000-0000-000000000001");
    assert_eq!(response.candidate.engine_id, "00000000-0000-0000-0000-000000000002");
    assert!(response.additions.iter().any(|change| change.name == "aether.test.added"));
    assert!(response.removals.iter().any(|change| change.name == "aether.test.removed"));
    assert!(response.removals.iter().any(|change| change.category == "fallback"));
    assert!(response.changes.iter().any(|change| change.category == "handler_input_schema"));
    assert!(response.changes.iter().any(|change| change.category == "handler_reply"));
    assert!(response.changes.iter().any(|change| change.category == "config"));
    assert!(response.changes.iter().all(|change| change.before.is_some() && change.after.is_some()));
}

#[test]
fn config_addition_and_fallback_gain_respect_their_opposite_boundaries() {
    let baseline = snapshot();
    let mut candidate = snapshot();
    candidate.config = Some(ConfigContract {
        id: "knd-config".to_owned(),
        name: "aether.test.config".to_owned(),
        schema: SchemaType::String,
    });
    candidate.fallback = true;
    let response = diff_contracts(baseline, candidate);
    assert!(!response.compatible, "Config addition changes the boot contract");
    assert!(response.additions.iter().any(|change| change.category == "config"));
    assert!(response.additions.iter().any(|change| change.category == "fallback"));
}

#[test]
fn tool_router_registers_two_explicit_contract_subjects() {
    let tool = Mcp::tool_router()
        .list_all()
        .into_iter()
        .find(|tool| tool.name.as_ref() == "compare_component_contracts")
        .expect("compare_component_contracts is registered");
    let schema = serde_json::to_value(tool.input_schema).expect("tool schema serializes");
    assert!(schema["required"].as_array().is_some_and(|required| {
        ["baseline", "candidate"].iter().all(|name| required.iter().any(|value| value == name))
    }));
    assert_eq!(schema["properties"]["baseline"]["$ref"], "#/$defs/ComponentContractSubject");
    assert_eq!(schema["$defs"]["ComponentContractSubject"]["required"], serde_json::json!(["engine_id", "component"]));
}

#[tokio::test]
async fn strict_refresh_failure_returns_an_error_before_any_contract_verdict() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let engine = EngineId(Uuid::from_u128(0x4755));
    assert!(
        mcp.refresh_engine_kinds_strict(engine).await.is_err(),
        "an unreachable routed engine is inconclusive, never a stale successful refresh"
    );
}

fn live_inventory(name: &str, schema: &SchemaType) -> ListKindsResult {
    ListKindsResult {
        kinds: vec![KindDescriptorWire {
            id: KindId(kind_id_from_parts(name, schema)),
            name: name.to_owned(),
            schema_wire: wire::to_vec(schema).expect("schema wire-encodes"),
        }],
    }
}

#[test]
fn capability_descriptor_lookup_rejects_handler_and_config_id_mismatches() {
    let name = "aether.test.subject";
    let schema = SchemaType::String;
    let kinds = HashMap::from([(name.to_owned(), KindDescriptor { name: name.to_owned(), schema: schema.clone() })]);
    let expected = KindId(kind_id_from_parts(name, &schema));
    assert!(descriptor(&kinds, name, expected, "handler input").is_ok());
    for role in ["handler input", "Config"] {
        let error = descriptor(&kinds, name, KindId(expected.0 ^ 1), role)
            .expect_err("same-name descriptor must still match the capability's exact id");
        assert!(error.to_string().contains("not advertised"), "{role}: {error}");
    }
}

#[tokio::test]
async fn strict_refresh_rejects_a_wire_id_that_disagrees_with_its_name_and_schema() {
    let name = "aether.test.inconsistent";
    let mut inventory = live_inventory(name, &SchemaType::String);
    inventory.kinds[0].id = KindId(0x4755);
    let (_chassis, port) = boot_hub_with_route_loopback(inventory, Arc::new(AtomicUsize::new(0)));
    let mcp = connect_mcp(port);
    let error = mcp
        .refresh_engine_kinds_strict(EngineId(Uuid::from_u128(0x0047_5501)))
        .await
        .expect_err("an inconsistent live descriptor is inconclusive");
    assert!(error.to_string().contains("canonically identify"));
}

#[tokio::test]
async fn router_dispatches_a_fresh_compatible_comparison_with_explicit_subject_identities() {
    let baseline = "aether.component/aether.embedded:baseline";
    let candidate = "aether.component/aether.embedded:candidate";
    let first_engine = EngineId(Uuid::from_u128(0x0047_5502));
    let second_engine = EngineId(Uuid::from_u128(0x0047_5503));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let replies = Arc::new(Mutex::new(VecDeque::from([
        TerrainRouteReply {
            events: vec![TerrainReplyEvent {
                kind: DescribeComponentResult::ID,
                payload: DescribeComponentResult::Ok { capabilities: ComponentCapabilities::default() }
                    .encode_into_bytes(),
            }],
            settle: true,
        },
        TerrainRouteReply {
            events: vec![TerrainReplyEvent {
                kind: DescribeComponentResult::ID,
                payload: DescribeComponentResult::Ok { capabilities: ComponentCapabilities::default() }
                    .encode_into_bytes(),
            }],
            settle: true,
        },
    ])));
    let inventory = ListKindsResult { kinds: Vec::new() };
    let Ok((_chassis, port)) = try_boot_hub_with_terrain_route_loopback(inventory, Arc::clone(&calls), replies) else {
        return;
    };
    let output = connect_mcp(port)
        .compare_component_contracts(Parameters(CompareComponentContractsArgs {
            baseline: ComponentContractSubject {
                engine_id: first_engine.0.to_string(),
                component: baseline.to_owned(),
            },
            candidate: ComponentContractSubject {
                engine_id: second_engine.0.to_string(),
                component: candidate.to_owned(),
            },
        }))
        .await
        .expect("both routed live subjects compare");
    let result: serde_json::Value = serde_json::from_str(&output).expect("comparison JSON");
    assert_eq!(result["compatible"], true);
    assert_eq!(result["baseline"]["engine_id"], first_engine.0.to_string());
    assert_eq!(result["candidate"]["engine_id"], second_engine.0.to_string());
    assert_eq!(result["baseline"]["canonical_lineage"], baseline);
    assert_eq!(result["candidate"]["canonical_lineage"], candidate);
    assert_eq!(
        result["baseline"]["mailbox_id"],
        tagged_id::encode(mailbox_id_from_path(baseline).0).expect("fixture mailbox id is taggable")
    );
    let calls = calls.lock().expect("calls mutex is sound");
    assert_eq!(calls.len(), 4, "each subject performs live describe plus strict inventory refresh");
    assert_eq!(calls.iter().filter(|call| call.kind == DescribeComponent::ID).count(), 2);
    assert_eq!(calls.iter().filter(|call| call.kind == ListKinds::ID).count(), 2);
    drop(calls);
}
