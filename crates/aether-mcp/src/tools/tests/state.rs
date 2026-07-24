#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;

/// One-field `{ button: String }` struct schema — the widened shape a
/// kind gains in place (issue 2672); the narrow shape is the empty
/// struct `narrow_struct_schema`.
fn widened_struct_schema() -> SchemaType {
    use aether_data::NamedField;
    SchemaType::Struct {
        fields: vec![NamedField { name: "button".into(), ty: SchemaType::String }].into(),
        repr_c: false,
    }
}

/// The empty-struct schema a widened kind had *before* it gained a
/// field — the stale shape the harness holds cached (issue 2672).
fn narrow_struct_schema() -> SchemaType {
    SchemaType::Struct { fields: vec![].into(), repr_c: false }
}

/// Build the single-entry `ListKindsResult` a `RouteInventorySink`
/// serves for `name` at `schema` — the wire projection the real
/// inventory cap performs (issue 2672).
fn canned_kinds_reply(name: &str, schema: &SchemaType) -> ListKindsResult {
    use aether_kinds::KindDescriptorWire;
    ListKindsResult {
        kinds: vec![KindDescriptorWire {
            id: KindId(kind_id_from_parts(name, schema)),
            name: name.to_owned(),
            schema_wire: wire::to_vec(schema).expect("SchemaType wire-encodes"),
        }],
    }
}

/// Issue 2672: a kind widened in place (same name, a new field) —
/// the harness holds the stale narrow descriptor, so the name
/// resolves to a cache hit and `lookup_descriptor` never refreshes.
/// The field-mismatch `encode_schema` failure must itself trigger the
/// `aether.inventory.kinds` refresh-and-retry, so a `send_mail` that
/// supplies the new field succeeds against the widened schema. The
/// last correctness gap in ADR-0091's lazy-on-miss cache.
#[tokio::test]
async fn resolve_and_encode_refreshes_on_field_mismatch() {
    use aether_data::KindDescriptor;

    let name = "aether.test.widened_kind";
    let widened = widened_struct_schema();

    // The live engine's vocabulary carries the widened shape.
    let calls = Arc::new(AtomicUsize::new(0));
    let (_chassis, port) = boot_hub_with_route_loopback(canned_kinds_reply(name, &widened), Arc::clone(&calls));
    let mcp = connect_mcp(port);

    // Pre-seed the per-engine cache with the STALE narrow shape, so
    // the name is a cache hit (no unknown-kind-miss refresh) — only
    // the encode failure can drive the refresh.
    let engine = EngineId(Uuid::from_u128(0x2672_dead_beef));
    mcp.merge_into_engine_cache(engine, vec![KindDescriptor { name: name.to_owned(), schema: narrow_struct_schema() }]);

    // Params carrying the new field: rejected by the narrow cached
    // schema, accepted by the widened live one.
    let params = serde_json::json!({ "button": "left" });
    let (desc, payload) = mcp
        .resolve_and_encode(engine, name, params.clone())
        .await
        .expect("field-mismatch encode failure refreshes and retries");

    assert_eq!(calls.load(Ordering::Relaxed), 1, "the field-mismatch triggered exactly one refresh RPC");
    assert_eq!(desc.schema, widened, "resolve_and_encode returns the fresh (widened) descriptor");
    let decoded = aether_codec::decode_schema(&payload, &widened).expect("payload decodes against the widened schema");
    assert_eq!(decoded, params, "the new field round-trips through the refreshed schema");
}

/// Issue 2672: the refresh-and-retry is bounded to exactly one
/// refresh — when the fresh vocabulary *still* rejects the params (a
/// field that isn't in the live schema either, not an in-place
/// widening), `resolve_and_encode` surfaces the error after a single
/// refresh rather than looping. The tripwire for the "retry-once"
/// invariant.
#[tokio::test]
async fn resolve_and_encode_retry_is_bounded_to_one_refresh() {
    use aether_data::KindDescriptor;

    let name = "aether.test.narrow_kind";

    // The live vocabulary is *also* narrow — the refresh changes
    // nothing, so the re-encode fails identically.
    let calls = Arc::new(AtomicUsize::new(0));
    let (_chassis, port) =
        boot_hub_with_route_loopback(canned_kinds_reply(name, &narrow_struct_schema()), Arc::clone(&calls));
    let mcp = connect_mcp(port);

    let engine = EngineId(Uuid::from_u128(0x2672_beef_cafe));
    mcp.merge_into_engine_cache(engine, vec![KindDescriptor { name: name.to_owned(), schema: narrow_struct_schema() }]);

    let params = serde_json::json!({ "button": "left" });
    let result = mcp.resolve_and_encode(engine, name, params).await;

    assert!(result.is_err(), "a field the fresh vocab still lacks surfaces an error, not a hang");
    assert_eq!(calls.load(Ordering::Relaxed), 1, "the retry refreshed exactly once — no loop");
}

/// ADR-0091 issue 1232 (end-to-end): a kind registered in the
/// substrate's `Registry` — emulating the post-`load_component`
/// state for a component-defined kind like `aether.kit.mesh.load` —
/// flows through `InventoryCapability`'s `ListKinds` projection
/// onto the wire, lands in the harness's per-engine encode cache,
/// and the next `send_mail` encodes correctly. This is the
/// forcing-function path the issue calls out: a kind NOT in
/// `descriptors::all()` becomes encodable the moment the substrate
/// holds it.
///
/// Test addresses the engines cap with `engine = None` (the hub
/// fixture's local dispatch path) so the round-trip closes against
/// the same chassis without needing a separately-routed engine
/// proxy; the cache machinery under test is engine-keyed but
/// engine-agnostic at the RPC layer.
#[tokio::test]
async fn lookup_descriptor_picks_up_a_post_load_kind_via_inventory() {
    use aether_data::{KindDescriptor, SchemaType};

    // The component-defined kind in this scenario: present in the
    // substrate's `Registry` but not in `descriptors::all()`.
    let component_kind =
        KindDescriptor { name: "aether.test.component_defined_kind".to_owned(), schema: SchemaType::String };

    let extras = vec![component_kind.clone()];
    let (_chassis, port) = boot_hub_with_inventory(&extras);
    let session = RpcSession::connect(&format!("127.0.0.1:{port}")).expect("session connects");
    let mcp = Mcp::new(
        Arc::new(session),
        Arc::new(ComponentCache::default()),
        Arc::new(ReverseNameCache::default()),
        Arc::new(KindsCache::default()),
    );

    // Pre-condition: the static prefill does NOT carry the
    // component's kind. (If a future change accidentally promotes
    // it to native, the test surfaces immediately rather than
    // silently bypassing the cache-refresh path.)
    assert!(
        !descriptors::all().iter().any(|d| d.name == component_kind.name),
        "test invariant: the component kind must not be in the static descriptors",
    );

    // Address the hub's local `aether.inventory` via the engines-
    // cap path: the hub-fixture's RPC server routes
    // `engine = Some(uuid)` envelopes through the engines cap,
    // which knows no matching engine and warn-drops. To exercise
    // the cache against the local cap, route as a local Call
    // by stamping `engine = None`. We bypass `lookup_descriptor`'s
    // `engine_envelope` here because the test fixture is hub-
    // shaped (the engines cap doesn't proxy to a separate
    // substrate); in production the hub forwards to the engine
    // and the engine answers via its local `aether.inventory`.
    let reply =
        mcp.session.call_one(local_envelope(INVENTORY_CAP, &ListKinds {})).await.expect("aether.inventory.kinds reply");
    let result = ListKindsResult::decode_from_bytes(&reply.payload).expect("ListKindsResult decodes");
    // The reply must include the registered component kind with a
    // schema that decodes back to the originally registered shape
    // — the wire path the harness's cache reads from.
    let entry = result.kinds.iter().find(|k| k.name == component_kind.name).unwrap_or_else(|| {
        panic!(
            "ListKindsResult should include the registered component kind; \
                 got {:?}",
            result.kinds.iter().map(|k| &k.name).collect::<Vec<_>>(),
        )
    });
    let decoded_schema: SchemaType = wire::from_bytes(&entry.schema_wire).expect("schema_wire decodes");
    assert!(matches!(decoded_schema, SchemaType::String), "the registered schema round-trips through the wire");

    // Now drive the schema encode path directly. Direct-mail preparation also
    // performs engine-owned recipient resolution, which is covered below by
    // the routed resolver double.
    let engine = EngineId(Uuid::from_u128(0x1232_dead_beef));
    mcp.merge_into_engine_cache(engine, vec![component_kind.clone()]);
    let (_descriptor, payload) = mcp
        .resolve_and_encode(engine, &component_kind.name, serde_json::Value::String("hello".to_owned()))
        .await
        .expect("component-defined kind encodes from the live cache");
    let decoded = aether_codec::decode_schema(&payload, &component_kind.schema)
        .expect("payload decodes against the cached schema");
    assert_eq!(
        decoded,
        serde_json::Value::String("hello".to_owned()),
        "the encoded payload round-trips through aether_codec against the live schema",
    );
}

#[tokio::test]
async fn engine_address_resolver_returns_the_engine_mailbox_without_local_folding() {
    let supplied = "aether.test://short";
    let canonical = "aether.test/aether.test.child:short";
    let engine_answer = MailboxId(0xABCD_EF01_2345_6789);
    #[allow(clippy::disallowed_methods)]
    let locally_folded = mailbox_id_from_path(supplied);
    assert_ne!(engine_answer, locally_folded, "test answer must expose accidental client-side folding");

    let calls = Arc::new(Mutex::new(Vec::new()));
    let (_chassis, port) = boot_hub_with_address_route_loopback(engine_answer, canonical, Arc::clone(&calls));
    let mcp = connect_mcp(port);
    let engine = EngineId(Uuid::from_u128(0x4057));

    let resolved =
        mcp.resolve_engine_address(engine, supplied).await.expect("routed engine resolves abbreviated address");
    assert_eq!(resolved, (engine_answer, canonical.to_owned()));

    let calls = calls.lock().expect("address-route calls mutex is never poisoned");
    assert_eq!(calls.len(), 1, "textual address performs exactly one uncached resolver RPC");
    assert_eq!(calls[0].kind, ResolveAddress::ID);
    let request = ResolveAddress::decode_from_bytes(&calls[0].payload).expect("resolver request decodes");
    drop(calls);
    assert_eq!(request.address, supplied);
}
