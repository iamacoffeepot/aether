// Claude-facing MCP tool surface. ADR-0006 V0 + ADR-0007 +
// ADR-0008 + ADR-0009: send_mail / list_engines / describe_kinds /
// receive_mail / spawn_substrate.
//
// The rmcp `Service` factory is invoked per session, so `Hub` is cheap
// to clone and shares a single `HubState` via `Arc`. Per-tool output is
// returned as a JSON-encoded `String`; rmcp wraps it into a
// `Content::text` automatically via `IntoContents`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aether_hub_protocol::{
    EngineId, HubToEngine, KindDescriptor, KindEncoding, MailFrame, SessionToken, Uuid,
};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};

use crate::encoder::{encode_pod, encode_schema};
use crate::registry::{EngineRecord, EngineRegistry};
use crate::session::{QueuedMail, SessionHandle, SessionRegistry};
use crate::spawn::{DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_TERMINATE_GRACE, PendingSpawns, SpawnOpts};

/// Default port the hub binds for MCP clients. Overridable via
/// `AETHER_MCP_PORT`.
pub const DEFAULT_MCP_PORT: u16 = 8888;

/// Shared state across all rmcp sessions. Cheap to `Arc::clone` into
/// each per-session `Hub` instance.
pub struct HubState {
    engines: EngineRegistry,
    sessions: SessionRegistry,
    pending_spawns: PendingSpawns,
    /// Address of the hub's engine TCP listener. Injected as
    /// `AETHER_HUB_URL` into spawned substrates so they dial back to
    /// this hub instance.
    hub_engine_addr: SocketAddr,
}

impl HubState {
    pub fn new(
        engines: EngineRegistry,
        sessions: SessionRegistry,
        pending_spawns: PendingSpawns,
        hub_engine_addr: SocketAddr,
    ) -> Arc<Self> {
        Arc::new(Self {
            engines,
            sessions,
            pending_spawns,
            hub_engine_addr,
        })
    }
}

/// Per-session rmcp service. rmcp calls the factory once per MCP
/// session and may clone the result for concurrent tool dispatch;
/// `session` is an `Arc<SessionHandle>` so the registry entry only
/// goes away when the last clone drops.
#[derive(Clone)]
pub struct Hub {
    state: Arc<HubState>,
    tool_router: ToolRouter<Self>,
    session: Arc<SessionHandle>,
    /// Drain for this session's inbound observation mail. `receive_mail`
    /// pulls from it non-blocking; wrapping in an `Arc<Mutex<_>>` lets
    /// rmcp's per-tool-call clones share the same receiver.
    inbound: Arc<Mutex<mpsc::Receiver<QueuedMail>>>,
}

impl Hub {
    pub fn new(state: Arc<HubState>) -> Self {
        let (session, rx) = SessionHandle::mint(&state.sessions);
        Self {
            state,
            tool_router: Self::tool_router(),
            session: Arc::new(session),
            inbound: Arc::new(Mutex::new(rx)),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeKindsArgs {
    /// Hub-assigned engine UUID as a string (from `list_engines`).
    pub engine_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendMailArgs {
    /// One or more mail items to deliver. Each item is processed
    /// independently — a single failure doesn't abort the batch. The
    /// response carries a per-item status so the caller decides retry
    /// vs abort policy.
    pub mails: Vec<MailSpec>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MailSpec {
    /// Hub-assigned engine UUID as a string (from `list_engines`).
    pub engine_id: String,
    /// Mailbox name as registered by the engine.
    pub recipient_name: String,
    /// Kind name (e.g. `"aether.tick"`) the engine's registry knows.
    pub kind_name: String,
    /// Structured params for schema-driven encoding. The hub looks up
    /// this kind's descriptor (from `describe_kinds`) and writes bytes
    /// matching the kind's wire format — `#[repr(C)]` layout for
    /// cast-shaped kinds, postcard for everything else.
    ///
    /// ADR-0019 PR 5 removed the `payload_bytes` escape hatch from
    /// this surface. Every aether-shipped kind has a schema; if a
    /// future kind doesn't, that's an engine bug to fix, not a
    /// workaround to paper over.
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    /// Count carried on the mail frame. For single-struct payloads
    /// this is 1 (the default); for cast-shaped slices it's the
    /// number of elements the bytes represent.
    #[serde(default = "one")]
    pub count: u32,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MailStatus {
    /// Index into the `mails` array the caller supplied.
    pub index: u32,
    /// `"delivered"` on success, or `"error: <reason>"` on failure.
    pub status: String,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct EngineInfo {
    pub engine_id: String,
    pub name: String,
    pub pid: u32,
    pub version: String,
    /// `true` if this engine was spawned by the hub (ADR-0009), `false`
    /// if it connected externally.
    pub spawned: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpawnSubstrateArgs {
    /// Absolute path to the substrate binary the hub should launch.
    /// The hub does not build, resolve, or locate binaries — the caller
    /// passes a path that exists.
    pub binary_path: String,
    /// Additional command-line arguments to pass to the substrate.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child. `AETHER_HUB_URL` is
    /// injected automatically to point at this hub; if the caller
    /// includes it here, the caller's value wins.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Handshake timeout in milliseconds. Defaults to 5 seconds if
    /// omitted — override for slow CI machines or debug builds.
    #[serde(default)]
    pub timeout_ms: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SpawnResult {
    /// UUID assigned by the hub once the substrate completed its
    /// `Hello` handshake.
    pub engine_id: String,
    /// Operating-system PID of the spawned substrate.
    pub pid: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TerminateSubstrateArgs {
    /// Hub-assigned engine UUID (from `list_engines`).
    pub engine_id: String,
    /// SIGTERM grace period in milliseconds. Defaults to 2 seconds —
    /// long enough for a well-behaved substrate to drain, short enough
    /// not to stall interactive agent flows.
    #[serde(default)]
    pub grace_ms: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TerminateResult {
    pub engine_id: String,
    /// `true` if the child ignored SIGTERM and the hub escalated to
    /// SIGKILL after the grace window. `false` for clean exits.
    pub sigkilled: bool,
    /// Exit code if the child exited normally; `null` if it was killed
    /// by a signal (the common case when `sigkilled` is true).
    pub exit_code: Option<i32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReceiveMailArgs {
    /// Maximum number of items to return in this call. `None` drains
    /// everything currently queued. Defaults to unlimited.
    #[serde(default)]
    pub max: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReceivedMail {
    /// Hub-assigned UUID of the engine that sent this mail.
    pub engine_id: String,
    /// Kind name the engine declared at handshake.
    pub kind_name: String,
    /// Raw payload bytes. Decode against the engine's kind descriptor
    /// (via `describe_kinds`) if you need structured fields.
    pub payload_bytes: Vec<u8>,
    /// `true` if this mail was addressed to every attached session;
    /// `false` if it was a reply targeted at this session specifically.
    pub broadcast: bool,
    /// Substrate-attested name of the emitting mailbox (ADR-0011).
    /// Distinguishes components that share a kind. `None` for mail
    /// pushed by substrate core (e.g. the frame loop's `FrameStats`),
    /// which has no sending mailbox.
    pub origin: Option<String>,
}

#[tool_router]
impl Hub {
    #[tool(
        description = "Send one or more mail items. Each item takes `params` — structured JSON encoded via the engine's kind descriptor (cast-shaped or postcard, ADR-0019). The batch is best-effort: per-item status is returned and failures don't abort siblings. 'delivered' means the hub queued the frame to the engine's socket — not that the engine processed it."
    )]
    async fn send_mail(
        &self,
        Parameters(args): Parameters<SendMailArgs>,
    ) -> Result<String, McpError> {
        let mut statuses = Vec::with_capacity(args.mails.len());
        for (i, spec) in args.mails.into_iter().enumerate() {
            let result = deliver_one(spec, &self.state.engines, self.session.token).await;
            let status = match result {
                Ok(()) => "delivered".into(),
                Err(e) => format!("error: {e}"),
            };
            statuses.push(MailStatus {
                index: i as u32,
                status,
            });
        }
        serde_json::to_string(&statuses).map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        description = "List every kind the given engine declared at handshake, with enough structural detail for clients to build params for send_mail. ADR-0019 Schema kinds describe their full shape (scalars, strings, vecs, options, enums, nested structs); the cast-shaped subset (`Struct{repr_c:true}`) is wire-compatible with `#[repr(C)]` layout."
    )]
    async fn describe_kinds(
        &self,
        Parameters(args): Parameters<DescribeKindsArgs>,
    ) -> Result<String, McpError> {
        let uuid = Uuid::parse_str(&args.engine_id).map_err(|e| {
            McpError::invalid_params(format!("engine_id is not a valid UUID: {e}"), None)
        })?;
        let id = EngineId(uuid);
        let Some(record) = self.state.engines.get(&id) else {
            return Err(McpError::invalid_params(
                format!("unknown engine_id {}", args.engine_id),
                None,
            ));
        };
        serde_json::to_string(&record.kinds)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        description = "Drain observation mail addressed to this MCP session. Returns everything currently queued (up to `max`, if provided). Each item reports the originating engine_id, the kind name, the raw payload bytes, a `broadcast` flag indicating whether this mail also went to every other attached session (true) or was targeted specifically at this one (false), and an optional `origin` — the substrate-local mailbox name of the emitting component (absent for substrate-core pushes with no sending mailbox). Non-blocking: returns an empty array if nothing is queued."
    )]
    async fn receive_mail(
        &self,
        Parameters(args): Parameters<ReceiveMailArgs>,
    ) -> Result<String, McpError> {
        let cap = args.max.map(|n| n as usize).unwrap_or(usize::MAX);
        let mut rx = self.inbound.lock().await;
        let mut out = Vec::new();
        while out.len() < cap {
            match rx.try_recv() {
                Ok(m) => out.push(ReceivedMail {
                    engine_id: m.engine_id.0.to_string(),
                    kind_name: m.kind_name,
                    payload_bytes: m.payload,
                    broadcast: m.broadcast,
                    origin: m.origin,
                }),
                Err(_) => break,
            }
        }
        serde_json::to_string(&out).map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        description = "Launch a substrate binary as a child process of the hub. The hub injects `AETHER_HUB_URL` so the child dials back to this hub instance. Blocks until the substrate completes its `Hello` handshake (or the handshake timeout fires — default 5 seconds, overridable via `timeout_ms`). Returns the assigned `engine_id` and the child `pid`. The hub owns the child for its lifetime; dropping the engine from the registry (socket disconnect or `terminate_substrate`) reaps the process."
    )]
    async fn spawn_substrate(
        &self,
        Parameters(args): Parameters<SpawnSubstrateArgs>,
    ) -> Result<String, McpError> {
        let handshake_timeout = args
            .timeout_ms
            .map(|ms| Duration::from_millis(ms as u64))
            .unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT);
        let opts = SpawnOpts {
            binary_path: PathBuf::from(args.binary_path),
            args: args.args,
            env: args.env,
            handshake_timeout,
        };
        let engine_id = crate::spawn::spawn_substrate(
            opts,
            self.state.hub_engine_addr,
            &self.state.pending_spawns,
            &self.state.engines,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("spawn failed: {e}"), None))?;

        let pid = self
            .state
            .engines
            .get(&engine_id)
            .map(|r| r.pid)
            .unwrap_or(0);
        let result = SpawnResult {
            engine_id: engine_id.0.to_string(),
            pid,
        };
        serde_json::to_string(&result).map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        description = "Terminate a substrate the hub previously spawned. Sends SIGTERM, waits up to `grace_ms` milliseconds (default 2000) for the child to exit, then escalates to SIGKILL if it's still running. Returns the exit code (if any) and a `sigkilled` flag indicating whether escalation was necessary. Errors if the engine id is unknown or refers to an externally connected substrate — the hub only terminates children it owns."
    )]
    async fn terminate_substrate(
        &self,
        Parameters(args): Parameters<TerminateSubstrateArgs>,
    ) -> Result<String, McpError> {
        let uuid = Uuid::parse_str(&args.engine_id).map_err(|e| {
            McpError::invalid_params(format!("engine_id is not a valid UUID: {e}"), None)
        })?;
        let id = EngineId(uuid);

        if self.state.engines.get(&id).is_none() {
            return Err(McpError::invalid_params(
                format!("unknown engine_id {}", args.engine_id),
                None,
            ));
        }

        let Some(child) = self.state.engines.take_child(&id) else {
            return Err(McpError::invalid_params(
                format!(
                    "engine {} is not hub-spawned; terminate it externally",
                    args.engine_id
                ),
                None,
            ));
        };

        let grace = args
            .grace_ms
            .map(|ms| Duration::from_millis(ms as u64))
            .unwrap_or(DEFAULT_TERMINATE_GRACE);

        let outcome = crate::spawn::terminate_substrate(child, grace)
            .await
            .map_err(|e| McpError::internal_error(format!("terminate failed: {e}"), None))?;

        let result = TerminateResult {
            engine_id: args.engine_id,
            sigkilled: outcome.sigkilled,
            exit_code: outcome.exit_code,
        };
        serde_json::to_string(&result).map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        description = "List all engines currently connected to the hub. Each item reports the hub-assigned engine_id, the engine's self-declared name/pid/version, and a `spawned` flag: `true` if the hub launched this engine as a child process (ADR-0009), `false` if it connected externally."
    )]
    async fn list_engines(&self) -> Result<String, McpError> {
        let engines: Vec<EngineInfo> = self
            .state
            .engines
            .list()
            .into_iter()
            .map(|r| EngineInfo {
                engine_id: r.id.0.to_string(),
                name: r.name,
                pid: r.pid,
                version: r.version,
                spawned: r.spawned,
            })
            .collect();
        serde_json::to_string(&engines).map_err(|e| McpError::internal_error(e.to_string(), None))
    }
}

#[tool_handler]
impl ServerHandler for Hub {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "aether-hub".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

/// Resolve one `MailSpec` against the registry, encode its payload,
/// and push to the engine's mail channel. Returns a human-readable
/// error string rather than `Err` so the batch driver can emit it as
/// a per-mail status without losing sibling success.
async fn deliver_one(
    spec: MailSpec,
    engines: &EngineRegistry,
    sender: SessionToken,
) -> Result<(), String> {
    let uuid = Uuid::parse_str(&spec.engine_id)
        .map_err(|e| format!("engine_id is not a valid UUID: {e}"))?;
    let id = EngineId(uuid);
    let record = engines
        .get(&id)
        .ok_or_else(|| format!("unknown engine_id {}", spec.engine_id))?;

    let payload = resolve_payload(&spec, &record)?;

    let frame = HubToEngine::Mail(MailFrame {
        recipient_name: spec.recipient_name,
        kind_name: spec.kind_name,
        payload,
        count: spec.count,
        sender,
    });
    record
        .mail_tx
        .send(frame)
        .await
        .map_err(|_| "engine disconnected".to_owned())
}

/// Decide which encoding path a mail goes through based on the
/// kind's descriptor. ADR-0019 PR 5 removed the `payload_bytes`
/// escape hatch — every mail must come through `params`, and the
/// hub picks cast vs postcard from the schema.
fn resolve_payload(spec: &MailSpec, record: &EngineRecord) -> Result<Vec<u8>, String> {
    let desc = find_kind(record, &spec.kind_name)
        .ok_or_else(|| format!("kind {:?} has no descriptor on this engine", spec.kind_name))?;
    match (&spec.params, &desc.encoding) {
        // Empty-payload shortcut: no params for a kind whose schema
        // is empty (Signal or Schema(Unit)) — both legal.
        (None, KindEncoding::Signal) => Ok(Vec::new()),
        (None, KindEncoding::Schema(aether_hub_protocol::SchemaType::Unit)) => Ok(Vec::new()),
        (None, _) => Err(format!(
            "kind {:?} requires `params` (no `payload_bytes` escape hatch — ADR-0019)",
            spec.kind_name
        )),
        (Some(p), KindEncoding::Signal) => {
            if !is_empty_params(p) {
                return Err(format!(
                    "kind {:?} is Signal; params must be absent or empty",
                    spec.kind_name
                ));
            }
            Ok(Vec::new())
        }
        (Some(p), KindEncoding::Pod { fields }) => encode_pod(p, fields).map_err(|e| e.to_string()),
        (Some(_), KindEncoding::Opaque) => Err(format!(
            "kind {:?} is Opaque — no encoder available, and `payload_bytes` was removed in ADR-0019 PR 5. Engine must publish a real Schema for this kind.",
            spec.kind_name
        )),
        (Some(p), KindEncoding::Schema(schema)) => {
            encode_schema(p, schema).map_err(|e| e.to_string())
        }
    }
}

fn find_kind<'a>(record: &'a EngineRecord, name: &str) -> Option<&'a KindDescriptor> {
    record.kinds.iter().find(|k| k.name == name)
}

fn is_empty_params(v: &serde_json::Value) -> bool {
    v.is_null() || v.as_object().is_some_and(|o| o.is_empty())
}

/// Bind an axum server on `addr` exposing the MCP tool surface at
/// `/mcp`. Returns on axum error. The caller owns the cancellation.
pub async fn run_mcp_server(addr: SocketAddr, state: Arc<HubState>) -> std::io::Result<()> {
    let factory_state = Arc::clone(&state);
    let service = StreamableHttpService::new(
        move || Ok(Hub::new(Arc::clone(&factory_state))),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    eprintln!("aether-hub: mcp listener bound on http://{bound}/mcp");
    axum::serve(listener, app).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::EngineRecord;
    use aether_hub_protocol::EngineId;
    use tokio::sync::mpsc;

    /// Build a `HubState` with spawn fields stubbed for tests that don't
    /// exercise the spawn path. Real spawn tests construct the state
    /// with `HubState::new` directly so they can inject a listener
    /// address.
    fn test_state(engines: EngineRegistry, sessions: SessionRegistry) -> Arc<HubState> {
        HubState::new(
            engines,
            sessions,
            PendingSpawns::new(),
            "127.0.0.1:0".parse().unwrap(),
        )
    }

    fn record(id_u128: u128) -> (EngineRecord, mpsc::Receiver<HubToEngine>) {
        // Default kinds: just `aether.tick` as Schema(Unit) so tests
        // that don't care about a specific schema can use the default
        // record and still send tick mail. ADR-0019 PR 5 removed
        // `payload_bytes`, so a kind without a descriptor is now
        // unreachable from `send_mail`.
        let tick = aether_hub_protocol::KindDescriptor {
            name: "aether.tick".into(),
            encoding: aether_hub_protocol::KindEncoding::Schema(
                aether_hub_protocol::SchemaType::Unit,
            ),
        };
        record_with_kinds(id_u128, vec![tick])
    }

    fn record_with_kinds(
        id_u128: u128,
        kinds: Vec<aether_hub_protocol::KindDescriptor>,
    ) -> (EngineRecord, mpsc::Receiver<HubToEngine>) {
        let (tx, rx) = mpsc::channel(16);
        let rec = EngineRecord {
            id: EngineId(Uuid::from_u128(id_u128)),
            name: format!("engine-{id_u128}"),
            pid: 42,
            version: "test".into(),
            kinds,
            mail_tx: tx,
            spawned: false,
        };
        (rec, rx)
    }

    #[tokio::test]
    async fn list_engines_reflects_registry() {
        let engines = EngineRegistry::new();
        let (a, _rx_a) = record(1);
        let (b, _rx_b) = record(2);
        engines.insert(a);
        engines.insert(b);
        let state = test_state(engines, SessionRegistry::new());
        let hub = Hub::new(state);

        let json = hub.list_engines().await.unwrap();
        let list: Vec<EngineInfo> = serde_json::from_str(&json).unwrap();
        assert_eq!(list.len(), 2);
    }

    fn spec(engine_id: String, kind: &str, params: Option<serde_json::Value>) -> MailSpec {
        MailSpec {
            engine_id,
            recipient_name: "hello".into(),
            kind_name: kind.into(),
            params,
            count: 1,
        }
    }

    async fn run(hub: &Hub, mails: Vec<MailSpec>) -> Vec<MailStatus> {
        let json = hub
            .send_mail(Parameters(SendMailArgs { mails }))
            .await
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[tokio::test]
    async fn send_mail_stamps_sender_with_session_token() {
        let engines = EngineRegistry::new();
        let sessions = SessionRegistry::new();
        let (rec, mut rx) = record(100);
        let id = rec.id;
        engines.insert(rec);
        let hub = Hub::new(test_state(engines, sessions));
        let expected = hub.session.token;

        let statuses = run(&hub, vec![spec(id.0.to_string(), "aether.tick", None)]).await;
        assert_eq!(statuses[0].status, "delivered");

        let HubToEngine::Mail(m) = rx.try_recv().unwrap() else {
            panic!()
        };
        assert_eq!(m.sender, expected);
        assert_ne!(m.sender, SessionToken::NIL);
    }

    #[tokio::test]
    async fn two_hubs_mint_distinct_session_tokens() {
        let state = test_state(EngineRegistry::new(), SessionRegistry::new());
        let a = Hub::new(Arc::clone(&state));
        let b = Hub::new(Arc::clone(&state));
        assert_ne!(a.session.token, b.session.token);
        // Both live in the registry.
        assert_eq!(state.sessions.len(), 2);
    }

    #[tokio::test]
    async fn dropping_hub_deregisters_session() {
        let state = test_state(EngineRegistry::new(), SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));
        let token = hub.session.token;
        assert!(state.sessions.get(&token).is_some());
        drop(hub);
        assert!(state.sessions.get(&token).is_none());
    }

    #[tokio::test]
    async fn send_mail_params_encodes_via_descriptor() {
        use aether_hub_protocol::{
            KindDescriptor, KindEncoding, PodField, PodFieldType, PodPrimitive,
        };

        let engines = EngineRegistry::new();
        let kinds = vec![KindDescriptor {
            name: "aether.mouse_move".into(),
            encoding: KindEncoding::Pod {
                fields: vec![
                    PodField {
                        name: "x".into(),
                        ty: PodFieldType::Scalar(PodPrimitive::F32),
                    },
                    PodField {
                        name: "y".into(),
                        ty: PodFieldType::Scalar(PodPrimitive::F32),
                    },
                ],
            },
        }];
        let (rec, mut rx) = record_with_kinds(3, kinds);
        let id = rec.id;
        engines.insert(rec);
        let hub = Hub::new(test_state(engines, SessionRegistry::new()));

        let statuses = run(
            &hub,
            vec![spec(
                id.0.to_string(),
                "aether.mouse_move",
                Some(serde_json::json!({"x": 10.5, "y": 20.0})),
            )],
        )
        .await;
        assert_eq!(statuses[0].status, "delivered");

        let HubToEngine::Mail(m) = rx.try_recv().unwrap() else {
            panic!()
        };
        let mut expected = Vec::new();
        expected.extend_from_slice(&10.5f32.to_le_bytes());
        expected.extend_from_slice(&20.0f32.to_le_bytes());
        assert_eq!(m.payload, expected);
    }

    #[tokio::test]
    async fn send_mail_signal_no_params() {
        use aether_hub_protocol::{KindDescriptor, KindEncoding};

        let engines = EngineRegistry::new();
        let kinds = vec![KindDescriptor {
            name: "aether.tick".into(),
            encoding: KindEncoding::Signal,
        }];
        let (rec, mut rx) = record_with_kinds(4, kinds);
        let id = rec.id;
        engines.insert(rec);
        let hub = Hub::new(test_state(engines, SessionRegistry::new()));

        let statuses = run(&hub, vec![spec(id.0.to_string(), "aether.tick", None)]).await;
        assert_eq!(statuses[0].status, "delivered");

        let HubToEngine::Mail(m) = rx.try_recv().unwrap() else {
            panic!()
        };
        assert!(m.payload.is_empty());
    }

    #[tokio::test]
    async fn send_mail_batch_reports_per_item_status() {
        let engines = EngineRegistry::new();
        let (rec, mut rx) = record(5);
        let id = rec.id;
        engines.insert(rec);
        let hub = Hub::new(test_state(engines, SessionRegistry::new()));

        let good = spec(id.0.to_string(), "aether.tick", None);
        let bad = spec(Uuid::from_u128(0xdead).to_string(), "aether.tick", None);
        let good2 = spec(id.0.to_string(), "aether.tick", None);

        let statuses = run(&hub, vec![good, bad, good2]).await;
        assert_eq!(statuses.len(), 3);
        assert_eq!(statuses[0].status, "delivered");
        assert!(statuses[1].status.starts_with("error: unknown engine_id"));
        assert_eq!(statuses[2].status, "delivered");

        // Two frames actually went through.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn send_mail_opaque_kind_rejects_with_helpful_error() {
        // ADR-0019 PR 5: `payload_bytes` is gone. If a kind is still
        // Opaque (foreign engine, V0 holdover), agents have no way to
        // send it — the error message should make the cause explicit.
        use aether_hub_protocol::{KindDescriptor, KindEncoding};

        let engines = EngineRegistry::new();
        let kinds = vec![KindDescriptor {
            name: "hello.opaque".into(),
            encoding: KindEncoding::Opaque,
        }];
        let (rec, _rx) = record_with_kinds(9, kinds);
        let id = rec.id;
        engines.insert(rec);
        let hub = Hub::new(test_state(engines, SessionRegistry::new()));

        let statuses = run(
            &hub,
            vec![spec(
                id.0.to_string(),
                "hello.opaque",
                Some(serde_json::json!({})),
            )],
        )
        .await;
        assert!(
            statuses[0].status.contains("Opaque"),
            "got: {}",
            statuses[0].status
        );
    }

    #[tokio::test]
    async fn describe_kinds_returns_descriptors() {
        use aether_hub_protocol::{KindDescriptor, KindEncoding};

        let kinds = vec![
            KindDescriptor {
                name: "aether.tick".into(),
                encoding: KindEncoding::Signal,
            },
            KindDescriptor {
                name: "hello.custom".into(),
                encoding: KindEncoding::Opaque,
            },
        ];
        let engines = EngineRegistry::new();
        let (rec, _rx) = record_with_kinds(11, kinds.clone());
        let id = rec.id;
        engines.insert(rec);
        let state = test_state(engines, SessionRegistry::new());
        let hub = Hub::new(state);

        let args = DescribeKindsArgs {
            engine_id: id.0.to_string(),
        };
        let json = hub.describe_kinds(Parameters(args)).await.unwrap();
        let back: Vec<KindDescriptor> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kinds);
    }

    #[tokio::test]
    async fn describe_kinds_unknown_engine_errors() {
        let state = test_state(EngineRegistry::new(), SessionRegistry::new());
        let hub = Hub::new(state);
        let args = DescribeKindsArgs {
            engine_id: Uuid::from_u128(1).to_string(),
        };
        let err = hub.describe_kinds(Parameters(args)).await.unwrap_err();
        assert!(format!("{err:?}").contains("unknown engine_id"));
    }

    async fn push_queued(sessions: &SessionRegistry, token: SessionToken, mail: QueuedMail) {
        sessions
            .get(&token)
            .expect("session")
            .mail_tx
            .send(mail)
            .await
            .expect("send");
    }

    fn queued(engine: u128, kind: &str, payload: Vec<u8>, broadcast: bool) -> QueuedMail {
        QueuedMail {
            engine_id: EngineId(Uuid::from_u128(engine)),
            kind_name: kind.into(),
            payload,
            broadcast,
            origin: None,
        }
    }

    async fn drain(hub: &Hub, max: Option<u32>) -> Vec<ReceivedMail> {
        let json = hub
            .receive_mail(Parameters(ReceiveMailArgs { max }))
            .await
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[tokio::test]
    async fn receive_mail_empty_queue_returns_empty_array() {
        let state = test_state(EngineRegistry::new(), SessionRegistry::new());
        let hub = Hub::new(state);
        let got = drain(&hub, None).await;
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn receive_mail_drains_everything_by_default() {
        let state = test_state(EngineRegistry::new(), SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));
        let token = hub.session.token;

        push_queued(
            &state.sessions,
            token,
            queued(7, "aether.observation.ping", vec![1, 2], false),
        )
        .await;
        push_queued(
            &state.sessions,
            token,
            queued(7, "aether.observation.world", vec![9], true),
        )
        .await;

        let got = drain(&hub, None).await;
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].kind_name, "aether.observation.ping");
        assert_eq!(got[0].payload_bytes, vec![1, 2]);
        assert!(!got[0].broadcast);
        assert_eq!(got[0].engine_id, Uuid::from_u128(7).to_string());
        assert!(got[0].origin.is_none());
        assert!(got[1].broadcast);
        assert!(got[1].origin.is_none());

        // Queue is now empty.
        let got = drain(&hub, None).await;
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn receive_mail_surfaces_origin() {
        let state = test_state(EngineRegistry::new(), SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));
        let token = hub.session.token;

        let mut with_origin = queued(3, "aether.observation.frame_stats", vec![], true);
        with_origin.origin = Some("render".into());
        push_queued(&state.sessions, token, with_origin).await;

        let got = drain(&hub, None).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].origin.as_deref(), Some("render"));
    }

    #[tokio::test]
    async fn receive_mail_respects_max() {
        let state = test_state(EngineRegistry::new(), SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));
        let token = hub.session.token;
        for i in 0..5u8 {
            push_queued(
                &state.sessions,
                token,
                queued(1, "aether.tick", vec![i], false),
            )
            .await;
        }

        let first = drain(&hub, Some(2)).await;
        assert_eq!(first.len(), 2);
        let second = drain(&hub, Some(2)).await;
        assert_eq!(second.len(), 2);
        let rest = drain(&hub, None).await;
        assert_eq!(rest.len(), 1);
    }

    #[tokio::test]
    async fn receive_mail_scoped_to_own_session() {
        // Push into session A's queue; session B's drain should see nothing.
        let state = test_state(EngineRegistry::new(), SessionRegistry::new());
        let hub_a = Hub::new(Arc::clone(&state));
        let hub_b = Hub::new(Arc::clone(&state));

        push_queued(
            &state.sessions,
            hub_a.session.token,
            queued(1, "aether.tick", vec![42], false),
        )
        .await;

        let got_b = drain(&hub_b, None).await;
        assert!(got_b.is_empty());

        let got_a = drain(&hub_a, None).await;
        assert_eq!(got_a.len(), 1);
        assert_eq!(got_a[0].payload_bytes, vec![42]);
    }

    #[tokio::test]
    async fn spawn_substrate_tool_surfaces_bad_path() {
        let state = HubState::new(
            EngineRegistry::new(),
            SessionRegistry::new(),
            PendingSpawns::new(),
            "127.0.0.1:1".parse().unwrap(),
        );
        let hub = Hub::new(state);

        let err = hub
            .spawn_substrate(Parameters(SpawnSubstrateArgs {
                binary_path: "/this/path/definitely/does/not/exist".into(),
                args: vec![],
                env: HashMap::new(),
                timeout_ms: None,
            }))
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("spawn failed"), "unexpected error: {msg}");
    }

    #[tokio::test]
    async fn terminate_substrate_rejects_unknown_engine() {
        let state = HubState::new(
            EngineRegistry::new(),
            SessionRegistry::new(),
            PendingSpawns::new(),
            "127.0.0.1:1".parse().unwrap(),
        );
        let hub = Hub::new(state);
        let err = hub
            .terminate_substrate(Parameters(TerminateSubstrateArgs {
                engine_id: Uuid::from_u128(0xdead).to_string(),
                grace_ms: None,
            }))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("unknown engine_id"));
    }

    #[tokio::test]
    async fn terminate_substrate_rejects_externally_connected_engine() {
        let engines = EngineRegistry::new();
        let (rec, _rx) = record(77);
        let id = rec.id;
        // rec.spawned is false (default) and no child is adopted.
        engines.insert(rec);
        let state = HubState::new(
            engines,
            SessionRegistry::new(),
            PendingSpawns::new(),
            "127.0.0.1:1".parse().unwrap(),
        );
        let hub = Hub::new(state);

        let err = hub
            .terminate_substrate(Parameters(TerminateSubstrateArgs {
                engine_id: id.0.to_string(),
                grace_ms: None,
            }))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("not hub-spawned"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn terminate_substrate_tool_kills_spawned_child() {
        use std::process::Stdio;
        use tokio::process::Command;

        let engines = EngineRegistry::new();
        let (mut rec, _rx) = record(88);
        rec.spawned = true;
        let id = rec.id;
        engines.insert(rec);

        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 60")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh");
        engines.adopt_child(id, child);

        let state = HubState::new(
            engines.clone(),
            SessionRegistry::new(),
            PendingSpawns::new(),
            "127.0.0.1:1".parse().unwrap(),
        );
        let hub = Hub::new(state);

        let json = hub
            .terminate_substrate(Parameters(TerminateSubstrateArgs {
                engine_id: id.0.to_string(),
                grace_ms: Some(2000),
            }))
            .await
            .expect("terminate ok");
        let result: TerminateResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.engine_id, id.0.to_string());
        assert!(!result.sigkilled, "sh should exit on SIGTERM within grace");

        // Child entry is gone from the registry (take_child fired).
        assert!(!engines.has_child(&id));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn spawn_substrate_tool_surfaces_timeout() {
        // /bin/sh + sleep will never handshake; configured timeout
        // fires and the tool turns the SpawnError into an MCP error.
        let state = HubState::new(
            EngineRegistry::new(),
            SessionRegistry::new(),
            PendingSpawns::new(),
            "127.0.0.1:1".parse().unwrap(),
        );
        let hub = Hub::new(state);

        let err = hub
            .spawn_substrate(Parameters(SpawnSubstrateArgs {
                binary_path: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 60".into()],
                env: HashMap::new(),
                timeout_ms: Some(150),
            }))
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("spawn failed"), "unexpected error: {msg}");
        assert!(msg.contains("handshake"), "expected timeout wording: {msg}");
    }
}
