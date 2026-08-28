//! The output boundary's inline-versus-addressed decision.
//!
//! This is where the two context ceilings actually bind, and where the rule
//! that makes them safe lives: an oversized output is addressed *whole*, never
//! leaf-substituted. Substituting the oversized `Bytes` leaf would change that
//! property's declared type while leaving the surrounding result looking
//! inline, so the result would stop conforming to the `outputSchema` the tool
//! advertises on exactly the path a client is least likely to check.

use aether_data::{Schema, wire};
use serde::{Deserialize, Serialize};

use crate::kinds::ToolInvocationResult;
use crate::runtime::request::{OutputLimits, OutputProjection, project_output};
use crate::runtime::response_resources::{ResponseStore, ResponseStoreLimits};

/// A tool output with both a text leaf and a byte leaf, so the two ceilings can
/// be crossed independently.
#[derive(Schema, Serialize, Deserialize)]
struct Output {
    note: String,
    blob: Vec<u8>,
}

/// The generated one-field output value wrapper the provider actually sends.
#[derive(Schema, Serialize, Deserialize)]
struct OutputWrapper {
    output: Output,
}

const TOOL: &str = "check_thing";

fn limits() -> OutputLimits {
    OutputLimits {
        provider_wire_maximum_bytes: 1_048_576,
        maximum_output_values: 262_144,
        reply_inline_maximum_bytes: 16_384,
        response_inline_maximum_bytes: 32_768,
    }
}

fn store() -> ResponseStore {
    ResponseStore::new(ResponseStoreLimits {
        maximum_bytes: 1_048_576,
        total_bytes: 67_108_864,
        maximum_entries: 128,
        lifetime_secs: 600,
    })
}

fn provider_result(note: &str, blob_bytes: usize) -> ToolInvocationResult {
    let wrapper = OutputWrapper { output: Output { note: note.to_string(), blob: vec![7; blob_bytes] } };
    ToolInvocationResult::Ok { output_bytes: wire::to_vec(&wrapper).expect("the wrapper serializes") }
}

fn project(limits: OutputLimits, responses: &mut ResponseStore, result: &ToolInvocationResult) -> serde_json::Value {
    project_output(
        responses,
        0,
        &OutputProjection { tool_name: TOOL, output_wrapper_schema: &OutputWrapper::SCHEMA, limits },
        result,
    )
}

/// A small output rides inline, wrapped one level deeper than the declared
/// value so the boundary object's "exactly one of the two is non-null"
/// invariant holds even when the output itself serializes as null.
#[test]
fn a_small_output_is_inline_under_both_ceilings() {
    let projected = project(limits(), &mut store(), &provider_result("brief", 8));

    assert_eq!(projected["isError"], false);
    assert_eq!(projected["structuredContent"]["inline"]["output"]["note"], "brief");
    assert_eq!(projected["structuredContent"]["addressed"], serde_json::Value::Null);
    assert!(
        projected["content"].as_array().expect("a content array").iter().all(|block| block["type"] == "text"),
        "an inline result carries no resource link: {projected}",
    );
}

/// Crossing the whole-output ceiling addresses the output and leaves a resource
/// link behind. Nothing about the tool's declared value appears inline, because
/// half a result would be a result that does not conform.
#[test]
fn an_output_past_the_serialized_ceiling_is_addressed_whole() {
    let mut responses = store();
    let small_leaf_big_output = OutputLimits { response_inline_maximum_bytes: 64, ..limits() };

    let projected = project(small_leaf_big_output, &mut responses, &provider_result(&"n".repeat(256), 4));

    assert_eq!(projected["isError"], false);
    assert_eq!(projected["structuredContent"]["inline"], serde_json::Value::Null);
    let addressed = &projected["structuredContent"]["addressed"];
    assert!(addressed["uri"].as_str().expect("an address").starts_with("aether://mcp/response/"), "got {addressed}");
    assert_eq!(responses.len(), 1, "the addressed output must actually be stored");

    let link = projected["content"]
        .as_array()
        .expect("a content array")
        .iter()
        .find(|block| block["type"] == "resource_link")
        .expect("an addressed result carries a resource link");
    assert_eq!(link["uri"], addressed["uri"]);
    assert_eq!(link["mimeType"], "application/json");
    assert_eq!(link["name"], format!("{TOOL} output"));
    assert_eq!(link["size"], addressed["bytes"]);
}

/// A byte leaf past the *leaf* ceiling addresses the whole output even though
/// the serialized output is comfortably under the other ceiling. This is the
/// leaf-substitution rule as a test: nothing in the result is a swapped-out
/// leaf, the entire value moved to an address.
#[test]
fn a_wide_byte_leaf_addresses_the_whole_output_rather_than_the_leaf() {
    let mut responses = store();
    let narrow_leaf = OutputLimits { reply_inline_maximum_bytes: 16, ..limits() };

    let projected = project(narrow_leaf, &mut responses, &provider_result("brief", 64));

    assert_eq!(
        projected["structuredContent"]["inline"],
        serde_json::Value::Null,
        "the whole output moved: {projected}"
    );
    assert!(projected["structuredContent"]["addressed"]["uri"].is_string());

    // The stored bytes are the raw declared output, unchanged — the leaf is
    // still the array of integers the schema declares it to be.
    let uri = projected["structuredContent"]["addressed"]["uri"].as_str().expect("an address").to_string();
    let stored: serde_json::Value =
        serde_json::from_slice(responses.read(&uri, 0).expect("the address reads back")).expect("stored json");
    assert_eq!(stored["note"], "brief");
    assert_eq!(stored["blob"].as_array().expect("a byte array").len(), 64);
}

/// A store that cannot take the spill fails the call rather than falling back
/// to an oversized inline response. A fallback would make the ceilings advisory
/// at exactly the moment they matter.
#[test]
fn a_full_response_store_fails_the_call_rather_than_inlining() {
    let mut responses = ResponseStore::new(ResponseStoreLimits {
        maximum_bytes: 8,
        total_bytes: 8,
        maximum_entries: 1,
        lifetime_secs: 600,
    });
    let narrow_leaf = OutputLimits { reply_inline_maximum_bytes: 4, ..limits() };

    let projected = project(narrow_leaf, &mut responses, &provider_result("brief", 64));

    assert_eq!(projected["isError"], true, "got {projected}");
    assert!(projected.get("structuredContent").is_none(), "a failed call declares no conforming output");
    assert!(
        projected["content"][0]["text"].as_str().expect("a text block").contains("response_store_exhausted"),
        "got {projected}",
    );
}

/// A provider's own refusal is a *result*, not a protocol fault: the invocation
/// resolved against a registered descriptor and ran. A client that saw a
/// JSON-RPC error here would learn to retry a request that was never wrong.
#[test]
fn a_provider_refusal_is_a_successful_response_carrying_is_error() {
    let refusal = ToolInvocationResult::Err {
        category: "invalid_state".to_string(),
        message: "the commission is already sealed".to_string(),
    };

    let projected = project(limits(), &mut store(), &refusal);

    assert_eq!(projected["isError"], true);
    assert!(projected.get("structuredContent").is_none());
    assert!(projected["content"][0]["text"].as_str().expect("a text block").contains("already sealed"));
}

/// Bytes that do not decode against the registered contract are the provider's
/// failure to produce a valid declared output, which is what `isError` means —
/// and the call must not be answered with a value that violates its own
/// advertised schema.
#[test]
fn malformed_provider_bytes_become_a_tool_error() {
    let malformed = ToolInvocationResult::Ok { output_bytes: vec![0xff, 0xff, 0xff] };

    let projected = project(limits(), &mut store(), &malformed);

    assert_eq!(projected["isError"], true, "got {projected}");
    assert!(projected.get("structuredContent").is_none());
}

/// The provider wire ceiling is checked before the decode, so oversized bytes
/// are refused without being expanded into a value tree first.
#[test]
fn output_bytes_past_the_provider_ceiling_are_refused_before_decoding() {
    let tiny_ceiling = OutputLimits { provider_wire_maximum_bytes: 8, ..limits() };

    let projected = project(tiny_ceiling, &mut store(), &provider_result("brief", 256));

    assert_eq!(projected["isError"], true);
    assert!(
        projected["content"][0]["text"].as_str().expect("a text block").contains("output_too_large"),
        "got {projected}",
    );
}
