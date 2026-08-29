//! Initialization, the initialized notification, and ping.
//!
//! `initialize` is negotiation without retained state. Nothing about the
//! client is kept: the server offers exactly one revision, so there is no
//! per-connection decision to remember, and every later request must stand on
//! its own. That is also why the initialize result never mints
//! `Mcp-Session-Id` and why `notifications/initialized` changes nothing —
//! it is answered with an empty `202` because the transport requires an
//! answer, not because it advances a state machine.

use serde_json::{Value, json};

use super::remote_procedure_call::{PROTOCOL_REVISION, ProtocolError, reject_unknown_members};

/// The `serverInfo.name` this implementation reports.
pub const SERVER_NAME: &str = "aether";

/// The `serverInfo.version` this implementation reports — the workspace
/// package version.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Server-wide instructions.
///
/// Deliberately short and self-contained: one client's published guidance
/// only guarantees to read the first 512 characters, so everything that
/// matters is said well inside that.
pub const SERVER_INSTRUCTIONS: &str = "Use tools for actions and aether resource addresses for files and large \
     results. Read a returned resource link when more content is needed.";

/// Who the client says it is.
///
/// Parsed and then dropped. It is read because the 2025-06-18 lifecycle
/// requires the fields to be present and well-shaped, not because the server
/// keeps anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
}

/// The parsed `initialize` parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitializeParams {
    /// The revision the client proposes. It may name a revision newer than
    /// this server's; the response still selects [`PROTOCOL_REVISION`], and
    /// the client either accepts that or disconnects.
    pub protocol_version: String,
    pub client: ClientInfo,
}

/// Parse `initialize`.
///
/// This is the one method that tolerates members it does not know, at every
/// level of its parameters — the parameter object, the client capabilities,
/// and the client implementation information. A client proposing a newer
/// revision necessarily sends fields from that revision, and refusing them
/// would make version negotiation impossible before it began. The budgets
/// still apply; tolerance is about unknown *names*, not unbounded size.
pub fn parse_initialize(params: Option<&Value>) -> Result<InitializeParams, ProtocolError> {
    let members = params
        .and_then(Value::as_object)
        .ok_or_else(|| ProtocolError::invalid_params("`initialize` requires a parameter object"))?;

    let protocol_version = members
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::invalid_params("`protocolVersion` must be a string"))?;

    if !members.get("capabilities").is_some_and(Value::is_object) {
        return Err(ProtocolError::invalid_params("`capabilities` must be an object"));
    }

    let info = members
        .get("clientInfo")
        .and_then(Value::as_object)
        .ok_or_else(|| ProtocolError::invalid_params("`clientInfo` must be an object"))?;
    let name = info
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::invalid_params("`clientInfo.name` must be a string"))?;
    let version = info
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::invalid_params("`clientInfo.version` must be a string"))?;

    Ok(InitializeParams {
        protocol_version: protocol_version.to_string(),
        client: ClientInfo {
            name: name.to_string(),
            version: version.to_string(),
            title: info.get("title").and_then(Value::as_str).map(str::to_string),
        },
    })
}

/// The `initialize` result.
///
/// `tools.listChanged` is false and there is no logging or prompts
/// capability, because the stateless profile has no channel on which to
/// deliver a server-originated notification. Advertising one would promise
/// something no code path can send.
#[must_use]
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_REVISION,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": {}
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION
        },
        "instructions": SERVER_INSTRUCTIONS
    })
}

/// The `ping` result: an empty object.
///
/// A protocol-level liveness probe with no application side effect, which is
/// what makes it safe for the tunnel and a client to call freely.
#[must_use]
pub fn ping_result() -> Value {
    json!({})
}

/// Parse `ping`, which declares no parameters of its own.
pub fn parse_ping(params: Option<&Value>) -> Result<(), ProtocolError> {
    reject_unknown_members(params, &[])
}
