//! Tests for the resource URI grammar and the resource surface.
//!
//! The grammar is the whole of provider isolation: longest-prefix dispatch is
//! only safe if one resource has exactly one spelling. Every rejection here
//! closes an alternate spelling that would otherwise let a request reach a
//! provider that was not meant to answer it.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::json;

use crate::kinds::ResourceDescriptor;
use crate::protocol::remote_procedure_call::{INTERNAL_ERROR, INVALID_PARAMS, RESOURCE_NOT_FOUND};
use crate::protocol::resources::{
    RESPONSE_RESOURCE_PREFIX, descriptor_to_json, list_resources_result, normalize_provider_prefix,
    normalize_resource_uri, parse_read_resource, protocol_error_for_read_failure, read_resource_blob_result,
    read_resource_text_result,
};

fn descriptor(uri: &str, name: &str) -> ResourceDescriptor {
    ResourceDescriptor {
        uri: uri.to_string(),
        name: name.to_string(),
        title: None,
        description: None,
        mime_type: None,
        size_bytes: None,
    }
}

/// The addresses the grammar admits, spelled the one way it admits them.
#[test]
fn well_formed_addresses_normalize_to_themselves() {
    for uri in [
        "aether://mcp/response/7f4c9c3d6b9944bca0f6798bc5cfa092",
        "aether://bloomery/artifacts/abc123",
        "aether://bloomery/artifacts/abc123/chunks/0/524288",
        "aether://bloomery/artifacts/",
        "aether://host-1.local/a",
    ] {
        assert_eq!(normalize_resource_uri(uri).as_deref(), Ok(uri), "{uri} should normalize to itself");
    }
}

/// Every alternate spelling the grammar closes. Each of these would give one
/// resource a second address, and a second address is a second chance to
/// match a prefix that was not meant to match.
#[test]
fn alternate_spellings_are_refused() {
    for uri in [
        "http://bloomery/artifacts/x",
        "AETHER://bloomery/artifacts/x",
        "aether://BLOOMERY/artifacts/x",
        "aether://bloomery",
        "aether:///artifacts/x",
        "aether://bloomery/artifacts//x",
        "aether://bloomery/artifacts/./x",
        "aether://bloomery/artifacts/../secrets",
        "aether://bloomery/artifacts/x?full=1",
        "aether://bloomery/artifacts/x#frag",
        "aether://bloomery/artifacts/%2e%2e/x",
        "aether://user@bloomery/artifacts/x",
    ] {
        assert!(normalize_resource_uri(uri).is_err(), "{uri} should be refused");
    }
}

/// A provider prefix must end in `/`, so `.../artifacts/` cannot reach
/// `.../artifacts-private/x`. Without the rule, one provider's claim silently
/// covers a sibling namespace.
#[test]
fn a_provider_prefix_must_end_in_a_separator() {
    assert_eq!(
        normalize_provider_prefix("aether://bloomery/artifacts/").as_deref(),
        Ok("aether://bloomery/artifacts/")
    );
    assert!(normalize_provider_prefix("aether://bloomery/artifacts").is_err(), "a bare path is not a prefix");

    let claimed = normalize_provider_prefix("aether://bloomery/artifacts/").expect("a valid prefix");
    let sibling = normalize_resource_uri("aether://bloomery/artifacts-private/x").expect("a valid address");
    assert!(!sibling.starts_with(&claimed), "a prefix must not reach into a sibling namespace");
}

/// The reserved response-store prefix is itself a well-formed prefix, so the
/// server's own addresses go through the same grammar every provider does.
#[test]
fn the_reserved_response_prefix_is_well_formed() {
    assert_eq!(normalize_provider_prefix(RESPONSE_RESOURCE_PREFIX).as_deref(), Ok(RESPONSE_RESOURCE_PREFIX));
}

/// `resources/read` normalizes before anything matches. A read that reached
/// prefix dispatch with an unchecked raw string is the bug this closes.
#[test]
fn a_read_normalizes_its_address_or_refuses_it() {
    assert_eq!(
        parse_read_resource(Some(&json!({ "uri": "aether://bloomery/artifacts/abc" }))).as_deref(),
        Ok("aether://bloomery/artifacts/abc")
    );

    for params in [
        json!({ "uri": "aether://bloomery/artifacts/../x" }),
        json!({ "uri": 7 }),
        json!({}),
        json!({ "uri": "aether://bloomery/a", "extra": 1 }),
    ] {
        assert_eq!(parse_read_resource(Some(&params)).expect_err("refused").code, INVALID_PARAMS, "{params}");
    }
}

/// The wire kind names its fields in Aether's spelling and the protocol names
/// them in its own. This fails if the rename is dropped, which would make a
/// descriptor's size and media type invisible to every client.
#[test]
fn a_descriptor_renders_the_protocol_spellings() {
    let full = ResourceDescriptor {
        title: Some("Artifact".to_string()),
        description: Some("a stored artifact".to_string()),
        mime_type: Some("application/octet-stream".to_string()),
        size_bytes: Some(4_096),
        ..descriptor("aether://bloomery/artifacts/abc", "abc")
    };
    let rendered = descriptor_to_json(&full);

    assert_eq!(rendered["mimeType"], json!("application/octet-stream"));
    assert_eq!(rendered["size"], json!(4_096));
    assert!(rendered.get("mime_type").is_none(), "the Aether spelling must not leak to the wire");
    assert!(rendered.get("size_bytes").is_none());

    let bare = descriptor_to_json(&descriptor("aether://bloomery/artifacts/abc", "abc"));
    for absent in ["title", "description", "mimeType", "size"] {
        assert!(bare.get(absent).is_none(), "an absent {absent} is omitted rather than nulled");
    }
}

/// The discoverable catalog is ordered by name for the same reason the tool
/// catalog is: a client diffing two responses should see only real changes.
#[test]
fn the_resource_catalog_is_ordered_by_name() {
    let listed = list_resources_result(&[
        descriptor("aether://a/z", "zebra"),
        descriptor("aether://a/a", "alpha"),
        descriptor("aether://a/m", "middle"),
    ]);
    let names: Vec<&str> = listed["resources"]
        .as_array()
        .expect("resources is an array")
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect();

    assert_eq!(names, ["alpha", "middle", "zebra"]);
}

/// Binary content is base64-encoded here and nowhere else, so a provider
/// cannot hand over a differently-encoded string that decodes to garbage.
#[test]
fn blob_contents_are_encoded_at_the_boundary() {
    let bytes = [0_u8, 1, 250, 255];
    let rendered = read_resource_blob_result("aether://a/b", "application/octet-stream", &bytes);
    let encoded = rendered["contents"][0]["blob"].as_str().expect("a blob entry");

    assert_eq!(BASE64.decode(encoded).as_deref(), Ok(bytes.as_slice()));
    assert!(rendered["contents"][0].get("text").is_none(), "a blob entry carries no text");
}

/// A text entry carries text and no blob, and both forms echo the requested
/// address.
#[test]
fn a_text_entry_echoes_its_address() {
    let rendered = read_resource_text_result("aether://a/b", "text/plain", "hello");

    assert_eq!(rendered["contents"][0]["uri"], json!("aether://a/b"));
    assert_eq!(rendered["contents"][0]["text"], json!("hello"));
    assert!(rendered["contents"][0].get("blob").is_none());
}

/// A read stays a protocol request after dispatch, so a provider failure is a
/// JSON-RPC error rather than the `isError` convention that belongs to
/// `tools/call`. Only `not_found` gets the resource-specific code.
#[test]
fn provider_read_failures_map_by_category() {
    let missing = protocol_error_for_read_failure("aether://a/b", "not_found", "no such digest");
    assert_eq!(missing.code, RESOURCE_NOT_FOUND);

    for category in ["adapter", "forbidden", "internal"] {
        let failure = protocol_error_for_read_failure("aether://a/b", category, "detail");
        assert_eq!(failure.code, INTERNAL_ERROR, "{category} is the server's problem to describe");
    }
}
