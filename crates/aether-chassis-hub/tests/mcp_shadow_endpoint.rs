//! The hub's shadow Model Context Protocol endpoint, over a real socket.
//!
//! This is the only place the whole step-4 assembly is exercised as one
//! thing: an operating-system listener, the HTTP capability's request
//! reader, the protocol capability's route, the tool registry, the minted
//! tool kinds, actor dispatch to the hub provider, a deferred round trip to
//! `FleetServer`, and the reply mapping that answers the original POST.
//!
//! A unit test can reach any one of those. None of them can catch the
//! failures that only appear when they are wired together — a route claimed
//! but never registered, a catalog whose descriptors never reached the
//! capability, a tool whose minted request kind the actor dispatcher does
//! not accept, a deferral whose reply comes back correlated to nothing. Each
//! would leave every component's own tests green.
//!
//! **Ordering is load-bearing.** The first successful `tools/list` freezes
//! the catalog's names, and a registration arriving after that freeze is
//! refused. Route and tool registration are asynchronous `wire` mail, so the
//! session below probes each tool live *before* it lists — a test that
//! listed first would freeze a partial catalog and then watch the remaining
//! tools be refused, which is exactly the failure a hub must not ship.

// The fixture is a deliberate embedder: it builds a bare `TestChassis` through
// `Builder::new` rather than the composed boot seam `HubChassis` uses, because
// that chassis' driver blocks on a shutdown signal.
#![allow(clippy::disallowed_methods)] // aether-suppression-request: fixture embeds a bare TestChassis via Builder::new; the composed boot seam blocks on shutdown

use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aether_chassis_hub::HubToolProvider;
use aether_fleet::{FleetConfig, FleetServer};
use aether_http::{HttpServerCapability, HttpServerConfig, HttpServerHandle};
use aether_mcp::{McpServerCapability, McpServerConfiguration};
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::testing::{TestChassis, fresh_substrate};
use serde_json::{Value, json};

/// The bearer token the fixture's endpoint is configured with. An enabled
/// server with an empty token fails closed, so the fixture must set one.
const TOKEN: &str = "shadow-endpoint-test-token";

/// The one revision this server offers.
const REVISION: &str = "2025-06-18";

/// Wall-clock budget for a tool's registration mail to land. Everything here
/// is in-process and answers in milliseconds; this only decides how long a
/// genuinely wedged run takes to fail.
const LIVE_BUDGET: Duration = Duration::from_secs(20);

/// Spacing between registration probes. Wide enough that a full wait stays
/// inside the server's steady-state admission rate.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The server-busy code an admission refusal carries.
const ADMISSION_REFUSED: i64 = -32000;

/// A booted hub-shaped chassis, its endpoint port, and the temporary fleet
/// store it owns.
struct Endpoint {
    _chassis: PassiveChassis<TestChassis>,
    port: u16,
    store_root: PathBuf,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        // The fleet cap opens a real content-addressed store; the fixture
        // gave it a private directory rather than the developer's, so it
        // removes that directory rather than leaving one per test run.
        let _ = fs::remove_dir_all(&self.store_root);
    }
}

/// Boot the hub's capability shape with the shadow endpoint enabled on an
/// operating-system-assigned port.
///
/// This mirrors what `HubChassis::compose` installs for the endpoint — the
/// HTTP capability, the protocol capability, the hub tool provider, and the
/// fleet cap they answer from — over the passive test chassis, because the
/// real chassis' driver blocks on a shutdown signal. The composition seam's
/// own decisions (the loopback interface, the request deadline, the enabled
/// flag tracking the protocol server's) are asserted where they are made, in
/// the chassis unit tests; what this fixture supplies is a live endpoint.
fn boot() -> Endpoint {
    let store_root = aether_harness_fleet::allocate_store_root_for_test();
    let (registry, mailer) = fresh_substrate();

    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<FleetServer>(
            (),
            FleetConfig {
                binary_store_dir: Some(store_root.to_string_lossy().into_owned()),
                fleet_store_root: Some(store_root.to_string_lossy().into_owned()),
                ..FleetConfig::default()
            },
        )
        .with_actor_configured::<HttpServerCapability>(
            (),
            HttpServerConfig {
                enabled: true,
                bind_addr: "127.0.0.1:0".to_string(),
                request_timeout_millis: 30_000,
                ..HttpServerConfig::default()
            },
        )
        .with_actor_configured::<McpServerCapability>(
            (),
            McpServerConfiguration {
                enabled: true,
                authorization_token: TOKEN.to_string(),
                ..McpServerConfiguration::default()
            },
        )
        .with_actor::<HubToolProvider>(())
        .build_passive()
        .expect("the hub-shaped fixture chassis boots");

    let port = chassis.handle::<HttpServerHandle>().expect("HttpServerHandle published").local_port;
    Endpoint { _chassis: chassis, port, store_root }
}

/// One HTTP response, split into the two things every assertion here reads.
struct Answer {
    status: u16,
    body: String,
}

impl Answer {
    /// The response body as JSON-RPC.
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("a {} response must carry JSON: {error}; body {:?}", self.status, self.body))
    }
}

/// POST one protocol message and read the whole answer.
///
/// `version` carries the `MCP-Protocol-Version` header when the exchange
/// requires it — `initialize` is the one message sent before a revision has
/// been negotiated, and the transport rules make the header mandatory on
/// every message after it.
fn post(port: u16, message: &Value, version: Option<&str>) -> Answer {
    let body = message.to_string();
    let mut head = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        body.len(),
    );
    if let Some(version) = version {
        let _ = write!(head, "MCP-Protocol-Version: {version}\r\n");
    }
    head.push_str("\r\n");

    let mut socket = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to the shadow endpoint");
    socket.set_read_timeout(Some(Duration::from_secs(30))).expect("set a read timeout");
    socket.write_all(head.as_bytes()).expect("write the request head");
    socket.write_all(body.as_bytes()).expect("write the request body");
    socket.flush().expect("flush the request");

    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).expect("read the response");
    let raw = String::from_utf8_lossy(&raw).into_owned();

    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or_else(|| panic!("a complete response head: {raw:?}"));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("a status line: {head:?}"));
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        dechunk(body)
    } else {
        body.to_string()
    };
    Answer { status, body }
}

/// Reassemble a chunked body into its payload.
fn dechunk(body: &str) -> String {
    let mut rest = body;
    let mut out = String::new();
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if size == 0 || tail.len() < size {
            break;
        }
        out.push_str(&tail[..size]);
        rest = tail[size..].strip_prefix("\r\n").unwrap_or("");
    }
    out
}

/// Call one tool and return the JSON-RPC response.
fn call(port: u16, name: &str, arguments: &Value) -> Value {
    post(
        port,
        &json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": {"name": name, "arguments": arguments}}),
        Some(REVISION),
    )
    .json()
}

/// Call `name` until the capability stops answering "no tool named", i.e.
/// until this tool's registration mail has landed.
///
/// Every argument passed here is deliberately harmless: a listing, or a
/// refusal the fleet decides before it forks anything. The returned response
/// is the first live one, so a caller's assertions run against a registered
/// tool rather than against the race.
///
/// The interval is spaced rather than tight because the server applies a
/// real admission bucket — 600 messages a minute over the whole endpoint.
/// A 25-millisecond poll would spend that budget inside one wait and then
/// report an admission refusal in place of whatever the tool was going to
/// say, turning a diagnosable registration failure into a misleading one.
/// An admission refusal is retried for the same reason.
fn call_live(port: u16, name: &str, arguments: &Value) -> Value {
    let deadline = Instant::now() + LIVE_BUDGET;
    loop {
        let response = call(port, name, arguments);
        let retry = match (response["error"]["code"].as_i64(), response["error"]["message"].as_str()) {
            (Some(ADMISSION_REFUSED), _) => true,
            (_, Some(message)) => message.contains("no tool named"),
            _ => false,
        };
        if !retry {
            return response;
        }
        assert!(Instant::now() < deadline, "`{name}` never became callable within {LIVE_BUDGET:?}: {response}");
        thread::sleep(POLL_INTERVAL);
    }
}

/// The successful-call payload: `tools/call` answers a *successful* JSON-RPC
/// response whose result carries the tool's own verdict.
fn result_of(response: &Value) -> &Value {
    assert!(response.get("error").is_none(), "a dispatched tool never answers a protocol error: {response}");
    &response["result"]
}

/// A complete client session against the shadow endpoint: negotiate, list
/// the catalog, and read the fleet through it.
///
/// The single test walks the whole session because the catalog freeze makes
/// the order part of the contract — splitting the list away from the probes
/// that precede it would assert the freeze against a partially registered
/// catalog.
#[test]
fn the_shadow_endpoint_serves_a_negotiated_session_and_the_engine_catalog() {
    let endpoint = boot();
    let port = endpoint.port;

    let initialized = post(
        port,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": REVISION, "capabilities": {},
                           "clientInfo": {"name": "hub-shadow-test", "version": "0.1.0"}}}),
        None,
    );
    assert_eq!(initialized.status, 200, "initialize is answered with JSON, not a transport error");
    let negotiated = initialized.json();
    assert_eq!(negotiated["result"]["protocolVersion"], REVISION, "the server names the one revision it offers");
    assert!(
        negotiated["result"]["capabilities"]["tools"].is_object(),
        "a server carrying a tool catalog advertises the tools capability: {negotiated}",
    );

    let acknowledged = post(port, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), Some(REVISION));
    assert_eq!(acknowledged.status, 202, "a notification is accepted, never answered with a body");
    assert!(acknowledged.body.is_empty(), "a 202 carries no body: {:?}", acknowledged.body);

    // Probe every tool live before listing: the first `tools/list` freezes
    // the catalog's names, and a registration landing after it is refused.
    let listed_engines = call_live(port, "list_engines", &json!({}));
    let refused_spawn = call_live(
        port,
        "spawn_substrate",
        &json!({"selector": "no-such-binary", "chassis": null, "caps": [],
                                                   "target": null, "args": []}),
    );
    let refused_terminate = call_live(port, "terminate_substrate", &json!({"engine_id": "not-a-uuid"}));

    let catalog = post(port, &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}), Some(REVISION)).json();
    let tools = catalog["result"]["tools"].as_array().expect("a tool catalog");
    let names: Vec<&str> = tools.iter().filter_map(|tool| tool["name"].as_str()).collect();
    assert_eq!(
        names,
        vec!["list_engines", "spawn_substrate", "terminate_substrate"],
        "the whole engine-lifecycle group is listed, in name order: {catalog}",
    );

    // The translated schemas are what a client validates against before it
    // ever calls. An input schema that arrived unschematized, or a boundary
    // output schema missing its addressed arm, would leave a client unable
    // to tell a valid call from an invalid one.
    let spawn = tools.iter().find(|tool| tool["name"] == "spawn_substrate").expect("the spawn descriptor");
    assert_eq!(spawn["inputSchema"]["type"], "object", "a tool input schema is object-shaped: {spawn}");
    let properties = &spawn["inputSchema"]["properties"];
    for field in ["selector", "chassis", "caps", "target", "args"] {
        assert!(properties.get(field).is_some(), "`{field}` is a declared input property: {spawn}");
    }
    assert_eq!(properties["caps"]["type"], "array", "a Vec<String> translates to an array");
    assert!(properties["selector"]["anyOf"].is_array(), "an Option translates to an anyOf with null: {spawn}");
    let output_properties = &spawn["outputSchema"]["properties"];
    assert!(
        output_properties.get("inline").is_some() && output_properties.get("addressed").is_some(),
        "every tool's output schema is the inline-or-addressed boundary envelope: {spawn}",
    );
    assert_eq!(spawn["annotations"]["readOnlyHint"], false, "spawning is not read-only");
    assert_eq!(spawn["annotations"]["destructiveHint"], false, "spawning was declared non-destructive");

    // An empty fleet is a successful answer with an empty listing — not an
    // error, and not an absent field.
    let listing = result_of(&listed_engines);
    assert_eq!(listing["isError"], false, "listing an empty fleet succeeds: {listed_engines}");
    let engines = &listing["structuredContent"]["inline"]["output"];
    assert_eq!(engines["engines"], json!([]), "no engine has been spawned: {listed_engines}");
    assert_eq!(engines["recently_died"], json!([]), "and none has died: {listed_engines}");

    // Both refusals crossed the protocol/tool error line in the right
    // direction: the call resolved, dispatched, and the *tool* declined.
    for (label, response) in [("spawn", &refused_spawn), ("terminate", &refused_terminate)] {
        let refusal = result_of(response);
        assert_eq!(refusal["isError"], true, "a {label} refusal is a tool error: {response}");
    }
}

/// A selector naming no stored binary is a tool error, not a protocol error.
///
/// This is the boundary the design draws: resolution, validation, and
/// admission of the call all succeeded, so the JSON-RPC response is a
/// success and the failure rides `isError`. A server that answered `-32602`
/// here would tell a client its *request* was malformed, and the client
/// would rewrite arguments that were always correct. The refusal must also
/// carry the fleet's own reason — a bare "spawn failed" leaves an operator
/// with no way to tell a missing binary from a broken host.
#[test]
fn an_unresolved_selector_answers_a_tool_error_carrying_the_fleet_reason() {
    let endpoint = boot();

    let response = call_live(
        endpoint.port,
        "spawn_substrate",
        &json!({"selector": "definitely-not-a-stored-binary", "chassis": null,
                "caps": [], "target": null, "args": []}),
    );

    let refusal = result_of(&response);
    assert_eq!(refusal["isError"], true, "an unresolvable selector is a tool error: {response}");
    assert!(
        refusal.get("structuredContent").is_none(),
        "a failed call declares no output value, so it carries no structured content: {response}",
    );
    let text = refusal["content"][0]["text"].as_str().expect("a bounded text content block");
    assert!(
        text.contains("no binary in the registry matched selector"),
        "the caller learns why the selector missed: {text}",
    );
    assert!(!text.contains('/'), "no fleet-host path crosses the boundary: {text}");
}
