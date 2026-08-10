use std::collections::BTreeMap;

use super::super::contracts::{
    ConfigContract, ContractIdentity, ContractSnapshot, HandlerContract, ReplyContractSnapshot, diff_contracts,
};
use super::super::test_support::{boot_hub, connect_mcp};
use super::super::*;
use aether_data::{EngineId, Uuid};

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
