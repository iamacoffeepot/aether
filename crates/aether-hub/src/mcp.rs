// Claude-facing MCP tool surface. ADR-0006 V0 + ADR-0007: three tools
// — `send_mail` (plural, schema-driven), `list_engines`, and
// `describe_kinds` (read-only introspection over the per-engine kind
// vocabulary shipped at handshake).
//
// The rmcp `Service` factory is invoked per session, so `Hub` is cheap
// to clone and shares a single `HubState` via `Arc`. Per-tool output is
// returned as a JSON-encoded `String`; rmcp wraps it into a
// `Content::text` automatically via `IntoContents`.

use std::net::SocketAddr;
use std::sync::Arc;

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

use crate::encoder::encode_pod;
use crate::registry::{EngineRecord, EngineRegistry};
use crate::session::{QueuedMail, SessionHandle, SessionRegistry};

/// Default port the hub binds for MCP clients. Overridable via
/// `AETHER_MCP_PORT`.
pub const DEFAULT_MCP_PORT: u16 = 8888;

/// Shared state across all rmcp sessions. Cheap to `Arc::clone` into
/// each per-session `Hub` instance.
pub struct HubState {
    engines: EngineRegistry,
    sessions: SessionRegistry,
}

impl HubState {
    pub fn new(engines: EngineRegistry, sessions: SessionRegistry) -> Arc<Self> {
        Arc::new(Self { engines, sessions })
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
    /// matching the engine's `#[repr(C)]` layout. Mutually exclusive
    /// with `payload_bytes`.
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    /// Raw payload bytes. Escape hatch for `Opaque` kinds or anything
    /// the hub doesn't know how to encode. Mutually exclusive with
    /// `params`.
    #[serde(default)]
    pub payload_bytes: Option<Vec<u8>>,
    /// Count carried on the mail frame. For single-struct payloads
    /// this is 1 (the default); for encoded slices it's the number
    /// of elements the bytes represent.
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
}

#[tool_router]
impl Hub {
    #[tool(
        description = "Send one or more mail items. Each item takes either `params` (structured, encoded via the engine's kind descriptor) or `payload_bytes` (raw escape hatch for Opaque kinds). The batch is best-effort: per-item status is returned and failures don't abort siblings. 'delivered' means the hub queued the frame to the engine's socket — not that the engine processed it."
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
        description = "List every kind the given engine declared at handshake, with enough structural detail for clients to build params for send_mail. Signal kinds take no payload; Pod kinds list their fields and primitive types; Opaque kinds must use the payload_bytes escape hatch on send_mail."
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
        description = "Drain observation mail addressed to this MCP session. Returns everything currently queued (up to `max`, if provided). Each item reports the originating engine_id, the kind name, the raw payload bytes, and a `broadcast` flag indicating whether this mail also went to every other attached session (true) or was targeted specifically at this one (false). Non-blocking: returns an empty array if nothing is queued."
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
                }),
                Err(_) => break,
            }
        }
        serde_json::to_string(&out).map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(description = "List all engines currently connected to the hub.")]
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

/// Decide which encoding path a mail goes through based on
/// `params`/`payload_bytes` presence and the kind's descriptor.
fn resolve_payload(spec: &MailSpec, record: &EngineRecord) -> Result<Vec<u8>, String> {
    match (&spec.params, &spec.payload_bytes) {
        (Some(_), Some(_)) => Err("params and payload_bytes are mutually exclusive".to_owned()),
        (None, Some(bytes)) => Ok(bytes.clone()),
        (Some(p), None) => {
            let desc = find_kind(record, &spec.kind_name).ok_or_else(|| {
                format!(
                    "kind {:?} has no descriptor on this engine; provide payload_bytes instead",
                    spec.kind_name
                )
            })?;
            match &desc.encoding {
                KindEncoding::Signal => {
                    if !is_empty_params(p) {
                        return Err(format!(
                            "kind {:?} is Signal; params must be absent or empty",
                            spec.kind_name
                        ));
                    }
                    Ok(Vec::new())
                }
                KindEncoding::Pod { fields } => encode_pod(p, fields).map_err(|e| e.to_string()),
                KindEncoding::Opaque => Err(format!(
                    "kind {:?} is Opaque; use payload_bytes",
                    spec.kind_name
                )),
            }
        }
        (None, None) => {
            // Neither given: permissible only if the descriptor says
            // Signal. Anything else is ambiguous — fail loudly.
            match find_kind(record, &spec.kind_name) {
                Some(desc) if matches!(desc.encoding, KindEncoding::Signal) => Ok(Vec::new()),
                _ => Err("missing params or payload_bytes".to_owned()),
            }
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

    fn record(id_u128: u128) -> (EngineRecord, mpsc::Receiver<HubToEngine>) {
        record_with_kinds(id_u128, vec![])
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
        let state = HubState::new(engines, SessionRegistry::new());
        let hub = Hub::new(state);

        let json = hub.list_engines().await.unwrap();
        let list: Vec<EngineInfo> = serde_json::from_str(&json).unwrap();
        assert_eq!(list.len(), 2);
    }

    fn spec(
        engine_id: String,
        kind: &str,
        params: Option<serde_json::Value>,
        payload_bytes: Option<Vec<u8>>,
    ) -> MailSpec {
        MailSpec {
            engine_id,
            recipient_name: "hello".into(),
            kind_name: kind.into(),
            params,
            payload_bytes,
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
        let hub = Hub::new(HubState::new(engines, sessions));
        let expected = hub.session.token;

        let statuses = run(
            &hub,
            vec![spec(id.0.to_string(), "aether.tick", None, Some(vec![]))],
        )
        .await;
        assert_eq!(statuses[0].status, "delivered");

        let HubToEngine::Mail(m) = rx.try_recv().unwrap() else {
            panic!()
        };
        assert_eq!(m.sender, expected);
        assert_ne!(m.sender, SessionToken::NIL);
    }

    #[tokio::test]
    async fn two_hubs_mint_distinct_session_tokens() {
        let state = HubState::new(EngineRegistry::new(), SessionRegistry::new());
        let a = Hub::new(Arc::clone(&state));
        let b = Hub::new(Arc::clone(&state));
        assert_ne!(a.session.token, b.session.token);
        // Both live in the registry.
        assert_eq!(state.sessions.len(), 2);
    }

    #[tokio::test]
    async fn dropping_hub_deregisters_session() {
        let state = HubState::new(EngineRegistry::new(), SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));
        let token = hub.session.token;
        assert!(state.sessions.get(&token).is_some());
        drop(hub);
        assert!(state.sessions.get(&token).is_none());
    }

    #[tokio::test]
    async fn send_mail_payload_bytes_passthrough() {
        let engines = EngineRegistry::new();
        let (rec, mut rx) = record(7);
        let id = rec.id;
        engines.insert(rec);
        let hub = Hub::new(HubState::new(engines, SessionRegistry::new()));

        let statuses = run(
            &hub,
            vec![spec(
                id.0.to_string(),
                "aether.tick",
                None,
                Some(vec![9, 9]),
            )],
        )
        .await;
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].status, "delivered");

        let frame = rx.try_recv().expect("frame");
        let HubToEngine::Mail(m) = frame else {
            panic!("wrong variant")
        };
        assert_eq!(m.payload, vec![9, 9]);
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
        let hub = Hub::new(HubState::new(engines, SessionRegistry::new()));

        let statuses = run(
            &hub,
            vec![spec(
                id.0.to_string(),
                "aether.mouse_move",
                Some(serde_json::json!({"x": 10.5, "y": 20.0})),
                None,
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
        let hub = Hub::new(HubState::new(engines, SessionRegistry::new()));

        let statuses = run(
            &hub,
            vec![spec(id.0.to_string(), "aether.tick", None, None)],
        )
        .await;
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
        let hub = Hub::new(HubState::new(engines, SessionRegistry::new()));

        let good = spec(id.0.to_string(), "aether.tick", None, Some(vec![]));
        let bad = spec(
            Uuid::from_u128(0xdead).to_string(),
            "aether.tick",
            None,
            Some(vec![]),
        );
        let good2 = spec(id.0.to_string(), "aether.tick", None, Some(vec![1]));

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
    async fn send_mail_params_and_bytes_are_mutually_exclusive() {
        let engines = EngineRegistry::new();
        let (rec, _rx) = record(6);
        let id = rec.id;
        engines.insert(rec);
        let hub = Hub::new(HubState::new(engines, SessionRegistry::new()));

        let both = MailSpec {
            engine_id: id.0.to_string(),
            recipient_name: "hello".into(),
            kind_name: "aether.tick".into(),
            params: Some(serde_json::json!({})),
            payload_bytes: Some(vec![]),
            count: 1,
        };
        let statuses = run(&hub, vec![both]).await;
        assert!(statuses[0].status.contains("mutually exclusive"));
    }

    #[tokio::test]
    async fn send_mail_opaque_kind_rejects_params() {
        use aether_hub_protocol::{KindDescriptor, KindEncoding};

        let engines = EngineRegistry::new();
        let kinds = vec![KindDescriptor {
            name: "hello.opaque".into(),
            encoding: KindEncoding::Opaque,
        }];
        let (rec, _rx) = record_with_kinds(9, kinds);
        let id = rec.id;
        engines.insert(rec);
        let hub = Hub::new(HubState::new(engines, SessionRegistry::new()));

        let statuses = run(
            &hub,
            vec![spec(
                id.0.to_string(),
                "hello.opaque",
                Some(serde_json::json!({})),
                None,
            )],
        )
        .await;
        assert!(statuses[0].status.contains("Opaque"));
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
        let state = HubState::new(engines, SessionRegistry::new());
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
        let state = HubState::new(EngineRegistry::new(), SessionRegistry::new());
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
        let state = HubState::new(EngineRegistry::new(), SessionRegistry::new());
        let hub = Hub::new(state);
        let got = drain(&hub, None).await;
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn receive_mail_drains_everything_by_default() {
        let state = HubState::new(EngineRegistry::new(), SessionRegistry::new());
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
        assert!(got[1].broadcast);

        // Queue is now empty.
        let got = drain(&hub, None).await;
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn receive_mail_respects_max() {
        let state = HubState::new(EngineRegistry::new(), SessionRegistry::new());
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
        let state = HubState::new(EngineRegistry::new(), SessionRegistry::new());
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
}
