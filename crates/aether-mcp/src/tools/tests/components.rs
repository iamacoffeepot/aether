#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;
use crate::tools::components::{binary_listing_response, component_listing_response, store_listing_response};
use aether_actor::actor;
use aether_kinds::{
    ListComponentBinariesResult, ListEngineBinariesResult, SetArtifactPinned, SetArtifactPinnedResult, UploadBinary,
    UploadBinaryResult, UploadComponent, UploadComponentResult,
};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::testing::boot_authority;
use std::sync::{Arc, Mutex};

/// Hub-local fleet double for pin/upload forwarding tests. Installed at
/// `aether.fleet` so engine=None Calls land as the typed kinds themselves
/// (an engine-routed Call would arrive as `RouteEnvelope` and these
/// handlers would not fire). It never reads `staged_path`.
#[derive(Clone)]
struct FleetLocalCells {
    binary: Arc<Mutex<Vec<UploadBinary>>>,
    component: Arc<Mutex<Vec<UploadComponent>>>,
    pins: Arc<Mutex<Vec<SetArtifactPinned>>>,
    binary_reply: Arc<Mutex<UploadBinaryResult>>,
    component_reply: Arc<Mutex<UploadComponentResult>>,
    pin_reply: Arc<Mutex<SetArtifactPinnedResult>>,
}

impl FleetLocalCells {
    fn new() -> Self {
        Self {
            binary: Arc::new(Mutex::new(Vec::new())),
            component: Arc::new(Mutex::new(Vec::new())),
            pins: Arc::new(Mutex::new(Vec::new())),
            binary_reply: Arc::new(Mutex::new(UploadBinaryResult::Ok { hash: "bin-hash".to_owned(), name: None })),
            component_reply: Arc::new(Mutex::new(UploadComponentResult::Ok {
                hash: "cmp-hash".to_owned(),
                name: None,
            })),
            pin_reply: Arc::new(Mutex::new(SetArtifactPinnedResult::Ok { hash: "pin-hash".to_owned(), pinned: true })),
        }
    }
}

struct FleetLocalSink {
    cells: FleetLocalCells,
}

#[actor(singleton, root)]
impl NativeActor for FleetLocalSink {
    type Config = ();
    type Params = FleetLocalCells;
    const NAMESPACE: &'static str = "aether.fleet";

    fn init((): (), cells: FleetLocalCells, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { cells })
    }

    #[handler::single]
    fn on_upload_binary(&mut self, _ctx: &mut NativeCtx<'_>, mail: UploadBinary) -> UploadBinaryResult {
        self.cells.binary.lock().expect("binary log mutex").push(mail);
        self.cells.binary_reply.lock().expect("binary reply mutex").clone()
    }

    #[handler::single]
    fn on_upload_component(&mut self, _ctx: &mut NativeCtx<'_>, mail: UploadComponent) -> UploadComponentResult {
        self.cells.component.lock().expect("component log mutex").push(mail);
        self.cells.component_reply.lock().expect("component reply mutex").clone()
    }

    #[handler::single]
    fn on_set_artifact_pinned(&mut self, _ctx: &mut NativeCtx<'_>, mail: SetArtifactPinned) -> SetArtifactPinnedResult {
        self.cells.pins.lock().expect("pin log mutex").push(mail);
        self.cells.pin_reply.lock().expect("pin reply mutex").clone()
    }
}

fn boot_hub_with_fleet_local_sink(cells: FleetLocalCells) -> (PassiveChassis<TestChassis>, u16) {
    let registry = Arc::new(Registry::new());
    for descriptor in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(&boot_authority(), descriptor);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<FleetLocalSink>(cells)
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams {
                peer_kind: PeerKind::Substrate {
                    engine_name: "test-hub".into(),
                    engine_version: "0.1.0".into(),
                    kinds: vec![],
                },
                route_target: None,
            },
            RpcServerConfig { port: Some(0) },
        )
        .build_passive()
        .expect("fleet-local sink hub boots");
    let port = chassis.handle::<RpcServerHandle>().expect("RpcServerHandle published").local_port;
    (chassis, port)
}

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

#[test]
fn store_listing_response_reports_truncation_metadata_and_notice() {
    let response = store_listing_response(vec!["first", "second"], 5);
    let value = serde_json::to_value(response).expect("listing response serializes");
    assert_eq!(value["entries"], serde_json::json!(["first", "second"]));
    assert_eq!(value["total_matched"], 5);
    assert_eq!(value["shown"], 2);
    assert_eq!(value["truncated"], true);
    assert!(value["notice"].as_str().is_some_and(|notice| notice.contains("larger explicit `limit`")));

    let complete =
        serde_json::to_value(store_listing_response(vec!["only"], 1)).expect("complete listing response serializes");
    assert_eq!(complete["truncated"], false);
    assert!(complete["notice"].is_null());
}

#[test]
fn binary_listing_wraps_entries_and_match_count() {
    let response = binary_listing_response(ListEngineBinariesResult {
        binaries: vec![aether_kinds::BinaryEntry {
            hash: "abc".to_owned(),
            name: Some("headless".to_owned()),
            manifest: aether_kinds::BinaryManifest {
                chassis: "headless".to_owned(),
                caps: vec!["aether.fs".to_owned()],
                git_sha: "deadbee".to_owned(),
                profile: "debug".to_owned(),
                target: "x86_64-unknown-linux-gnu".to_owned(),
                env_keys: vec!["AETHER_TICK_HZ".to_owned(), "AETHER_RPC_PORT".to_owned()],
                argv_flags: vec!["tick-hz".to_owned(), "rpc-port".to_owned(), "workers".to_owned()],
            },
        }],
        total_matched: 3,
    });
    let value = serde_json::to_value(response).expect("binary listing response serializes");
    assert_eq!(value["entries"][0]["hash"], "abc");
    assert_eq!(value["entries"][0]["manifest"]["chassis"], "headless");
    // ADR-0162: the config surface is rendered as bounded counts, not full
    // lists — the full sets live in the store, not the listing.
    assert_eq!(value["entries"][0]["manifest"]["env_key_count"], 2);
    assert_eq!(value["entries"][0]["manifest"]["argv_flag_count"], 3);
    assert!(value["entries"][0]["manifest"].get("env_keys").is_none(), "env_keys is not inlined in the listing");
    assert!(value["entries"][0]["manifest"].get("argv_flags").is_none(), "argv_flags is not inlined in the listing");
    assert_eq!(value["total_matched"], 3);
    assert_eq!(value["shown"], 1);
    assert_eq!(value["truncated"], true);
}

#[test]
fn component_listing_names_kinds_omits_union_and_preserves_manifest_fields() {
    use aether_kinds::{ComponentActor, ComponentEntry, ComponentManifest, Tick};

    let unknown = KindId(0xDEAD_BEEF_DEAD_BEEF);
    let response = component_listing_response(ListComponentBinariesResult {
        components: vec![ComponentEntry {
            hash: "def".to_owned(),
            name: Some("probe".to_owned()),
            manifest: ComponentManifest {
                namespaces: vec!["test.probe".to_owned(), "test.probe.child".to_owned()],
                actors: vec![ComponentActor {
                    namespace: "test.probe".to_owned(),
                    handled_kinds: vec![Tick::ID, unknown],
                    fallback: true,
                }],
                handled_kinds: vec![Tick::ID, unknown],
                fallback: true,
                provenance: "rustc 1.test".to_owned(),
                default_entry: Some("test.probe".to_owned()),
            },
        }],
        total_matched: 1,
    });
    let value = serde_json::to_value(response).expect("component listing response serializes");
    let manifest = &value["entries"][0]["manifest"];
    assert_eq!(manifest["namespaces"], serde_json::json!(["test.probe", "test.probe.child"]));
    assert_eq!(manifest["actors"][0]["namespace"], "test.probe");
    assert_eq!(manifest["actors"][0]["handled_kinds"][0], Tick::NAME);
    assert_eq!(manifest["actors"][0]["handled_kinds"][1], unknown.to_string());
    assert_eq!(manifest["actors"][0]["fallback"], true);
    assert!(manifest.get("handled_kinds").is_none(), "the redundant manifest-wide union is omitted");
    assert_eq!(manifest["fallback"], true);
    assert_eq!(manifest["provenance"], "rustc 1.test");
    assert_eq!(manifest["default_entry"], "test.probe");
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

// Tripwire: config_path bounds the read against the RPC frame cap up
// front, the same as the `$file` sigil embed path, instead of failing
// late at frame encode.
#[tokio::test]
async fn component_config_path_oversized_is_rejected_up_front() {
    let cap = max_frame_size();
    let path = stage_blob_file("config-oversize", &vec![0u8; cap + 1]);
    let schema = config_struct_schema();
    let kind = config_kind(&schema);
    let err = component_config_bytes(Some(&kind), None, Some(path.to_str().expect("utf-8 temp path")), "test")
        .await
        .expect_err("oversized config_path must be rejected up front");
    assert!(err.to_string().contains("RPC frame cap"), "unexpected error: {err}");
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
/// the default actor namespace — the bug this catches is a fan-out base
/// name that disagrees with what an unreplicated load would resolve to.
#[test]
fn replica_base_name_follows_name_export_namespace_precedence() {
    assert_eq!(replica_base_name(Some("caller"), Some("export-ns"), Some("default-ns")), Some("caller".to_owned()),);
    assert_eq!(replica_base_name(None, Some("export-ns"), Some("default-ns")), Some("export-ns".to_owned()),);
    assert_eq!(replica_base_name(None, None, Some("default-ns")), Some("default-ns".to_owned()),);
    assert_eq!(replica_base_name(None, None, None), None);
}

/// `replica_names` names replica 0 for the bare base and suffixes the rest,
/// so a fan-out registers the name a peer's `ctx.peer::<R>()` folds and
/// `replicas: 1` loads exactly what an omitted field loads. The bug this
/// catches is a boot-readiness prediction that drifts from the names the
/// chassis fan-out actually registers — `spawn_substrate` would then wait
/// out its readiness budget on a name nothing ever claims.
#[test]
fn replica_names_claim_the_bare_base_then_suffix() {
    assert_eq!(replica_names("handler", 3), vec!["handler", "handler-1", "handler-2"],);
    assert_eq!(replica_names("handler", 1), vec!["handler"]);
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
            engine_id: Some("00000000-0000-0000-0000-000000000001".to_owned()),
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
            engine_id: Some("00000000-0000-0000-0000-000000000001".to_owned()),
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
            "00000000-0000-0000-0000-000000000001",
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
    assert_eq!(reply["engine_id"], "00000000-0000-0000-0000-000000000001", "the reply names its engine: {reply}");
    assert_eq!(reply["instances"].as_array().map(Vec::len), Some(3));
    assert!(reply["instances"][0].get("capabilities").is_none());
    assert_eq!(reply["capabilities"]["handlers"][0]["doc"], "One line.");
}

/// `replace_component` with a malformed tagged mailbox address is
/// rejected before any RPC — the `mbx-` fast path still parses locally
/// now that `address` also accepts a lineage name.
#[tokio::test]
async fn replace_component_bad_mailbox_address_is_tool_error() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let result = mcp
        .replace_component(Parameters(ReplaceComponentArgs {
            engine_id: Some("00000000-0000-0000-0000-000000000001".to_owned()),
            address: "mbx-not-a-tagged-id".to_owned(),
            selector: "any-selector".to_owned(),
            config: None,
            config_path: None,
            export: None,
            full: false,
        }))
        .await;
    assert!(result.is_err(), "a malformed mbx- address should be a tool error");
}

#[test]
fn upload_binary_args_default_pin_is_false() {
    let args: UploadBinaryArgs =
        serde_json::from_value(serde_json::json!({ "staged_path": "/tmp/bin" })).expect("decode");
    assert!(!args.pin, "omitted pin JSON-defaults to false");
    assert!(args.name.is_none());
}

#[test]
fn upload_binary_args_explicit_pin_true() {
    let args: UploadBinaryArgs =
        serde_json::from_value(serde_json::json!({ "staged_path": "/tmp/bin", "pin": true })).expect("decode");
    assert!(args.pin);
}

#[test]
fn upload_component_args_default_and_explicit_pin() {
    let defaulted: UploadComponentArgs =
        serde_json::from_value(serde_json::json!({ "staged_path": "/tmp/c.wasm" })).expect("decode");
    assert!(!defaulted.pin);
    let pinned: UploadComponentArgs =
        serde_json::from_value(serde_json::json!({ "staged_path": "/tmp/c.wasm", "pin": true })).expect("decode");
    assert!(pinned.pin);
}

#[tokio::test]
async fn upload_binary_forwards_default_and_explicit_pin_hub_local() {
    let cells = FleetLocalCells::new();
    let (_chassis, port) = boot_hub_with_fleet_local_sink(cells.clone());
    let mcp = connect_mcp(port);
    let missing = "/no-such-aether-pin-fixture.bin";

    let out = mcp
        .upload_binary(Parameters(UploadBinaryArgs { staged_path: missing.to_owned(), name: None, pin: false }))
        .await
        .expect("scripted upload ok");
    assert_eq!(out, r#"{"hash":"bin-hash","name":null}"#);

    *cells.binary_reply.lock().expect("binary reply mutex") =
        UploadBinaryResult::Ok { hash: "pinned-hash".to_owned(), name: Some("keep".to_owned()) };
    let pinned_out = mcp
        .upload_binary(Parameters(UploadBinaryArgs {
            staged_path: missing.to_owned(),
            name: Some("keep".to_owned()),
            pin: true,
        }))
        .await
        .expect("scripted pinned upload ok");
    assert_eq!(pinned_out, r#"{"hash":"pinned-hash","name":"keep"}"#);

    let forwarded = cells.binary.lock().expect("binary log mutex").clone();
    assert_eq!(forwarded.len(), 2, "both uploads must reach the hub-local fleet handler");
    assert_eq!(forwarded[0].staged_path, missing);
    assert!(!forwarded[0].pin);
    assert!(forwarded[0].name.is_none());
    assert_eq!(forwarded[1].staged_path, missing);
    assert!(forwarded[1].pin);
    assert_eq!(forwarded[1].name.as_deref(), Some("keep"));
}

#[tokio::test]
async fn upload_binary_propagates_typed_error() {
    let cells = FleetLocalCells::new();
    *cells.binary_reply.lock().expect("binary reply mutex") =
        UploadBinaryResult::Err { error: "describe failed".to_owned() };
    let (_chassis, port) = boot_hub_with_fleet_local_sink(cells);
    let mcp = connect_mcp(port);
    let err = mcp
        .upload_binary(Parameters(UploadBinaryArgs {
            staged_path: "/no-such-aether-pin-fixture.bin".to_owned(),
            name: None,
            pin: true,
        }))
        .await
        .expect_err("typed Err is a tool error");
    assert!(err.to_string().contains("describe failed"), "got {err}");
}

#[tokio::test]
async fn upload_component_forwards_pin_hub_local_and_errors() {
    let cells = FleetLocalCells::new();
    let (_chassis, port) = boot_hub_with_fleet_local_sink(cells.clone());
    let mcp = connect_mcp(port);
    let missing = "/no-such-aether-pin-fixture.wasm";

    let out = mcp
        .upload_component(Parameters(UploadComponentArgs { staged_path: missing.to_owned(), name: None, pin: true }))
        .await
        .expect("scripted component upload ok");
    assert_eq!(out, r#"{"hash":"cmp-hash","name":null}"#);

    *cells.component_reply.lock().expect("component reply mutex") =
        UploadComponentResult::Err { error: "unparseable wasm".to_owned() };
    let err = mcp
        .upload_component(Parameters(UploadComponentArgs { staged_path: missing.to_owned(), name: None, pin: false }))
        .await
        .expect_err("typed Err is a tool error");
    assert!(err.to_string().contains("unparseable wasm"), "got {err}");

    let forwarded = cells.component.lock().expect("component log mutex").clone();
    assert_eq!(forwarded.len(), 2, "ok and error uploads must both reach the hub-local handler");
    assert_eq!(forwarded[0].staged_path, missing);
    assert!(forwarded[0].pin);
    assert!(forwarded[0].name.is_none());
    assert_eq!(forwarded[1].staged_path, missing);
    assert!(!forwarded[1].pin);
    assert!(forwarded[1].name.is_none());
}

#[tokio::test]
async fn pin_and_unpin_artifact_forward_exact_hash_and_bit() {
    let cells = FleetLocalCells::new();
    let (_chassis, port) = boot_hub_with_fleet_local_sink(cells.clone());
    let mcp = connect_mcp(port);

    let pin_out =
        mcp.pin_artifact(Parameters(ArtifactPinArgs { hash: "abc".to_owned() })).await.expect("scripted pin ok");
    assert_eq!(pin_out, r#"{"hash":"pin-hash","pinned":true}"#);

    *cells.pin_reply.lock().expect("pin reply mutex") =
        SetArtifactPinnedResult::Ok { hash: "abc".to_owned(), pinned: false };
    let unpin_out =
        mcp.unpin_artifact(Parameters(ArtifactPinArgs { hash: "abc".to_owned() })).await.expect("scripted unpin ok");
    assert_eq!(unpin_out, r#"{"hash":"abc","pinned":false}"#);

    let forwarded = cells.pins.lock().expect("pin log mutex").clone();
    assert_eq!(forwarded.len(), 2);
    assert_eq!(forwarded[0].hash, "abc");
    assert!(forwarded[0].pinned);
    assert_eq!(forwarded[1].hash, "abc");
    assert!(!forwarded[1].pinned);
}

#[tokio::test]
async fn pin_and_unpin_artifact_propagate_typed_errors() {
    let cells = FleetLocalCells::new();
    *cells.pin_reply.lock().expect("pin reply mutex") =
        SetArtifactPinnedResult::Err { error: "no stored artifact has hash \"missing\"".to_owned() };
    let (_chassis, port) = boot_hub_with_fleet_local_sink(cells.clone());
    let mcp = connect_mcp(port);
    let pin_err = mcp
        .pin_artifact(Parameters(ArtifactPinArgs { hash: "missing".to_owned() }))
        .await
        .expect_err("typed pin Err is a tool error");
    assert!(pin_err.to_string().contains("no stored artifact has hash"), "got {pin_err}");
    let unpin_err = mcp
        .unpin_artifact(Parameters(ArtifactPinArgs { hash: "missing".to_owned() }))
        .await
        .expect_err("typed unpin Err is a tool error");
    assert!(unpin_err.to_string().contains("no stored artifact has hash"), "got {unpin_err}");
    let forwarded = cells.pins.lock().expect("pin log mutex").clone();
    assert_eq!(forwarded.len(), 2);
    assert_eq!(forwarded[0].hash, "missing");
    assert!(forwarded[0].pinned);
    assert_eq!(forwarded[1].hash, "missing");
    assert!(!forwarded[1].pinned);
}
