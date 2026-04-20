//! Request / response shapes for the MCP tool surface. These types
//! only carry data — no hub state, no business logic. Each request type
//! is deserialized from the MCP tool call's `Parameters`, and each
//! response type is serialized back out as JSON. `JsonSchema` is
//! required on every type that appears in a tool signature so rmcp
//! can publish the correct schema to the client.

use std::collections::HashMap;

use aether_hub_protocol::LogLevel;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

pub(super) fn one() -> u32 {
    1
}

pub(super) fn log_level_to_str(level: LogLevel) -> &'static str {
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
pub(super) enum CaptureFrameResultWire {
    Ok { png: Vec<u8> },
    Err { error: String },
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadComponentArgs {
    /// Hub-assigned engine UUID as a string (from `list_engines`).
    pub engine_id: String,
    /// Absolute path to the WASM component binary on the hub's
    /// filesystem. The hub reads the file and sends the bytes to the
    /// substrate — agents don't inline wasm bytes through the tool
    /// call. Must exist as given; no `~` expansion, no relative-path
    /// resolution. Matches `spawn_substrate`'s path rule.
    ///
    /// ADR-0028: the component's kind vocabulary is embedded in the
    /// wasm's `aether.kinds` custom section at compile time. The
    /// loader doesn't declare kinds — the substrate reads them
    /// straight from the binary.
    pub binary_path: String,
    /// Optional human-readable name. The substrate defaults one if
    /// absent and echoes it back in the result.
    #[serde(default)]
    pub name: Option<String>,
    /// Maximum time to wait for the substrate's `LoadResult` reply,
    /// in milliseconds. Defaults to 5000. Clamped to 30000.
    #[serde(default)]
    pub timeout_ms: Option<u32>,
}

/// Wire-format mirror of `aether.control.load_result`. Postcard-
/// decoded from the substrate's reply payload. Must stay in lockstep
/// with `aether-kinds::LoadResult` — that type is canonical.
#[derive(Debug, Deserialize)]
pub(super) enum LoadResultWire {
    Ok { mailbox_id: u32, name: String },
    Err { error: String },
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LoadComponentResponse {
    /// Substrate-assigned mailbox id for the loaded component. Hand
    /// this to `replace_component` or use as the `mailbox` in
    /// `subscribe_input`.
    pub mailbox_id: u32,
    /// Substrate-resolved name. Matches the `name` in the request if
    /// provided; otherwise the substrate-defaulted value.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplaceComponentArgs {
    /// Hub-assigned engine UUID as a string (from `list_engines`).
    pub engine_id: String,
    /// Mailbox id of the live component to replace (from a prior
    /// `load_component` or `list_engines`-derived lookup).
    pub mailbox_id: u32,
    /// Absolute path to the replacement WASM binary on the hub's
    /// filesystem. Same filesystem rule as `load_component`. Kind
    /// vocabulary is embedded in the wasm's `aether.kinds` custom
    /// section (ADR-0028).
    pub binary_path: String,
    /// Drain timeout in milliseconds for in-flight mail on the old
    /// instance (ADR-0022). `None` uses the substrate default (5000).
    /// If the drain exceeds this, the replace fails and the old
    /// instance stays bound.
    #[serde(default)]
    pub drain_timeout_ms: Option<u32>,
    /// Maximum time to wait for the substrate's `ReplaceResult` reply,
    /// in milliseconds. Defaults to 5000. Clamped to 30000. Set higher
    /// than `drain_timeout_ms` so the reply has room to arrive after
    /// the substrate declares drain failure.
    #[serde(default)]
    pub timeout_ms: Option<u32>,
}

/// Wire-format mirror of `aether.control.replace_result`. Must stay
/// in lockstep with `aether-kinds::ReplaceResult`.
#[derive(Debug, Deserialize)]
pub(super) enum ReplaceResultWire {
    Ok,
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
