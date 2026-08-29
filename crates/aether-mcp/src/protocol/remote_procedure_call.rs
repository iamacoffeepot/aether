//! The JSON-RPC 2.0 envelope, as revision 2025-06-18 constrains it.
//!
//! Revision 2025-06-18 removed batching, so a top-level array — empty
//! included — is refused rather than iterated. One request owns one response;
//! a notification owns none.

use serde_json::{Value, json};
use std::fmt::Write as _;

use super::json::{self, JsonError, ParseLimits};

/// The text was not JSON.
pub const PARSE_ERROR: i32 = -32700;
/// The document was JSON but was not a usable request envelope.
pub const INVALID_REQUEST: i32 = -32600;
/// The method is outside the supported surface.
pub const METHOD_NOT_FOUND: i32 = -32601;
/// The parameters do not match the method's contract.
pub const INVALID_PARAMS: i32 = -32602;
/// The server failed in a way the caller did not cause.
pub const INTERNAL_ERROR: i32 = -32603;
/// Admission refused the request; it never crossed into execution.
pub const SERVER_BUSY: i32 = -32000;
/// The resource-specific not-found code from the resource surface.
pub const RESOURCE_NOT_FOUND: i32 = -32002;

/// The protocol revision this server implements, and the only one it offers.
pub const PROTOCOL_REVISION: &str = "2025-06-18";

/// Bytes of diagnostic text an error message may carry.
///
/// A downstream failure can be arbitrarily verbose; an error object a model
/// reads should not be. The bound is applied where the message is built, so
/// no path can widen it later.
pub const ERROR_MESSAGE_MAXIMUM_BYTES: usize = 2_048;

/// Longest accepted string identifier, in UTF-8 bytes.
pub const IDENTIFIER_STRING_MAXIMUM_BYTES: usize = 256;
/// Longest accepted numeric identifier source token, in bytes.
pub const IDENTIFIER_NUMBER_MAXIMUM_BYTES: usize = 128;

/// A request identifier, kept in the form the client sent it.
///
/// A numeric identifier retains its **source token** rather than a parsed
/// number, and the response copies that text back. An exponent, a fraction, a
/// large integer, or a negative zero does not survive a round trip through a
/// binary float, and narrowing it to Aether's own `RequestId` — an actor-mail
/// correlation type with different semantics — would lose it outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageId {
    Text(String),
    /// The verbatim source token, already validated against the JSON number
    /// grammar.
    Number(String),
}

impl MessageId {
    /// Render the identifier exactly as it will appear in the response.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Text(text) => Value::String(text.clone()).to_string(),
            Self::Number(token) => token.clone(),
        }
    }
}

/// An error object, ready to become the `error` member of a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl ProtocolError {
    fn coded(code: i32, message: impl AsRef<str>) -> Self {
        Self { code, message: bounded_text(message.as_ref(), ERROR_MESSAGE_MAXIMUM_BYTES), data: None }
    }

    #[must_use]
    pub fn parse_error(detail: impl AsRef<str>) -> Self {
        Self::coded(PARSE_ERROR, detail)
    }

    #[must_use]
    pub fn invalid_request(detail: impl AsRef<str>) -> Self {
        Self::coded(INVALID_REQUEST, detail)
    }

    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::coded(METHOD_NOT_FOUND, format!("method `{method}` is not supported"))
    }

    #[must_use]
    pub fn invalid_params(detail: impl AsRef<str>) -> Self {
        Self::coded(INVALID_PARAMS, detail)
    }

    #[must_use]
    pub fn internal_error(detail: impl AsRef<str>) -> Self {
        Self::coded(INTERNAL_ERROR, detail)
    }

    #[must_use]
    pub fn resource_not_found(uri: &str) -> Self {
        Self::coded(RESOURCE_NOT_FOUND, format!("resource `{uri}` was not found"))
    }

    /// Admission refused the request, with the delay after which retrying is
    /// worthwhile.
    ///
    /// The retry hint is the only `data` this boundary volunteers. Internal
    /// paths, authorization material, and unbounded downstream diagnostics
    /// never appear there.
    #[must_use]
    pub fn server_busy(retry_after_millis: u64) -> Self {
        Self {
            data: Some(json!({ "retryAfterMillis": retry_after_millis })),
            ..Self::coded(SERVER_BUSY, "the server is at its admission limit")
        }
    }
}

/// A message that carries an identifier and expects exactly one response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub id: MessageId,
    pub method: String,
    /// Always an object when present; the envelope refuses any other shape.
    pub params: Option<Value>,
}

/// A message with no identifier, which is answered with an empty `202` and
/// never with a JSON body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub method: String,
    pub params: Option<Value>,
}

/// What arrived on the endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Request(Request),
    Notification(Notification),
    /// A response to a request this server never made.
    ///
    /// Legal at the transport layer and meaningless here — the stateless
    /// profile creates no server-to-client requests — so it is accepted,
    /// discarded, and answered with the same empty `202` a notification gets.
    StrayResponse,
}

/// Parse one incoming message.
///
/// Every refusal carries a null identifier. That is deliberate for the
/// ceiling and duplicate cases too: the parser does not trust an identifier
/// recovered from a document it deliberately stopped reading, or from one
/// whose members were ambiguous.
pub fn parse_incoming(body: &str, limits: ParseLimits) -> Result<Incoming, ProtocolError> {
    let document = json::parse(body, limits).map_err(|error| protocol_error_for_json(&error))?;

    let Value::Object(members) = &document.value else {
        return Err(ProtocolError::invalid_request(match &document.value {
            Value::Array(_) => "revision 2025-06-18 removed batching; send one message per request",
            _ => "a message must be a JSON object",
        }));
    };

    if members.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(ProtocolError::invalid_request("`jsonrpc` must be the string \"2.0\""));
    }

    let id = read_identifier(members.get("id"), &document, body)?;
    let Some(method) = members.get("method") else {
        // No method and an identifier present: a client answering a request
        // this server never sent.
        return match id {
            Some(_) if members.contains_key("result") || members.contains_key("error") => Ok(Incoming::StrayResponse),
            _ => Err(ProtocolError::invalid_request("a message must carry a `method`")),
        };
    };
    let Some(method) = method.as_str() else {
        return Err(ProtocolError::invalid_request("`method` must be a string"));
    };

    let params = read_params(members.get("params"))?;

    Ok(match id {
        Some(id) => Incoming::Request(Request { id, method: method.to_string(), params }),
        None => Incoming::Notification(Notification { method: method.to_string(), params }),
    })
}

/// Read the identifier, distinguishing "absent" (a notification) from
/// "present and unusable" (an invalid request).
///
/// An explicit `id: null` is the case worth naming: base JSON-RPC allows it,
/// this revision does not, and silently treating it as a notification would
/// leave the client waiting for a response that will never come.
fn read_identifier(
    id: Option<&Value>,
    document: &json::Document,
    body: &str,
) -> Result<Option<MessageId>, ProtocolError> {
    let Some(id) = id else {
        return Ok(None);
    };

    match id {
        Value::Null => Err(ProtocolError::invalid_request("an explicit null identifier is not valid")),
        Value::String(text) => {
            if text.len() > IDENTIFIER_STRING_MAXIMUM_BYTES {
                return Err(ProtocolError::invalid_request(format!(
                    "a string identifier may be at most {IDENTIFIER_STRING_MAXIMUM_BYTES} bytes"
                )));
            }
            Ok(Some(MessageId::Text(text.clone())))
        }
        Value::Number(number) => {
            let token = document.member_source(body, "id").map_or_else(|| number.to_string(), str::to_string);
            if token.len() > IDENTIFIER_NUMBER_MAXIMUM_BYTES {
                return Err(ProtocolError::invalid_request(format!(
                    "a numeric identifier may be at most {IDENTIFIER_NUMBER_MAXIMUM_BYTES} bytes"
                )));
            }
            Ok(Some(MessageId::Number(token)))
        }
        _ => Err(ProtocolError::invalid_request("an identifier must be a string or a number")),
    }
}

/// Parameters are an object or absent.
///
/// A present non-object is refused as invalid parameters rather than an
/// invalid envelope: the message routed fine, its argument shape did not.
fn read_params(params: Option<&Value>) -> Result<Option<Value>, ProtocolError> {
    match params {
        None => Ok(None),
        Some(Value::Object(_)) => Ok(params.cloned()),
        Some(_) => Err(ProtocolError::invalid_params("`params` must be an object when present")),
    }
}

/// How a reader refusal becomes a protocol refusal.
///
/// Only genuinely malformed text is a parse error. A ceiling crossing, a
/// duplicate member, or an over-deep body is legal JSON the boundary declined,
/// which is an invalid request.
#[must_use]
pub fn protocol_error_for_json(error: &JsonError) -> ProtocolError {
    match error {
        JsonError::Malformed { .. } => ProtocolError::parse_error(error.to_string()),
        _ => ProtocolError::invalid_request(error.to_string()),
    }
}

/// One JSON response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Success {
        id: MessageId,
        result: Value,
    },
    /// A failure. The identifier is absent when the envelope never yielded a
    /// usable one, and the response then carries `id: null`.
    Failure {
        id: Option<MessageId>,
        error: ProtocolError,
    },
}

impl Response {
    /// Serialize the response body.
    ///
    /// The envelope is assembled by hand rather than derived, because the
    /// identifier must be copied through as source text; a derived
    /// serialization would re-render a numeric identifier from a parsed
    /// number and lose its original spelling.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"jsonrpc\":\"2.0\",\"id\":");

        match self {
            Self::Success { id, result } => {
                out.push_str(&id.render());
                out.push_str(",\"result\":");
                out.push_str(&result.to_string());
            }
            Self::Failure { id, error } => {
                out.push_str(&id.as_ref().map_or_else(|| "null".to_string(), MessageId::render));
                let _ = write!(
                    out,
                    ",\"error\":{{\"code\":{},\"message\":{}",
                    error.code,
                    Value::String(error.message.clone())
                );
                if let Some(data) = &error.data {
                    let _ = write!(out, ",\"data\":{data}");
                }
                out.push('}');
            }
        }

        out.push('}');
        out
    }
}

/// The extension member every method tolerates.
///
/// Its contents are ignored, including a progress token — this profile has no
/// stream on which to deliver progress, so accepting the token and sending
/// nothing is the honest behavior.
pub const META_MEMBER: &str = "_meta";

/// Refuse a parameter object carrying a member the method does not declare.
///
/// Strictness here is what makes a typo loud instead of silently ignored: a
/// client that sends `curser` gets told, rather than getting default
/// behavior it did not ask for. `initialize` deliberately does not use this.
pub fn reject_unknown_members(params: Option<&Value>, declared: &[&str]) -> Result<(), ProtocolError> {
    let Some(members) = params.and_then(Value::as_object) else {
        return Ok(());
    };

    members
        .keys()
        .find(|name| name.as_str() != META_MEMBER && !declared.contains(&name.as_str()))
        .map_or(Ok(()), |unexpected| Err(ProtocolError::invalid_params(format!("unexpected parameter `{unexpected}`"))))
}

/// The parameter shape both list methods share.
///
/// The catalogs are bounded by registry limits and returned whole, so there
/// is no `nextCursor` to follow and no cursor to honor. A present `cursor` —
/// including `null` or an empty string — is refused rather than ignored,
/// because ignoring it would let a paginating client silently loop on the
/// first page forever.
pub fn parse_list_params(params: Option<&Value>) -> Result<(), ProtocolError> {
    reject_unknown_members(params, &["cursor"])?;

    match params.and_then(Value::as_object).and_then(|members| members.get("cursor")) {
        None => Ok(()),
        Some(_) => Err(ProtocolError::invalid_params("this server returns whole catalogs and accepts no `cursor`")),
    }
}

/// Read a required string parameter.
pub fn required_string(params: Option<&Value>, name: &str) -> Result<String, ProtocolError> {
    params
        .and_then(Value::as_object)
        .and_then(|members| members.get(name))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ProtocolError::invalid_params(format!("`{name}` must be a string")))
}

/// Truncate on a character boundary so a bounded diagnostic stays valid
/// UTF-8.
#[must_use]
pub fn bounded_text(text: &str, maximum_bytes: usize) -> String {
    if text.len() <= maximum_bytes {
        return text.to_string();
    }
    let mut end = maximum_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}
