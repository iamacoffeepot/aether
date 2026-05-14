//! Request / response shapes for the `aether-mcp` tool surface
//! (issue 763 P5b). Pure data — `serde` + `schemars::JsonSchema` so
//! `rmcp` can derive the JSON Schema it advertises to MCP clients.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `spawn_substrate` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpawnSubstrateArgs {
    /// Absolute path to the substrate binary the hub should fork+exec.
    /// The hub doesn't resolve or locate binaries — pass a path that
    /// exists.
    pub binary_path: String,
    /// Extra command-line arguments forwarded to the substrate
    /// verbatim. `AETHER_RPC_PORT` is injected by the hub regardless.
    #[serde(default)]
    pub args: Vec<String>,
}

/// `terminate_substrate` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TerminateSubstrateArgs {
    /// Engine UUID, as returned by `spawn_substrate` / `list_engines`.
    pub engine_id: String,
}

/// `send_mail` arguments — a best-effort batch.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendMailArgs {
    /// One or more mail items. Each is routed independently — a single
    /// failure doesn't abort the batch; the response carries a
    /// per-item status.
    pub mails: Vec<MailSpec>,
}

/// One item in a `send_mail` batch.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MailSpec {
    /// Engine UUID the mail targets (from `list_engines`).
    pub engine_id: String,
    /// Mailbox name on that engine (e.g. `"aether.fs"`).
    pub recipient_name: String,
    /// Kind name (e.g. `"aether.fs.list"`), resolved against the
    /// substrate kind vocabulary baked into `aether-mcp`.
    pub kind_name: String,
    /// Structured params, schema-encoded to wire bytes against the
    /// kind's descriptor. Omit or `null` for a fieldless kind.
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

/// One supervised engine, as reported by `list_engines`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct EngineInfo {
    /// Engine UUID — pass to `send_mail` / `terminate_substrate`.
    pub engine_id: String,
    /// The localhost RPC port the hub assigned this substrate.
    pub rpc_port: u16,
}

/// Per-item outcome from a `send_mail` batch.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MailStatus {
    /// Index into the `mails` array the caller supplied.
    pub index: usize,
    /// `"delivered"` once the call reached the substrate and its
    /// dispatch chain settled, or `"error: <reason>"` on failure.
    pub status: String,
}

/// `load_component` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadComponentArgs {
    /// Engine UUID the component loads into (from `list_engines`).
    pub engine_id: String,
    /// Absolute path to the component's `.wasm`. `aether-mcp` reads
    /// the bytes and forwards them to the substrate — agents never
    /// inline wasm through the tool call.
    pub binary_path: String,
    /// Optional human-readable name. The substrate defaults one from
    /// the wasm if omitted; the reply echoes the resolved name.
    #[serde(default)]
    pub name: Option<String>,
}

/// `replace_component` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplaceComponentArgs {
    /// Engine UUID hosting the component (from `list_engines`).
    pub engine_id: String,
    /// Tagged mailbox id (`mbx-…`) of the component to replace, as
    /// returned by `load_component`.
    pub mailbox_id: String,
    /// Absolute path to the replacement `.wasm`.
    pub binary_path: String,
    /// Accepted for wire compatibility; currently ignored by the
    /// substrate (post-ADR-0038 the splice is structural).
    #[serde(default)]
    pub drain_timeout_ms: Option<u32>,
}

/// `describe_component` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeComponentArgs {
    /// Engine UUID hosting the component (from `list_engines`).
    pub engine_id: String,
    /// Tagged mailbox id (`mbx-…`) of the loaded component.
    pub mailbox_id: String,
}

/// One mail in a `capture_frame` bundle. Like [`MailSpec`] but without
/// `engine_id` — every bundle entry is dispatched on the engine being
/// captured, so the engine is already fixed by `CaptureFrameArgs`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CaptureMailSpec {
    /// Mailbox name on the captured engine (e.g. `"aether.render"`).
    pub recipient_name: String,
    /// Kind name, resolved against the substrate kind vocabulary.
    pub kind_name: String,
    /// Structured params, schema-encoded to wire bytes. Omit or
    /// `null` for a fieldless kind.
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

/// `capture_frame` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CaptureFrameArgs {
    /// Engine UUID to capture (from `list_engines`).
    pub engine_id: String,
    /// Mail dispatched *before* the frame is read back — state changes
    /// whose effects should appear in the image. Resolved atomically:
    /// any bad entry aborts the whole capture.
    #[serde(default)]
    pub mails: Vec<CaptureMailSpec>,
    /// Mail dispatched *after* readback — cleanup such as restoring a
    /// flag the caller flipped for the capture.
    #[serde(default)]
    pub after_mails: Vec<CaptureMailSpec>,
}
