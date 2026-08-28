//! Tests for the JSON-RPC envelope: what is a request, what is a
//! notification, what is neither, and how a refusal is rendered.

use aether_codec::{DecodeError, EncodeError};
use serde_json::{Value, json};

use crate::protocol::json::ParseLimits;
use crate::protocol::remote_procedure_call::{
    IDENTIFIER_STRING_MAXIMUM_BYTES, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, MessageId, PARSE_ERROR,
    ProtocolError, RESOURCE_NOT_FOUND, Response, SERVER_BUSY, bounded_text, parse_incoming,
};
use crate::protocol::tools::{call_tool_failure_for_decode, protocol_error_for_encode};
use crate::protocol::{Incoming, Request};

fn read(source: &str) -> Result<Incoming, ProtocolError> {
    parse_incoming(source, ParseLimits::default())
}

fn refusal_code(source: &str) -> i32 {
    read(source).expect_err("this message should be refused").code
}

fn request(source: &str) -> Request {
    match read(source).expect("a valid request") {
        Incoming::Request(request) => request,
        other => panic!("expected a request, got {other:?}"),
    }
}

/// An explicit `id: null` is legal base JSON-RPC and invalid under this
/// revision. Treating it as an absent identifier — the shape a naive
/// `Option<Value>` check produces — would answer it with a `202` and leave
/// the caller waiting for a response that never comes.
#[test]
fn an_explicit_null_identifier_is_an_invalid_request() {
    assert_eq!(refusal_code(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#), INVALID_REQUEST);
}

/// Revision 2025-06-18 removed batching. An empty array is included
/// deliberately: it is the case a "iterate the batch" implementation answers
/// with silence rather than an error.
#[test]
fn a_top_level_array_is_an_invalid_request() {
    for source in ["[]", r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#] {
        assert_eq!(refusal_code(source), INVALID_REQUEST, "{source} should be refused as a batch");
    }
}

/// The envelope's own required members.
#[test]
fn a_malformed_envelope_is_an_invalid_request() {
    for source in [
        r#"{"id":1,"method":"ping"}"#,
        r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
        r#"{"jsonrpc":2.0,"id":1,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":7}"#,
        r#"{"jsonrpc":"2.0","id":1}"#,
        r#"{"jsonrpc":"2.0","id":true,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":[1],"method":"ping"}"#,
        "\"a string\"",
    ] {
        assert_eq!(refusal_code(source), INVALID_REQUEST, "{source} should be refused");
    }
}

/// Text that is not JSON is the one parse error; everything above is legal
/// JSON the boundary declined. Collapsing the two would tell a caller their
/// syntax was wrong when their envelope was.
#[test]
fn only_unparseable_text_is_a_parse_error() {
    assert_eq!(refusal_code("{"), PARSE_ERROR);
    assert_eq!(refusal_code("not json"), PARSE_ERROR);
}

/// A duplicate member makes the envelope ambiguous, and the refusal must not
/// echo an identifier recovered from it. This fails if the parser reports the
/// duplicate but keeps the id it happened to read first.
#[test]
fn an_ambiguous_envelope_is_refused_with_a_null_identifier() {
    let error = read(r#"{"jsonrpc":"2.0","id":1,"id":2,"method":"ping"}"#).expect_err("ambiguous");
    let rendered = Response::Failure { id: None, error }.to_json();

    assert!(rendered.contains(r#""id":null"#), "{rendered}");
    assert!(rendered.contains(r#""code":-32600"#), "{rendered}");
}

/// A numeric identifier is copied into the response as source text. Each of
/// these tokens is changed by a round trip through a parsed number — `-0`
/// becomes `0`, `1.50` becomes `1.5`, `1e2` becomes `100.0` — so this fails
/// the moment the response re-renders instead of copying.
#[test]
fn numeric_identifiers_are_echoed_byte_for_byte() {
    for token in ["-0", "1.50", "1e2", "-1.5E+10", "12345678901234567890"] {
        let source = format!(r#"{{"jsonrpc":"2.0","id":{token},"method":"ping"}}"#);
        let request = request(&source);

        assert_eq!(request.id, MessageId::Number(token.to_string()));
        assert_eq!(
            Response::Success { id: request.id, result: json!({}) }.to_json(),
            format!(r#"{{"jsonrpc":"2.0","id":{token},"result":{{}}}}"#)
        );
    }
}

/// A string identifier is echoed as a JSON string, escapes and all.
#[test]
fn string_identifiers_round_trip_through_the_response() {
    let request = request(r#"{"jsonrpc":"2.0","id":"a\"b","method":"ping"}"#);

    assert_eq!(request.id, MessageId::Text("a\"b".to_string()));
    assert_eq!(
        Response::Success { id: request.id, result: json!(null) }.to_json(),
        r#"{"jsonrpc":"2.0","id":"a\"b","result":null}"#
    );
}

/// The identifier ceilings are what stop a caller from making the server
/// retain and echo an unbounded string.
#[test]
fn oversized_identifiers_are_refused() {
    let long = "x".repeat(IDENTIFIER_STRING_MAXIMUM_BYTES + 1);
    assert_eq!(refusal_code(&format!(r#"{{"jsonrpc":"2.0","id":"{long}","method":"ping"}}"#)), INVALID_REQUEST);

    let digits = "1".repeat(200);
    assert_eq!(refusal_code(&format!(r#"{{"jsonrpc":"2.0","id":{digits}e1,"method":"ping"}}"#)), INVALID_REQUEST);
}

/// An absent identifier is a notification, whatever the method is. A
/// request-only method arriving without one is still a notification under the
/// transport rule — the server must not execute it and must still answer
/// `202`.
#[test]
fn an_absent_identifier_makes_any_method_a_notification() {
    for method in ["notifications/initialized", "tools/call", "notifications/unheard_of"] {
        let source = format!(r#"{{"jsonrpc":"2.0","method":"{method}"}}"#);

        assert!(
            matches!(read(&source), Ok(Incoming::Notification(_))),
            "{method} without an identifier is a notification"
        );
    }
}

/// A client answering a request this server never made is legal at the
/// transport layer. It must be recognized rather than refused as a
/// method-less envelope, because the transport requires a `202` for it.
#[test]
fn a_client_response_is_recognized_and_discarded() {
    assert_eq!(read(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#), Ok(Incoming::StrayResponse));
    assert_eq!(read(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"x"}}"#), Ok(Incoming::StrayResponse));
}

/// Parameters are an object or absent. A present array or scalar is a
/// parameter problem, not an envelope problem, so it is `-32602`.
#[test]
fn non_object_parameters_are_invalid_params() {
    assert_eq!(refusal_code(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":[1]}"#), INVALID_PARAMS);
    assert_eq!(refusal_code(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":7}"#), INVALID_PARAMS);
    assert!(read(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).is_ok(), "absent params is fine");
}

/// The parser's own ceilings become invalid-request refusals rather than
/// parse errors, and they do not trust an identifier from a document they
/// stopped reading.
#[test]
fn parser_ceilings_become_invalid_requests() {
    let limits = ParseLimits { maximum_depth: 3, maximum_values: 8 };
    let deep = r#"{"jsonrpc":"2.0","id":1,"method":"x","params":{"a":{"b":{"c":1}}}}"#;

    assert_eq!(parse_incoming(deep, limits).expect_err("too deep").code, INVALID_REQUEST);
}

/// Admission refusal is the one error that volunteers data, and it carries
/// exactly the retry hint — never an internal path or a downstream
/// diagnostic.
#[test]
fn a_busy_refusal_carries_only_the_retry_hint() {
    let error = ProtocolError::server_busy(250);

    assert_eq!(error.code, SERVER_BUSY);
    assert_eq!(error.data, Some(json!({ "retryAfterMillis": 250 })));
    assert!(
        Response::Failure { id: Some(MessageId::Number("1".into())), error }
            .to_json()
            .contains(r#""data":{"retryAfterMillis":250}"#)
    );
}

/// Every encoder failure has a decided code. The mapping is exhaustive in
/// source, so a new `EncodeError` variant is a compile error rather than a
/// silent default; this pins which side of the line each existing one falls
/// on. `UnsupportedSchema` is ours because registration already applied the
/// encoder's admissibility checks.
#[test]
fn encode_failures_map_to_their_decided_codes() {
    let caller_faults = [
        EncodeError::NotAnObject,
        EncodeError::MissingField("a".into()),
        EncodeError::UnexpectedField("b".into()),
        EncodeError::TypeMismatch { field: "c".into(), expected: "string" },
        EncodeError::OutOfRange { field: "d".into(), reason: "too large".into() },
        EncodeError::ArrayLengthMismatch { field: "e".into(), expected: 2, got: 3 },
    ];

    for error in &caller_faults {
        assert_eq!(protocol_error_for_encode(error).code, INVALID_PARAMS, "{error:?} is the caller's mistake");
    }

    let ours = protocol_error_for_encode(&EncodeError::UnsupportedSchema("x"));
    assert_eq!(ours.code, INTERNAL_ERROR);
    assert!(!ours.message.contains('x'), "an internal diagnostic must not leak the codec's detail: {}", ours.message);
}

/// Every decode failure is past the protocol line, so all of them become a
/// successful response carrying `isError: true` — never a JSON-RPC error.
/// The categories separate the ones a caller can act on from the rest.
#[test]
fn decode_failures_become_tool_errors_with_a_category() {
    let expected = [
        (DecodeError::NonFiniteFloat { path: "$".into() }, "non_finite_output"),
        (DecodeError::DuplicateMapKey { path: "$".into() }, "ambiguous_output"),
        (DecodeError::ValueBudgetExceeded { path: "$".into(), budget: 4 }, "output_too_large"),
        (DecodeError::UnsupportedSchema("x"), "unsupported_output_schema"),
        (DecodeError::Truncated { path: "$".into(), needed: 4, had: 1 }, "malformed_output"),
        (DecodeError::TrailingBytes { path: "$".into(), remaining: 2 }, "malformed_output"),
        (DecodeError::InvalidBool { path: "$".into(), byte: 7 }, "malformed_output"),
        (DecodeError::InvalidUtf8 { path: "$".into() }, "malformed_output"),
        (DecodeError::UnknownEnumDiscriminant { path: "$".into(), discriminant: 9 }, "malformed_output"),
    ];

    for (error, category) in &expected {
        let result = call_tool_failure_for_decode(error);

        assert_eq!(result["isError"], json!(true), "{error:?} must stay a successful response");
        assert!(result.get("structuredContent").is_none(), "a failure declares no conforming output");
        assert!(
            result["content"][0]["text"].as_str().is_some_and(|text| text.starts_with(category)),
            "{error:?} should be categorized {category}, got {result}"
        );
    }
}

/// A resource that is not found has its own protocol code, and the message
/// names the address rather than anything about where it would have lived.
#[test]
fn a_missing_resource_uses_the_resource_specific_code() {
    let error = ProtocolError::resource_not_found("aether://mcp/response/abc");

    assert_eq!(error.code, RESOURCE_NOT_FOUND);
    assert!(error.message.contains("aether://mcp/response/abc"));
}

/// Diagnostics are bounded where they are built, and truncation lands on a
/// character boundary. Slicing bytes directly would panic on multi-byte text,
/// turning a verbose downstream error into a crash.
#[test]
fn bounded_diagnostics_truncate_on_a_character_boundary() {
    let text = "é".repeat(100);

    for limit in 0..16 {
        let bounded = bounded_text(&text, limit);

        assert!(bounded.len() <= limit, "the bound must hold at {limit}");
        assert!(text.starts_with(&bounded), "truncation must keep a prefix");
    }
}

/// An error message built from an unbounded downstream string is capped
/// before it reaches the response.
#[test]
fn error_messages_are_capped_at_construction() {
    let error = ProtocolError::invalid_params("x".repeat(10_000));

    assert!(error.message.len() <= 2_048, "an error message must be bounded, was {}", error.message.len());
}

/// The response envelope carries exactly one of `result` or `error`.
#[test]
fn a_response_carries_one_outcome() {
    let success = Response::Success { id: MessageId::Number("1".into()), result: Value::Bool(true) }.to_json();
    assert!(success.contains(r#""result":true"#) && !success.contains("error"), "{success}");

    let failure = Response::Failure { id: None, error: ProtocolError::method_not_found("prompts/list") }.to_json();
    assert!(failure.contains(r#""error""#) && !failure.contains(r#""result""#), "{failure}");
}
