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
    EngineId, HubToEngine, KindDescriptor, LogLevel, MailFrame, SchemaType, SessionToken, Uuid,
};
use base64::Engine as _;
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;

use crate::decoder::decode_schema;
use crate::encoder::encode_schema;
use crate::log_store::{LogStore, TOOL_DEFAULT_ENTRIES, TOOL_MAX_ENTRIES};
use crate::registry::{EngineRecord, EngineRegistry};
use crate::session::{QueuedMail, SessionHandle, SessionRegistry};
use crate::spawn::{DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_TERMINATE_GRACE, PendingSpawns, SpawnOpts};

/// Default port the hub binds for MCP clients. Overridable via
/// `AETHER_MCP_PORT`.
pub const DEFAULT_MCP_PORT: u16 = 8888;

/// Substrate control-plane kind name for a capture request. String
/// constants rather than an `aether-kinds` dependency to keep the hub
/// agnostic of the substrate's typed kind vocabulary at crate level —
/// the schema-driven descriptor path already provides wire-level
/// compatibility.
const KIND_CAPTURE_FRAME: &str = "aether.control.capture_frame";
const KIND_CAPTURE_FRAME_RESULT: &str = "aether.control.capture_frame_result";

/// Default cap on how long `capture_frame` waits for the substrate's
/// reply before returning an error. Long enough to tolerate one
/// stalled frame (monitor refresh + GPU readback + PNG encode); short
/// enough that a stuck substrate doesn't hang the tool call.
const DEFAULT_CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard ceiling on `timeout_ms`. A request that needs more than 30s
/// is probably a bug on the substrate side; surfacing it as a bound
/// rather than letting it hang preserves the harness's responsiveness.
const MAX_CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared state across all rmcp sessions. Cheap to `Arc::clone` into
/// each per-session `Hub` instance.
pub struct HubState {
    engines: EngineRegistry,
    sessions: SessionRegistry,
    pending_spawns: PendingSpawns,
    /// ADR-0023 per-engine log buffers. Outlives engine records so
    /// post-mortem `engine_logs` polls succeed after a substrate exit.
    logs: LogStore,
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

fn log_level_to_str(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "trace",
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
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
pub struct EngineLogsArgs {
    /// Hub-assigned engine UUID as a string (from `list_engines`).
    pub engine_id: String,
    /// Maximum entries to return. Defaults to 100; clamped to 1000.
    #[serde(default)]
    pub max: Option<u32>,
    /// Minimum log level (`"trace"|"debug"|"info"|"warn"|"error"`).
    /// Defaults to `"trace"` (every captured entry).
    #[serde(default)]
    pub level: Option<String>,
    /// Cursor: only entries with `sequence > since` are returned.
    /// Pass back the previous response's `next_since` to poll
    /// incrementally without re-receiving.
    #[serde(default)]
    pub since: Option<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct EngineLogsResponse {
    pub engine_id: String,
    pub entries: Vec<EngineLogEntry>,
    /// Highest `sequence` returned in `entries`, or the request's
    /// `since` value if the response is empty. Use as the next
    /// `since` for incremental polling.
    pub next_since: u64,
    /// Set when the hub-side ring evicted entries the caller hadn't
    /// seen — `Some(seq)` means the gap above the request's `since`
    /// extends up to (but not including) `seq`. Treat as a signal to
    /// poll more often or accept the loss.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_before: Option<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct EngineLogEntry {
    pub timestamp_unix_ms: u64,
    /// Lowercase level: `"trace"|"debug"|"info"|"warn"|"error"`.
    pub level: String,
    /// Module path the event was emitted from
    /// (e.g. `"aether_substrate::scheduler"`).
    pub target: String,
    /// Already-formatted event text. Tracing's structured fields are
    /// flattened into this string by the substrate's capture layer.
    pub message: String,
    pub sequence: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CaptureFrameArgs {
    /// Hub-assigned engine UUID as a string (from `list_engines`).
    pub engine_id: String,
    /// Optional bundle of mails to dispatch on the substrate *before*
    /// the capture fires. Each item has the same shape as a
    /// `send_mail` entry — `recipient_name`, `kind_name`, structured
    /// `params` encoded via the kind's descriptor, `count`. The
    /// substrate resolves every envelope atomically: if any fails
    /// (unknown kind, unknown recipient), no mail is dispatched and
    /// the reply is an `Err`. An empty bundle means "just capture the
    /// current state."
    #[serde(default)]
    pub mails: Vec<MailSpec>,
    /// Optional bundle of cleanup mails dispatched *after* the frame
    /// is captured. Useful for "set a flag, capture, unset the flag"
    /// patterns in one atomic tool call. Resolved against the same
    /// abort-on-first-failure policy as `mails` — a bad envelope in
    /// either bundle aborts the whole request before any mail moves.
    #[serde(default)]
    pub after_mails: Vec<MailSpec>,
    /// Maximum time to wait for the substrate's reply, in
    /// milliseconds. Defaults to 5000 (5 s). Clamped to 30000 (30 s).
    #[serde(default)]
    pub timeout_ms: Option<u32>,
}

/// Wire-format mirror of the substrate's `CaptureFrameResult` kind.
/// Lives in the hub so we can postcard-decode the reply payload
/// without pulling in the substrate's typed kind crate. Must stay in
/// lockstep with `aether-kinds::CaptureFrameResult` — the comment on
/// that type is the canonical spec.
#[derive(Debug, Deserialize)]
enum CaptureFrameResultWire {
    Ok { png: Vec<u8> },
    Err { error: String },
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
    /// Structured value decoded against the engine's kind descriptor —
    /// the symmetric counterpart to `send_mail`'s `params` field
    /// (ADR-0020). `null` if the hub couldn't decode (no descriptor for
    /// this kind, or decode failed), in which case `decode_error` is
    /// populated and the agent falls back to `payload_bytes`.
    pub params: Option<serde_json::Value>,
    /// Decode failure reason. Populated only when `params` is `null`
    /// because the hub's decoder rejected the payload (e.g. schema
    /// drift, truncation, unknown kind). Always paired with intact
    /// `payload_bytes` for an escape-hatch decode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_error: Option<String>,
    /// Raw payload bytes. Always populated. Prefer `params`; reach for
    /// `payload_bytes` only when `params` is `null` (decode failed) or
    /// when the agent genuinely needs the wire bytes.
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
        description = "Drain observation mail addressed to this MCP session. Returns everything currently queued (up to `max`, if provided). Each item reports the originating engine_id, the kind name, structured `params` decoded against the engine's kind descriptor (ADR-0020 — symmetric to `send_mail`), the raw `payload_bytes` (always populated; primarily a fallback when `params` is null), an optional `decode_error` explaining why decode failed when `params` is null, a `broadcast` flag indicating whether this mail also went to every other attached session (true) or was targeted specifically at this one (false), and an optional `origin` — the substrate-local mailbox name of the emitting component (absent for substrate-core pushes with no sending mailbox). Non-blocking: returns an empty array if nothing is queued."
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
                Ok(m) => {
                    let (params, decode_error) =
                        decode_inbound(&m.engine_id, &m.kind_name, &m.payload, &self.state.engines);
                    out.push(ReceivedMail {
                        engine_id: m.engine_id.0.to_string(),
                        kind_name: m.kind_name,
                        params,
                        decode_error,
                        payload_bytes: m.payload,
                        broadcast: m.broadcast,
                        origin: m.origin,
                    });
                }
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
        description = "Drain captured substrate log entries for an engine (ADR-0023). Cursor-based polling: pass back `next_since` from the previous response to receive only new entries. `level` (default `\"trace\"`) filters server-side. `max` defaults to 100, clamped to 1000. `truncated_before` is set when the hub-side ring evicted entries the caller hadn't seen — treat it as a signal to poll more often or accept the gap. Buffer survives engine exit until hub shutdown, so post-mortem polls work after the substrate has crashed."
    )]
    async fn engine_logs(
        &self,
        Parameters(args): Parameters<EngineLogsArgs>,
    ) -> Result<String, McpError> {
        let uuid = Uuid::parse_str(&args.engine_id).map_err(|e| {
            McpError::invalid_params(format!("engine_id is not a valid UUID: {e}"), None)
        })?;
        let id = EngineId(uuid);
        let min_level = match args.level.as_deref() {
            None | Some("trace") => LogLevel::Trace,
            Some("debug") => LogLevel::Debug,
            Some("info") => LogLevel::Info,
            Some("warn") => LogLevel::Warn,
            Some("error") => LogLevel::Error,
            Some(other) => {
                return Err(McpError::invalid_params(
                    format!("level {other:?} is not one of trace|debug|info|warn|error",),
                    None,
                ));
            }
        };
        let max = args
            .max
            .map(|n| n as usize)
            .unwrap_or(TOOL_DEFAULT_ENTRIES)
            .min(TOOL_MAX_ENTRIES);
        let since = args.since.unwrap_or(0);
        let result = self.state.logs.read(id, max, min_level, since);
        let response = EngineLogsResponse {
            engine_id: args.engine_id,
            entries: result
                .entries
                .into_iter()
                .map(|e| EngineLogEntry {
                    timestamp_unix_ms: e.timestamp_unix_ms,
                    level: log_level_to_str(e.level).to_owned(),
                    target: e.target,
                    message: e.message,
                    sequence: e.sequence,
                })
                .collect(),
            next_since: result.next_since,
            truncated_before: result.truncated_before,
        };
        serde_json::to_string(&response).map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        description = "Capture the current frame contents of the given engine as a PNG and return it inline as MCP image content. The substrate renders to an offscreen target so the capture works regardless of window visibility (important on macOS where occluded windows would otherwise block). Optionally carries two mail bundles dispatched atomically around the capture: `mails` fires *before* the frame is read back (state-changing mail whose effects should appear in the image), and `after_mails` fires *after* readback (cleanup such as restoring a flag the caller flipped for the capture). The substrate resolves every envelope in both bundles first (abort-on-first-failure); any failure means no mail moves. The render loop's `queue.wait_idle()` ensures the pre-capture bundle has been fully processed before the frame is read back. Empty bundles mean \"just capture\" / \"no cleanup\". Rejects if another capture is already in flight on this session, or if the wait exceeds `timeout_ms` (default 5000, clamped to 30000). Use this to verify substrate rendering end-to-end and to bundle \"set X, show me, restore X\" into one atomic tool call."
    )]
    async fn capture_frame(
        &self,
        Parameters(args): Parameters<CaptureFrameArgs>,
    ) -> Result<CallToolResult, McpError> {
        let uuid = Uuid::parse_str(&args.engine_id).map_err(|e| {
            McpError::invalid_params(format!("engine_id is not a valid UUID: {e}"), None)
        })?;
        let id = EngineId(uuid);
        let record = self.state.engines.get(&id).ok_or_else(|| {
            McpError::invalid_params(format!("unknown engine_id {}", args.engine_id), None)
        })?;

        // Encode each envelope in both bundles against its kind's
        // descriptor. Done before we register the reply-waiter or
        // send anything, so a bad bundle produces a clean invalid-
        // params error and never touches the engine wire.
        let envelopes = encode_capture_bundle(&args.mails, &record).map_err(|e| {
            McpError::invalid_params(format!("capture_frame mails bundle: {e}"), None)
        })?;
        let after_envelopes = encode_capture_bundle(&args.after_mails, &record).map_err(|e| {
            McpError::invalid_params(format!("capture_frame after_mails bundle: {e}"), None)
        })?;
        let bundle_params = serde_json::json!({
            "mails": envelopes,
            "after_mails": after_envelopes,
        });

        // Register the reply-waiter BEFORE sending the request, so the
        // engine reader can divert the reply to our oneshot the moment
        // it arrives. If another capture is already pending on this
        // session, surface it as a clear error rather than silently
        // waiting behind the first.
        let (_guard, rx) = self
            .session
            .replies
            .register(KIND_CAPTURE_FRAME_RESULT.to_owned())
            .ok_or_else(|| {
                McpError::invalid_params(
                    "a capture is already in flight on this session; wait for it to complete"
                        .to_owned(),
                    None,
                )
            })?;

        // Send the capture_frame request carrying the pre-encoded
        // bundle. The substrate's schema decoder reconstructs each
        // envelope's `payload: Vec<u8>` from the JSON byte array the
        // hub encoder wrote — postcard-equivalent round-trip.
        let spec = MailSpec {
            engine_id: args.engine_id.clone(),
            recipient_name: "aether.control".to_owned(),
            kind_name: KIND_CAPTURE_FRAME.to_owned(),
            params: Some(bundle_params),
            count: 1,
        };
        deliver_one(spec, &self.state.engines, self.session.token)
            .await
            .map_err(|e| McpError::internal_error(format!("send failed: {e}"), None))?;

        // Wait for the reply (or timeout). `_guard` drops when this
        // future resolves — on any path — which removes the registry
        // entry. The record lookup above already pinned `record` for
        // the duration of the send; we don't need it past this point.
        let _ = record;
        let wait = args
            .timeout_ms
            .map(|ms| Duration::from_millis(ms as u64).min(MAX_CAPTURE_TIMEOUT))
            .unwrap_or(DEFAULT_CAPTURE_TIMEOUT);
        let queued = match timeout(wait, rx).await {
            Ok(Ok(m)) => m,
            Ok(Err(_)) => {
                return Err(McpError::internal_error(
                    "capture reply channel closed before reply arrived".to_owned(),
                    None,
                ));
            }
            Err(_) => {
                return Err(McpError::internal_error(
                    format!(
                        "timed out after {}ms waiting for capture_frame_result",
                        wait.as_millis()
                    ),
                    None,
                ));
            }
        };

        // Decode the reply. The payload is a postcard-encoded
        // `CaptureFrameResult` — either `Ok { png }` or `Err { error }`.
        let result: CaptureFrameResultWire = postcard::from_bytes(&queued.payload)
            .map_err(|e| McpError::internal_error(format!("decode reply: {e}"), None))?;
        match result {
            CaptureFrameResultWire::Err { error } => Err(McpError::internal_error(
                format!("substrate capture failed: {error}"),
                None,
            )),
            CaptureFrameResultWire::Ok { png } => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&png);
                Ok(CallToolResult::success(vec![Content::image(
                    encoded,
                    "image/png",
                )]))
            }
        }
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

/// Decide the wire bytes for a mail by looking up the kind's
/// descriptor and feeding `params` through `encode_schema`. ADR-0019:
/// every kind has a schema; absent params is only legal when the
/// schema is `Unit`.
fn resolve_payload(spec: &MailSpec, record: &EngineRecord) -> Result<Vec<u8>, String> {
    let desc = find_kind(record, &spec.kind_name)
        .ok_or_else(|| format!("kind {:?} has no descriptor on this engine", spec.kind_name))?;
    match (&spec.params, &desc.schema) {
        (None, SchemaType::Unit) => Ok(Vec::new()),
        (None, _) => Err(format!(
            "kind {:?} requires `params` (only Unit kinds may omit them)",
            spec.kind_name
        )),
        (Some(p), schema) => encode_schema(p, schema).map_err(|e| e.to_string()),
    }
}

fn find_kind<'a>(record: &'a EngineRecord, name: &str) -> Option<&'a KindDescriptor> {
    record.kinds.iter().find(|k| k.name == name)
}

/// Encode each `MailSpec` in a `capture_frame` bundle against the
/// engine's descriptors, producing the JSON shape the `CaptureFrame`
/// kind's schema expects (`{mails: [{recipient_name, kind_name,
/// payload, count}]}`'s `mails` array — returned here as a JSON
/// array ready to be slotted under the outer `mails` key).
///
/// Abort-on-first-failure: a single bad envelope aborts the whole
/// bundle, matching the substrate's atomic-dispatch guarantee. This
/// also short-circuits the tool before it touches the engine wire.
fn encode_capture_bundle(
    specs: &[MailSpec],
    record: &EngineRecord,
) -> Result<Vec<serde_json::Value>, String> {
    let mut out = Vec::with_capacity(specs.len());
    for (i, spec) in specs.iter().enumerate() {
        let payload = resolve_payload(spec, record)
            .map_err(|e| format!("envelope[{i}] ({}): {e}", spec.kind_name))?;
        let payload_bytes: Vec<serde_json::Value> = payload
            .into_iter()
            .map(|b| serde_json::Value::Number(b.into()))
            .collect();
        out.push(serde_json::json!({
            "recipient_name": spec.recipient_name,
            "kind_name": spec.kind_name,
            "payload": payload_bytes,
            "count": spec.count,
        }));
    }
    Ok(out)
}

/// Decode an inbound observation payload against the originating
/// engine's kind descriptor (ADR-0020). Returns the structured `params`
/// on success; on any failure (engine no longer in the registry, kind
/// not declared, decode error) returns `(None, Some(reason))` so the
/// agent sees both the bytes and a human-readable explanation. Lookup
/// failures are treated as decode failures rather than tool errors —
/// the rest of the batch should still drain.
fn decode_inbound(
    engine_id: &EngineId,
    kind_name: &str,
    payload: &[u8],
    engines: &EngineRegistry,
) -> (Option<serde_json::Value>, Option<String>) {
    let Some(record) = engines.get(engine_id) else {
        return (
            None,
            Some(format!(
                "engine {} no longer connected; cannot resolve schema",
                engine_id.0
            )),
        );
    };
    let Some(desc) = find_kind(&record, kind_name) else {
        return (
            None,
            Some(format!(
                "kind {kind_name:?} has no descriptor on this engine"
            )),
        );
    };
    match decode_schema(payload, &desc.schema) {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e.to_string())),
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
        let tick = aether_hub_protocol::KindDescriptor {
            name: "aether.tick".into(),
            schema: aether_hub_protocol::SchemaType::Unit,
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
        use aether_hub_protocol::{KindDescriptor, NamedField, Primitive, SchemaType};

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
    async fn send_mail_unit_kind_no_params() {
        use aether_hub_protocol::{KindDescriptor, SchemaType};

        let engines = EngineRegistry::new();
        let kinds = vec![KindDescriptor {
            name: "aether.tick".into(),
            schema: SchemaType::Unit,
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
        use aether_hub_protocol::{KindDescriptor, NamedField, Primitive, SchemaType};

        let kinds = vec![
            KindDescriptor {
                name: "aether.tick".into(),
                schema: SchemaType::Unit,
            },
            KindDescriptor {
                name: "hello.note".into(),
                schema: SchemaType::Struct {
                    repr_c: false,
                    fields: vec![NamedField {
                        name: "body".into(),
                        ty: SchemaType::String,
                    }],
                },
            },
            KindDescriptor {
                name: "hello.cast".into(),
                schema: SchemaType::Struct {
                    repr_c: true,
                    fields: vec![NamedField {
                        name: "n".into(),
                        ty: SchemaType::Scalar(Primitive::U32),
                    }],
                },
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
    async fn receive_mail_decodes_params_against_descriptor() {
        // FrameStats-shaped: cast struct with two u64 fields. The
        // engine ships raw cast bytes; the hub looks up the descriptor
        // and lifts them into structured `params`.
        use aether_hub_protocol::{KindDescriptor, NamedField, Primitive, SchemaType};

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
                ],
            },
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
        use aether_hub_protocol::{KindDescriptor, NamedField, Primitive, SchemaType};

        let engines = EngineRegistry::new();
        let kinds = vec![KindDescriptor {
            name: "demo.short".into(),
            schema: SchemaType::Struct {
                repr_c: true,
                fields: vec![NamedField {
                    name: "n".into(),
                    ty: SchemaType::Scalar(Primitive::U64),
                }],
            },
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
        use aether_hub_protocol::{KindDescriptor, SchemaType};

        let engines = EngineRegistry::new();
        let kinds = vec![KindDescriptor {
            name: "aether.observation.ping".into(),
            schema: SchemaType::Unit,
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
    fn capture_frame_kind_descriptor() -> aether_hub_protocol::KindDescriptor {
        use aether_hub_protocol::{KindDescriptor, NamedField, Primitive, SchemaType};
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
            ],
        };
        KindDescriptor {
            name: "aether.control.capture_frame".into(),
            schema: SchemaType::Struct {
                repr_c: false,
                fields: vec![
                    NamedField {
                        name: "mails".into(),
                        ty: SchemaType::Vec(Box::new(envelope.clone())),
                    },
                    NamedField {
                        name: "after_mails".into(),
                        ty: SchemaType::Vec(Box::new(envelope)),
                    },
                ],
            },
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
        use aether_hub_protocol::{KindDescriptor, SchemaType};

        let engines = EngineRegistry::new();
        // Two kinds on this engine: capture_frame plus a `demo.tick`
        // Unit kind we'll bundle into the capture request.
        let kinds = vec![
            capture_frame_kind_descriptor(),
            KindDescriptor {
                name: "demo.tick".into(),
                schema: SchemaType::Unit,
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
            }))
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("spawn failed"), "unexpected error: {msg}");
        assert!(msg.contains("handshake"), "expected timeout wording: {msg}");
    }
}
