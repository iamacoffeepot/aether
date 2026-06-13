//! Request / response shapes for the `aether-mcp` tool surface
//! (issue 763 P5b). Pure data — `serde` + `schemars::JsonSchema` so
//! `rmcp` can derive the JSON Schema it advertises to MCP clients.

use aether_data::{KindId, MailboxId, TransformId};
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
    /// Components to auto-load at boot, in order. When non-empty,
    /// `aether-mcp` stages a temporary boot-manifest JSON of these specs
    /// and hands its path to the hub, which injects it as
    /// `AETHER_BOOT_MANIFEST` at the fork — so the spawned engine comes
    /// up with these components already loading, in one call, with no
    /// follow-up `load_component`. Spawn is single-host, so the substrate
    /// reads each `binary_path` (and `config_path`) itself — pass paths
    /// that exist on the host running the fleet. Empty (default) boots a
    /// bare engine.
    #[serde(default)]
    pub components: Vec<ComponentSpec>,
}

/// One component in a `spawn_substrate` boot list. Mirrors the
/// `load_component` arguments (path-addressed, ADR-0096 export
/// selector), but the substrate reads the files itself at boot rather
/// than `aether-mcp` forwarding the bytes.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ComponentSpec {
    /// Absolute path to the component's `.wasm` on the fleet host.
    pub binary_path: String,
    /// Optional human-readable load name. The substrate defaults one
    /// from the wasm if omitted.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional absolute path to a file holding the component's
    /// init-config bytes (ADR-0090), already encoded to the component's
    /// `Config` kind wire shape. Omit for a no-config component.
    #[serde(default)]
    pub config_path: Option<String>,
    /// ADR-0096: which exported actor type to instantiate from a
    /// multi-actor module, named by its `Actor::NAMESPACE`. Omit to load
    /// the module's entry type.
    #[serde(default)]
    pub export: Option<String>,
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
    /// When `true`, dispatch every item without awaiting its reply
    /// (today's pre-issue-1242 behaviour): each `status` is
    /// `"dispatched"` and `replies` is empty. Default `false` — `send_mail`
    /// now blocks per item until the dispatch chain settles and surfaces
    /// the correlated reply payloads. Set this for a fire-and-poke item
    /// (e.g. a `DrawTriangle` before a `capture_frame`) or a cap that
    /// never replies, so the call returns immediately instead of waiting
    /// out the await timeout.
    #[serde(default)]
    pub fire_and_forget: bool,
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
    /// Milliseconds since the hub last confirmed this engine alive
    /// (issue 1339): `0` right after spawn, refreshed each heartbeat
    /// interval. A value climbing past the heartbeat interval means the
    /// engine is going stale; the hub evicts it (drops it from this
    /// list) once it crosses the miss limit.
    pub last_heartbeat_age_millis: u64,
}

/// Per-item outcome from a `send_mail` batch.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MailStatus {
    /// Index into the `mails` array the caller supplied.
    pub index: usize,
    /// `"delivered"` once the call reached the substrate, its dispatch
    /// chain settled, and any correlated replies are in `replies`;
    /// `"dispatched"` when `fire_and_forget` was set (no await);
    /// `"timeout"` when the await hit the cap before settlement (any
    /// replies collected so far are still in `replies`, and `timed_out`
    /// is `true`); or `"error: <reason>"` on a transport / encode
    /// failure.
    pub status: String,
    /// Correlated reply payloads the substrate emitted for this item, in
    /// arrival order. Empty for a fire-and-forget item, an item that
    /// produced no reply, or an error item.
    #[serde(default)]
    pub replies: Vec<ReplyEventJson>,
    /// `true` when the await hit the timeout before the chain settled.
    /// `replies` still carries whatever arrived before the cap.
    #[serde(default)]
    pub timed_out: bool,
}

/// One correlated reply the substrate emitted in response to a
/// `send_mail` / `send_mail_traced` item (issue 1242). `params` is the
/// best-effort schema-decode of the raw payload; `payload_bytes` is the
/// base64 fallback surfaced only when the static vocabulary can't name
/// or decode the kind (issue 1246 — a clean decode omits it).
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReplyEventJson {
    /// Tagged kind id (`knd-…`, ADR-0064) of the reply payload.
    pub kind_id: String,
    /// Substrate kind name resolved from the static vocabulary, or
    /// `null` for a component-defined kind `aether-mcp` can't name.
    pub kind_name: Option<String>,
    /// Best-effort `decode_schema` of the raw payload against the kind's
    /// descriptor, or `null` when the kind is unknown or the decode
    /// failed. On a clean decode this is the only surfacing of the
    /// payload — `payload_bytes` is omitted to avoid the duplicate.
    pub params: Option<serde_json::Value>,
    /// Base64 of the raw wire payload, present **only** on a decode miss
    /// (the sole signal when `params` is `null`) — absent on a clean
    /// decode (issue 1246). Avoids re-surfacing decoded bytes as a
    /// 4×-inflated JSON int-array.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_bytes: Option<String>,
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
    /// ADR-0090 (issue 1257): optional absolute path to a file holding
    /// the component's init-config bytes (already encoded to the
    /// component's `Config` kind wire shape). `aether-mcp` reads the
    /// file and forwards the bytes on the load mail — paths, not inline
    /// bytes, per the MCP convention. Omit for a no-config component;
    /// `describe_component` reports the expected config kind.
    #[serde(default)]
    pub config_path: Option<String>,
    /// ADR-0096: which exported actor type to instantiate from a
    /// multi-actor module, named by its `Actor::NAMESPACE` (e.g.
    /// `"ui.panel"`). Omit to load the module's entry type — the first
    /// in its `export!` list, and the only type a single-actor module
    /// has. An export the module doesn't declare comes back as a
    /// `LoadResult::Err`.
    #[serde(default)]
    pub export: Option<String>,
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
    /// ADR-0090 (issue 1257): optional absolute path to a file holding
    /// the replacement instance's init-config bytes, threaded to its
    /// typed `init` the same way [`LoadComponentArgs::config_path`] is
    /// on first load.
    #[serde(default)]
    pub config_path: Option<String>,
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

/// `actor_logs` arguments — pull entries out of one actor's
/// per-actor log ring (ADR-0081). The substrate-side aggregator
/// retired; each call queries a single actor by name. Aggregate
/// client-side if you want a cross-actor view.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ActorLogsArgs {
    /// Engine UUID to pull from (from `list_engines`).
    pub engine_id: String,
    /// Mailbox name of the actor to query (e.g. `"aether.audio"`,
    /// `"aether.component/aether.embedded:camera"`). The substrate's
    /// dispatch loop services `aether.log.tail` for every actor
    /// automatically; agents don't need to know which actor
    /// implements the handler.
    pub mailbox_name: String,
    /// Cap on returned entries. Defaults to 100; clamped to 1000.
    /// Use the response's `next_since` to walk past the cap on the
    /// next call.
    #[serde(default)]
    pub max: Option<u32>,
    /// Minimum severity to return — `"trace"`, `"debug"`, `"info"`,
    /// `"warn"`, or `"error"`. Omitted returns every entry in the
    /// ring (subject to the chassis's `AETHER_LOG_FILTER`).
    #[serde(default)]
    pub level: Option<String>,
    /// Cursor: return only entries with `sequence > since`. Omitted
    /// returns from the oldest entry currently in the ring; thread
    /// the prior response's `next_since` here to poll without
    /// re-receiving entries.
    #[serde(default)]
    pub since: Option<u64>,
}

/// One log entry as `actor_logs` returns it. Mirrors
/// `aether_kinds::LogEntry` but renders `level` as a string.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorLogEntry {
    /// Unix epoch milliseconds the entry was stamped at on the
    /// substrate's wall clock.
    pub timestamp_unix_ms: u64,
    /// Severity: `"trace"` | `"debug"` | `"info"` | `"warn"` | `"error"`.
    pub level: String,
    /// `tracing` target — typically the module path the event was
    /// emitted from.
    pub target: String,
    /// Pre-formatted event body; structured fields are flattened
    /// into the message string.
    pub message: String,
    /// Monotonic per-actor sequence; cursor for the next call.
    pub sequence: u64,
}

/// `actor_logs` response. `next_since` echoes the cursor to thread
/// into the next call; `truncated_before` is `Some(seq)` when the
/// ring evicted entries the caller hadn't seen yet (the lowest
/// sequence still in the ring), `null` otherwise.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorLogsResponse {
    pub engine_id: String,
    pub mailbox_name: String,
    pub entries: Vec<ActorLogEntry>,
    pub next_since: u64,
    pub truncated_before: Option<u64>,
}

/// `actor_cost` arguments — dump one actor's per-handler
/// execution-cost EWMA table (iamacoffeepot/aether#1128). Phase 0
/// dark instrumentation: the substrate folds `(Finished − Received)`
/// from the dispatch trace bracket into a per-handler EWMA; this tool
/// reads it back. Measure-only — no scheduling effect.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ActorCostArgs {
    /// Engine UUID to pull from (from `list_engines`).
    pub engine_id: String,
    /// Mailbox name of the actor to query (e.g. `"aether.audio"`,
    /// `"aether.component/aether.embedded:camera"`). Every actor serves
    /// `aether.cost.tail` via the substrate's framework dispatch arm.
    pub mailbox_name: String,
    /// Optional kind-id filter (tagged `knd-XXXX-XXXX-XXXX` or raw
    /// decimal). Omitted dumps every handler row the actor declares.
    #[serde(default)]
    pub kind_id: Option<String>,
}

/// One per-handler cost row as `actor_cost` returns it. Mirrors
/// `aether_kinds::CostRow`. `mean_nanos` / `mad_nanos` are the
/// fixed-point-nanos EWMA mean + mean-absolute-deviation of the
/// handler's execution time; `samples` is the folded-sample count
/// (`0` is the neutral seed — a handler the actor declares but hasn't
/// run yet).
#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorCostRow {
    /// Tagged kind id (`knd-XXXX-XXXX-XXXX`).
    pub kind_id: String,
    /// Substrate-resolved kind name, or `null` for a component-defined
    /// kind the engine can't name.
    pub kind_name: Option<String>,
    pub mean_nanos: u64,
    pub mad_nanos: u64,
    pub samples: u64,
}

/// `actor_cost` response. `rows` is one [`ActorCostRow`] per handler
/// the queried actor declares (filtered to `kind_id` when set), in
/// unspecified order.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorCostResponse {
    pub engine_id: String,
    pub mailbox_name: String,
    pub rows: Vec<ActorCostRow>,
}

/// `describe_handles` arguments (ADR-0049 §10). Summarizes a
/// substrate's persistent handle store.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeHandlesArgs {
    /// Engine UUID to summarize (from `list_engines`).
    pub engine_id: String,
    /// Cap on the `top_by_size` / `top_by_recency` lists. Defaults to
    /// 16; clamped to 256.
    #[serde(default)]
    pub max: Option<u32>,
}

/// One native transform's metadata as `describe_transforms` renders it
/// (ADR-0048 §2). `transform_id` / `*_kind_id` are tagged-id strings
/// (ADR-0064).
#[derive(Debug, Serialize, JsonSchema)]
pub struct TransformListing {
    pub transform_id: String,
    pub name: &'static str,
    pub input_kind_ids: Vec<String>,
    pub output_kind_id: String,
}

/// One handle's summary line as `describe_handles` renders it. `handle_id`
/// and `kind_id` are tagged-id strings (ADR-0064).
#[derive(Debug, Serialize, JsonSchema)]
pub struct HandleSummaryJson {
    pub handle_id: String,
    pub kind_id: String,
    pub bytes_len: u32,
    pub pinned: bool,
    pub refcount: u32,
    pub created_at_ms: u64,
}

/// `describe_handles` response — the persistent store summary
/// (ADR-0049 §10).
#[derive(Debug, Serialize, JsonSchema)]
pub struct DescribeHandlesResponse {
    pub engine_id: String,
    pub total_entries: u32,
    pub in_memory_entries: u32,
    pub on_disk_entries: u32,
    pub pinned_entries: u32,
    pub in_memory_bytes: u64,
    pub on_disk_bytes: u64,
    pub on_disk_budget_bytes: u64,
    pub top_by_size: Vec<HandleSummaryJson>,
    pub top_by_recency: Vec<HandleSummaryJson>,
}

/// `describe_handlers` arguments (ADR-0109 §5) — describe a substrate's
/// native chassis caps' reply contracts, the native analogue of
/// `describe_component`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeHandlersArgs {
    /// Engine UUID to query (from `list_engines`).
    pub engine_id: String,
}

/// One native `#[handler]`'s reply contract as `describe_handlers`
/// renders it (ADR-0109 §5). `input_id` / `reply_id` are tagged-id
/// strings (`knd-XXXX-XXXX-XXXX`). `reply_id` / `reply_name` are `null`
/// for a fire-and-forget `-> ()` handler; `reply_name` is `null` for a
/// component-defined reply kind the static substrate vocabulary can't
/// name.
#[derive(Debug, Serialize, JsonSchema)]
pub struct NativeHandlerJson {
    pub input_id: String,
    pub input_name: String,
    pub reply_id: Option<String>,
    pub reply_name: Option<String>,
}

/// One native cap's handlers, grouped under its mailbox `namespace`
/// (ADR-0109 §5) — the `describe_component`-style view for a native cap.
#[derive(Debug, Serialize, JsonSchema)]
pub struct NativeCapHandlers {
    pub namespace: String,
    pub handlers: Vec<NativeHandlerJson>,
}

/// `describe_handlers` response — the native handler manifest folded per
/// mailbox namespace (ADR-0109 §5).
#[derive(Debug, Serialize, JsonSchema)]
pub struct DescribeHandlersResponse {
    pub engine_id: String,
    pub caps: Vec<NativeCapHandlers>,
}

/// `send_mail_traced` arguments — atomic batched dispatch with a shared
/// trace root (issue iamacoffeepot/aether#749). Every spec lands on the
/// same engine and inherits the same chassis root, so the response carries one
/// combined trace tree covering the whole batch.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendMailTracedArgs {
    /// Engine UUID the batch targets (from `list_engines`). All specs
    /// share this engine — atomic dispatch is per-engine.
    pub engine_id: String,
    /// One or more mail items, dispatched as children of one shared
    /// trace root. A bad spec aborts the whole batch before any mail
    /// moves (mirrors `capture_frame`'s bundle semantics).
    pub mails: Vec<TracedMailSpec>,
    /// Cap on wall-clock wait for the batch's chain to settle, in
    /// milliseconds. Defaults to 300000 (300s); clamped to 600000
    /// (600s) — sized to clear a provider cap's API timeout (e.g. the
    /// gemini cap's 180s) with margin, not the old 30s ceiling.
    #[serde(default)]
    pub settlement_timeout_ms: Option<u32>,
    /// When `true`, return the synchronous ack (the shared `root`)
    /// without awaiting chain settlement: `status` is `"dispatched"`,
    /// and `mails` / `in_flight` / `replies` are `null`. Default
    /// `false` — the call now blocks until the batch settles and
    /// returns the trace tree plus the correlated replies.
    #[serde(default)]
    pub fire_and_forget: bool,
}

/// One mail in a `send_mail_traced` batch. Like [`CaptureMailSpec`] but
/// scoped to the trace-dispatch surface — the engine is fixed once at
/// the batch level by [`SendMailTracedArgs::engine_id`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TracedMailSpec {
    /// Mailbox name on the target engine (e.g. `"aether.render"`).
    pub recipient_name: String,
    /// Kind name, resolved against the substrate kind vocabulary.
    pub kind_name: String,
    /// Structured params, schema-encoded to wire bytes. Omit or
    /// `null` for a fieldless kind.
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

/// `send_mail_traced` response. One combined trace tree for the whole
/// batch (shared-root atomic dispatch), or a `status` telling the
/// caller the batch never settled.
#[derive(Debug, Serialize, JsonSchema)]
pub struct SendMailTracedResponse {
    /// `"settled"` once the batch's chain settled and the tree is
    /// populated, `"timeout"` when the substrate didn't reply within
    /// the `settlement_timeout_ms` window, or `"dispatched"` when
    /// `fire_and_forget` was set (ack only, no settlement wait).
    pub status: String,
    /// Chassis-root `MailId` every spec inherited. Populated on
    /// `settled` and `dispatched`, `null` on `timeout`.
    pub root: Option<MailIdJson>,
    /// Mail nodes in the settled tree. Order is unspecified — agents
    /// reconstruct chains via `parent` edges. `null` on `dispatched` /
    /// `timeout`.
    pub mails: Option<Vec<MailNodeJson>>,
    /// Root's `in_flight` count at describe time. `0` for a fully-
    /// settled batch; non-zero indicates the chain re-armed after the
    /// initial settle (rare; reflects late-arriving descendants).
    pub in_flight: Option<u32>,
    /// Correlated reply payloads the batch's shared `cid` collected, in
    /// arrival order — one flat list for the whole atomic batch (the
    /// batch is one wire `Call`, so there is no per-item correlation to
    /// group by). `null` on `dispatched`; an empty list on `settled`
    /// when no reply was emitted.
    pub replies: Option<Vec<ReplyEventJson>>,
}

/// `MailId` rendered for MCP: the sender mailbox as a tagged-id
/// string (ADR-0064) plus the per-actor correlation counter.
#[derive(Debug, Serialize, JsonSchema)]
pub struct MailIdJson {
    /// Tagged mailbox id (`mbx-…`) of the producer that minted this
    /// `MailId`. `mbx-aaaa-aaaa-aaaa` is the `aether.chassis` sender,
    /// the marker for chassis-originated roots (ADR-0080 §1).
    pub sender: String,
    /// Per-actor monotonic counter at mint time. Combined with
    /// `sender` it uniquely identifies the mail across the substrate.
    pub correlation_id: u64,
}

/// One mail node in a `send_mail_traced` tree (`MailNodeWire`
/// transcoded for MCP — tagged-id strings, JSON-shaped timestamps).
#[derive(Debug, Serialize, JsonSchema)]
pub struct MailNodeJson {
    pub mail_id: MailIdJson,
    /// `null` for chassis-root mail (no producer-side parent).
    pub parent: Option<MailIdJson>,
    /// Tagged mailbox id (`mbx-…`) of the producer.
    pub sender: String,
    /// Tagged mailbox id (`mbx-…`) of the recipient.
    pub recipient: String,
    /// Tagged kind id (`knd-…`) of the payload schema.
    pub kind: String,
    /// iamacoffeepot/aether#1158: the instant the producer's outbound
    /// blob opened (the first buffered send of the flush window).
    /// `t_sent − t_construct_start` is the **construct** span (the
    /// producer building the blob); on eager paths it equals `t_sent`.
    /// Monotonic nanoseconds since substrate boot.
    pub t_construct_start: u64,
    /// Monotonic nanoseconds since substrate boot.
    pub t_sent: u64,
    /// Set once the recipient dispatcher entered the handler; `null`
    /// until then.
    pub t_received: Option<u64>,
    /// Set once the recipient dispatcher exited the handler; `null`
    /// until then (i.e. mail still in flight).
    pub t_finished: Option<u64>,
    /// OS thread the handler ran on (issue 734). `None` until the
    /// `Received` event lands or for anonymous threads.
    pub thread_name: Option<String>,
}

/// One frame check in a `capture_frame` `checks` list. Names a
/// substrate-side reduction the render thread scores on the raw RGBA
/// the PNG is built from, so a smoke demo asserts without decoding the
/// returned PNG (iamacoffeepot/aether#1777).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CaptureCheckSpec {
    /// Which reduction to run: `"not_all_black"`,
    /// `"differs_from_background"`, `"coverage"`, `"centroid"`, or
    /// `"bounding_box"`.
    pub reduction: String,
    /// Per-channel tolerance (0-255) for the lit/background partition
    /// the silhouette reductions share. Defaults to 0.
    #[serde(default)]
    pub tolerance: u8,
    /// Explicit background RGB the reduction partitions against. Omit
    /// or `null` to use the frame's top-left pixel (the
    /// `differs_from_background` convention).
    #[serde(default)]
    pub background: Option<[u8; 3]>,
}

/// Optional reference-image similarity check for `capture_frame`. The
/// render thread scores the captured RGBA against a decoded reference
/// image with a normalised mean-absolute-error metric and returns
/// `similarity_score` / `similarity_pass` alongside the PNG
/// (iamacoffeepot/aether#1780).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CaptureSimilaritySpec {
    /// Filesystem namespace the reference image lives in (the same
    /// namespaces `aether.fs` exposes, e.g. `"assets"`).
    pub namespace: String,
    /// Path to the reference image within `namespace`.
    pub reference_path: String,
    /// Maximum normalised MAE in `[0.0, 1.0]` that still counts as a
    /// match: `similarity_pass` is `true` when the score is `<=` this.
    /// `0.0` demands an exact match; `1.0` passes any frame.
    pub threshold: f32,
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
    /// Reductions to score substrate-side on the captured frame's raw
    /// RGBA, returned as a `verdict` alongside the PNG. Omit for a
    /// PNG-only capture (iamacoffeepot/aether#1777).
    #[serde(default)]
    pub checks: Vec<CaptureCheckSpec>,
    /// Optional reference-image similarity check scored on the captured
    /// frame's raw RGBA, returned as `similarity_score` /
    /// `similarity_pass`. Omit for no comparison
    /// (iamacoffeepot/aether#1780).
    #[serde(default)]
    pub similarity: Option<CaptureSimilaritySpec>,
}

// `SubmitDagArgs` lives in its own module so a *scoped*
// `#![allow(unused_qualifications)]` can cover schemars' `schema_with`
// codegen without relaxing the workspace lint anywhere else. The
// `schema_with` expansion emits a `_SchemarsSchemaWithFunction` wrapper
// containing `impl schemars::JsonSchema` / `<… as schemars::JsonSchema>`
// — paths that are redundant in this crate's lint context (we
// `use schemars::JsonSchema`) and so trip `unused_qualifications`. That
// generated `impl` is a module-level sibling of the struct, so the lint
// can only be silenced at module scope (an `#[allow]` on the struct or
// field never reaches it). Isolating the one struct that uses
// `schema_with` keeps the allow surgical.
mod submit_dag_args {
    #![allow(unused_qualifications)]

    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::DagDescriptorArg;

    /// `schema_with` hook for [`SubmitDagArgs::descriptor`]: returns the
    /// `DagDescriptorArg` schema *inline* (`json_schema` yields the
    /// type's own object definition, not a `subschema_for` `$ref`), so
    /// the advertised property carries `type: object` with the full
    /// node/edge structure directly — clients then send a nested object
    /// rather than stringifying this top-level arg.
    fn descriptor_inline_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        DagDescriptorArg::json_schema(generator)
    }

    /// `submit_dag` arguments (ADR-0047 §9).
    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct SubmitDagArgs {
        /// Engine UUID the DAG submits to (from `list_engines`).
        pub engine_id: String,
        /// The DAG descriptor, encoded against the `aether.dag.descriptor`
        /// kind schema. `nodes` is an array of externally-tagged `Node`
        /// variants (`{ "Source": { id, mailbox, kind_id, payload_path }
        /// }`, `{ "Observer": { id, recipient, kind_id } }`, `{ "Call": {
        /// id, recipient, kind_id } }`); `edges` is an array of `{ from,
        /// to, slot }`; `version` is `1`. Tagged-string ids (`mbx-…`,
        /// `knd-…`) per ADR-0064/0065.
        ///
        /// **`payload_path` is a tool-layer virtual field.** Each
        /// `Source` carries `payload_path: String` instead of the wire
        /// `payload: Vec<u8>`: `submit_dag` reads the file at that path
        /// and substitutes the bytes into the wire `payload` before
        /// encoding. The path must be readable from the MCP process
        /// (colocated with the substrate in v1). A `Source` may instead
        /// carry an inline `payload` byte array; `payload_path` takes
        /// precedence when both are present.
        ///
        /// Deserializes into the typed [`DagDescriptorArg`], so the
        /// node/edge shape and tagged ids are validated with precise
        /// parse errors. `schema_with` advertises that type's schema
        /// *inline* (`type: object` with the full structure on this
        /// property) rather than a bare `serde_json::Value` "any" or a
        /// `$ref` — clients then send a nested object instead of
        /// stringifying this top-level arg (an untyped arg got delivered
        /// as a string, which `Submit` decode rejected as "expected
        /// object").
        #[schemars(schema_with = "descriptor_inline_schema")]
        pub descriptor: DagDescriptorArg,
        /// Cap on wall-clock wait for the synchronous validation verdict,
        /// in milliseconds. Defaults to 5000. Guards against a hung
        /// validator, not normal latency — validation is microseconds,
        /// and execution is async (poll via `dag_status`).
        #[serde(default)]
        pub timeout_ms: Option<u32>,
    }
}

pub use submit_dag_args::SubmitDagArgs;

/// Tool-layer mirror of `aether_kinds::dag::DagDescriptor`. Differs from
/// the wire kind only where the MCP boundary needs it to: `Source`
/// carries a `payload_path` (a file the tool reads into the wire
/// `payload` bytes) instead of inline bytes, and ids arrive as tagged
/// strings. The id fields are the real `MailboxId` / `KindId` /
/// `TransformId` — their serde already parses the tagged `mbx-` / `knd-`
/// / `tfm-` form in JSON; `#[schemars(with = "String")]` only patches
/// the *advertised* schema to `string` (those types intentionally don't
/// implement `JsonSchema`, which lives in the tool layer, not the
/// `no_std` data layer).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DagDescriptorArg {
    /// Descriptor version (`1` for the current wire shape).
    pub version: u16,
    /// DAG nodes — externally-tagged `Source` / `Transform` / `Call` /
    /// `Observer` variants.
    pub nodes: Vec<NodeArg>,
    /// Directed edges wiring a producer node's output into a consumer
    /// node's input slot.
    pub edges: Vec<EdgeArg>,
}

/// One DAG node — tool-layer mirror of `aether_kinds::dag::Node`,
/// externally tagged (`{ "Source": { … } }`).
#[derive(Debug, Deserialize, JsonSchema)]
pub enum NodeArg {
    /// Root node: dispatches a payload to `mailbox` as `kind_id` and
    /// feeds the reply downstream. Sources have no incoming edges.
    Source {
        /// Descriptor-local node id, unique within this descriptor.
        id: u32,
        /// Sink mailbox (tagged `mbx-…`).
        #[schemars(with = "String")]
        mailbox: MailboxId,
        /// Kind dispatched to `mailbox` (tagged `knd-…`).
        #[schemars(with = "String")]
        kind_id: KindId,
        /// Filesystem path the tool reads into the wire payload bytes
        /// (readable from the MCP process). Takes precedence over inline
        /// `payload` when both are present.
        #[serde(default)]
        payload_path: Option<String>,
        /// Inline payload bytes — an alternative to `payload_path` for
        /// small payloads.
        #[serde(default)]
        payload: Option<Vec<u8>>,
    },
    /// Mid-graph pure native transform (ADR-0048).
    Transform {
        /// Descriptor-local node id.
        id: u32,
        /// Registered native transform (tagged `tfm-…`).
        #[schemars(with = "String")]
        transform_id: TransformId,
        /// The kind this transform produces (tagged `knd-…`).
        #[schemars(with = "String")]
        output_kind_id: KindId,
        /// Per-call deadline in ms; `None` uses the executor default.
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// Mid-graph effectful cap dispatch; its output handle is a
    /// self-describing `Bundle`.
    Call {
        /// Descriptor-local node id.
        id: u32,
        /// Dispatch recipient (tagged `mbx-…`).
        #[schemars(with = "String")]
        recipient: MailboxId,
        /// Kind dispatched to `recipient` (tagged `knd-…`).
        #[schemars(with = "String")]
        kind_id: KindId,
    },
    /// Terminal node: assembles `kind_id` from incoming edges and
    /// dispatches it to `recipient`. Observers have no outgoing edges.
    Observer {
        /// Descriptor-local node id.
        id: u32,
        /// Dispatch recipient (tagged `mbx-…`).
        #[schemars(with = "String")]
        recipient: MailboxId,
        /// Kind assembled and dispatched to `recipient` (tagged `knd-…`).
        #[schemars(with = "String")]
        kind_id: KindId,
    },
}

/// One directed DAG edge — mirror of `aether_kinds::dag::Edge`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EdgeArg {
    /// Producing node id.
    pub from: u32,
    /// Consuming node id.
    pub to: u32,
    /// Consumer-side input slot index.
    pub slot: u32,
}

/// `dag_status` arguments (ADR-0047 §9).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DagStatusArgs {
    /// Engine UUID hosting the DAG (from `list_engines`).
    pub engine_id: String,
    /// Tagged DAG id (`dag-…`) returned by `submit_dag`.
    pub dag_id: String,
}

/// `dag_cancel` arguments (ADR-0047 §9).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DagCancelArgs {
    /// Engine UUID hosting the DAG (from `list_engines`).
    pub engine_id: String,
    /// Tagged DAG id (`dag-…`) returned by `submit_dag`.
    pub dag_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: `submit_dag`'s `descriptor` must advertise as an
    /// object — either inline `type: object` on the property or via a
    /// named definition it `$ref`s. As a bare `serde_json::Value` it
    /// advertised no type at all, and MCP clients stringify an untyped
    /// top-level object arg — the substrate then sees a string at
    /// `$.descriptor` and rejects it ("expected object"), blocking every
    /// DAG submission over MCP.
    #[test]
    fn submit_dag_descriptor_advertises_object_type() {
        let schema = schemars::schema_for!(SubmitDagArgs);
        let v = serde_json::to_value(&schema).expect("schema serializes");
        let object = serde_json::Value::String("object".to_owned());
        let defs = v.get("$defs").or_else(|| v.get("definitions"));
        let advertises_object = v["properties"]["descriptor"]["type"] == object
            || defs
                .and_then(|d| d.get("DagDescriptorArg"))
                .is_some_and(|d| d["type"] == object);
        assert!(
            advertises_object,
            "descriptor must advertise as an object (inline or via a named def); schema: {v}"
        );
    }
}
