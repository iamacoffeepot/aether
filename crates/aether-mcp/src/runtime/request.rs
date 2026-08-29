//! One POST, from the transport edge to either a response or a deferral.
//!
//! The file is ordered the way a request travels: admit, parse, negotiate,
//! meter, dispatch, and — for the two methods that reach a provider — hold the
//! obligation and project the reply. Every refusal names which side of the
//! protocol/tool line it fell on, because that is the one distinction a caller
//! acts on: a JSON-RPC error says repeating the request unchanged will fail
//! again, `isError: true` says the request was fine and the operation was not.

use std::str::from_utf8;
use std::sync::Arc;

use aether_actor::Manual;
use aether_codec::{decode_schema_strict, encode_schema};
use aether_data::{EnumVariant, Kind, KindId, MailboxId, SchemaType};
use aether_http::kinds::HttpServerResponse;
use aether_http::{Ctx, Outcome};
use aether_kinds::trace::Settled;
use aether_substrate::actor::native::NativeCtx;
use aether_substrate::mail::registry::MailboxEntry;
use serde_json::{Value, json};

use crate::kinds::{AddressedOutput, ReadResource, ReadResourceResult, ToolInvocationResult};
use crate::protocol::remote_procedure_call::{Incoming, ProtocolError, Response, parse_incoming};
use crate::protocol::resources::{
    RESPONSE_RESOURCE_PREFIX, parse_list_resources, parse_read_resource, protocol_error_for_read_failure,
    read_resource_blob_result, read_resource_text_result,
};
use crate::protocol::tools::{
    call_tool_failure, call_tool_failure_for_decode, call_tool_success, parse_call_tool, parse_list_tools,
    protocol_error_for_encode, wrap_arguments,
};
use crate::protocol::{MessageId, MethodSupport, Request, RequestMethod, classify, lifecycle};
use crate::schema::{SchemaBudget, validate_client_value};

use super::registry::ToolUnavailable;
use super::response_resources::{ResponseStore, summarize};
use super::state::{McpServerState, PendingCall, PendingOperation};
use super::transport;
use crate::McpServerConfiguration;

/// Nodes the output byte-leaf scan will visit before giving up.
///
/// The scan runs over a value the strict decoder already bounded, so this is a
/// second bound rather than the only one; it exists so the walk cannot be made
/// to run long by a shape the decoder counted differently than this walk does.
const OUTPUT_SCAN_MAXIMUM_NODES: usize = 262_144;

/// The media type an addressed output is stored and served under.
const ADDRESSED_OUTPUT_MEDIA_TYPE: &str = "application/json";

/// Serve one `POST /mcp`.
pub fn serve(state: &mut McpServerState, mut ctx: Ctx<'_, NativeCtx<'_, Manual>>) -> Outcome {
    let request = ctx.request().clone();

    let admitted = match transport::admit(&request, &state.config) {
        Ok(admitted) => admitted,
        Err(refusal) => return Outcome::Reply(refusal.to_response()),
    };

    let Ok(body) = from_utf8(admitted.body) else {
        return reply(
            state,
            &Response::Failure { id: None, error: ProtocolError::parse_error("the request body is not valid UTF-8") },
        );
    };

    let incoming = match parse_incoming(body, state.parse_limits()) {
        Ok(incoming) => incoming,
        Err(error) => return reply(state, &Response::Failure { id: None, error }),
    };

    match incoming {
        // A response to a request this server never made is legal at the
        // transport layer and meaningless here, so it is accepted and dropped
        // rather than answered — answering would assert a correlation that does
        // not exist.
        Incoming::StrayResponse => Outcome::Reply(transport::accepted_notification()),
        Incoming::Notification(notification) => {
            serve_notification(state, admitted.protocol_version.as_deref(), &notification.method)
        }
        Incoming::Request(protocol_request) => {
            let is_initialize = protocol_request.method == RequestMethod::Initialize.name();
            if let Err(refusal) = transport::check_protocol_version(admitted.protocol_version.as_deref(), is_initialize)
            {
                return Outcome::Reply(refusal.to_response());
            }
            if let Err(retry_after_millis) = state.rate.admit(state.now_millis()) {
                return reply(
                    state,
                    &Response::Failure {
                        id: Some(protocol_request.id.clone()),
                        error: ProtocolError::server_busy(retry_after_millis),
                    },
                );
            }
            dispatch(state, &mut ctx, &protocol_request)
        }
    }
}

/// A syntactically valid notification always ends in an empty `202`, unless
/// admission itself refuses it — which has no JSON-RPC identifier to answer
/// under, so it is the one notification case the transport answers.
fn serve_notification(state: &mut McpServerState, declared_version: Option<&str>, method: &str) -> Outcome {
    if let Err(refusal) = transport::check_protocol_version(declared_version, false) {
        return Outcome::Reply(refusal.to_response());
    }
    if state.rate.admit(state.now_millis()).is_err() {
        return Outcome::Reply(HttpServerResponse { status: 429, headers: Vec::new(), body: Vec::new() });
    }
    tracing::debug!(target: "aether_mcp::server", method, "notification accepted");
    Outcome::Reply(transport::accepted_notification())
}

/// Route one request to its method.
fn dispatch(state: &mut McpServerState, ctx: &mut Ctx<'_, NativeCtx<'_, Manual>>, request: &Request) -> Outcome {
    let id = request.id.clone();
    let params = request.params.as_ref();

    let method = match classify(&request.method) {
        MethodSupport::Served(method) => method,
        // Both refusals answer the same way; the distinction between a
        // deliberately excluded method and an unknown one is for diagnostics,
        // not for the caller.
        MethodSupport::Excluded { reason } => {
            tracing::debug!(target: "aether_mcp::server", method = %request.method, reason, "excluded method refused");
            return failure(state, id, ProtocolError::method_not_found(&request.method));
        }
        MethodSupport::Unknown => return failure(state, id, ProtocolError::method_not_found(&request.method)),
    };

    let immediate = match method {
        RequestMethod::Initialize => lifecycle::parse_initialize(params).map(|_| lifecycle::initialize_result()),
        RequestMethod::Ping => lifecycle::parse_ping(params).map(|()| lifecycle::ping_result()),
        RequestMethod::ListTools => parse_list_tools(params).map(|()| state.tools.freeze_and_list()),
        RequestMethod::ListResources => parse_list_resources(params).map(|()| state.resources.freeze_and_list()),
        RequestMethod::CallTool => return call_tool(state, ctx, id, params),
        RequestMethod::ReadResource => return read_resource(state, ctx, id, params),
    };

    match immediate {
        Ok(result) => reply(state, &Response::Success { id, result }),
        Err(error) => failure(state, id, error),
    }
}

/// `tools/call`: resolve, validate, admit, dispatch.
///
/// The order is the error boundary made executable. Name resolution and
/// argument validation happen first and answer with JSON-RPC errors; from the
/// moment a live target is selected, every failure is a successful response
/// carrying `isError: true`.
fn call_tool(
    state: &mut McpServerState,
    ctx: &mut Ctx<'_, NativeCtx<'_, Manual>>,
    id: MessageId,
    params: Option<&Value>,
) -> Outcome {
    let call = match parse_call_tool(params) {
        Ok(call) => call,
        Err(error) => return failure(state, id, error),
    };

    // The routing registry is cloned out first so the liveness predicate owns
    // its own handle rather than borrowing the state the catalog lookup needs
    // mutably.
    let live = live_predicate(state);
    let dispatch = match state.tools.dispatch(&call.name, &live) {
        Ok(dispatch) => dispatch,
        Err(ToolUnavailable::Unknown) => {
            return failure(state, id, ProtocolError::invalid_params(format!("no tool named `{}`", call.name)));
        }
        // The name resolved against a frozen descriptor, so this is past the
        // protocol line: the request was well formed and the operation cannot
        // run right now.
        Err(ToolUnavailable::NoLiveTarget) => {
            return reply(
                state,
                &Response::Success {
                    id,
                    result: call_tool_failure("unavailable", &format!("tool `{}` has no live provider", call.name)),
                },
            );
        }
    };

    let arguments = match wrap_arguments(call.arguments, dispatch.unit_input) {
        Ok(arguments) => arguments,
        Err(error) => return failure(state, id, error),
    };
    if let Err(error) = validate_client_value(&arguments, &dispatch.request_wrapper_schema, SchemaBudget::default()) {
        return failure(state, id, ProtocolError::invalid_params(error.to_string()));
    }
    let bytes = match encode_schema(&arguments, &dispatch.request_wrapper_schema) {
        Ok(bytes) => bytes,
        Err(error) => return failure(state, id, protocol_error_for_encode(&error)),
    };
    if bytes.len() > state.config.provider_wire_maximum_bytes {
        return failure(
            state,
            id,
            ProtocolError::invalid_params(format!(
                "the encoded arguments are {} bytes, past the {}-byte provider ceiling",
                bytes.len(),
                state.config.provider_wire_maximum_bytes,
            )),
        );
    }

    // The permit is taken *before* the obligation is retained, so a refusal
    // still has an inbound to answer through and gives back nothing.
    if !state.in_flight.acquire() {
        return reply(
            state,
            &Response::Failure { id: Some(id), error: ProtocolError::server_busy(state.config.tool_timeout_millis) },
        );
    }

    defer(
        state,
        ctx,
        id,
        dispatch.target,
        dispatch.request_kind,
        &bytes,
        PendingOperation::Tool { name: call.name, output_wrapper_schema: dispatch.output_wrapper_schema },
    )
}

/// `resources/read`: the server's own store first, then a provider prefix.
fn read_resource(
    state: &mut McpServerState,
    ctx: &mut Ctx<'_, NativeCtx<'_, Manual>>,
    id: MessageId,
    params: Option<&Value>,
) -> Outcome {
    let uri = match parse_read_resource(params) {
        Ok(uri) => uri,
        Err(error) => return failure(state, id, error),
    };

    if uri.starts_with(RESPONSE_RESOURCE_PREFIX) {
        let now = state.now_millis();
        // A base64 blob rather than a text entry: the stored bytes are already
        // JSON, and a text entry would escape them a second time inside the
        // response's own JSON string.
        return match state.responses.read(&uri, now) {
            Some(bytes) => {
                let result = read_resource_blob_result(&uri, ADDRESSED_OUTPUT_MEDIA_TYPE, bytes);
                reply(state, &Response::Success { id, result })
            }
            None => failure(state, id, ProtocolError::resource_not_found(&uri)),
        };
    }

    let Some(target) = state.resources.resolve(&uri) else {
        return failure(state, id, ProtocolError::resource_not_found(&uri));
    };
    if !state.in_flight.acquire() {
        return reply(
            state,
            &Response::Failure { id: Some(id), error: ProtocolError::server_busy(state.config.tool_timeout_millis) },
        );
    }

    let request = ReadResource { uri: uri.clone() };
    defer(
        state,
        ctx,
        id,
        target,
        <ReadResource as Kind>::ID,
        &request.encode_into_bytes(),
        PendingOperation::Resource { uri },
    )
}

/// Retain this request's obligation, dispatch to the provider, and arm its
/// deadline.
///
/// The dispatch is a *detached* root rather than an inherited send, so the
/// capability can tell a provider's reply from its chain settling without one.
/// The retained obligation is what keeps the original HTTP request open
/// meanwhile.
fn defer(
    state: &mut McpServerState,
    ctx: &mut Ctx<'_, NativeCtx<'_, Manual>>,
    id: MessageId,
    target: MailboxId,
    kind: KindId,
    bytes: &[u8],
    operation: PendingOperation,
) -> Outcome {
    let inbound = ctx.take_inbound();
    let mail_id = ctx.send_envelope_detached(target, kind, bytes);

    if let Some(settlement) = state.mailer.settlement_registry() {
        settlement.subscribe_settlement_mail(
            mail_id,
            state.self_mailbox,
            <Settled as Kind>::ID,
            Arc::clone(&state.mailer),
        );
    }

    let generation = state.next_generation();
    state.pending.insert(mail_id.correlation_id, PendingCall { inbound, id, operation, generation });

    if let Some(timer) = &state.timer {
        timer.arm(
            mail_id.correlation_id,
            generation,
            state.now_millis().saturating_add(state.config.tool_timeout_millis),
        );
    }

    Outcome::Deferred
}

/// Complete a pending operation with an already-built JSON-RPC response.
///
/// Releasing the permit here rather than at each call site is what makes the
/// four completion paths — reply, settlement, deadline, and refusal — provably
/// symmetric: every one of them removes its entry through this function.
pub fn complete(state: &mut McpServerState, correlation_id: u64, result: impl FnOnce(&PendingCall) -> Response) {
    let Some(pending) = state.pending.remove(&correlation_id) else {
        // A reply with no live correlation is ordinary after a timeout or a
        // completed call; it is a diagnostic, not an error.
        tracing::debug!(target: "aether_mcp::server", correlation_id, "reply for no pending operation dropped");
        return;
    };
    state.in_flight.release();

    let response = result(&pending);
    pending.inbound.reply(&render(state, &response));
}

/// Project a provider's `ToolInvocationResult` into a `tools/call` result.
pub fn project_tool_result(state: &mut McpServerState, correlation_id: u64, result: &ToolInvocationResult) {
    let Some(pending) = state.pending.remove(&correlation_id) else {
        tracing::debug!(target: "aether_mcp::server", correlation_id, "tool result for no pending call dropped");
        return;
    };
    state.in_flight.release();

    let PendingOperation::Tool { name, output_wrapper_schema } = &pending.operation else {
        tracing::warn!(target: "aether_mcp::server", correlation_id, "tool result answered a resource read");
        return;
    };

    let projection = OutputProjection {
        tool_name: name,
        output_wrapper_schema,
        limits: OutputLimits::from_configuration(&state.config),
    };
    let now = state.now_millis();
    let projected = project_output(&mut state.responses, now, &projection, result);

    let response = Response::Success { id: pending.id.clone(), result: projected };
    pending.inbound.reply(&render(state, &response));
}

/// The ceilings the output boundary decides against.
///
/// Split out of the configuration so the decision is a function of four numbers
/// and a store, not of a booted capability: inline-versus-addressed is the
/// choice most worth exercising directly, and it should not need a chassis.
#[derive(Debug, Clone, Copy)]
pub struct OutputLimits {
    pub provider_wire_maximum_bytes: usize,
    pub maximum_output_values: usize,
    pub reply_inline_maximum_bytes: usize,
    pub response_inline_maximum_bytes: usize,
}

impl OutputLimits {
    #[must_use]
    pub fn from_configuration(config: &McpServerConfiguration) -> Self {
        Self {
            provider_wire_maximum_bytes: config.provider_wire_maximum_bytes,
            maximum_output_values: config.maximum_output_values,
            reply_inline_maximum_bytes: config.reply_inline_maximum_bytes,
            response_inline_maximum_bytes: config.response_inline_maximum_bytes,
        }
    }
}

/// What one tool's output is projected against.
pub struct OutputProjection<'a> {
    pub tool_name: &'a str,
    pub output_wrapper_schema: &'a SchemaType,
    pub limits: OutputLimits,
}

/// The output boundary: decode, re-check, and decide inline or addressed.
pub fn project_output(
    responses: &mut ResponseStore,
    now_millis: u64,
    projection: &OutputProjection<'_>,
    result: &ToolInvocationResult,
) -> Value {
    let OutputProjection { tool_name, output_wrapper_schema, limits } = projection;
    let output_bytes = match result {
        ToolInvocationResult::Err { category, message } => return call_tool_failure(category, message),
        ToolInvocationResult::Ok { output_bytes } => output_bytes,
    };

    if output_bytes.len() > limits.provider_wire_maximum_bytes {
        return call_tool_failure(
            "output_too_large",
            &format!(
                "the provider returned {} bytes, past the {}-byte provider ceiling",
                output_bytes.len(),
                limits.provider_wire_maximum_bytes,
            ),
        );
    }

    let decoded = match decode_schema_strict(output_bytes, output_wrapper_schema, limits.maximum_output_values) {
        Ok(decoded) => decoded,
        Err(error) => return call_tool_failure_for_decode(&error),
    };
    // Normally redundant with the decode, and kept because it is not always:
    // a known typed identifier whose reserved sentinel projected as a numeric
    // compatibility value passes the decoder and violates the tagged-string
    // shape the tool advertised.
    if let Err(error) = validate_client_value(&decoded, output_wrapper_schema, SchemaBudget::default()) {
        return call_tool_failure(
            "invalid_output",
            &format!("the provider's output does not match its schema: {error}"),
        );
    }

    let child = decoded.get("output").cloned().unwrap_or(Value::Null);
    let raw = child.to_string();
    let widest_leaf = widest_bytes_leaf(&decoded, output_wrapper_schema);

    if raw.len() <= limits.response_inline_maximum_bytes && widest_leaf <= limits.reply_inline_maximum_bytes {
        return call_tool_success(tool_name, &json!({ "inline": decoded, "addressed": Value::Null }), None);
    }

    let bytes = raw.len();
    match responses.store(raw.into_bytes(), now_millis) {
        Ok(uri) => {
            let addressed = AddressedOutput { uri, bytes: bytes as u64, summary: summarize(&child) };
            let boundary = json!({ "inline": Value::Null, "addressed": {
                "uri": addressed.uri,
                "bytes": addressed.bytes,
                "summary": addressed.summary
            }});
            call_tool_success(tool_name, &boundary, Some(&addressed))
        }
        // Never a fallback to an oversized inline response: the ceilings bound
        // what reaches a model context, and a fallback would make them
        // advisory exactly when they matter.
        Err(refusal) => call_tool_failure("response_store_exhausted", &refusal.to_string()),
    }
}

/// Project a provider's `ReadResourceResult` into a `resources/read` result.
///
/// Unlike a tool call this stays a protocol request after dispatch: the
/// resource surface has no `isError` convention, so a not-found is `-32002` and
/// anything else is `-32603`.
pub fn project_resource_result(state: &mut McpServerState, correlation_id: u64, result: &ReadResourceResult) {
    complete(state, correlation_id, |pending| {
        let PendingOperation::Resource { uri } = &pending.operation else {
            return Response::Failure {
                id: Some(pending.id.clone()),
                error: ProtocolError::internal_error("a resource result answered a tool call"),
            };
        };

        match result {
            // The echoed address is checked rather than trusted: a provider
            // answering under a different spelling would let a caller receive
            // content for an address it did not ask for.
            ReadResourceResult::Text { uri: echoed, mime_type, text } if echoed == uri => {
                Response::Success { id: pending.id.clone(), result: read_resource_text_result(uri, mime_type, text) }
            }
            ReadResourceResult::Blob { uri: echoed, mime_type, bytes } if echoed == uri => {
                Response::Success { id: pending.id.clone(), result: read_resource_blob_result(uri, mime_type, bytes) }
            }
            ReadResourceResult::Err { category, message } => Response::Failure {
                id: Some(pending.id.clone()),
                error: protocol_error_for_read_failure(uri, category, message),
            },
            _ => Response::Failure {
                id: Some(pending.id.clone()),
                error: ProtocolError::internal_error("the provider answered a different address than it was asked for"),
            },
        }
    });
}

/// A dispatched chain that settled without ever replying.
pub fn settle_without_reply(state: &mut McpServerState, correlation_id: u64) {
    if !state.pending.contains_key(&correlation_id) {
        return;
    }
    complete(state, correlation_id, |pending| match &pending.operation {
        PendingOperation::Tool { .. } => Response::Success {
            id: pending.id.clone(),
            result: call_tool_failure("no_reply", "the provider completed without producing a result"),
        },
        PendingOperation::Resource { .. } => Response::Failure {
            id: Some(pending.id.clone()),
            error: ProtocolError::internal_error("the resource provider completed without producing a result"),
        },
    });
}

/// A deadline came due.
///
/// The generation check is the whole point: a correlation the substrate reused
/// would otherwise be expired by a timer armed for its previous occupant.
pub fn expire(state: &mut McpServerState, correlation_id: u64, generation: u64) {
    if state.pending.get(&correlation_id).is_none_or(|pending| pending.generation != generation) {
        return;
    }
    let timeout_millis = state.config.tool_timeout_millis;
    complete(state, correlation_id, |pending| match &pending.operation {
        PendingOperation::Tool { .. } => Response::Success {
            id: pending.id.clone(),
            result: call_tool_failure("timeout", &format!("the tool did not answer within {timeout_millis} ms")),
        },
        PendingOperation::Resource { .. } => Response::Failure {
            id: Some(pending.id.clone()),
            error: ProtocolError::internal_error(format!(
                "the resource provider did not answer within {timeout_millis} ms"
            )),
        },
    });
}

/// Serialize a response and enforce the HTTP response ceiling.
///
/// Checked after final serialization, because that is the only point at which
/// the actual byte count exists — every earlier bound is on a component of it.
/// An over-ceiling response is replaced rather than truncated: truncated JSON
/// is not a protocol message at all.
fn render(state: &McpServerState, response: &Response) -> HttpServerResponse {
    let body = response.to_json();
    if body.len() <= state.config.maximum_http_response_bytes {
        return transport::protocol_response(body);
    }

    let id = match response {
        Response::Success { id, .. } => Some(id.clone()),
        Response::Failure { id, .. } => id.clone(),
    };
    tracing::warn!(
        target: "aether_mcp::server",
        bytes = body.len(),
        ceiling = state.config.maximum_http_response_bytes,
        "protocol response exceeded the http response ceiling and was replaced",
    );
    transport::protocol_response(
        Response::Failure {
            id,
            error: ProtocolError::internal_error("the response exceeded this server's serialized response ceiling"),
        }
        .to_json(),
    )
}

/// Answer the current request now with one JSON-RPC response.
fn reply(state: &McpServerState, response: &Response) -> Outcome {
    Outcome::Reply(render(state, response))
}

fn failure(state: &McpServerState, id: MessageId, error: ProtocolError) -> Outcome {
    reply(state, &Response::Failure { id: Some(id), error })
}

/// The widest `Bytes` leaf in a decoded output.
///
/// A single oversized byte leaf addresses the *whole* output. Substituting just
/// that leaf would change its declared type — a `Bytes` property is an array of
/// integers — so the surrounding result would look inline while no longer
/// satisfying the advertised `outputSchema`.
///
/// Iterative with an explicit stack and a node budget, per the repository rule:
/// the schema is public and serializable, so the shape being walked is not
/// bounded by anything in this crate.
fn widest_bytes_leaf(value: &Value, schema: &SchemaType) -> usize {
    let mut widest = 0;
    let mut visited = 0;
    let mut stack: Vec<(&Value, &SchemaType)> = vec![(value, schema)];

    while let Some((value, schema)) = stack.pop() {
        visited += 1;
        if visited > OUTPUT_SCAN_MAXIMUM_NODES {
            break;
        }
        match schema {
            SchemaType::Bytes => {
                if let Value::Array(items) = value {
                    widest = widest.max(items.len());
                }
            }
            SchemaType::Option(inner) => {
                if !value.is_null() {
                    stack.push((value, inner));
                }
            }
            SchemaType::Vec(inner) => {
                if let Value::Array(items) = value {
                    stack.extend(items.iter().map(|item| (item, &**inner)));
                }
            }
            SchemaType::Array { element, .. } => {
                if let Value::Array(items) = value {
                    stack.extend(items.iter().map(|item| (item, &**element)));
                }
            }
            SchemaType::Struct { fields, .. } => {
                if let Value::Object(members) = value {
                    for field in fields.iter() {
                        if let Some(member) = members.get(field.name.as_ref()) {
                            stack.push((member, &field.ty));
                        }
                    }
                }
            }
            SchemaType::Map { value: item, .. } => {
                if let Value::Object(members) = value {
                    stack.extend(members.values().map(|member| (member, &**item)));
                }
            }
            SchemaType::Enum { variants } => {
                push_enum_body(&mut stack, value, variants);
            }
            _ => {}
        }
    }
    widest
}

/// Push the selected variant's body onto the scan stack.
///
/// The value already passed the public-shape validation, so an unrecognized
/// shape here is not an error to report — there is nothing left to scan, and
/// reporting from a size scan would misattribute a validation concern.
fn push_enum_body<'a>(stack: &mut Vec<(&'a Value, &'a SchemaType)>, value: &'a Value, variants: &'a [EnumVariant]) {
    let Value::Object(members) = value else {
        return;
    };
    let Some((name, body)) = members.iter().next() else {
        return;
    };
    let Some(variant) = variants.iter().find(|variant| variant.name() == name) else {
        return;
    };

    match variant {
        EnumVariant::Unit { .. } => {}
        EnumVariant::Tuple { fields, .. } => match (body, fields.len()) {
            (_, 1) => stack.push((body, &fields[0])),
            (Value::Array(items), _) => stack.extend(items.iter().zip(fields.iter())),
            _ => {}
        },
        EnumVariant::Struct { fields, .. } => {
            if let Value::Object(body) = body {
                for field in fields.iter() {
                    if let Some(member) = body.get(field.name.as_ref()) {
                        stack.push((member, &field.ty));
                    }
                }
            }
        }
    }
}

/// A mailbox-liveness predicate that owns its registry handle.
///
/// Consulted at dispatch as well as at registration: a monitor notice may not
/// have been delivered when a call arrives, and the two checks together are
/// what stop a call landing on a holder that has already departed.
fn live_predicate(state: &McpServerState) -> impl Fn(MailboxId) -> bool + use<> {
    let registry = Arc::clone(state.mailer.registry());
    move |mailbox| matches!(registry.entry(mailbox), Some(MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_)))
}
