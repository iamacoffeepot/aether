//! Tests for the method surface: what is served, what is refused, and
//! whether the advertised capabilities match either answer.

use serde_json::{Map, Value, json};

use crate::kinds::{AddressedOutput, ToolAnnotations};
use crate::protocol::PROTOCOL_REVISION;
use crate::protocol::lifecycle::{SERVER_INSTRUCTIONS, initialize_result, parse_initialize, parse_ping, ping_result};
use crate::protocol::remote_procedure_call::INVALID_PARAMS;
use crate::protocol::tools::{
    CallToolParams, ToolDescriptor, call_tool_failure, call_tool_success, list_tools_result, parse_call_tool,
    parse_list_tools, wrap_arguments,
};
use crate::protocol::{MethodSupport, RequestMethod, classify};

const SERVED: [RequestMethod; 6] = [
    RequestMethod::Initialize,
    RequestMethod::Ping,
    RequestMethod::ListTools,
    RequestMethod::CallTool,
    RequestMethod::ListResources,
    RequestMethod::ReadResource,
];

const EXCLUDED: [&str; 10] = [
    "resources/templates/list",
    "resources/subscribe",
    "resources/unsubscribe",
    "prompts/list",
    "prompts/get",
    "completion/complete",
    "roots/list",
    "sampling/createMessage",
    "elicitation/create",
    "logging/setLevel",
];

fn descriptor(name: &str) -> ToolDescriptor {
    ToolDescriptor {
        name: name.to_string(),
        title: None,
        description: "a tool".to_string(),
        input_schema: json!({ "type": "object" }),
        output_schema: json!({ "type": "object" }),
        annotations: ToolAnnotations::default(),
    }
}

/// `RequestMethod::name` and `classify` are two independent tables over the
/// same six names. This fails if either drifts — a renamed method that
/// classify no longer recognizes would become `-32601` while the rest of the
/// server still believes it is served.
#[test]
fn the_served_method_tables_agree() {
    for method in SERVED {
        assert_eq!(classify(method.name()), MethodSupport::Served(method), "{} did not round-trip", method.name());
    }
}

/// Every excluded family is refused, and refused *deliberately* — the
/// classification distinguishes "considered and declined" from "never heard
/// of", so a reviewer can see the decision rather than infer it from a
/// fallthrough.
#[test]
fn every_excluded_method_is_refused_with_a_recorded_reason() {
    for method in EXCLUDED {
        match classify(method) {
            MethodSupport::Excluded { reason } => {
                assert!(!reason.is_empty(), "{method} is excluded without a reason");
            }
            other => panic!("{method} should be a recorded exclusion, got {other:?}"),
        }
        assert!(classify(method).is_method_not_found(), "{method} must answer method-not-found");
    }

    assert_eq!(classify("tools/invent"), MethodSupport::Unknown);
    assert!(classify("tools/invent").is_method_not_found());
}

/// The advertised capabilities and the served methods must describe the same
/// server. This fails in either direction: a family advertised but answering
/// method-not-found, or a family served without being advertised.
#[test]
fn the_advertised_capabilities_match_the_served_methods() {
    let result = initialize_result();
    let capabilities = result["capabilities"].as_object().expect("capabilities is an object");

    for family in ["tools", "resources"] {
        assert!(capabilities.contains_key(family), "{family} is served and must be advertised");
        assert!(
            SERVED.iter().any(|method| method.name().starts_with(&format!("{family}/"))),
            "{family} is advertised and must have a served method"
        );
    }

    for family in ["prompts", "logging", "completions"] {
        assert!(!capabilities.contains_key(family), "{family} is not served and must not be advertised");
    }

    // `listChanged: false` is the truthful value: there is no stream on which
    // a change notification could be delivered.
    assert_eq!(result["capabilities"]["tools"]["listChanged"], json!(false));
    assert_eq!(result["protocolVersion"], json!(PROTOCOL_REVISION));
    assert!(result["capabilities"]["resources"].as_object().is_some_and(Map::is_empty), "no subscribe, no listChanged");
}

/// The instructions have to be useful within the window one client
/// guarantees to read. This fails if they grow into a manual.
#[test]
fn the_instructions_fit_the_guaranteed_window() {
    assert!(SERVER_INSTRUCTIONS.chars().count() <= 512, "instructions must be self-contained in 512 characters");
    assert!(!SERVER_INSTRUCTIONS.is_empty());
}

/// `initialize` is the deliberate exception to strict parameter checking: a
/// client proposing a newer revision necessarily sends members from it, and
/// refusing them would make negotiation impossible before it started.
#[test]
fn initialize_tolerates_a_newer_revision_and_its_additive_members() {
    let params = json!({
        "protocolVersion": "2099-01-01",
        "capabilities": { "roots": { "listChanged": true }, "somethingNew": {} },
        "clientInfo": { "name": "client", "version": "9.9", "title": "Client", "futureField": 1 },
        "futureMember": [1, 2, 3]
    });

    let parsed = parse_initialize(Some(&params)).expect("additive members must be tolerated");

    assert_eq!(parsed.protocol_version, "2099-01-01");
    assert_eq!(parsed.client.title.as_deref(), Some("Client"));
}

/// The fields `initialize` does consume are still required and still typed.
#[test]
fn initialize_requires_the_fields_it_reads() {
    for params in [
        json!({ "capabilities": {}, "clientInfo": { "name": "c", "version": "1" } }),
        json!({ "protocolVersion": 1, "capabilities": {}, "clientInfo": { "name": "c", "version": "1" } }),
        json!({ "protocolVersion": "x", "clientInfo": { "name": "c", "version": "1" } }),
        json!({ "protocolVersion": "x", "capabilities": [], "clientInfo": { "name": "c", "version": "1" } }),
        json!({ "protocolVersion": "x", "capabilities": {} }),
        json!({ "protocolVersion": "x", "capabilities": {}, "clientInfo": { "name": "c" } }),
    ] {
        assert_eq!(
            parse_initialize(Some(&params)).expect_err("incomplete initialize").code,
            INVALID_PARAMS,
            "{params} should be refused"
        );
    }
}

/// There is no pagination, so a present cursor is refused rather than
/// ignored. Ignoring it would let a paginating client loop on the first page
/// forever, which is worse than a clear error.
#[test]
fn a_present_cursor_is_refused_in_every_spelling() {
    for cursor in [json!(null), json!(""), json!("abc"), json!(0)] {
        let params = json!({ "cursor": cursor });

        assert_eq!(
            parse_list_tools(Some(&params)).expect_err("a cursor is never accepted").code,
            INVALID_PARAMS,
            "cursor {cursor} should be refused"
        );
    }

    assert!(parse_list_tools(None).is_ok(), "an absent cursor is the normal call");
    assert!(parse_list_tools(Some(&json!({}))).is_ok(), "an empty parameter object is fine");
}

/// Unknown members are refused so a typo is loud, while `_meta` is the one
/// extension point every method tolerates.
#[test]
fn unknown_parameters_are_refused_but_meta_is_tolerated() {
    assert_eq!(parse_list_tools(Some(&json!({ "curser": "x" }))).expect_err("typo").code, INVALID_PARAMS);
    assert!(parse_list_tools(Some(&json!({ "_meta": { "progressToken": 1 } }))).is_ok(), "_meta is always allowed");
}

/// The catalog is ordered by name, not by registration order, so two servers
/// with the same tools render the same list and a client diffing responses
/// sees only real changes.
#[test]
fn the_tool_catalog_is_ordered_by_name() {
    let listed = list_tools_result(&[descriptor("zebra"), descriptor("alpha"), descriptor("middle")]);
    let names: Vec<&str> = listed["tools"]
        .as_array()
        .expect("tools is an array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();

    assert_eq!(names, ["alpha", "middle", "zebra"]);
}

/// A descriptor renders all four hints, always, so a client never has to
/// guess a default — and the protocol's conservative defaults are what an
/// undeclared tool reports.
#[test]
fn a_descriptor_always_renders_all_four_hints() {
    let annotations = descriptor("t").to_json()["annotations"].clone();

    assert_eq!(
        annotations,
        json!({ "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true })
    );
}

/// An absent `title` is omitted rather than nulled, and a present one
/// appears. The protocol distinguishes the two.
#[test]
fn an_absent_descriptor_title_is_omitted() {
    assert!(descriptor("t").to_json().get("title").is_none());

    let titled = ToolDescriptor { title: Some("Titled".into()), ..descriptor("t") };
    assert_eq!(titled.to_json()["title"], json!("Titled"));
}

/// Absent `arguments` is the same call as an empty object — a no-argument
/// tool must be callable without sending one.
#[test]
fn absent_arguments_become_the_empty_object() {
    assert_eq!(
        parse_call_tool(Some(&json!({ "name": "t" }))).expect("a valid call"),
        CallToolParams { name: "t".to_string(), arguments: Map::new() }
    );
    assert_eq!(
        parse_call_tool(Some(&json!({ "name": "t", "arguments": {} }))).expect("a valid call").arguments,
        Map::new()
    );
}

/// A non-object `arguments`, a missing name, and an unknown member are all
/// caller errors.
#[test]
fn a_malformed_call_is_invalid_params() {
    for params in [
        json!({ "name": "t", "arguments": [1] }),
        json!({ "name": "t", "arguments": "x" }),
        json!({ "arguments": {} }),
        json!({ "name": 7 }),
        json!({ "name": "t", "extra": 1 }),
    ] {
        assert_eq!(parse_call_tool(Some(&params)).expect_err("malformed call").code, INVALID_PARAMS, "{params}");
    }
}

/// A unit-input tool takes nothing, and the wrapper says so as null rather
/// than as an empty object — the shape the generated `{ input }` wrapper
/// decodes. Sending arguments to it is a caller error, not a silent ignore.
#[test]
fn unit_input_wraps_as_null_and_refuses_arguments() {
    assert_eq!(wrap_arguments(Map::new(), true).expect("no arguments"), json!({ "input": null }));

    let mut arguments = Map::new();
    arguments.insert("a".to_string(), json!(1));
    assert_eq!(wrap_arguments(arguments.clone(), true).expect_err("this tool takes nothing").code, INVALID_PARAMS);

    assert_eq!(wrap_arguments(arguments, false).expect("a struct input"), json!({ "input": { "a": 1 } }));
}

/// A success carries the boundary object as structured content, mirrors it as
/// text, and adds a resource link only when the result was addressed. The
/// link is what makes an addressed result fetchable; adding one to an inline
/// result would point at nothing.
#[test]
fn a_successful_call_mirrors_its_boundary_and_links_only_when_addressed() {
    let inline = json!({ "inline": { "output": { "count": 1 } }, "addressed": null });
    let rendered = call_tool_success("my_tool", &inline, None);

    assert_eq!(rendered["isError"], json!(false));
    assert_eq!(rendered["structuredContent"], inline);
    assert_eq!(rendered["content"][0]["text"], json!(inline.to_string()));
    assert_eq!(rendered["content"].as_array().map(Vec::len), Some(1), "an inline result links to nothing");

    let addressed = AddressedOutput {
        uri: "aether://mcp/response/7f4c".to_string(),
        bytes: 48_123,
        summary: "object with 14 keys".to_string(),
    };
    let boundary =
        json!({ "inline": null, "addressed": { "uri": addressed.uri, "bytes": 48_123, "summary": addressed.summary } });
    let linked = call_tool_success("my_tool", &boundary, Some(&addressed));
    let link = &linked["content"][1];

    assert_eq!(link["type"], json!("resource_link"));
    assert_eq!(link["uri"], json!("aether://mcp/response/7f4c"));
    assert_eq!(link["name"], json!("my_tool output"));
    assert_eq!(link["mimeType"], json!("application/json"));
    assert_eq!(link["size"], json!(48_123));
    assert!(link.get("path").is_none(), "a link carries no host-derived metadata");
}

/// A failure omits `structuredContent` entirely. Emitting a null there would
/// violate the advertised `outputSchema` on the path a client is least likely
/// to check.
#[test]
fn a_failed_call_declares_no_structured_output() {
    let rendered = call_tool_failure("invalid_state", "the bloom is held");

    assert_eq!(rendered["isError"], json!(true));
    assert!(rendered.get("structuredContent").is_none());
    assert_eq!(rendered["content"][0]["text"], json!("invalid_state: the bloom is held"));
}

/// Tool error content is bounded, so a verbose provider cannot fill a model's
/// context through the error path.
#[test]
fn tool_error_content_is_bounded() {
    let rendered = call_tool_failure("adapter", &"detail ".repeat(2_000));
    let text = rendered["content"][0]["text"].as_str().expect("text content");

    assert!(text.len() <= 2_048, "tool error text must be bounded, was {}", text.len());
}

/// Ping is a liveness probe with no payload and no side effect.
#[test]
fn ping_returns_an_empty_object() {
    assert_eq!(ping_result(), json!({}));
    assert!(parse_ping(None).is_ok());
    assert_eq!(parse_ping(Some(&json!({ "unexpected": 1 }))).expect_err("strict").code, INVALID_PARAMS);
}

/// The initialize result never mints a session identifier, at any depth. A
/// session header would take this endpoint out of the stateless profile the
/// whole design rests on.
#[test]
fn the_initialize_result_mints_no_session() {
    let rendered = Value::to_string(&initialize_result());

    assert!(!rendered.to_lowercase().contains("session"), "{rendered}");
}
