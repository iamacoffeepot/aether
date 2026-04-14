// Claude-facing MCP tool surface. ADR-0006 V0: two tools —
// `send_mail` forwards to a specific engine, `list_engines` is
// read-only introspection.
//
// The rmcp `Service` factory is invoked per session, so `Hub` is cheap
// to clone and shares a single `HubState` via `Arc`. Per-tool output is
// returned as a JSON-encoded `String`; rmcp wraps it into a
// `Content::text` automatically via `IntoContents`.

use std::net::SocketAddr;
use std::sync::Arc;

use aether_hub_protocol::{EngineId, HubToEngine, MailFrame, Uuid};
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

use crate::registry::EngineRegistry;

/// Default port the hub binds for MCP clients. Overridable via
/// `AETHER_MCP_PORT`.
pub const DEFAULT_MCP_PORT: u16 = 8888;

/// Shared state across all rmcp sessions. Cheap to `Arc::clone` into
/// each per-session `Hub` instance.
pub struct HubState {
    engines: EngineRegistry,
}

impl HubState {
    pub fn new(engines: EngineRegistry) -> Arc<Self> {
        Arc::new(Self { engines })
    }
}

/// Per-session rmcp service. `ToolRouter<Self>` is built once in
/// `new`; state is shared via `Arc<HubState>`.
#[derive(Clone)]
pub struct Hub {
    state: Arc<HubState>,
    tool_router: ToolRouter<Self>,
}

impl Hub {
    pub fn new(state: Arc<HubState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
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
    /// Hub-assigned engine UUID as a string (from `list_engines`).
    pub engine_id: String,
    /// Mailbox name as registered by the engine.
    pub recipient_name: String,
    /// Kind name (e.g. `"aether.tick"`) the engine's registry knows.
    pub kind_name: String,
    /// Payload bytes. Encoding is per-kind and agreed between sender
    /// and the engine — V0 has no server-side schema validation.
    #[serde(default)]
    pub payload: Vec<u8>,
    /// Number of items the payload encodes. Typically 1.
    #[serde(default = "one")]
    pub count: u32,
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

#[tool_router]
impl Hub {
    #[tool(
        description = "Send mail to a mailbox on a specific engine. Returns 'delivered' when the hub queued the frame to the engine's socket (not when the engine processes it)."
    )]
    async fn send_mail(
        &self,
        Parameters(args): Parameters<SendMailArgs>,
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
        let frame = HubToEngine::Mail(MailFrame {
            recipient_name: args.recipient_name,
            kind_name: args.kind_name,
            payload: args.payload,
            count: args.count,
        });
        record
            .mail_tx
            .send(frame)
            .await
            .map_err(|_| McpError::internal_error("engine disconnected", None))?;
        Ok("delivered".into())
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
        let state = HubState::new(engines);
        let hub = Hub::new(state);

        let json = hub.list_engines().await.unwrap();
        let list: Vec<EngineInfo> = serde_json::from_str(&json).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn send_mail_forwards_to_engine_channel() {
        let engines = EngineRegistry::new();
        let (rec, mut rx) = record(7);
        let id = rec.id;
        engines.insert(rec);
        let state = HubState::new(engines);
        let hub = Hub::new(state);

        let args = SendMailArgs {
            engine_id: id.0.to_string(),
            recipient_name: "hello".into(),
            kind_name: "aether.tick".into(),
            payload: vec![9, 9],
            count: 3,
        };
        let ack = hub.send_mail(Parameters(args)).await.unwrap();
        assert_eq!(ack, "delivered");

        let frame = rx.try_recv().expect("expected a frame");
        match frame {
            HubToEngine::Mail(m) => {
                assert_eq!(m.recipient_name, "hello");
                assert_eq!(m.kind_name, "aether.tick");
                assert_eq!(m.payload, vec![9, 9]);
                assert_eq!(m.count, 3);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_mail_unknown_engine_errors() {
        let state = HubState::new(EngineRegistry::new());
        let hub = Hub::new(state);

        let args = SendMailArgs {
            engine_id: Uuid::from_u128(99).to_string(),
            recipient_name: "x".into(),
            kind_name: "y".into(),
            payload: vec![],
            count: 1,
        };
        let err = hub.send_mail(Parameters(args)).await.unwrap_err();
        assert!(format!("{err:?}").contains("unknown engine_id"));
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
        let state = HubState::new(engines);
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
        let state = HubState::new(EngineRegistry::new());
        let hub = Hub::new(state);
        let args = DescribeKindsArgs {
            engine_id: Uuid::from_u128(1).to_string(),
        };
        let err = hub.describe_kinds(Parameters(args)).await.unwrap_err();
        assert!(format!("{err:?}").contains("unknown engine_id"));
    }
}
