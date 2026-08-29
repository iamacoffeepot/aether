//! The HTTP edge: everything decided before a protocol message exists.
//!
//! These are the only failures this server answers with an HTTP status code. It
//! is a real line, not a stylistic one — once a valid request identifier and
//! method envelope exist, a failure is a JSON-RPC response at HTTP `200`,
//! because the caller needs the identifier echoed back to correlate it. A
//! server that answered `400` to an unknown tool would leave a client unable to
//! tell which of its in-flight requests failed.
//!
//! Nothing here reads a socket. The route is registered with
//! `HttpServerCapability`, so the request arrives already parsed and
//! body-bounded; this module decides whether it may become a protocol message.

use aether_http::kinds::{HttpHeader, HttpMethod, HttpServerRequest, HttpServerResponse};

use crate::McpServerConfiguration;
use crate::protocol::PROTOCOL_REVISION;

/// The header a client must echo after negotiation.
pub const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
/// The session header this server never mints and always refuses.
pub const SESSION_HEADER: &str = "mcp-session-id";

/// The authentication challenge a `401` carries. It names the realm and no
/// token material.
const BEARER_CHALLENGE: &str = "Bearer realm=\"aether-mcp\"";

/// The two media ranges an `Accept` header must list explicitly.
const REQUIRED_ACCEPT: [&str; 2] = ["application/json", "text/event-stream"];

/// A refusal at the transport layer, ready to become an HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportRefusal {
    pub status: u16,
    pub message: String,
    pub headers: Vec<HttpHeader>,
}

impl TransportRefusal {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), headers: Vec::new() }
    }

    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push(HttpHeader { name: name.to_string(), value: value.to_string() });
        self
    }

    /// Render the refusal. The body is the plain message — a transport refusal
    /// is not a protocol message, so wrapping it in a JSON-RPC envelope would
    /// tell a client that a request it never made had failed.
    #[must_use]
    pub fn to_response(&self) -> HttpServerResponse {
        HttpServerResponse {
            status: self.status,
            headers: self.headers.clone(),
            body: self.message.clone().into_bytes(),
        }
    }
}

/// A request that cleared the transport edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedRequest<'a> {
    /// The raw body, still bytes — a body that is not UTF-8 is malformed JSON,
    /// which is a `-32700` protocol answer rather than a transport refusal.
    pub body: &'a [u8],
    /// The declared protocol revision, absent on the initialize exchange that
    /// has not negotiated one yet.
    pub protocol_version: Option<String>,
}

/// Decide whether one request may become a protocol message.
///
/// Order matters. The method check comes first because `GET /mcp` is a
/// *protocol-defined* answer — the transport lets a server decline the optional
/// event stream with `405`, and a client probing for it should learn that
/// without also learning whether its credentials are good. Origin and
/// authentication follow, then the negotiation headers, so a misconfigured
/// client sees the first thing wrong with it rather than a cascade.
pub fn admit<'a>(
    request: &'a HttpServerRequest,
    config: &McpServerConfiguration,
) -> Result<AdmittedRequest<'a>, TransportRefusal> {
    if request.method != HttpMethod::Post {
        return Err(TransportRefusal::new(
            405,
            "this endpoint accepts POST only; it opens no event stream and holds no session",
        )
        .with_header("Allow", "POST"));
    }

    check_origin(request, config)?;
    check_authorization(request, config)?;

    // A session header can only have been fabricated: the initialize result
    // never mints one. Answering `400` rather than ignoring it is what makes a
    // client accidentally pointed at a session-configured endpoint fail loudly
    // instead of silently losing state it believes it has.
    if header(request, SESSION_HEADER).is_some() {
        return Err(TransportRefusal::new(
            400,
            "this server is stateless and mints no `Mcp-Session-Id`; a request carrying one is \
             addressed to a different endpoint",
        ));
    }

    check_content_type(request)?;
    check_accept(request)?;

    Ok(AdmittedRequest { body: &request.body, protocol_version: header(request, PROTOCOL_VERSION_HEADER) })
}

/// An absent `Origin` is a native client, not a browser, and is accepted. A
/// present one must match the allowlist exactly — the default empty allowlist
/// therefore rejects every browser, which is the DNS-rebinding guard the
/// transport asks for without conflating the two kinds of caller.
fn check_origin(request: &HttpServerRequest, config: &McpServerConfiguration) -> Result<(), TransportRefusal> {
    match header(request, "origin") {
        None => Ok(()),
        Some(origin) if config.allowed_origins.contains(&origin) => Ok(()),
        Some(_) => Err(TransportRefusal::new(403, "this origin is not on the server's allowlist")),
    }
}

/// The static bearer guard.
///
/// An enabled server with no configured token answers `401` to everything. That
/// is deliberate: a deployment that forgot to set the token is shut rather than
/// open, and the operator's symptom is an immediate, obvious failure instead of
/// an endpoint quietly serving anyone who finds it.
fn check_authorization(request: &HttpServerRequest, config: &McpServerConfiguration) -> Result<(), TransportRefusal> {
    let refuse = || {
        Err(TransportRefusal::new(401, "a bearer token is required").with_header("WWW-Authenticate", BEARER_CHALLENGE))
    };

    if config.authorization_token.is_empty() {
        return refuse();
    }
    let Some(presented) = header(request, "authorization").and_then(|value| bearer_token(&value)) else {
        return refuse();
    };
    if tokens_match(&presented, &config.authorization_token) {
        Ok(())
    } else {
        refuse()
    }
}

/// Compare two tokens without letting the comparison's duration report how much
/// of a guess was right.
fn tokens_match(presented: &str, configured: &str) -> bool {
    if presented.len() != configured.len() {
        return false;
    }
    presented.bytes().zip(configured.bytes()).fold(0_u8, |differences, (left, right)| differences | (left ^ right)) == 0
}

fn bearer_token(value: &str) -> Option<String> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim().to_string())
}

/// `application/json`, optionally with a charset parameter, and no content
/// encoding other than identity.
fn check_content_type(request: &HttpServerRequest) -> Result<(), TransportRefusal> {
    if let Some(encoding) = header(request, "content-encoding")
        && !encoding.trim().eq_ignore_ascii_case("identity")
    {
        return Err(TransportRefusal::new(415, "this endpoint accepts identity content encoding only"));
    }

    let Some(content_type) = header(request, "content-type") else {
        return Err(TransportRefusal::new(415, "`Content-Type: application/json` is required"));
    };
    let mut parameters = content_type.split(';');
    let media_type = parameters.next().unwrap_or_default().trim();
    if !media_type.eq_ignore_ascii_case("application/json") {
        return Err(TransportRefusal::new(415, "`Content-Type: application/json` is required"));
    }
    // The one tolerated parameter is a UTF-8 charset. A different charset would
    // describe a body this server cannot read, and silently ignoring it would
    // mean answering as if it had.
    for parameter in parameters {
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("charset") && !value.trim().trim_matches('"').eq_ignore_ascii_case("utf-8")
        {
            return Err(TransportRefusal::new(415, "the only accepted charset is utf-8"));
        }
    }
    Ok(())
}

/// Both required media ranges must appear explicitly at positive quality.
///
/// A wildcard does not stand in for either. The transport asks a client to
/// declare that it can read both a JSON answer and an event stream, and a
/// client that says only `*/*` has declared nothing — accepting it would mean
/// discovering at the next revision that it cannot in fact read what it asked
/// for.
fn check_accept(request: &HttpServerRequest) -> Result<(), TransportRefusal> {
    let accept = header(request, "accept").unwrap_or_default();
    let missing: Vec<&str> =
        REQUIRED_ACCEPT.into_iter().filter(|required| !accepts_media_range(&accept, required)).collect();

    if missing.is_empty() {
        Ok(())
    } else {
        Err(TransportRefusal::new(
            406,
            format!("`Accept` must list {} explicitly at positive quality", missing.join(" and ")),
        ))
    }
}

fn accepts_media_range(accept: &str, required: &str) -> bool {
    accept.split(',').any(|range| {
        let mut parameters = range.split(';');
        let media_type = parameters.next().unwrap_or_default().trim();
        media_type.eq_ignore_ascii_case(required) && parameters.all(|parameter| !is_zero_quality(parameter))
    })
}

fn is_zero_quality(parameter: &str) -> bool {
    let Some((name, value)) = parameter.split_once('=') else {
        return false;
    };
    name.trim().eq_ignore_ascii_case("q") && value.trim().parse::<f32>().is_ok_and(|quality| quality <= 0.0)
}

/// Whether a declared protocol version is acceptable for this message.
///
/// A missing header is tolerated only on `initialize`, which is the exchange
/// that establishes it. On every later message the transport says a server with
/// no retained negotiated state must assume the *previous* revision — a revision
/// this server does not implement — so an absent header is refused rather than
/// silently answered under a contract neither side agreed to.
pub fn check_protocol_version(declared: Option<&str>, is_initialize: bool) -> Result<(), TransportRefusal> {
    match declared {
        Some(version) if version == PROTOCOL_REVISION => Ok(()),
        Some(version) => Err(TransportRefusal::new(
            400,
            format!("this server implements revision {PROTOCOL_REVISION} only; the request declared {version}"),
        )),
        None if is_initialize => Ok(()),
        None => Err(TransportRefusal::new(
            400,
            format!("every request after `initialize` must carry `MCP-Protocol-Version: {PROTOCOL_REVISION}`"),
        )),
    }
}

/// Read one header, case-insensitively by name.
fn header(request: &HttpServerRequest, name: &str) -> Option<String> {
    request
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.trim().to_string())
}

/// An accepted notification: `202` with a zero-byte body.
///
/// The transport requires the empty body, and it is load-bearing — a client
/// that received JSON here would have to decide whether its notification had
/// somehow produced a response.
#[must_use]
pub fn accepted_notification() -> HttpServerResponse {
    HttpServerResponse { status: 202, headers: Vec::new(), body: Vec::new() }
}

/// One JSON protocol response at HTTP `200`.
#[must_use]
pub fn protocol_response(body: String) -> HttpServerResponse {
    HttpServerResponse {
        status: 200,
        headers: vec![HttpHeader { name: "Content-Type".to_string(), value: "application/json".to_string() }],
        body: body.into_bytes(),
    }
}
