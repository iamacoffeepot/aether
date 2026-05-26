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

/// `actor_logs` arguments — pull entries out of one actor's
/// per-actor log ring (ADR-0081). The substrate-side aggregator
/// retired; each call queries a single actor by name. Aggregate
/// client-side if you want a cross-actor view.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ActorLogsArgs {
    /// Engine UUID to pull from (from `list_engines`).
    pub engine_id: String,
    /// Mailbox name of the actor to query (e.g. `"aether.audio"`,
    /// `"aether.component.trampoline:camera"`). The substrate's
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
    /// `"aether.component.trampoline:camera"`). Every actor serves
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
    /// milliseconds. Defaults to 5000; clamped to 30000.
    #[serde(default)]
    pub settlement_timeout_ms: Option<u32>,
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
    /// the `settlement_timeout_ms` window.
    pub status: String,
    /// Chassis-root `MailId` every spec inherited. Populated on
    /// `settled`, `null` on `timeout`.
    pub root: Option<MailIdJson>,
    /// Mail nodes in the settled tree. Order is unspecified — agents
    /// reconstruct chains via `parent` edges.
    pub mails: Option<Vec<MailNodeJson>>,
    /// Root's `in_flight` count at describe time. `0` for a fully-
    /// settled batch; non-zero indicates the chain re-armed after the
    /// initial settle (rare; reflects late-arriving descendants).
    pub in_flight: Option<u32>,
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
