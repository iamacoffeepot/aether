//! Claude-facing MCP tool surface (ADR-0006 V0, ADR-0007, ADR-0008,
//! ADR-0009). Exposes eight tools for sending and receiving mail,
//! introspecting engine state, spawning and terminating substrates,
//! draining logs, and capturing frames.
//!
//! The rmcp `Service` factory is invoked per session, so `Hub` is cheap
//! to clone and shares a single `HubState` via `Arc`. Per-tool output
//! is returned as a JSON-encoded `String`; rmcp wraps it into a
//! `Content::text` automatically via `IntoContents`.
//!
//! Submodule split:
//!   - `args`: request/response shapes (pure data).
//!   - `codecs`: schema-driven encode/decode between tool JSON and
//!     wire bytes.
//!   - `tools`: the `#[tool_router] impl Hub` block and the
//!     `ServerHandler` impl.

use std::net::SocketAddr;
use std::sync::Arc;

use rmcp::{
    handler::server::router::tool::ToolRouter,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use tokio::sync::{Mutex, mpsc};

use crate::log_store::LogStore;
use crate::registry::EngineRegistry;
use crate::session::{QueuedMail, SessionHandle, SessionRegistry};
use crate::spawn::PendingSpawns;

pub(crate) mod args;
mod codecs;
mod tools;

// Bring args types and commonly-referenced protocol types into mcp's
// namespace so the tests module below can reach them via `use
// super::*;` without repeating imports for every case. Internal
// helpers in `codecs` stay `pub(super)` and are reachable from
// `tools` only.
#[cfg(test)]
use crate::wire::{HubToEngine, SessionToken, Uuid};
#[cfg(test)]
use args::{
    CaptureFrameArgs, DescribeKindsArgs, EngineInfo, MailSpec, MailStatus, ReceiveMailArgs,
    ReceivedMail, SendMailArgs, SpawnSubstrateArgs, TerminateResult, TerminateSubstrateArgs,
};
#[cfg(test)]
use base64::Engine as _;
#[cfg(test)]
use rmcp::handler::server::wrapper::Parameters;
#[cfg(test)]
use std::collections::HashMap;

/// Default port the hub binds for MCP clients. Overridable via
/// `AETHER_MCP_PORT`.
pub const DEFAULT_MCP_PORT: u16 = 8888;

/// Shared state across all rmcp sessions. Cheap to `Arc::clone` into
/// each per-session `Hub` instance.
pub struct HubState {
    pub(crate) engines: EngineRegistry,
    pub(crate) sessions: SessionRegistry,
    pub(crate) pending_spawns: PendingSpawns,
    /// ADR-0023 per-engine log buffers. Outlives engine records so
    /// post-mortem `engine_logs` polls succeed after a substrate exit.
    pub(crate) logs: LogStore,
    /// Address of the hub's engine TCP listener. Injected as
    /// `AETHER_HUB_URL` into spawned substrates so they dial back to
    /// this hub instance.
    pub(crate) hub_engine_addr: SocketAddr,
}

impl HubState {
    pub fn new(
        engines: EngineRegistry,
        sessions: SessionRegistry,
        pending_spawns: PendingSpawns,
        logs: LogStore,
        hub_engine_addr: SocketAddr,
    ) -> Arc<Self> {
        Arc::new(Self {
            engines,
            sessions,
            pending_spawns,
            logs,
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
    pub(crate) state: Arc<HubState>,
    pub(crate) tool_router: ToolRouter<Self>,
    pub(crate) session: Arc<SessionHandle>,
    /// Drain for this session's inbound observation mail.
    /// `receive_mail` pulls from it non-blocking; wrapping in an
    /// `Arc<Mutex<_>>` lets rmcp's per-tool-call clones share the same
    /// receiver.
    pub(crate) inbound: Arc<Mutex<mpsc::Receiver<QueuedMail>>>,
}

// `Hub::new` lives in `tools.rs` next to the `#[tool_router]` impl —
// rmcp's macro emits a private `Self::tool_router()` that the
// constructor calls, and a sibling impl block in the same module can
// reach it without widening the macro-generated visibility.

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
    eprintln!("aether-substrate-hub: mcp listener bound on http://{bound}/mcp");
    axum::serve(listener, app).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::EngineRecord;
    use crate::wire::EngineId;
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
            LogStore::new(),
            "127.0.0.1:0".parse().unwrap(),
        )
    }

    fn record(id_u128: u128) -> (EngineRecord, mpsc::Receiver<HubToEngine>) {
        // Default kinds: just `aether.tick` as Schema(Unit) so tests
        // that don't care about a specific schema can use the default
        // record and still send tick mail. ADR-0019 PR 5 removed
        // `payload_bytes`, so a kind without a descriptor is now
        // unreachable from `send_mail`.
        let tick = aether_data::KindDescriptor {
            name: "aether.tick".into(),
            schema: aether_data::SchemaType::Unit,
            is_stream: false,
        };
        record_with_kinds(id_u128, vec![tick])
    }

    fn record_with_kinds(
        id_u128: u128,
        kinds: Vec<aether_data::KindDescriptor>,
    ) -> (EngineRecord, mpsc::Receiver<HubToEngine>) {
        let (tx, rx) = mpsc::channel(16);
        let rec = EngineRecord {
            id: EngineId(Uuid::from_u128(id_u128)),
            name: format!("engine-{id_u128}"),
            pid: 42,
            version: "test".into(),
            kinds,
            components: HashMap::new(),
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
        use aether_data::{KindDescriptor, NamedField, Primitive, SchemaType};
        let engines = EngineRegistry::new();
        let kinds = vec![KindDescriptor {
            name: "aether.mouse_move".into(),
            schema: SchemaType::Struct {
                repr_c: true,
                fields: vec![
                    NamedField {
                        name: "x".into(),
                        ty: SchemaType::Scalar(Primitive::F32),
                    },
                    NamedField {
                        name: "y".into(),
                        ty: SchemaType::Scalar(Primitive::F32),
                    },
                ]
                .into(),
            },
            is_stream: false,
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
    async fn send_mail_unit_kind_no_params() {
        use aether_data::{KindDescriptor, SchemaType};
        let engines = EngineRegistry::new();
        let kinds = vec![KindDescriptor {
            name: "aether.tick".into(),
            schema: SchemaType::Unit,
            is_stream: false,
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
    async fn send_mail_unknown_kind_errors() {
        // ADR-0019 cleanup: there's no fallback path for kinds without
        // a descriptor — an agent gets a clear "no descriptor" error.
        let engines = EngineRegistry::new();
        let (rec, _rx) = record_with_kinds(9, vec![]);
        let id = rec.id;
        engines.insert(rec);
        let hub = Hub::new(test_state(engines, SessionRegistry::new()));

        let statuses = run(
            &hub,
            vec![spec(
                id.0.to_string(),
                "hello.unknown",
                Some(serde_json::json!({})),
            )],
        )
        .await;
        assert!(
            statuses[0].status.contains("no descriptor"),
            "got: {}",
            statuses[0].status
        );
    }

    #[tokio::test]
    async fn describe_kinds_returns_descriptors() {
        use aether_data::{KindDescriptor, NamedField, Primitive, SchemaType};
        let kinds = vec![
            KindDescriptor {
                name: "aether.tick".into(),
                schema: SchemaType::Unit,
                is_stream: false,
            },
            KindDescriptor {
                name: "hello.note".into(),
                schema: SchemaType::Struct {
                    repr_c: false,
                    fields: vec![NamedField {
                        name: "body".into(),
                        ty: SchemaType::String,
                    }]
                    .into(),
                },
                is_stream: false,
            },
            KindDescriptor {
                name: "hello.cast".into(),
                schema: SchemaType::Struct {
                    repr_c: true,
                    fields: vec![NamedField {
                        name: "n".into(),
                        ty: SchemaType::Scalar(Primitive::U32),
                    }]
                    .into(),
                },
                is_stream: false,
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

    // ADR-0033: `describe_component` returns stored capabilities for
    // `(engine, mailbox)`. `load_component` normally populates the
    // record; tests seed it directly via `upsert_component` to isolate
    // the lookup path from the full load flow.

    fn stub_capabilities() -> args::ComponentCapabilitiesWire {
        args::ComponentCapabilitiesWire {
            handlers: vec![
                args::HandlerCapabilityWire {
                    id: 42,
                    name: "aether.tick".into(),
                    doc: Some("heartbeat".into()),
                },
                args::HandlerCapabilityWire {
                    id: 0xff,
                    name: "aether.ping".into(),
                    doc: None,
                },
            ],
            fallback: None,
            doc: Some("A canary component.".into()),
        }
    }

    #[tokio::test]
    async fn describe_component_returns_stored_capabilities() {
        use crate::registry::ComponentRecord;
        use aether_data::tagged_id::{self, Tag};
        use aether_data::with_tag;

        let engines = EngineRegistry::new();
        let (rec, _rx) = record(50);
        let id = rec.id;
        engines.insert(rec);
        let capabilities = stub_capabilities();
        // ADR-0064: registry stores raw u64 with tag bits set; the
        // wire form is the tagged-string encoding of that u64.
        let mailbox_id = with_tag(Tag::Mailbox, 7);
        engines.upsert_component(
            &id,
            aether_data::MailboxId(mailbox_id),
            ComponentRecord {
                name: "hello".into(),
                capabilities: capabilities.clone(),
            },
        );
        let hub = Hub::new(test_state(engines, SessionRegistry::new()));

        let json = hub
            .describe_component(Parameters(args::DescribeComponentArgs {
                engine_id: id.0.to_string(),
                mailbox_id: tagged_id::encode(mailbox_id).unwrap(),
            }))
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(response["name"], "hello");
        assert_eq!(response["doc"], "A canary component.");
        let receives = response["receives"].as_array().unwrap();
        assert_eq!(receives.len(), 2);
        assert_eq!(receives[0]["name"], "aether.tick");
        assert_eq!(receives[0]["doc"], "heartbeat");
        assert_eq!(receives[1]["name"], "aether.ping");
        assert!(receives[1]["doc"].is_null());
        assert!(response["fallback"].is_null());
    }

    #[tokio::test]
    async fn describe_component_unknown_mailbox_errors() {
        use aether_data::tagged_id::{self, Tag};
        use aether_data::with_tag;

        let engines = EngineRegistry::new();
        let (rec, _rx) = record(51);
        let id = rec.id;
        engines.insert(rec);
        let hub = Hub::new(test_state(engines, SessionRegistry::new()));

        let bogus = tagged_id::encode(with_tag(Tag::Mailbox, 999)).unwrap();
        let err = hub
            .describe_component(Parameters(args::DescribeComponentArgs {
                engine_id: id.0.to_string(),
                mailbox_id: bogus.clone(),
            }))
            .await
            .unwrap_err();
        // The error message echoes the original tagged-string form so
        // the agent sees back what they passed in (not a re-encoded
        // u64). Asserting on the prefix keeps the test robust to the
        // base32 body's exact bytes.
        let msg = format!("{err:?}");
        assert!(msg.contains("no component at mailbox_id"));
        assert!(msg.contains(&bogus));
    }

    #[tokio::test]
    async fn describe_component_unknown_engine_errors() {
        use aether_data::tagged_id::{self, Tag};
        use aether_data::with_tag;

        let state = test_state(EngineRegistry::new(), SessionRegistry::new());
        let hub = Hub::new(state);

        let err = hub
            .describe_component(Parameters(args::DescribeComponentArgs {
                engine_id: Uuid::from_u128(0xdead).to_string(),
                mailbox_id: tagged_id::encode(with_tag(Tag::Mailbox, 0)).unwrap(),
            }))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("unknown engine_id"));
    }

    #[tokio::test]
    async fn describe_component_rejects_malformed_mailbox_id() {
        // ADR-0064: a bare-number mailbox id is no longer valid wire
        // form. Reject at the boundary with a typed error so the
        // agent sees a clear "looks malformed" signal rather than a
        // mysterious lookup miss.
        let engines = EngineRegistry::new();
        let (rec, _rx) = record(52);
        let id = rec.id;
        engines.insert(rec);
        let hub = Hub::new(test_state(engines, SessionRegistry::new()));

        let err = hub
            .describe_component(Parameters(args::DescribeComponentArgs {
                engine_id: id.0.to_string(),
                mailbox_id: "999".into(),
            }))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").to_lowercase().contains("mailbox_id"));
    }

    #[tokio::test]
    async fn describe_component_rejects_wrong_tag() {
        // Passing a kind id (`knd-...`) where a mailbox id is expected
        // is exactly what the tag bits exist to catch. Verify the
        // boundary surfaces it as a tag-mismatch error rather than
        // silently treating the kind id as a mailbox id and missing
        // the lookup.
        use aether_data::tagged_id::{self, Tag};
        use aether_data::with_tag;

        let engines = EngineRegistry::new();
        let (rec, _rx) = record(53);
        let id = rec.id;
        engines.insert(rec);
        let hub = Hub::new(test_state(engines, SessionRegistry::new()));

        let kind_form = tagged_id::encode(with_tag(Tag::Kind, 42)).unwrap();
        let err = hub
            .describe_component(Parameters(args::DescribeComponentArgs {
                engine_id: id.0.to_string(),
                mailbox_id: kind_form,
            }))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("tag mismatch"));
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
    async fn receive_mail_decodes_params_against_descriptor() {
        // FrameStats-shaped: cast struct with two u64 fields. The
        // engine ships raw cast bytes; the hub looks up the descriptor
        // and lifts them into structured `params`.
        use aether_data::{KindDescriptor, NamedField, Primitive, SchemaType};
        let engines = EngineRegistry::new();
        let kinds = vec![KindDescriptor {
            name: "aether.observation.frame_stats".into(),
            schema: SchemaType::Struct {
                repr_c: true,
                fields: vec![
                    NamedField {
                        name: "frame".into(),
                        ty: SchemaType::Scalar(Primitive::U64),
                    },
                    NamedField {
                        name: "triangles".into(),
                        ty: SchemaType::Scalar(Primitive::U64),
                    },
                ]
                .into(),
            },
            is_stream: false,
        }];
        let (rec, _rx) = record_with_kinds(33, kinds);
        let id = rec.id;
        engines.insert(rec);
        let state = test_state(engines, SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));
        let token = hub.session.token;

        let mut payload = Vec::new();
        payload.extend_from_slice(&120u64.to_le_bytes());
        payload.extend_from_slice(&7u64.to_le_bytes());
        push_queued(
            &state.sessions,
            token,
            QueuedMail {
                engine_id: id,
                kind_name: "aether.observation.frame_stats".into(),
                payload,
                broadcast: true,
                origin: None,
            },
        )
        .await;

        let got = drain(&hub, None).await;
        assert_eq!(got.len(), 1);
        let item = &got[0];
        assert_eq!(
            item.params,
            Some(serde_json::json!({"frame": 120u64, "triangles": 7u64}))
        );
        assert!(item.decode_error.is_none());
        // payload_bytes still populated alongside params (escape hatch).
        assert_eq!(item.payload_bytes.len(), 16);
    }

    #[tokio::test]
    async fn receive_mail_decode_failure_populates_error() {
        // Descriptor declares 8-byte u64; hub gets only 2 bytes.
        // Decoder must surface a Truncated error in `decode_error` and
        // leave `params` null without dropping the item.
        use aether_data::{KindDescriptor, NamedField, Primitive, SchemaType};
        let engines = EngineRegistry::new();
        let kinds = vec![KindDescriptor {
            name: "demo.short".into(),
            schema: SchemaType::Struct {
                repr_c: true,
                fields: vec![NamedField {
                    name: "n".into(),
                    ty: SchemaType::Scalar(Primitive::U64),
                }]
                .into(),
            },
            is_stream: false,
        }];
        let (rec, _rx) = record_with_kinds(34, kinds);
        let id = rec.id;
        engines.insert(rec);
        let state = test_state(engines, SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));
        let token = hub.session.token;

        push_queued(
            &state.sessions,
            token,
            QueuedMail {
                engine_id: id,
                kind_name: "demo.short".into(),
                payload: vec![1, 2],
                broadcast: false,
                origin: None,
            },
        )
        .await;

        let got = drain(&hub, None).await;
        assert_eq!(got.len(), 1);
        let item = &got[0];
        assert!(item.params.is_none());
        let err = item
            .decode_error
            .as_deref()
            .expect("decode error populated");
        assert!(err.contains("truncated"), "got: {err}");
        assert_eq!(item.payload_bytes, vec![1, 2]);
    }

    #[tokio::test]
    async fn receive_mail_unknown_kind_falls_back_to_bytes() {
        // Engine sent a kind it never declared at handshake (or the
        // descriptor was lost). Decode reports the missing descriptor;
        // bytes survive for the agent to inspect.
        let engines = EngineRegistry::new();
        let (rec, _rx) = record_with_kinds(35, vec![]);
        let id = rec.id;
        engines.insert(rec);
        let state = test_state(engines, SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));
        let token = hub.session.token;

        push_queued(
            &state.sessions,
            token,
            QueuedMail {
                engine_id: id,
                kind_name: "demo.unknown".into(),
                payload: vec![9, 9, 9],
                broadcast: false,
                origin: None,
            },
        )
        .await;

        let got = drain(&hub, None).await;
        assert_eq!(got.len(), 1);
        assert!(got[0].params.is_none());
        let err = got[0].decode_error.as_deref().unwrap();
        assert!(err.contains("no descriptor"), "got: {err}");
        assert_eq!(got[0].payload_bytes, vec![9, 9, 9]);
    }

    #[tokio::test]
    async fn receive_mail_unit_kind_decodes_to_null_params() {
        use aether_data::{KindDescriptor, SchemaType};
        let engines = EngineRegistry::new();
        let kinds = vec![KindDescriptor {
            name: "aether.observation.ping".into(),
            schema: SchemaType::Unit,
            is_stream: false,
        }];
        let (rec, _rx) = record_with_kinds(36, kinds);
        let id = rec.id;
        engines.insert(rec);
        let state = test_state(engines, SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));
        let token = hub.session.token;

        push_queued(
            &state.sessions,
            token,
            QueuedMail {
                engine_id: id,
                kind_name: "aether.observation.ping".into(),
                payload: vec![],
                broadcast: false,
                origin: None,
            },
        )
        .await;

        // Read raw JSON rather than going through `drain` — `Option<Value>`
        // collapses `Some(Null)` and `None` over deserialization, so the
        // distinction between "decoded to null" and "no value" is only
        // observable in the wire form. The MCP client sees the wire form.
        let json = hub
            .receive_mail(Parameters(ReceiveMailArgs { max: None }))
            .await
            .unwrap();
        let raw: serde_json::Value = serde_json::from_str(&json).unwrap();
        let item = &raw.as_array().unwrap()[0];
        assert_eq!(item.get("params"), Some(&serde_json::Value::Null));
        // decode_error skipped on success thanks to `skip_serializing_if`.
        assert!(item.get("decode_error").is_none());
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

    /// Descriptor for `aether.control.capture_frame` that matches
    /// the real substrate's schema: a postcard struct with `mails`
    /// and `after_mails` fields, both `Vec<MailEnvelope>`. Used
    /// across capture_frame tests so they all exercise the same
    /// wire shape.
    fn capture_frame_kind_descriptor() -> aether_data::KindDescriptor {
        use aether_data::{KindDescriptor, NamedField, Primitive, SchemaCell, SchemaType};
        let envelope = SchemaType::Struct {
            repr_c: false,
            fields: vec![
                NamedField {
                    name: "recipient_name".into(),
                    ty: SchemaType::String,
                },
                NamedField {
                    name: "kind_name".into(),
                    ty: SchemaType::String,
                },
                NamedField {
                    name: "payload".into(),
                    ty: SchemaType::Bytes,
                },
                NamedField {
                    name: "count".into(),
                    ty: SchemaType::Scalar(Primitive::U32),
                },
            ]
            .into(),
        };
        KindDescriptor {
            name: "aether.control.capture_frame".into(),
            schema: SchemaType::Struct {
                repr_c: false,
                fields: vec![
                    NamedField {
                        name: "mails".into(),
                        ty: SchemaType::Vec(SchemaCell::owned(envelope.clone())),
                    },
                    NamedField {
                        name: "after_mails".into(),
                        ty: SchemaType::Vec(SchemaCell::owned(envelope)),
                    },
                ]
                .into(),
            },
            is_stream: false,
        }
    }

    #[tokio::test]
    async fn capture_frame_returns_image_on_substrate_reply() {
        // End-to-end of the MCP-side plumbing with a stubbed "substrate":
        // the tool sends request mail (which we drain off the engine's
        // mail_tx and ignore), then we inject a postcard-encoded
        // `Ok { png }` reply through the session's reply registry. The
        // tool should return a `CallToolResult` carrying the PNG as
        // image content.

        let engines = EngineRegistry::new();
        let kinds = vec![capture_frame_kind_descriptor()];
        let (rec, mut rx) = record_with_kinds(777, kinds);
        let id = rec.id;
        engines.insert(rec);
        let state = test_state(engines, SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));
        let session_token = hub.session.token;

        // Stub substrate: wait until the tool has sent its request
        // (which means the reply registration is already installed),
        // then push the reply through the session's mail tx path.
        let sessions_clone = state.sessions.clone();
        let substrate_task = tokio::spawn(async move {
            // Drain the request the tool sent to the engine.
            let _req = rx.recv().await.expect("tool should send request");
            // Build the reply payload — postcard-encoded
            // `CaptureFrameResult::Ok { png: vec![PNG...] }`. Use the
            // local wire mirror so this test doesn't depend on
            // aether-kinds being present.
            #[derive(serde::Serialize)]
            enum ReplyMirror {
                #[allow(dead_code)]
                Ok { png: Vec<u8> },
                #[allow(dead_code)]
                Err { error: String },
            }
            let fake_png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4];
            let payload = postcard::to_allocvec(&ReplyMirror::Ok {
                png: fake_png.clone(),
            })
            .expect("encode");
            // Push as if it were a `ClaudeAddress::Session(token)`
            // mail landing on this session. The await-reply diversion
            // runs inside the engine_reader's `try_deliver`; in tests
            // we short-circuit by driving the diversion directly.
            let record = sessions_clone.get(&session_token).expect("session");
            let kind: String = "aether.control.capture_frame_result".into();
            let queued = QueuedMail {
                engine_id: id,
                kind_name: kind.clone(),
                payload,
                broadcast: false,
                origin: None,
            };
            let remainder = record.replies.try_deliver(&kind, queued);
            assert!(
                remainder.is_none(),
                "reply registry should have consumed the mail; tool wasn't awaiting"
            );
        });

        let result = hub
            .capture_frame(Parameters(CaptureFrameArgs {
                engine_id: id.0.to_string(),
                mails: vec![],
                after_mails: vec![],
                timeout_ms: Some(2_000),
            }))
            .await
            .expect("tool should succeed");

        substrate_task.await.expect("stub substrate task");

        assert_eq!(result.content.len(), 1);
        let image = result.content[0]
            .as_image()
            .expect("content should be image");
        assert_eq!(image.mime_type, "image/png");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .expect("valid base64");
        assert_eq!(
            decoded,
            vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4]
        );
    }

    #[tokio::test]
    async fn capture_frame_surfaces_substrate_err_variant() {
        let engines = EngineRegistry::new();
        let kinds = vec![capture_frame_kind_descriptor()];
        let (rec, mut rx) = record_with_kinds(778, kinds);
        let id = rec.id;
        engines.insert(rec);
        let state = test_state(engines, SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));
        let session_token = hub.session.token;
        let sessions_clone = state.sessions.clone();

        let substrate_task = tokio::spawn(async move {
            let _req = rx.recv().await.expect("tool should send request");
            #[derive(serde::Serialize)]
            enum ReplyMirror {
                #[allow(dead_code)]
                Ok {
                    png: Vec<u8>,
                },
                Err {
                    error: String,
                },
            }
            let payload = postcard::to_allocvec(&ReplyMirror::Err {
                error: "gpu lost".into(),
            })
            .expect("encode");
            let record = sessions_clone.get(&session_token).expect("session");
            let kind: String = "aether.control.capture_frame_result".into();
            let queued = QueuedMail {
                engine_id: id,
                kind_name: kind.clone(),
                payload,
                broadcast: false,
                origin: None,
            };
            record.replies.try_deliver(&kind, queued);
        });

        let err = hub
            .capture_frame(Parameters(CaptureFrameArgs {
                engine_id: id.0.to_string(),
                mails: vec![],
                after_mails: vec![],
                timeout_ms: Some(2_000),
            }))
            .await
            .unwrap_err();

        substrate_task.await.expect("stub substrate task");

        let msg = format!("{err:?}");
        assert!(msg.contains("gpu lost"), "unexpected error: {msg}");
    }

    #[tokio::test]
    async fn capture_frame_rejects_second_concurrent_call() {
        let engines = EngineRegistry::new();
        let kinds = vec![capture_frame_kind_descriptor()];
        let (rec, _rx) = record_with_kinds(779, kinds);
        let id = rec.id;
        engines.insert(rec);
        let state = test_state(engines, SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));

        // Pre-register a waiter for capture_frame_result via the same
        // path the tool uses, so the second call sees a conflict
        // without us actually racing two tool calls.
        let (_guard, _rx) = hub
            .session
            .replies
            .register("aether.control.capture_frame_result".into())
            .expect("first registration");

        let err = hub
            .capture_frame(Parameters(CaptureFrameArgs {
                engine_id: id.0.to_string(),
                mails: vec![],
                after_mails: vec![],
                timeout_ms: Some(100),
            }))
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("already in flight"),
            "expected conflict: {msg}"
        );
    }

    #[tokio::test]
    async fn capture_frame_times_out_when_no_reply() {
        let engines = EngineRegistry::new();
        let kinds = vec![capture_frame_kind_descriptor()];
        let (rec, mut rx) = record_with_kinds(780, kinds);
        let id = rec.id;
        engines.insert(rec);
        let state = test_state(engines, SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));

        // Drain the request the tool will send so its mail_tx doesn't
        // back-pressure, but never reply. The timeout fires.
        let drain = tokio::spawn(async move {
            let _ = rx.recv().await;
        });

        let err = hub
            .capture_frame(Parameters(CaptureFrameArgs {
                engine_id: id.0.to_string(),
                mails: vec![],
                after_mails: vec![],
                timeout_ms: Some(50),
            }))
            .await
            .unwrap_err();
        drain.await.ok();
        let msg = format!("{err:?}");
        assert!(msg.contains("timed out"), "expected timeout: {msg}");
    }

    #[tokio::test]
    async fn capture_frame_encodes_bundle_into_request_payload() {
        // Verify the bundle path: tool takes a `mails` array, resolves
        // each MailSpec via its kind descriptor into bytes, wraps
        // into a CaptureFrame, and the request-mail payload on the
        // wire carries a valid postcard-encoded bundle the substrate
        // could decode.
        use aether_data::{KindDescriptor, SchemaType};
        let engines = EngineRegistry::new();
        // Two kinds on this engine: capture_frame plus a `demo.tick`
        // Unit kind we'll bundle into the capture request.
        let kinds = vec![
            capture_frame_kind_descriptor(),
            KindDescriptor {
                name: "demo.tick".into(),
                schema: SchemaType::Unit,
                is_stream: false,
            },
        ];
        let (rec, mut rx) = record_with_kinds(781, kinds);
        let id = rec.id;
        engines.insert(rec);
        let state = test_state(engines, SessionRegistry::new());
        let hub = Hub::new(Arc::clone(&state));
        let session_token = hub.session.token;
        let sessions_clone = state.sessions.clone();

        let substrate_task = tokio::spawn(async move {
            let req = rx.recv().await.expect("tool should send request");
            // Decode the CaptureFrame the tool built — a postcard
            // struct with a `Vec<MailEnvelope>` field. Using a local
            // mirror keeps this test in the hub crate.
            #[derive(serde::Deserialize, Debug)]
            struct EnvelopeMirror {
                recipient_name: String,
                kind_name: String,
                payload: Vec<u8>,
                count: u32,
            }
            #[derive(serde::Deserialize, Debug)]
            struct CaptureFrameMirror {
                mails: Vec<EnvelopeMirror>,
                after_mails: Vec<EnvelopeMirror>,
            }
            let HubToEngine::Mail(frame) = req else {
                panic!("expected Mail");
            };
            let decoded: CaptureFrameMirror =
                postcard::from_bytes(&frame.payload).expect("decode CaptureFrame");
            assert_eq!(decoded.mails.len(), 1);
            assert_eq!(decoded.mails[0].recipient_name, "target.box");
            assert_eq!(decoded.mails[0].kind_name, "demo.tick");
            assert_eq!(decoded.mails[0].count, 1);
            // `demo.tick` is Unit — payload is zero bytes.
            assert!(decoded.mails[0].payload.is_empty());
            // Test passes `after_mails: vec![]` — empty bundle.
            assert!(decoded.after_mails.is_empty());

            // Reply with a fake PNG so the tool completes.
            #[derive(serde::Serialize)]
            enum ReplyMirror {
                #[allow(dead_code)]
                Ok { png: Vec<u8> },
                #[allow(dead_code)]
                Err { error: String },
            }
            let reply = postcard::to_allocvec(&ReplyMirror::Ok {
                png: vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 42],
            })
            .expect("encode");
            let record = sessions_clone.get(&session_token).expect("session");
            let kind: String = "aether.control.capture_frame_result".into();
            let queued = QueuedMail {
                engine_id: id,
                kind_name: kind.clone(),
                payload: reply,
                broadcast: false,
                origin: None,
            };
            record.replies.try_deliver(&kind, queued);
        });

        let result = hub
            .capture_frame(Parameters(CaptureFrameArgs {
                engine_id: id.0.to_string(),
                mails: vec![MailSpec {
                    engine_id: id.0.to_string(),
                    recipient_name: "target.box".into(),
                    kind_name: "demo.tick".into(),
                    params: None,
                    count: 1,
                }],
                after_mails: vec![],
                timeout_ms: Some(2_000),
            }))
            .await
            .expect("bundle tool should succeed");

        substrate_task.await.expect("stub substrate");
        assert!(result.content[0].as_image().is_some());
    }

    #[tokio::test]
    async fn capture_frame_rejects_bundle_with_unknown_kind() {
        // Bundle contains a kind the engine doesn't declare. The tool
        // should abort before sending anything and surface a clear
        // invalid-params error naming the offending envelope.
        let engines = EngineRegistry::new();
        // Only capture_frame is declared; `demo.mystery` is not.
        let kinds = vec![capture_frame_kind_descriptor()];
        let (rec, _rx) = record_with_kinds(782, kinds);
        let id = rec.id;
        engines.insert(rec);
        let state = test_state(engines, SessionRegistry::new());
        let hub = Hub::new(state);

        let err = hub
            .capture_frame(Parameters(CaptureFrameArgs {
                engine_id: id.0.to_string(),
                mails: vec![MailSpec {
                    engine_id: id.0.to_string(),
                    recipient_name: "any".into(),
                    kind_name: "demo.mystery".into(),
                    params: None,
                    count: 1,
                }],
                after_mails: vec![],
                timeout_ms: Some(1_000),
            }))
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("no descriptor") && msg.contains("demo.mystery"),
            "expected abort with kind name: {msg}"
        );
    }

    #[tokio::test]
    async fn spawn_substrate_tool_surfaces_bad_path() {
        let state = HubState::new(
            EngineRegistry::new(),
            SessionRegistry::new(),
            PendingSpawns::new(),
            LogStore::new(),
            "127.0.0.1:1".parse().unwrap(),
        );
        let hub = Hub::new(state);

        let err = hub
            .spawn_substrate(Parameters(SpawnSubstrateArgs {
                binary_path: "/this/path/definitely/does/not/exist".into(),
                args: vec![],
                env: HashMap::new(),
                timeout_ms: None,
                components: vec![],
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
            LogStore::new(),
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
            LogStore::new(),
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
            LogStore::new(),
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
            LogStore::new(),
            "127.0.0.1:1".parse().unwrap(),
        );
        let hub = Hub::new(state);

        let err = hub
            .spawn_substrate(Parameters(SpawnSubstrateArgs {
                binary_path: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 60".into()],
                env: HashMap::new(),
                timeout_ms: Some(150),
                components: vec![],
            }))
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("spawn failed"), "unexpected error: {msg}");
        assert!(msg.contains("handshake"), "expected timeout wording: {msg}");
    }
}
