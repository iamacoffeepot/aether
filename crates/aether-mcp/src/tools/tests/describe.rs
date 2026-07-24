use super::super::bytes::response_inline_max_bytes;
#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;
use aether_kinds::{DescribeComponent, DescribeComponentResult};
use std::collections::VecDeque;
use std::fs;

/// `describe_kinds` with no `engine_id` and an empty hub returns the
/// substrate static inventory. The logical compact result is a non-empty array
/// of `{name,shape}` objects; target-specific inventories over the generic
/// response threshold arrive through the documented lossless spill envelope.
#[tokio::test]
async fn describe_kinds_returns_the_substrate_inventory() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let out = mcp
        .describe_kinds(Parameters(DescribeKindsArgs {
            engine_id: None,
            families: false,
            names: None,
            prefix: None,
            full: false,
        }))
        .await
        .expect("describe_kinds ok");
    let result: serde_json::Value = serde_json::from_str(&out).expect("describe_kinds result is JSON");
    let (arr, spill_summary) = result.as_array().map_or_else(
        || {
            let file = result["file"].as_str().expect("oversized result names its spill file");
            let bytes = result["bytes"].as_u64().expect("oversized result reports its byte length");
            assert!(
                bytes > u64::try_from(response_inline_max_bytes()).expect("response threshold fits u64"),
                "only an over-threshold response may spill: {result}",
            );
            let body = fs::read_to_string(file).expect("spilled describe_kinds response is readable");
            fs::remove_file(file).expect("remove consumed describe_kinds spill");
            assert_eq!(u64::try_from(body.len()).expect("response length fits u64"), bytes);
            (
                serde_json::from_str::<Vec<serde_json::Value>>(&body).expect("spilled response is the compact array"),
                Some(&result["summary"]),
            )
        },
        |arr| (arr.clone(), None),
    );
    assert!(!arr.is_empty(), "describe_kinds should list the substrate vocabulary");
    let first = &arr[0];
    assert!(
        first.get("name").is_some() && first.get("shape").is_some(),
        "compact entry must carry name and shape, got: {first}",
    );
    assert!(first.get("schema").is_none(), "compact entry must not carry schema, got: {first}");
    if let Some(summary) = spill_summary {
        assert_eq!(summary["kind"], "array");
        assert_eq!(summary["count"].as_u64(), u64::try_from(arr.len()).ok());
    }
}

/// `describe_kinds(families=true)` returns a sorted digest rather than
/// individual kind rows.
#[tokio::test]
async fn describe_kinds_families_returns_digest() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let out = mcp
        .describe_kinds(Parameters(DescribeKindsArgs {
            engine_id: None,
            families: true,
            names: None,
            prefix: None,
            full: false,
        }))
        .await
        .expect("families digest succeeds");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
    assert!(!rows.is_empty(), "families digest should not be empty");
    for row in &rows {
        assert!(row.get("family").is_some() && row.get("count").is_some(), "digest row shape: {row}");
        assert!(row.get("name").is_none() && row.get("shape").is_none(), "digest omits kind fields: {row}");
        assert!(row["count"].as_u64().is_some_and(|count| count > 0), "family count is positive: {row}");
    }
    assert!(
        rows.windows(2).all(|pair| pair[0]["family"].as_str() <= pair[1]["family"].as_str()),
        "families are sorted: {rows:?}",
    );
}

/// `families` composes with `prefix` by digesting only the matching
/// descriptor subset.
#[tokio::test]
async fn describe_kinds_families_with_prefix_digests_subset() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let out = mcp
        .describe_kinds(Parameters(DescribeKindsArgs {
            engine_id: None,
            families: true,
            names: None,
            prefix: Some("aether.fs".to_owned()),
            full: false,
        }))
        .await
        .expect("prefix-filtered families digest succeeds");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
    assert!(!rows.is_empty(), "aether.fs should contain at least one family");
    assert!(
        rows.iter().all(|row| row["family"].as_str().is_some_and(|family| family.starts_with("aether.fs"))),
        "only prefix-matching families are returned: {rows:?}",
    );
}

/// `families` is the active selector when `full` is also true, so the
/// digest succeeds and does not grow schema fields.
#[tokio::test]
async fn describe_kinds_families_ignores_full() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let out = mcp
        .describe_kinds(Parameters(DescribeKindsArgs {
            engine_id: None,
            families: true,
            names: None,
            prefix: None,
            full: true,
        }))
        .await
        .expect("families plus full succeeds as a digest");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
    assert!(!rows.is_empty(), "families digest should not be empty");
    assert!(
        rows.iter().all(|row| row.get("family").is_some() && row.get("schema").is_none()),
        "full is ignored for the family digest: {rows:?}",
    );
}

/// `describe_kinds(prefix="aether.fs")` narrows the array to only the
/// fs kinds — every returned name starts with the prefix.
#[tokio::test]
async fn describe_kinds_prefix_narrows_results() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let out = mcp
        .describe_kinds(Parameters(DescribeKindsArgs {
            engine_id: None,
            families: false,
            names: None,
            prefix: Some("aether.fs".to_owned()),
            full: false,
        }))
        .await
        .expect("describe_kinds ok");
    let arr: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
    assert!(!arr.is_empty(), "aether.fs prefix should match at least one kind");
    for entry in &arr {
        let name = entry["name"].as_str().expect("name is a string");
        assert!(name.starts_with("aether.fs"), "entry name {name:?} should start with \"aether.fs\"");
    }
}

/// `describe_kinds(names=[...])` returns exactly the named kind with no
/// prefix overmatch.
#[tokio::test]
async fn describe_kinds_names_returns_exact_kind() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let out = mcp
        .describe_kinds(Parameters(DescribeKindsArgs {
            engine_id: None,
            families: false,
            names: Some(vec!["aether.fs.write".to_owned()]),
            prefix: None,
            full: false,
        }))
        .await
        .expect("exact-name lookup succeeds");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
    assert_eq!(rows.len(), 1, "exact-name lookup returns one kind: {rows:?}");
    assert_eq!(rows[0]["name"], "aether.fs.write");
    assert!(rows[0].get("shape").is_some() && rows[0].get("schema").is_none(), "compact exact row: {rows:?}");
}

/// `names` composes with `full` to return only the exact kind's nested
/// schema.
#[tokio::test]
async fn describe_kinds_names_with_full_returns_exact_schema() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let out = mcp
        .describe_kinds(Parameters(DescribeKindsArgs {
            engine_id: None,
            families: false,
            names: Some(vec!["aether.fs.write".to_owned()]),
            prefix: None,
            full: true,
        }))
        .await
        .expect("exact-name full lookup succeeds");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
    assert_eq!(rows.len(), 1, "exact-name full lookup returns one kind: {rows:?}");
    assert_eq!(rows[0]["name"], "aether.fs.write");
    assert!(rows[0].get("schema").is_some() && rows[0].get("shape").is_none(), "full exact row: {rows:?}");
}

/// `describe_kinds(prefix=..., full=true)` returns objects with a
/// `schema` key (the full nested `SchemaType`) and no `shape` key.
#[tokio::test]
async fn describe_kinds_prefix_with_full_returns_schema_key() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let out = mcp
        .describe_kinds(Parameters(DescribeKindsArgs {
            engine_id: None,
            families: false,
            names: None,
            prefix: Some("aether.fs".to_owned()),
            full: true,
        }))
        .await
        .expect("describe_kinds ok");
    let arr: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
    assert!(!arr.is_empty(), "aether.fs prefix should match at least one kind");
    for entry in &arr {
        assert!(entry.get("schema").is_some(), "full entry must carry schema, got: {entry}");
        assert!(entry.get("shape").is_none(), "full entry must not carry shape, got: {entry}");
    }
}

/// Exact names are exclusive with other selectors, and unfiltered full
/// vocabulary dumps are refused.
#[tokio::test]
async fn describe_kinds_rejects_selector_conflicts_and_bare_full() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let cases = [
        (
            "names plus prefix",
            DescribeKindsArgs {
                engine_id: None,
                families: false,
                names: Some(vec!["aether.fs.write".to_owned()]),
                prefix: Some("aether.fs".to_owned()),
                full: false,
            },
        ),
        (
            "names plus families",
            DescribeKindsArgs {
                engine_id: None,
                families: true,
                names: Some(vec!["aether.fs.write".to_owned()]),
                prefix: None,
                full: false,
            },
        ),
        ("bare full", DescribeKindsArgs { engine_id: None, families: false, names: None, prefix: None, full: true }),
    ];
    for (label, args) in cases {
        let result = mcp.describe_kinds(Parameters(args)).await;
        assert!(result.is_err(), "{label} should be rejected");
    }
}

/// `describe_kinds(prefix="zzz.does.not.exist")` returns an empty
/// array — not an error.
#[tokio::test]
async fn describe_kinds_nonmatching_prefix_returns_empty() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let out = mcp
        .describe_kinds(Parameters(DescribeKindsArgs {
            engine_id: None,
            families: false,
            names: None,
            prefix: Some("zzz.does.not.exist".to_owned()),
            full: false,
        }))
        .await
        .expect("describe_kinds returns ok even with no matches");
    let arr: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
    assert!(arr.is_empty(), "non-matching prefix should return empty array, got {arr:?}");
}

#[tokio::test]
async fn describe_kinds_live_path_surfaces_component_defined_kind() {
    use aether_data::{KindDescriptor, SchemaType};

    let component_kind =
        KindDescriptor { name: "test.issue_2420.uniquely_named_kind".to_owned(), schema: SchemaType::String };

    // Pre-condition: absent from the static vocabulary in both the
    // production and the test binary — ensures the assertion below
    // can only pass if describe_kinds reads the engine cache.
    assert!(
        !descriptors::all().iter().any(|d| d.name == component_kind.name),
        "test invariant: the component kind must not be in descriptors::all()",
    );

    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);

    // Use a synthetic but well-formed engine UUID so parse_engine_id
    // accepts it; the hub doesn't supervise it, so refresh_engine_kinds
    // fails silently (ok().and_then() path), leaving the pre-seeded
    // entry intact.
    let engine = EngineId(Uuid::from_u128(0x2420_dead_beef));
    let engine_id_str = engine.0.to_string();

    // Pre-seed the per-engine cache as load_component / refresh_engine_kinds
    // would after a component with this kind is loaded.
    mcp.merge_into_engine_cache(engine, vec![component_kind.clone()]);

    let out = mcp
        .describe_kinds(Parameters(DescribeKindsArgs {
            engine_id: Some(engine_id_str),
            families: false,
            names: None,
            prefix: None,
            full: false,
        }))
        .await
        .expect("describe_kinds ok with engine_id");
    let arr: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
    assert!(
        arr.iter().any(|e| e["name"].as_str() == Some(&component_kind.name)),
        "describe_kinds must surface the component-defined kind from the engine cache; \
         got names: {:?}",
        arr.iter().filter_map(|e| e["name"].as_str()).collect::<Vec<_>>(),
    );
}

/// `describe_component` reads the component cache: an empty cache
/// errors, a seeded entry round-trips.
#[tokio::test]
async fn describe_component_reads_the_cache() {
    let (_chassis, port) = boot_hub();
    let mcp = connect_mcp(port);
    let engine_id = "00000000-0000-0000-0000-000000000001";
    // A real, taggable mailbox id (arbitrary u64s don't carry the
    // mailbox-domain bits `tagged_id::encode` needs).
    let mailbox = mailbox_id_from_name("aether.test.fake_component");
    let tagged = tagged_id::encode(mailbox.0).expect("mailbox id is taggable");

    // Empty cache, addressed by `mbx-` id → error (no name to forward
    // live, so the cache is the only source).
    let miss = mcp
        .describe_component(Parameters(DescribeComponentArgs {
            engine_id: engine_id.to_owned(),
            component: tagged.clone(),
            full: false,
        }))
        .await;
    assert!(miss.is_err(), "an uncached component addressed by id should be a tool error");

    // Seed the cache with a handler that declares a `-> R` reply
    // contract (ADR-0109). `describe_component` surfaces the `reply`
    // kind id verbatim through serde, so a caller reads `In -> Out`
    // before issuing the call.
    let engine = EngineId(Uuid::parse_str(engine_id).expect("test setup: engine_id is a valid uuid"));
    let multi_doc = "Summary line.\n\nFull body the default projection must drop.";
    let seeded = ComponentCapabilities {
        handlers: vec![HandlerCapability {
            id: KindId(0x11),
            name: "test.request".to_owned(),
            doc: Some(multi_doc.to_owned()),
            reply: aether_data::ReplyContract::One(KindId(0x22)),
        }],
        ..ComponentCapabilities::default()
    };
    mcp.components
        .lock()
        .expect("test setup: component cache mutex is never poisoned")
        .insert((engine, mailbox), seeded);
    let hit = mcp
        .describe_component(Parameters(DescribeComponentArgs {
            engine_id: engine_id.to_owned(),
            component: tagged.clone(),
            full: false,
        }))
        .await
        .expect("cached component describes");
    let caps: serde_json::Value = serde_json::from_str(&hit).expect("json");
    assert!(caps.get("handlers").is_some(), "capabilities shape: {hit}");
    assert!(!caps["handlers"][0]["reply"].is_null(), "the handler's ADR-0109 reply contract is surfaced: {hit}");
    assert_eq!(
        caps["handlers"][0]["doc"], "Summary line.",
        "full=false projects handler docs to the first rustdoc line: {hit}"
    );
    let hit_full = mcp
        .describe_component(Parameters(DescribeComponentArgs {
            engine_id: engine_id.to_owned(),
            component: tagged,
            full: true,
        }))
        .await
        .expect("full describe keeps multi-line docs");
    let caps_full: serde_json::Value = serde_json::from_str(&hit_full).expect("json");
    assert_eq!(caps_full["handlers"][0]["doc"], multi_doc, "full=true keeps the wire doc string: {hit_full}");
}

#[tokio::test]
async fn describe_component_uses_the_engine_resolved_id_and_canonical_path() {
    let supplied = "aether.component://camera";
    let canonical = "aether.component/aether.embedded:camera";
    let engine_answer = MailboxId(0x4057_0000_0000_0100);
    let engine = EngineId(Uuid::from_u128(0x4057));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let replies = Arc::new(Mutex::new(VecDeque::from([TerrainRouteReply {
        events: vec![TerrainReplyEvent {
            kind: DescribeComponentResult::ID,
            payload: DescribeComponentResult::Ok {
                capabilities: ComponentCapabilities {
                    handlers: vec![HandlerCapability {
                        id: KindId(0x33),
                        name: "test.by_name".to_owned(),
                        doc: None,
                        reply: aether_data::ReplyContract::None,
                    }],
                    ..ComponentCapabilities::default()
                },
            }
            .encode_into_bytes(),
        }],
        settle: true,
    }])));
    let (_chassis, port) = boot_hub_with_address_route_replies(engine_answer, canonical, Arc::clone(&calls), replies);
    let mcp = connect_mcp(port);

    let output = mcp
        .describe_component(Parameters(DescribeComponentArgs {
            engine_id: engine.0.to_string(),
            component: supplied.to_owned(),
            full: false,
        }))
        .await
        .expect("name-addressed describe resolves and forwards");
    let output: serde_json::Value = serde_json::from_str(&output).expect("json");
    assert_eq!(output["handlers"][0]["name"], "test.by_name");
    assert!(
        mcp.components.lock().expect("component cache mutex is never poisoned").contains_key(&(engine, engine_answer)),
        "capabilities cache uses the engine-returned id"
    );
    let calls = calls.lock().expect("address-route calls mutex is never poisoned");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].kind, ResolveAddress::ID);
    let describe = DescribeComponent::decode_from_bytes(&calls[1].payload).expect("describe request decodes");
    drop(calls);
    assert_eq!(describe.name, canonical, "component host receives the canonical engine path");
}
