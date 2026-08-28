//! `tools/list` and `tools/call`, and the line between a protocol error and
//! a tool execution error.
//!
//! That line is **successful resolution, validation, and admission of an
//! invocation against a named registered tool descriptor**. Before it,
//! failures are JSON-RPC errors. After it — live-target selection, actor
//! dispatch, and production of a conforming declared output — failures are a
//! *successful* JSON-RPC response carrying `isError: true`.
//!
//! The distinction matters to a caller: a JSON-RPC error says the request was
//! wrong and repeating it unchanged will fail again, while `isError: true`
//! says the request was well-formed and the operation did not succeed. A
//! server that blurs the two teaches clients to retry the wrong things.

use aether_codec::{DecodeError, EncodeError};
use serde_json::{Map, Value, json};

use crate::kinds::{AddressedOutput, ToolAnnotations};

use super::remote_procedure_call::{
    ERROR_MESSAGE_MAXIMUM_BYTES, ProtocolError, bounded_text, parse_list_params, reject_unknown_members,
    required_string,
};

/// The media type of an addressed tool output.
const ADDRESSED_OUTPUT_MEDIA_TYPE: &str = "application/json";

/// One entry of the frozen tool catalog, in the form `tools/list` renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDescriptor {
    pub name: String,
    pub title: Option<String>,
    pub description: String,
    /// The translated request-wrapper child schema, object-shaped.
    pub input_schema: Value,
    /// The translated boundary output schema, always object-shaped.
    pub output_schema: Value,
    pub annotations: ToolAnnotations,
}

impl ToolDescriptor {
    /// Render exactly the fields the protocol defines.
    ///
    /// Nothing is invented here. `SchemaType` carries no field
    /// documentation, so no property description is synthesized; a
    /// description a client reads must have been written by the tool author.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut rendered = Map::new();
        rendered.insert("name".to_string(), json!(self.name));
        if let Some(title) = &self.title {
            rendered.insert("title".to_string(), json!(title));
        }
        rendered.insert("description".to_string(), json!(self.description));
        rendered.insert("inputSchema".to_string(), self.input_schema.clone());
        rendered.insert("outputSchema".to_string(), self.output_schema.clone());
        rendered.insert(
            "annotations".to_string(),
            json!({
                "readOnlyHint": self.annotations.read_only,
                "destructiveHint": self.annotations.destructive,
                "idempotentHint": self.annotations.idempotent,
                "openWorldHint": self.annotations.open_world
            }),
        );
        Value::Object(rendered)
    }
}

/// Parse `tools/list`.
pub fn parse_list_tools(params: Option<&Value>) -> Result<(), ProtocolError> {
    parse_list_params(params)
}

/// The `tools/list` result: the whole catalog, ordered by name.
///
/// Lexical order is the contract, not an accident of registration order, so
/// two servers with the same registrations render the same catalog and a
/// client diffing two responses sees only real changes.
#[must_use]
pub fn list_tools_result(descriptors: &[ToolDescriptor]) -> Value {
    let mut sorted: Vec<&ToolDescriptor> = descriptors.iter().collect();
    sorted.sort_by(|left, right| left.name.cmp(&right.name));

    json!({ "tools": sorted.iter().map(|descriptor| descriptor.to_json()).collect::<Vec<Value>>() })
}

/// The parsed `tools/call` parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallToolParams {
    pub name: String,
    /// Always an object. An absent `arguments` member becomes the empty
    /// object, which is what a no-argument tool is called with.
    pub arguments: Map<String, Value>,
}

/// Parse `tools/call`.
pub fn parse_call_tool(params: Option<&Value>) -> Result<CallToolParams, ProtocolError> {
    reject_unknown_members(params, &["name", "arguments"])?;
    let name = required_string(params, "name")?;

    let arguments = match params.and_then(Value::as_object).and_then(|members| members.get("arguments")) {
        None => Map::new(),
        Some(Value::Object(arguments)) => arguments.clone(),
        Some(_) => return Err(ProtocolError::invalid_params("`arguments` must be an object when present")),
    };

    Ok(CallToolParams { name, arguments })
}

/// Wrap a call's arguments for the tool's hidden request kind.
///
/// The generated request wrapper has exactly one field, `input`. A unit input
/// accepts an absent or empty arguments object and wraps it as null; a
/// non-empty object for a unit input is a caller error, because the tool
/// declared that it takes nothing.
pub fn wrap_arguments(arguments: Map<String, Value>, unit_input: bool) -> Result<Value, ProtocolError> {
    if unit_input {
        if !arguments.is_empty() {
            return Err(ProtocolError::invalid_params("this tool takes no arguments"));
        }
        return Ok(json!({ "input": Value::Null }));
    }
    Ok(json!({ "input": Value::Object(arguments) }))
}

/// A successful `tools/call` result.
///
/// The boundary object goes into `structuredContent` because the tool
/// advertises an `outputSchema` and the protocol requires structured output
/// to conform whenever one is present. The same object is mirrored into a
/// text block for clients that read only text, and an addressed result adds a
/// resource link so the full value is one fetch away.
#[must_use]
pub fn call_tool_success(tool_name: &str, boundary: &Value, addressed: Option<&AddressedOutput>) -> Value {
    let mut content = vec![json!({ "type": "text", "text": boundary.to_string() })];

    if let Some(addressed) = addressed {
        content.push(json!({
            "type": "resource_link",
            "uri": addressed.uri,
            "name": format!("{tool_name} output"),
            "description": addressed.summary,
            "mimeType": ADDRESSED_OUTPUT_MEDIA_TYPE,
            "size": addressed.bytes
        }));
    }

    json!({ "isError": false, "structuredContent": boundary, "content": content })
}

/// A failed `tools/call` result.
///
/// `structuredContent` is omitted rather than nulled: no declared successful
/// output value exists, and emitting one that does not conform would violate
/// the advertised `outputSchema` on exactly the path a client is least likely
/// to check.
#[must_use]
pub fn call_tool_failure(category: &str, message: &str) -> Value {
    let text = bounded_text(&format!("{category}: {message}"), ERROR_MESSAGE_MAXIMUM_BYTES);

    json!({ "isError": true, "content": [{ "type": "text", "text": text }] })
}

/// How an input encoding failure becomes a protocol error.
///
/// Every shape failure is the caller's: they sent something the tool's
/// declared input does not describe. `UnsupportedSchema` is the exception and
/// is ours — registration applies the same admissibility checks the encoder
/// does, so a registered tool cannot normally raise it, and if it does the
/// server admitted a descriptor it cannot execute.
#[must_use]
pub fn protocol_error_for_encode(error: &EncodeError) -> ProtocolError {
    // Listed exhaustively rather than with a wildcard: a new encoder failure
    // is a boundary decision, and the compiler should ask for it rather than
    // let it default into whichever code happens to be the fallback.
    match error {
        EncodeError::NotAnObject
        | EncodeError::MissingField(_)
        | EncodeError::UnexpectedField(_)
        | EncodeError::TypeMismatch { .. }
        | EncodeError::OutOfRange { .. }
        | EncodeError::ArrayLengthMismatch { .. } => ProtocolError::invalid_params(error.to_string()),
        EncodeError::UnsupportedSchema(_) => {
            ProtocolError::internal_error("the server admitted a tool schema it cannot encode")
        }
    }
}

/// How an output decoding failure becomes tool error content.
///
/// Every one of these is past the protocol line: the invocation resolved,
/// validated, and ran, and the provider produced bytes that do not satisfy
/// the output contract. That is a failure to produce a valid declared output,
/// which is exactly what `isError: true` means.
#[must_use]
pub fn call_tool_failure_for_decode(error: &DecodeError) -> Value {
    // Exhaustive for the same reason as the encoder mapping: a new decode
    // failure must be classified deliberately, not absorbed by a wildcard.
    let category = match error {
        DecodeError::NonFiniteFloat { .. } => "non_finite_output",
        DecodeError::DuplicateMapKey { .. } => "ambiguous_output",
        DecodeError::ValueBudgetExceeded { .. } => "output_too_large",
        DecodeError::UnsupportedSchema(_) => "unsupported_output_schema",
        DecodeError::Truncated { .. }
        | DecodeError::TrailingBytes { .. }
        | DecodeError::InvalidBool { .. }
        | DecodeError::InvalidUtf8 { .. }
        | DecodeError::UnknownEnumDiscriminant { .. } => "malformed_output",
    };

    call_tool_failure(category, &error.to_string())
}
