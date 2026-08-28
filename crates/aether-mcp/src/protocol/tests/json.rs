//! Tests for the bounded JSON reader.
//!
//! Everything here is a behavior a general JSON reader does *not* have, which
//! is exactly why the reader exists. A test that merely showed valid JSON
//! parsing would be testing a parser rather than the refusals this one was
//! written for.

use serde_json::json;

use crate::protocol::json::{Document, JsonError, ParseLimits, parse};

fn read(source: &str) -> Result<Document, JsonError> {
    parse(source, ParseLimits::default())
}

/// A duplicate member must be refused wherever it appears, not only at the
/// envelope's top level. This fails if the check is applied to the root
/// object alone — the shape a naive implementation lands on, and the one that
/// would let a duplicated tool argument through.
#[test]
fn duplicate_members_are_refused_at_every_depth() {
    for source in [
        r#"{"method":"a","method":"b"}"#,
        r#"{"params":{"name":"a","name":"b"}}"#,
        r#"{"params":{"items":[{"x":1,"x":2}]}}"#,
    ] {
        assert!(
            matches!(read(source), Err(JsonError::Duplicate { .. })),
            "a duplicate should have been refused in {source}"
        );
    }
}

/// A repeated name in *different* objects is ordinary and must still parse.
/// This fails if duplicate detection is implemented with one document-wide
/// set of names, which would reject most real payloads.
#[test]
fn a_name_repeated_across_sibling_objects_is_fine() {
    let document = read(r#"{"a":{"name":1},"b":{"name":2}}"#).expect("distinct objects may share member names");

    assert_eq!(document.value, json!({ "a": { "name": 1 }, "b": { "name": 2 } }));
}

/// The ceilings must bind at the stated limit. A reader whose depth check is
/// off by one either refuses a legal body or admits one past the bound it
/// advertises.
#[test]
fn the_reader_ceilings_bind_exactly() {
    let limits = ParseLimits { maximum_depth: 3, maximum_values: 1_000 };

    assert!(parse("[[1]]", limits).is_ok(), "two levels is inside a limit of three");
    assert!(parse("[[[1]]]", limits).is_ok(), "three levels is exactly the limit");
    assert!(
        matches!(parse("[[[[1]]]]", limits), Err(JsonError::DepthExceeded { maximum: 3 })),
        "four levels is past the limit"
    );

    let values = ParseLimits { maximum_depth: 32, maximum_values: 4 };
    assert!(parse("[1,2,3]", values).is_ok(), "the array plus three elements is exactly four values");
    assert!(
        matches!(parse("[1,2,3,4]", values), Err(JsonError::ValuesExceeded { maximum: 4 })),
        "a fifth value is past the limit"
    );
}

/// A numeric identifier is echoed into the response as source text, so the
/// reader has to hand back the exact bytes. Every token here loses something
/// on a round trip through `f64` or through `serde_json`'s own rendering, so
/// this fails the moment the span is dropped and the number is re-rendered.
#[test]
fn root_member_spans_recover_the_exact_number_token() {
    for token in ["-0", "1e2", "1.50", "0.0", "12345678901234567890", "-1.5E+10"] {
        let source = format!(r#"{{"jsonrpc":"2.0","id":{token},"method":"ping"}}"#);
        let document = read(&source).expect("a valid envelope");

        assert_eq!(document.member_source(&source, "id"), Some(token), "the span should recover {token}");
    }
}

/// A span must survive a member whose value is a container, and must not be
/// confused by nested members read in between.
#[test]
fn root_member_spans_cover_container_values() {
    let source = r#"{"id":7,"params":{"a":{"b":[1,2]}},"method":"x"}"#;
    let document = read(source).expect("a valid envelope");

    assert_eq!(document.member_source(source, "params"), Some(r#"{"a":{"b":[1,2]}}"#));
    assert_eq!(document.member_source(source, "id"), Some("7"));
}

/// The JSON number grammar, enforced rather than inferred from a parse
/// attempt. Each of these is rejected by the specification and accepted by at
/// least one lenient reader, so leaning on `str::parse` alone would admit
/// them.
#[test]
fn malformed_numbers_are_refused() {
    for source in ["01", "1.", ".5", "1e", "1e+", "-", "+1", "1.2.3", "0x10"] {
        assert!(matches!(read(source), Err(JsonError::Malformed { .. })), "{source} is not a JSON number");
    }
}

/// A number no binary float can hold finitely is a parse failure rather than
/// a silent infinity, which is what an unchecked `parse::<f64>()` produces.
#[test]
fn an_unrepresentable_number_is_refused() {
    assert!(matches!(read("1e400"), Err(JsonError::Malformed { .. })));
}

/// Integer tokens must keep their exact value rather than round through a
/// float. This fails if every number is parsed as `f64`, which silently
/// corrupts identifiers and byte counts above 2^53.
#[test]
fn large_integers_keep_their_exact_value() {
    let document = read("9007199254740993").expect("a valid integer");

    assert_eq!(document.value.as_u64(), Some(9_007_199_254_740_993));
}

/// String escapes, including the surrogate pair join. A reader that emitted
/// the halves separately would produce a value that is not the string the
/// client sent.
#[test]
fn string_escapes_decode() {
    let document = read(r#""aé\n\t\\\/\"😀""#).expect("valid escapes");

    assert_eq!(document.value, json!("aé\n\t\\/\"😀"));
}

/// The malformed string cases: an unpaired surrogate, an unknown escape, a
/// raw control character, and an unterminated string.
#[test]
fn malformed_strings_are_refused() {
    for source in [r#""\ud83d""#, r#""\ud83dx""#, r#""\q""#, "\"a\nb\"", r#""unterminated"#] {
        assert!(matches!(read(source), Err(JsonError::Malformed { .. })), "{source} should be refused");
    }
}

/// Two documents in one body is not one document. Stopping at the first and
/// ignoring the rest would let a caller smuggle content past the boundary.
#[test]
fn trailing_content_is_refused() {
    assert!(matches!(read("{} {}"), Err(JsonError::Malformed { .. })));
    assert!(matches!(read("1 2"), Err(JsonError::Malformed { .. })));
}

/// Whitespace between every token, and the four bytes that count as it.
/// Anything else — a comment, a byte-order mark — is not JSON.
#[test]
fn only_the_four_whitespace_bytes_separate_tokens() {
    assert!(read(" \t\r\n{ \"a\" : [ 1 , 2 ] }\n").is_ok());
    assert!(matches!(read("{} // comment"), Err(JsonError::Malformed { .. })));
}
