//! The `#[tool_router] impl Hub` block — every MCP tool the hub
//! exposes lives here. Each method is a thin adapter that parses
//! arguments, looks up registry state, delegates to the helpers in
//! `codecs.rs` for encode/decode work, and serializes the response.
//! Business logic that isn't request/response glue belongs in
//! `crate::registry`, `crate::spawn`, `crate::session`, or the codec
//! module — not here.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aether_hub_protocol::{EngineId, LogLevel, Uuid};
use base64::Engine as _;
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::log_store::{TOOL_DEFAULT_ENTRIES, TOOL_MAX_ENTRIES};
use crate::session::SessionHandle;
use crate::spawn::{DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_TERMINATE_GRACE, SpawnOpts};

use super::args::{
    CaptureFrameArgs, CaptureFrameResultWire, DescribeComponentArgs, DescribeComponentResponse,
    DescribeKindsArgs, EngineInfo, EngineLogEntry, EngineLogsArgs, EngineLogsResponse,
    LoadComponentArgs, LoadComponentResponse, LoadResultWire, MailSpec, MailStatus,
    ReceiveMailArgs, ReceivedMail, ReplaceComponentArgs, ReplaceResultWire, SendMailArgs,
    SpawnResult, SpawnSubstrateArgs, TerminateResult, TerminateSubstrateArgs, log_level_to_str,
};
use super::codecs::{decode_inbound, deliver_one, encode_capture_bundle};
use super::{Hub, HubState};
use crate::registry::ComponentRecord;

/// Substrate control-plane kind name for a capture request. String
/// constants rather than an `aether-kinds` dependency to keep the hub
/// agnostic of the substrate's typed kind vocabulary at crate level —
/// the schema-driven descriptor path already provides wire-level
/// compatibility.
const KIND_CAPTURE_FRAME: &str = "aether.control.capture_frame";
const KIND_CAPTURE_FRAME_RESULT: &str = "aether.control.capture_frame_result";
const KIND_LOAD_COMPONENT: &str = "aether.control.load_component";
const KIND_LOAD_RESULT: &str = "aether.control.load_result";
const KIND_REPLACE_COMPONENT: &str = "aether.control.replace_component";
const KIND_REPLACE_RESULT: &str = "aether.control.replace_result";

/// Shared default/cap for await-reply tools. `DEFAULT_CAPTURE_TIMEOUT`
/// is reused directly by name from the capture path; these aliases
/// exist so the load/replace paths don't have to reach across the
/// implementation to share the constant. Same values: 5s default, 30s
/// ceiling.
const DEFAULT_REPLY_TIMEOUT: Duration = DEFAULT_CAPTURE_TIMEOUT;
const MAX_REPLY_TIMEOUT: Duration = MAX_CAPTURE_TIMEOUT;

/// Default cap on how long `capture_frame` waits for the substrate's
/// reply before returning an error. Long enough to tolerate one
/// stalled frame (monitor refresh + GPU readback + PNG encode); short
/// enough that a stuck substrate doesn't hang the tool call.
const DEFAULT_CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard ceiling on `timeout_ms`. A request that needs more than 30s
/// is probably a bug on the substrate side; surfacing it as a bound
/// rather than letting it hang preserves the harness's responsiveness.
const MAX_CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);

impl Hub {
    /// Construct a per-session `Hub`. Lives in this module (rather
    /// than `mcp.rs` with the struct) because `Self::tool_router()` is
    /// generated private by rmcp's `#[tool_router]` macro below and
    /// only siblings in the same module can call it.
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

#[tool_router]
impl Hub {
    #[tool(
        description = "Send one or more mail items. Each item takes `params` — structured JSON encoded via the engine's kind descriptor (cast-shaped or postcard, ADR-0019). The batch is best-effort: per-item status is returned and failures don't abort siblings. 'delivered' means the hub queued the frame to the engine's socket — not that the engine processed it."
    )]
    pub(super) async fn send_mail(
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
    pub(super) async fn describe_kinds(
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
    pub(super) async fn receive_mail(
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
    pub(super) async fn spawn_substrate(
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
    pub(super) async fn terminate_substrate(
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
    pub(super) async fn engine_logs(
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
    pub(super) async fn capture_frame(
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
        description = "Load a WASM component into the substrate by filesystem path. The hub reads the binary from `binary_path`, forwards the bytes to the substrate as `aether.control.load_component`, and waits for the substrate's `LoadResult` reply — returning `{mailbox_id, name}` inline or an error. Path must exist as given (no `~` expansion, no relative resolution — same rule as `spawn_substrate`). The component's kind vocabulary rides in the wasm's `aether.kinds` custom section (ADR-0028) — the loader doesn't declare kinds separately. Rejects with \"already in flight\" if a prior `load_component` on this session hasn't completed. Default timeout 5000ms; clamped to 30000. Agents never inline the wasm bytes through the tool call — that's what the path is for."
    )]
    pub(super) async fn load_component(
        &self,
        Parameters(args): Parameters<LoadComponentArgs>,
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

        let wasm = tokio::fs::read(&args.binary_path).await.map_err(|e| {
            McpError::invalid_params(
                format!("reading binary_path {:?}: {e}", args.binary_path),
                None,
            )
        })?;

        let params = serde_json::json!({
            "wasm": wasm,
            "name": args.name,
        });

        let (_guard, rx) = self
            .session
            .replies
            .register(KIND_LOAD_RESULT.to_owned())
            .ok_or_else(|| {
                McpError::invalid_params(
                    "a load_component is already in flight on this session; wait for it to complete"
                        .to_owned(),
                    None,
                )
            })?;

        let spec = MailSpec {
            engine_id: args.engine_id.clone(),
            recipient_name: "aether.control".to_owned(),
            kind_name: KIND_LOAD_COMPONENT.to_owned(),
            params: Some(params),
            count: 1,
        };
        deliver_one(spec, &self.state.engines, self.session.token)
            .await
            .map_err(|e| McpError::internal_error(format!("send failed: {e}"), None))?;

        let wait = args
            .timeout_ms
            .map(|ms| Duration::from_millis(ms as u64).min(MAX_REPLY_TIMEOUT))
            .unwrap_or(DEFAULT_REPLY_TIMEOUT);
        let queued = match timeout(wait, rx).await {
            Ok(Ok(m)) => m,
            Ok(Err(_)) => {
                return Err(McpError::internal_error(
                    "load_result channel closed before reply arrived".to_owned(),
                    None,
                ));
            }
            Err(_) => {
                return Err(McpError::internal_error(
                    format!(
                        "timed out after {}ms waiting for load_result",
                        wait.as_millis()
                    ),
                    None,
                ));
            }
        };

        let result: LoadResultWire = postcard::from_bytes(&queued.payload)
            .map_err(|e| McpError::internal_error(format!("decode reply: {e}"), None))?;
        match result {
            LoadResultWire::Err { error } => Err(McpError::internal_error(
                format!("substrate load failed: {error}"),
                None,
            )),
            LoadResultWire::Ok {
                mailbox_id,
                name,
                capabilities,
            } => {
                self.state.engines.upsert_component(
                    &id,
                    mailbox_id,
                    ComponentRecord {
                        name: name.clone(),
                        capabilities: capabilities.clone(),
                    },
                );
                let response = LoadComponentResponse {
                    mailbox_id,
                    name,
                    capabilities,
                };
                serde_json::to_string(&response)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))
            }
        }
    }

    #[tool(
        description = "Atomically replace a live component's WASM with a new binary loaded from a filesystem path (ADR-0022 freeze-drain-swap). The hub reads the binary from `binary_path` and forwards `aether.control.replace_component` to the substrate, which freezes the target mailbox, drains in-flight mail on the old instance up to `drain_timeout_ms` (substrate default 5000), then swaps. Kind vocabulary rides in the wasm's `aether.kinds` custom section (ADR-0028). On drain timeout the old instance stays bound and the reply is an error — a loud failure rather than silent dropped mail. Path must exist as given. Waits for the substrate's `ReplaceResult`; returns `\"Ok\"` on success. Rejects with \"already in flight\" if a prior replace is pending on this session. Tool timeout default 5000ms, clamped to 30000 — set it above `drain_timeout_ms`."
    )]
    pub(super) async fn replace_component(
        &self,
        Parameters(args): Parameters<ReplaceComponentArgs>,
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

        let wasm = tokio::fs::read(&args.binary_path).await.map_err(|e| {
            McpError::invalid_params(
                format!("reading binary_path {:?}: {e}", args.binary_path),
                None,
            )
        })?;

        let params = serde_json::json!({
            "mailbox_id": args.mailbox_id,
            "wasm": wasm,
            "drain_timeout_ms": args.drain_timeout_ms,
        });

        let (_guard, rx) = self
            .session
            .replies
            .register(KIND_REPLACE_RESULT.to_owned())
            .ok_or_else(|| {
                McpError::invalid_params(
                    "a replace_component is already in flight on this session; wait for it to complete"
                        .to_owned(),
                    None,
                )
            })?;

        let spec = MailSpec {
            engine_id: args.engine_id.clone(),
            recipient_name: "aether.control".to_owned(),
            kind_name: KIND_REPLACE_COMPONENT.to_owned(),
            params: Some(params),
            count: 1,
        };
        deliver_one(spec, &self.state.engines, self.session.token)
            .await
            .map_err(|e| McpError::internal_error(format!("send failed: {e}"), None))?;

        let wait = args
            .timeout_ms
            .map(|ms| Duration::from_millis(ms as u64).min(MAX_REPLY_TIMEOUT))
            .unwrap_or(DEFAULT_REPLY_TIMEOUT);
        let queued = match timeout(wait, rx).await {
            Ok(Ok(m)) => m,
            Ok(Err(_)) => {
                return Err(McpError::internal_error(
                    "replace_result channel closed before reply arrived".to_owned(),
                    None,
                ));
            }
            Err(_) => {
                return Err(McpError::internal_error(
                    format!(
                        "timed out after {}ms waiting for replace_result",
                        wait.as_millis()
                    ),
                    None,
                ));
            }
        };

        let result: ReplaceResultWire = postcard::from_bytes(&queued.payload)
            .map_err(|e| McpError::internal_error(format!("decode reply: {e}"), None))?;
        match result {
            ReplaceResultWire::Err { error } => Err(McpError::internal_error(
                format!("substrate replace failed: {error}"),
                None,
            )),
            ReplaceResultWire::Ok { capabilities } => {
                // ADR-0033: the replaced component may advertise a
                // different receive surface. Refresh the cached record
                // so `describe_component` reflects what's actually
                // bound now. Name is preserved — `replace_component`
                // keeps the existing mailbox + name by design.
                let existing = self.state.engines.get_component(&id, args.mailbox_id);
                let name = existing
                    .map(|r| r.name)
                    .unwrap_or_else(|| format!("mailbox_{}", args.mailbox_id));
                self.state.engines.upsert_component(
                    &id,
                    args.mailbox_id,
                    ComponentRecord {
                        name,
                        capabilities: capabilities.clone(),
                    },
                );
                serde_json::to_string(&capabilities)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))
            }
        }
    }

    #[tool(
        description = "Describe a loaded component's receive-side capabilities (ADR-0033). Returns the component's name, its top-level author-written documentation, the set of kinds it typed-handles (id, name, optional per-handler doc), and whether it has a `#[fallback]` catchall (with the fallback's own optional doc). The capability set is parsed from the component's `aether.kinds.inputs` wasm custom section at `load_component` / `replace_component` time. Strict receivers — components without a fallback — are distinguishable via `fallback: null` in the response. Components predating the `#[handlers]` macro ship with empty fields since they have no structured capability surface to advertise."
    )]
    pub(super) async fn describe_component(
        &self,
        Parameters(args): Parameters<DescribeComponentArgs>,
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
        let Some(component) = self.state.engines.get_component(&id, args.mailbox_id) else {
            return Err(McpError::invalid_params(
                format!(
                    "no component at mailbox_id {} on engine {}",
                    args.mailbox_id, args.engine_id
                ),
                None,
            ));
        };
        let ComponentRecord { name, capabilities } = component;
        let response = DescribeComponentResponse {
            name,
            doc: capabilities.doc,
            receives: capabilities.handlers,
            fallback: capabilities.fallback,
        };
        serde_json::to_string(&response).map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        description = "List all engines currently connected to the hub. Each item reports the hub-assigned engine_id, the engine's self-declared name/pid/version, and a `spawned` flag: `true` if the hub launched this engine as a child process (ADR-0009), `false` if it connected externally."
    )]
    pub(super) async fn list_engines(&self) -> Result<String, McpError> {
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
