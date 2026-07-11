#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;

/// Small typed-config-shaped schema for component config encoding tests.
fn config_struct_schema() -> SchemaType {
    use aether_data::NamedField;
    SchemaType::Struct {
        fields: vec![
            NamedField { name: "seed".into(), ty: SchemaType::Scalar(Primitive::U32) },
            NamedField { name: "label".into(), ty: SchemaType::String },
        ]
        .into(),
        repr_c: false,
    }
}

fn config_kind(schema: &SchemaType) -> KindDescriptorWire {
    KindDescriptorWire {
        id: KindId(kind_id_from_parts("test.config", schema)),
        name: "test.config".to_owned(),
        schema_wire: wire::to_vec(schema).expect("SchemaType wire-encodes"),
    }
}

#[tokio::test]
async fn component_config_inline_json_encodes_to_schema_bytes() {
    let schema = config_struct_schema();
    let kind = config_kind(&schema);
    let bytes =
        component_config_bytes(Some(&kind), Some(serde_json::json!({"seed": 7, "label": "demo"})), None, "test")
            .await
            .expect("config encodes")
            .expect("source present");
    let decoded = aether_codec::decode_schema(&bytes, &schema).expect("config decodes");
    assert_eq!(decoded, serde_json::json!({"seed": 7, "label": "demo"}));
}

#[tokio::test]
async fn component_config_path_is_json_and_encodes() {
    let path = stage_blob_file("config-json", br#"{"seed":9,"label":"from-file"}"#);
    let schema = config_struct_schema();
    let kind = config_kind(&schema);
    let bytes = component_config_bytes(Some(&kind), None, Some(path.to_str().expect("utf-8 temp path")), "test")
        .await
        .expect("config_path JSON encodes")
        .expect("source present");
    let decoded = aether_codec::decode_schema(&bytes, &schema).expect("config decodes");
    assert_eq!(decoded, serde_json::json!({"seed": 9, "label": "from-file"}));
    std_fs::remove_file(&path).ok();
}

#[tokio::test]
async fn component_config_rejects_both_sources() {
    let schema = config_struct_schema();
    let kind = config_kind(&schema);
    let err = component_config_bytes(
        Some(&kind),
        Some(serde_json::json!({"seed": 7, "label": "demo"})),
        Some("/tmp/ignored.json"),
        "test",
    )
    .await
    .expect_err("both config sources must be rejected");
    assert!(err.to_string().contains("set only one"), "unexpected error: {err}");
}

#[tokio::test]
async fn component_config_rejects_no_config_component() {
    let err = component_config_bytes(None, Some(serde_json::json!({"seed": 7, "label": "demo"})), None, "test")
        .await
        .expect_err("config for no-config component must be rejected");
    assert!(err.to_string().contains("declares no Config kind"), "unexpected error: {err}");
}

#[tokio::test]
async fn component_config_field_mismatch_is_invalid_params() {
    let schema = config_struct_schema();
    let kind = config_kind(&schema);
    let err = component_config_bytes(
        Some(&kind),
        Some(serde_json::json!({"seed": 7, "label": "demo", "extra": true})),
        None,
        "test",
    )
    .await
    .expect_err("field mismatch must be rejected");
    assert!(err.to_string().contains("does not match"), "unexpected error: {err}");
}

/// `components_all_loaded` checks membership, not count. The wrong-set
/// false positive: `actual` has one name (satisfying a count-`>= 1` check)
/// but it is NOT the name in `want` — membership returns false. This is the
/// regression the count-based `wait_for_loaded_components` would silently
/// pass: a non-requested trampoline (B) registers while the requested
/// component (A) stalls, and the count hits the threshold before A is up.
/// After the identity-based fix, only A's presence in `actual` satisfies
/// the check.
#[test]
fn components_all_loaded_wrong_set_is_not_ready() {
    let want = vec!["aether.component/aether.embedded:wanted".to_owned()];
    let actual = vec!["aether.component/aether.embedded:other".to_owned()];
    assert!(
        !components_all_loaded(&want, &actual),
        "a non-requested trampoline present while the requested one is absent \
         must not satisfy the identity check (count-based would pass)",
    );
}

/// `components_all_loaded` returns true once every wanted name is present,
/// and handles the empty-want case (no components requested → trivially
/// ready).
#[test]
fn components_all_loaded_exact_match_is_ready() {
    let want =
        vec!["aether.component/aether.embedded:alpha".to_owned(), "aether.component/aether.embedded:beta".to_owned()];
    let actual = vec![
        "aether.component/aether.embedded:baseline".to_owned(),
        "aether.component/aether.embedded:alpha".to_owned(),
        "aether.component/aether.embedded:beta".to_owned(),
    ];
    assert!(
        components_all_loaded(&want, &actual),
        "both wanted names present (alongside an extra baseline) should be ready",
    );
    assert!(components_all_loaded(&[], &[]), "empty want is trivially ready");
}

/// `components_all_loaded` is false when only a subset of the wanted names
/// is present — a stalled-requested case where one component comes up but
/// another does not.
#[test]
fn components_all_loaded_partial_match_is_not_ready() {
    let want = vec![
        "aether.component/aether.embedded:alpha".to_owned(),
        "aether.component/aether.embedded:stalled".to_owned(),
    ];
    let actual = vec!["aether.component/aether.embedded:alpha".to_owned()];
    assert!(
        !components_all_loaded(&want, &actual),
        "only one of two wanted names present means the engine is not yet ready",
    );
}

/// `replica_base_name` follows the same precedence the component host
/// applies at load: caller `name` wins over `export`, which wins over
/// the entry actor namespace — the bug this catches is a fan-out base
/// name that disagrees with what an unreplicated load would resolve to.
#[test]
fn replica_base_name_follows_name_export_namespace_precedence() {
    assert_eq!(replica_base_name(Some("caller"), Some("export-ns"), Some("entry-ns")), Some("caller".to_owned()),);
    assert_eq!(replica_base_name(None, Some("export-ns"), Some("entry-ns")), Some("export-ns".to_owned()),);
    assert_eq!(replica_base_name(None, None, Some("entry-ns")), Some("entry-ns".to_owned()),);
    assert_eq!(replica_base_name(None, None, None), None);
}

/// `replica_names` suffixes every instance — no bare-name special case
/// for index 0 — so `replicas: 1` differs from an omitted field only by
/// the `-0` suffix.
#[test]
fn replica_names_suffixes_every_instance() {
    assert_eq!(replica_names("handler", 3), vec!["handler-0", "handler-1", "handler-2"],);
    assert_eq!(replica_names("handler", 1), vec!["handler-0"]);
}

/// `reject_replicas_out_of_range` rejects 0 and values above [`MAX_REPLICAS`]
/// (ADR-0090 §4 + review bounds-cap); in-range and omitted stay ok.
#[test]
fn reject_replicas_out_of_range_enforces_bounds() {
    assert!(reject_replicas_out_of_range(Some(0), "sel").is_err());
    assert!(reject_replicas_out_of_range(Some(1), "sel").is_ok());
    assert!(reject_replicas_out_of_range(Some(MAX_REPLICAS), "sel").is_ok());
    assert!(reject_replicas_out_of_range(Some(MAX_REPLICAS + 1), "sel").is_err());
    assert!(reject_replicas_out_of_range(None, "sel").is_ok());
    assert!(reject_zero_replicas(Some(MAX_REPLICAS + 1), "sel").is_ok());
}

/// `load_component` with a selector that resolves to no stored
/// component is a tool error: the hub-local `ResolveComponent` misses
/// on the empty store (ADR-0116).
#[tokio::test]
async fn load_component_unresolvable_selector_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .load_component(Parameters(LoadComponentArgs {
            engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            selector: "no-such-component".to_owned(),
            name: None,
            config: None,
            config_path: None,
            export: None,
            replicas: None,
            full: false,
        }))
        .await;
    assert!(result.is_err(), "an unresolvable selector should be a tool error");
}

/// `load_component` rejects `replicas: 0` (issue 2626, ADR-0090 §4
/// posture) before it ever resolves the selector — a bad known value
/// is a hard tool error, never a silent zero-instance no-op.
#[tokio::test]
async fn load_component_replicas_zero_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .load_component(Parameters(LoadComponentArgs {
            engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            selector: "irrelevant".to_owned(),
            name: None,
            config: None,
            config_path: None,
            export: None,
            replicas: Some(0),
            full: false,
        }))
        .await;
    assert!(result.is_err(), "replicas: 0 must be a tool error, not a silent no-op");
}

/// Tripwire: a `replicas: N` reply is one shared capabilities block plus
/// N `{mailbox_id, name}` instances — no per-instance capabilities echo
/// (issue 3006). The tripwire exercises the same reply builder used by the
/// successful production fan-out path.
#[test]
fn replicas_reply_shape_is_shared_caps_plus_instances() {
    use aether_data::{KindId, ReplyContract};
    use aether_kinds::{ComponentCapabilities, HandlerCapability};

    let caps = ComponentCapabilities {
        handlers: vec![HandlerCapability {
            id: KindId(1),
            name: "aether.test.on".to_owned(),
            doc: Some("One line.\n\nMore body.".to_owned()),
            reply: ReplyContract::None,
        }],
        ..ComponentCapabilities::default()
    };
    let reply: serde_json::Value = serde_json::from_str(
        &replicas_reply(
            &caps,
            &[
                serde_json::json!({ "mailbox_id": "mbx-a", "name": "svc-0" }),
                serde_json::json!({ "mailbox_id": "mbx-b", "name": "svc-1" }),
                serde_json::json!({ "mailbox_id": "mbx-c", "name": "svc-2" }),
            ],
            false,
        )
        .expect("replica reply serializes"),
    )
    .expect("replica reply is JSON");
    assert!(reply.get("components").is_none(), "old components array must not appear: {reply}");
    assert_eq!(reply["instances"].as_array().map(Vec::len), Some(3));
    assert!(reply["instances"][0].get("capabilities").is_none());
    assert_eq!(reply["capabilities"]["handlers"][0]["doc"], "One line.");
}

/// `replace_component` with a malformed tagged mailbox id is
/// rejected before any RPC.
#[tokio::test]
async fn replace_component_bad_mailbox_id_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .replace_component(Parameters(ReplaceComponentArgs {
            engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
            mailbox_id: "not-a-tagged-id".to_owned(),
            selector: "any-selector".to_owned(),
            drain_timeout_ms: None,
            config: None,
            config_path: None,
            export: None,
            full: false,
        }))
        .await;
    assert!(result.is_err(), "a malformed mailbox_id should be a tool error");
}
