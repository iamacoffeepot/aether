#[allow(clippy::wildcard_imports)]
use super::super::test_support::*;
#[allow(clippy::wildcard_imports)]
use super::super::*;

/// Issue 1242 / 1246: `decode_reply_events` transcodes a correlated
/// reply into the MCP wire shape — a known substrate kind decodes to
/// its name + params, and on a clean decode the raw bytes are
/// omitted (issue 1246, no int-array duplicate). This is the
/// surfacing the await-by-default change adds; the decode is the
/// reusable core both tools share.
#[test]
fn decode_reply_events_decodes_known_substrate_kind() {
    // Pick a real substrate kind out of the static inventory and
    // round-trip a params object through `encode_schema` into the
    // reply envelope the substrate would have produced.
    let descriptors = descriptors::all();
    let desc =
        descriptors.iter().find(|d| d.name == "aether.fs.list").expect("aether.fs.list is in the static vocabulary");
    let params = serde_json::json!({ "namespace": "save", "prefix": "" });
    let payload = aether_codec::encode_schema(&params, &desc.schema).expect("encode list params");
    let kind = KindId(kind_id_from_parts(&desc.name, &desc.schema));
    let reply = MailEnvelope {
        to: MailboxAddress::local(mailbox_id_from_name("aether.fs")),
        from: None,
        kind,
        correlation_id: Some(7),
        payload,
    };

    // Empty engine-kinds map → falls through to the static vocabulary.
    let decoded = decode_reply_events(&[reply], &HashMap::new(), None);
    assert_eq!(decoded.len(), 1, "one reply in, one out");
    let only = &decoded[0];
    assert_eq!(only.kind_name.as_deref(), Some("aether.fs.list"), "the known kind resolves to its name");
    assert_eq!(only.params.as_ref(), Some(&params), "params decode back to the original JSON");
    assert!(only.payload_bytes.is_none(), "a clean decode omits the raw bytes (issue 1246)");
    assert!(only.kind_id.starts_with("knd-"), "the kind id renders as the ADR-0064 tagged string: {}", only.kind_id);
}

/// Issue 1242 / 1246: an unknown / undecodable reply kind never
/// fails the surfacing — `params` is `null`, `kind_name` is `null`,
/// and the raw bytes are still returned, now base64-encoded (the
/// disconnected-engine fallback contract).
#[test]
fn decode_reply_events_falls_back_on_unknown_kind() {
    let reply = MailEnvelope {
        to: MailboxAddress::local(MailboxId(1)),
        from: None,
        kind: KindId(0xDEAD_BEEF_DEAD_BEEF),
        correlation_id: None,
        payload: vec![1, 2, 3],
    };
    // No engine-kinds entry, no declared reply → falls through to base64.
    let decoded = decode_reply_events(&[reply], &HashMap::new(), None);
    assert_eq!(decoded.len(), 1);
    let only = &decoded[0];
    assert_eq!(only.kind_name, None, "an unknown kind has no name");
    assert_eq!(only.params, None, "an unknown kind doesn't decode");
    assert_eq!(only.payload_bytes.as_deref(), Some("AQID"), "raw bytes survive as base64 (issue 1246)");
}

/// Issue 1246: a clean-decode reply serializes to JSON with no
/// `payload_bytes` key at all — the `skip_serializing_if` guard
/// against the redundant-int-array regression this issue fixes.
#[test]
fn clean_decode_reply_omits_payload_bytes_key_in_json() {
    let descriptors = descriptors::all();
    let desc =
        descriptors.iter().find(|d| d.name == "aether.fs.list").expect("aether.fs.list is in the static vocabulary");
    let params = serde_json::json!({ "namespace": "save", "prefix": "" });
    let payload = aether_codec::encode_schema(&params, &desc.schema).expect("encode list params");
    let kind = KindId(kind_id_from_parts(&desc.name, &desc.schema));
    let reply = MailEnvelope {
        to: MailboxAddress::local(mailbox_id_from_name("aether.fs")),
        from: None,
        kind,
        correlation_id: Some(7),
        payload,
    };

    // Empty engine-kinds map → falls through to the static vocabulary.
    let decoded = decode_reply_events(&[reply], &HashMap::new(), None);
    let json = serde_json::to_value(&decoded[0]).expect("reply serializes");
    let obj = json.as_object().expect("reply is a JSON object");
    assert!(!obj.contains_key("payload_bytes"), "a clean decode omits the payload_bytes key entirely: {json}");
    assert!(obj.contains_key("params"), "params is still present");
}

/// Issue 1804: `decode_reply_events` decodes a reply whose kind is
/// component-defined (not in `descriptors::all()`) when the engine
/// kind cache carries the schema and the handler's declared reply kind
/// matches the envelope (ADR-0109). This is the core gap the issue
/// closes: a `send_mail` reply for a component-defined kind should
/// surface `params`, not base64.
#[test]
fn decode_reply_events_decodes_component_defined_reply_via_engine_cache() {
    use aether_data::{KindDescriptor, SchemaType};

    // A component-defined reply kind — not in `descriptors::all()`.
    let reply_kind = KindDescriptor { name: "test.component.reply".to_owned(), schema: SchemaType::String };
    let reply_kind_id = KindId(kind_id_from_parts(&reply_kind.name, &reply_kind.schema));

    // Encode a value against the component-defined schema, as the
    // substrate handler would produce.
    let value = serde_json::Value::String("hello from component".to_owned());
    let payload = aether_codec::encode_schema(&value, &reply_kind.schema).expect("encode reply value");

    let envelope = MailEnvelope {
        to: MailboxAddress::local(mailbox_id_from_name("aether.test.component")),
        from: None,
        kind: reply_kind_id,
        correlation_id: Some(1),
        payload,
    };

    // Pre-condition: the static vocabulary doesn't carry this kind, so
    // without the engine cache the decode would fall through to base64.
    assert!(
        !descriptors::all().iter().any(|d| d.name == reply_kind.name),
        "test invariant: the component kind must not be in the static vocabulary",
    );

    // Build an engine-kinds map as `load_component` / `ListKinds` would
    // populate it, and supply the handler's declared reply kind.
    let mut engine_kinds = HashMap::new();
    engine_kinds.insert(reply_kind.name.clone(), reply_kind);

    let decoded = decode_reply_events(&[envelope], &engine_kinds, Some(reply_kind_id));
    assert_eq!(decoded.len(), 1);
    let only = &decoded[0];
    assert_eq!(only.params.as_ref(), Some(&value), "component-defined reply kind decodes to params via engine cache");
    assert!(only.payload_bytes.is_none(), "a clean decode omits the raw bytes");
    assert_eq!(
        only.kind_name.as_deref(),
        Some("test.component.reply"),
        "the component-defined kind name is surfaced from the engine cache",
    );
}

/// Issue 1804: the base64 fallback is unchanged when neither the engine
/// kind cache nor the static vocabulary carries the reply kind, even
/// when `declared_reply` is `Some`. Covers fire-and-forget / unknown-
/// sender replies that never had a registered schema.
#[test]
fn decode_reply_events_base64_fallback_when_kind_absent_from_all_caches() {
    let absent_kind_id = KindId(0xC0FF_EE00_C0FF_EE00);
    let envelope = MailEnvelope {
        to: MailboxAddress::local(MailboxId(2)),
        from: None,
        kind: absent_kind_id,
        correlation_id: None,
        payload: vec![0xAB, 0xCD],
    };
    // Declared reply matches the envelope but the engine cache is empty.
    let decoded = decode_reply_events(&[envelope], &HashMap::new(), Some(absent_kind_id));
    assert_eq!(decoded.len(), 1);
    let only = &decoded[0];
    assert_eq!(only.params, None, "absent kind doesn't decode to params");
    assert!(only.payload_bytes.is_some(), "absent kind surfaces as base64 fallback");
}

fn projected_reply(id: &str, kind_name: Option<&str>, params: serde_json::Value) -> ReplyEventJson {
    ReplyEventJson {
        kind_id: id.to_owned(),
        kind_name: kind_name.map(str::to_owned),
        params: Some(params),
        payload_bytes: None,
    }
}

fn projected_ids(replies: &[ReplyEventJson]) -> Vec<&str> {
    replies.iter().map(|reply| reply.kind_id.as_str()).collect()
}

#[test]
fn reply_projection_defaults_to_terminal_and_handles_empty_stream() {
    assert_eq!(ReplyProjection::default(), ReplyProjection::Terminal);
    assert!(project_replies(Vec::new(), ReplyProjection::default()).is_empty());
}

#[test]
fn terminal_projection_keeps_last_successful_reply() {
    let replies = vec![
        projected_reply("first", Some("aether.test.ok"), serde_json::json!({"Ok": 1})),
        projected_reply("last", Some("aether.test.ok"), serde_json::json!({"Ok": 2})),
    ];

    assert_eq!(projected_ids(&project_replies(replies, ReplyProjection::Terminal)), ["last"]);
}

#[test]
fn terminal_projection_keeps_midstream_structured_error_and_last_reply() {
    let replies = vec![
        projected_reply("first", Some("aether.test.ok"), serde_json::json!({"Ok": 1})),
        projected_reply("error", Some("aether.test.result"), serde_json::json!({"Err": {"message": "expected"}})),
        projected_reply("last", Some("aether.test.ok"), serde_json::json!({"Ok": 2})),
    ];

    assert_eq!(projected_ids(&project_replies(replies, ReplyProjection::Terminal)), ["error", "last"]);
}

#[test]
fn terminal_projection_does_not_duplicate_last_error() {
    let replies = vec![
        projected_reply("first", Some("aether.test.ok"), serde_json::json!({"Ok": 1})),
        projected_reply("last-error", Some("aether.test.result"), serde_json::json!({"Err": "failed"})),
    ];

    assert_eq!(projected_ids(&project_replies(replies, ReplyProjection::Terminal)), ["last-error"]);
}

#[test]
fn none_projection_keeps_only_recognized_errors() {
    let replies = vec![
        projected_reply("success", Some("aether.test.ok"), serde_json::json!({"Ok": 1})),
        projected_reply("bare-error", Some("aether.test.result"), serde_json::json!("Err")),
        projected_reply("kind-error", Some("aether.test.transport_error"), serde_json::Value::Null),
    ];

    assert_eq!(projected_ids(&project_replies(replies, ReplyProjection::None)), ["bare-error", "kind-error"]);
}

#[test]
fn all_projection_preserves_every_reply_in_order() {
    let replies = vec![
        projected_reply("one", Some("aether.test.ok"), serde_json::json!(1)),
        projected_reply("two", Some("aether.test.result"), serde_json::json!("Err")),
        projected_reply("three", Some("aether.test.ok"), serde_json::json!(3)),
    ];

    assert_eq!(projected_ids(&project_replies(replies, ReplyProjection::All)), ["one", "two", "three"]);
}

#[test]
fn error_recognition_matches_exact_kind_segments_and_suffixes_case_insensitively() {
    for kind_name in ["aether.test.err", "aether.test.ERROR", "aether.test.decode_err", "aether.test.IO_ERROR"] {
        assert!(
            is_error_reply(&projected_reply("id", Some(kind_name), serde_json::Value::Null)),
            "{kind_name} should be recognized as an error kind"
        );
    }
}

#[test]
fn error_recognition_rejects_err_substring_false_positives() {
    for kind_name in ["aether.kit.terra.query_result", "aether.rpc.test.deferred_echo_reply"] {
        assert!(
            !is_error_reply(&projected_reply("id", Some(kind_name), serde_json::Value::Null)),
            "{kind_name} must not be recognized as an error kind"
        );
    }
}
