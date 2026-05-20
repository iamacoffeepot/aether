//! The `aether-mcp` tool surface: the per-session [`Mcp`] service, its
//! `#[tool_router]` impl, and the `ServerHandler` (issue 763 P5b/P5c).
//!
//! Each tool translates to RPC `Call`s over the shared [`RpcSession`].
//! Engine-management tools (`list_engines`, `spawn_substrate`,
//! `terminate_substrate`) address the hub's own `aether.engine` cap
//! (`engine = None`, dispatched locally on the hub); the per-engine
//! tools (`send_mail`, `load_component`, `replace_component`,
//! `capture_frame`) address a specific substrate (`engine = Some`),
//! which the hub routes through to the matching proxy. `describe_kinds`
//! and `describe_component` answer locally — from the substrate kind
//! inventory baked into `aether-kinds` and from a component-capability
//! cache populated by `load_component` / `replace_component`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aether_capabilities::rpc::{MailEnvelope, MailboxAddress};
use aether_data::MailId;
use aether_data::canonical::kind_id_from_parts;
use aether_data::{
    DagId, EngineId, Kind, KindDescriptor, KindId, MailboxId, Schema, Tag, Uuid,
    mailbox_id_from_name, tagged_id,
};
use aether_kinds::{
    Cancel, CancelResult, CaptureFrame, CaptureFrameResult, ComponentCapabilities, ListEngines,
    ListEnginesResult, LoadComponent, LoadResult, MailEnvelope as KindMailEnvelope,
    ReplaceComponent, ReplaceResult, SpawnEngine, SpawnEngineResult, Status, StatusResult, Submit,
    SubmitResult, TerminateEngine, TerminateEngineResult,
    trace::{
        DescribeTree, DescribeTreeResult, DispatchTraced, DispatchTracedAck, MailNodeWire,
        TRACE_OBSERVER_MAILBOX_NAME,
    },
};
use base64::Engine as _;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::args::ActorLogEntry;
use crate::args::ActorLogsArgs;
use crate::args::ActorLogsResponse;
use crate::args::{
    CaptureFrameArgs, CaptureMailSpec, DagCancelArgs, DagStatusArgs, DescribeComponentArgs,
    DescribeHandlesArgs, DescribeHandlesResponse, EngineInfo, HandleSummaryJson, LoadComponentArgs,
    MailIdJson, MailNodeJson, MailSpec, MailStatus, ReplaceComponentArgs, SendMailArgs,
    SendMailTracedArgs, SendMailTracedResponse, SpawnSubstrateArgs, SubmitDagArgs,
    TerminateSubstrateArgs, TracedMailSpec,
};
use crate::rpc::RpcSession;
use aether_kinds::descriptors;
use base64::engine::general_purpose::STANDARD;
use std::time::Duration;
use tokio::fs;
use tokio::time;

/// Mailbox name of the hub's engines cap — the `engine = None` target
/// for the engine-management tools.
const ENGINE_CAP: &str = "aether.engine";
/// Mailbox name of a substrate's component-host cap.
const COMPONENT_CAP: &str = "aether.component";
/// Mailbox name of a substrate's render cap.
const RENDER_CAP: &str = "aether.render";
/// Mailbox name of a substrate's handle-store cap (ADR-0045 / ADR-0049).
const HANDLE_CAP: &str = "aether.handle";
/// Mailbox name of a substrate's DAG-executor cap (ADR-0047).
const DAG_CAP: &str = "aether.dag";

/// Component receive-side capabilities, keyed by `(engine, mailbox)`.
/// Populated from `load_component` / `replace_component` replies and
/// read by `describe_component` — the forward-model stand-in for the
/// embedded hub's component registry.
pub type ComponentCache = Mutex<HashMap<(EngineId, MailboxId), ComponentCapabilities>>;

/// Per-session MCP service. `rmcp` calls the factory once per session
/// and may clone the result for concurrent tool dispatch — `session`
/// and `components` are `Arc`s, so clones share the one hub connection
/// and one component cache.
#[derive(Clone)]
pub struct Mcp {
    session: Arc<RpcSession>,
    components: Arc<ComponentCache>,
    // The `#[tool_router]` macro stores the router instance here; it's
    // consumed by `#[tool_handler]` codegen rather than read by name, so
    // the dead-code lint fires under `-D warnings` despite the field
    // being load-bearing. (rmcp 1.7 stopped tagging the field as used.)
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl Mcp {
    /// Construct a per-session service over an established hub
    /// connection + the process-wide component cache.
    pub fn new(session: Arc<RpcSession>, components: Arc<ComponentCache>) -> Self {
        Self {
            session,
            components,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl Mcp {
    #[tool(
        description = "List every engine the hub currently supervises. Each item reports the engine_id (pass it to send_mail / terminate_substrate) and the localhost RPC port the hub assigned its substrate."
    )]
    pub async fn list_engines(&self) -> Result<String, McpError> {
        let reply = self
            .session
            .call_one(local_envelope(ENGINE_CAP, &ListEngines {}))
            .await
            .map_err(internal)?;
        let result = ListEnginesResult::decode_from_bytes(&reply.payload)
            .ok_or_else(|| internal_msg("undecodable ListEnginesResult"))?;
        let engines: Vec<EngineInfo> = result
            .engines
            .into_iter()
            .map(|e| EngineInfo {
                engine_id: e.engine_id,
                rpc_port: e.rpc_port,
            })
            .collect();
        json(&engines)
    }

    #[tool(
        description = "Fork+exec a substrate binary as a child of the hub. The hub assigns the substrate a free localhost RPC port, injects it as AETHER_RPC_PORT, forks the binary, and connects a proxy. Returns the engine_id and rpc_port on success."
    )]
    pub async fn spawn_substrate(
        &self,
        Parameters(args): Parameters<SpawnSubstrateArgs>,
    ) -> Result<String, McpError> {
        let reply = self
            .session
            .call_one(local_envelope(
                ENGINE_CAP,
                &SpawnEngine {
                    binary_path: args.binary_path,
                    args: args.args,
                },
            ))
            .await
            .map_err(internal)?;
        match SpawnEngineResult::decode_from_bytes(&reply.payload) {
            Some(SpawnEngineResult::Ok {
                engine_id,
                rpc_port,
            }) => json(&EngineInfo {
                engine_id,
                rpc_port,
            }),
            Some(SpawnEngineResult::Err { error }) => Err(internal_msg(&error)),
            None => Err(internal_msg("undecodable SpawnEngineResult")),
        }
    }

    #[tool(
        description = "Terminate a substrate the hub supervises. The hub forwards the request to the engine's proxy, which SIGKILLs the child process and self-shuts-down."
    )]
    pub async fn terminate_substrate(
        &self,
        Parameters(args): Parameters<TerminateSubstrateArgs>,
    ) -> Result<String, McpError> {
        let reply = self
            .session
            .call_one(local_envelope(
                ENGINE_CAP,
                &TerminateEngine {
                    engine_id: args.engine_id,
                },
            ))
            .await
            .map_err(internal)?;
        match TerminateEngineResult::decode_from_bytes(&reply.payload) {
            Some(TerminateEngineResult::Ok) => json(&serde_json::json!({ "status": "terminated" })),
            Some(TerminateEngineResult::Err { error }) => Err(internal_msg(&error)),
            None => Err(internal_msg("undecodable TerminateEngineResult")),
        }
    }

    #[tool(
        description = "Send one or more mail items to substrate mailboxes. Each item carries structured `params`, schema-encoded against the substrate kind vocabulary. Best-effort batch: per-item status is returned and one failure doesn't abort siblings. 'delivered' means the call reached the substrate and its dispatch chain settled."
    )]
    pub async fn send_mail(
        &self,
        Parameters(args): Parameters<SendMailArgs>,
    ) -> Result<String, McpError> {
        // Snapshot the substrate descriptor inventory once for the
        // whole batch rather than per item.
        let descriptors = descriptors::all();
        let mut statuses = Vec::with_capacity(args.mails.len());
        for (index, spec) in args.mails.into_iter().enumerate() {
            let status = match self.deliver_one(&descriptors, spec).await {
                Ok(()) => "delivered".to_owned(),
                Err(e) => format!("error: {e}"),
            };
            statuses.push(MailStatus { index, status });
        }
        json(&statuses)
    }

    #[tool(
        description = "Atomic batched dispatch with combined trace tree. Like send_mail but every spec lands on the engine's aether.trace mailbox under one shared chassis root, and the response returns the full trace subtree once the chain settles — no window guessing, no separate describe_tree call. Two-call protocol behind the scenes: the substrate emits a synchronous ack with the root id, the caller waits for chain settlement on the wire, then issues a describe_tree against the captured root. Bad specs abort the whole batch before any mail moves (mirrors capture_frame). settlement_timeout_ms caps wall-clock wait (default 5000, max 30000); on timeout the response carries status:timeout with no root or tree."
    )]
    pub async fn send_mail_traced(
        &self,
        Parameters(args): Parameters<SendMailTracedArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let descriptors = descriptors::all();
        // Encode the batch before sending — a bad spec produces a
        // clean invalid-params error and never touches the wire.
        // Same shape `CaptureFrame` carries: `Vec<MailEnvelope>` with
        // name-level addressing the substrate resolves at dispatch
        // time via `resolve_bundle`.
        let mails = encode_traced_bundle(&descriptors, &args.mails)
            .map_err(|e| McpError::invalid_params(format!("send_mail_traced batch: {e}"), None))?;
        let timeout_ms = args.settlement_timeout_ms.unwrap_or(5000).min(30000);
        let dispatch_envelope = engine_envelope(
            engine,
            TRACE_OBSERVER_MAILBOX_NAME,
            &DispatchTraced { mails },
        );

        // Round 1: ack carries the chassis-root MailId; ReplyEnd
        // closes when the chain settles substrate-side.
        let ack_reply = match time::timeout(
            Duration::from_millis(u64::from(timeout_ms)),
            self.session.call_one(dispatch_envelope),
        )
        .await
        {
            Ok(Ok(reply)) => reply,
            Ok(Err(e)) => return Err(internal(e)),
            Err(_) => {
                return json(&SendMailTracedResponse {
                    status: "timeout".into(),
                    root: None,
                    mails: None,
                    in_flight: None,
                });
            }
        };
        let ack = DispatchTracedAck::decode_from_bytes(&ack_reply.payload)
            .ok_or_else(|| internal_msg("undecodable DispatchTracedAck"))?;
        let root = match ack {
            DispatchTracedAck::Ok { root } => root,
            DispatchTracedAck::Err { error } => {
                return Err(internal_msg(&format!(
                    "send_mail_traced dispatch failed: {error}"
                )));
            }
        };

        // Round 2: pull the populated tree. Microseconds — already
        // in-memory in the substrate's TraceObserver at this point.
        let tree_reply = self
            .session
            .call_one(engine_envelope(
                engine,
                TRACE_OBSERVER_MAILBOX_NAME,
                &DescribeTree { root },
            ))
            .await
            .map_err(internal)?;
        let tree = DescribeTreeResult::decode_from_bytes(&tree_reply.payload)
            .ok_or_else(|| internal_msg("undecodable DescribeTreeResult"))?;

        match tree {
            DescribeTreeResult::Ok {
                root,
                in_flight,
                mails,
            } => json(&SendMailTracedResponse {
                status: "settled".into(),
                root: Some(mail_id_to_json(root)),
                mails: Some(mails.into_iter().map(mail_node_to_json).collect()),
                in_flight: Some(in_flight),
            }),
            DescribeTreeResult::Err { not_found } => Err(internal_msg(&format!(
                "describe_tree: root {not_found:?} not found"
            ))),
        }
    }

    #[tool(
        description = "Load a WASM component into a substrate by filesystem path. aether-mcp reads the binary, forwards it as aether.component.load to the engine's aether.component mailbox, and awaits the LoadResult — returning {mailbox_id, name, capabilities} or an error. The path must exist as given (no ~ expansion, no relative resolution). The component's kind vocabulary rides in the wasm's aether.kinds custom section."
    )]
    pub async fn load_component(
        &self,
        Parameters(args): Parameters<LoadComponentArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let wasm = fs::read(&args.binary_path).await.map_err(|e| {
            McpError::invalid_params(
                format!("reading binary_path {:?}: {e}", args.binary_path),
                None,
            )
        })?;
        let reply = self
            .session
            .call_one(engine_envelope(
                engine,
                COMPONENT_CAP,
                &LoadComponent {
                    wasm,
                    name: args.name,
                },
            ))
            .await
            .map_err(internal)?;
        match LoadResult::decode_from_bytes(&reply.payload) {
            Some(LoadResult::Ok {
                mailbox_id,
                name,
                capabilities,
            }) => {
                self.components
                    .lock()
                    .expect("component cache mutex is never poisoned")
                    .insert((engine, mailbox_id), capabilities.clone());
                json(&serde_json::json!({
                    "mailbox_id": mailbox_id,
                    "name": name,
                    "capabilities": capabilities,
                }))
            }
            Some(LoadResult::Err { error }) => Err(internal_msg(&error)),
            None => Err(internal_msg("undecodable LoadResult")),
        }
    }

    #[tool(
        description = "Atomically replace a live component's WASM with a new binary loaded from a filesystem path (ADR-0022 structural splice). aether-mcp reads the binary and forwards aether.component.replace to the engine's aether.component mailbox. drain_timeout_ms is accepted for wire compatibility but currently ignored. Returns the replaced component's advertised capabilities."
    )]
    pub async fn replace_component(
        &self,
        Parameters(args): Parameters<ReplaceComponentArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let mailbox_id = parse_mailbox_id(&args.mailbox_id)?;
        let wasm = fs::read(&args.binary_path).await.map_err(|e| {
            McpError::invalid_params(
                format!("reading binary_path {:?}: {e}", args.binary_path),
                None,
            )
        })?;
        let reply = self
            .session
            .call_one(engine_envelope(
                engine,
                COMPONENT_CAP,
                &ReplaceComponent {
                    mailbox_id,
                    wasm,
                    drain_timeout_ms: args.drain_timeout_ms,
                },
            ))
            .await
            .map_err(internal)?;
        match ReplaceResult::decode_from_bytes(&reply.payload) {
            Some(ReplaceResult::Ok { capabilities }) => {
                self.components
                    .lock()
                    .expect("component cache mutex is never poisoned")
                    .insert((engine, mailbox_id), capabilities.clone());
                json(&capabilities)
            }
            Some(ReplaceResult::Err { error }) => Err(internal_msg(&error)),
            None => Err(internal_msg("undecodable ReplaceResult")),
        }
    }

    #[tool(
        description = "Capture an engine's current frame as a PNG, returned inline as image content. Optionally carries two mail bundles dispatched atomically around the capture: `mails` fires before readback (state changes that should appear in the image), `after_mails` fires after (cleanup). A bad bundle entry aborts the whole capture before any mail moves."
    )]
    pub async fn capture_frame(
        &self,
        Parameters(args): Parameters<CaptureFrameArgs>,
    ) -> Result<CallToolResult, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        // Encode both bundles before sending — a bad entry produces a
        // clean invalid-params error and never touches the wire.
        let descriptors = descriptors::all();
        let mails = encode_capture_bundle(&descriptors, &args.mails).map_err(|e| {
            McpError::invalid_params(format!("capture_frame mails bundle: {e}"), None)
        })?;
        let after_mails = encode_capture_bundle(&descriptors, &args.after_mails).map_err(|e| {
            McpError::invalid_params(format!("capture_frame after_mails bundle: {e}"), None)
        })?;
        let reply = self
            .session
            .call_one(engine_envelope(
                engine,
                RENDER_CAP,
                &CaptureFrame { mails, after_mails },
            ))
            .await
            .map_err(internal)?;
        match CaptureFrameResult::decode_from_bytes(&reply.payload) {
            Some(CaptureFrameResult::Ok { png }) => {
                let encoded = STANDARD.encode(&png);
                Ok(CallToolResult::success(vec![Content::image(
                    encoded,
                    "image/png",
                )]))
            }
            Some(CaptureFrameResult::Err { error }) => Err(internal_msg(&error)),
            None => Err(internal_msg("undecodable CaptureFrameResult")),
        }
    }

    #[tool(
        description = "List the substrate kind vocabulary — every aether.* kind with its full schema, enough to build send_mail params. This is the static vocabulary aether-mcp ships with, not a per-engine query; component-defined kinds aren't included (use describe_component for a loaded component's handlers)."
    )]
    pub async fn describe_kinds(&self) -> Result<String, McpError> {
        json(&descriptors::all())
    }

    #[tool(
        description = "Describe a loaded component's receive-side capabilities (ADR-0033): the kinds it typed-handles with per-handler docs, whether it has a fallback catchall, and its top-level doc. Reads aether-mcp's component cache, populated by load_component / replace_component — describing a component aether-mcp didn't load (or after an aether-mcp restart) returns an error."
    )]
    pub async fn describe_component(
        &self,
        Parameters(args): Parameters<DescribeComponentArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let mailbox_id = parse_mailbox_id(&args.mailbox_id)?;
        let caps = self
            .components
            .lock()
            .expect("component cache mutex is never poisoned")
            .get(&(engine, mailbox_id))
            .cloned();
        match caps {
            Some(caps) => json(&caps),
            None => Err(McpError::invalid_params(
                format!(
                    "no component cached at {} on engine {} — load_component / replace_component \
                     populate this cache",
                    args.mailbox_id, args.engine_id
                ),
                None,
            )),
        }
    }

    #[tool(
        description = "Pull recent log entries from one actor's per-actor log ring (ADR-0081). \
                       Sends aether.log.tail to the named mailbox and decodes aether.log.tail_result. \
                       Every actor — native or wasm trampoline — serves this kind via the substrate's \
                       framework dispatch arm, so any mailbox is queryable (e.g. \"aether.audio\", \
                       \"aether.component.trampoline:camera\"). `max` defaults to 100 and clamps to 1000; \
                       pass `level` (`trace|debug|info|warn|error`) for server-side filtering; pass \
                       `since` (the prior call's `next_since`) to walk past already-seen entries without \
                       double-reading. `truncated_before` in the reply is `Some(seq)` when the ring \
                       evicted entries the caller hadn't seen yet (the lowest sequence still held). \
                       Aggregate across actors by calling this tool once per mailbox client-side."
    )]
    pub async fn actor_logs(
        &self,
        Parameters(args): Parameters<ActorLogsArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let engine_id_str = args.engine_id.clone();
        let mailbox_name = args.mailbox_name.clone();
        let min_level = match args.level.as_deref() {
            Some(s) => Some(parse_level(s)?),
            None => None,
        };
        let request = aether_kinds::LogTail {
            max: args.max.unwrap_or(0),
            min_level,
            since: args.since,
        };
        let reply = self
            .session
            .call_one(engine_envelope(engine, &args.mailbox_name, &request))
            .await
            .map_err(internal)?;
        match aether_kinds::LogTailResult::decode_from_bytes(&reply.payload) {
            Some(aether_kinds::LogTailResult::Ok {
                entries,
                next_since,
                truncated_before,
            }) => {
                let response = ActorLogsResponse {
                    engine_id: engine_id_str,
                    mailbox_name,
                    entries: entries
                        .into_iter()
                        .map(|e| ActorLogEntry {
                            timestamp_unix_ms: e.timestamp_unix_ms,
                            level: level_to_str(e.level).to_owned(),
                            target: e.target,
                            message: e.message,
                            sequence: e.sequence,
                        })
                        .collect(),
                    next_since,
                    truncated_before,
                };
                json(&response)
            }
            // Issue 963: name the agent-supplied mailbox in the error
            // so an `actor_logs` against an unregistered mailbox (which
            // the substrate now answers with a synthesized
            // `LogTailResult::Err`, mailer.rs `None` arm) reads as
            // "that mailbox doesn't exist" rather than a bare relayed
            // substrate string.
            Some(aether_kinds::LogTailResult::Err { error }) => {
                Err(internal_msg(&actor_logs_err_message(&mailbox_name, &error)))
            }
            None => Err(internal_msg("undecodable LogTailResult")),
        }
    }

    #[tool(
        description = "Summarize a substrate's persistent handle store (ADR-0049 §10). Sends \
                       aether.handle.describe to the engine's aether.handle cap and decodes \
                       aether.handle.describe_result. Returns total / in-memory / on-disk / pinned \
                       entry counts, in-memory + on-disk bytes vs the disk budget, and the top-N \
                       handles by size and by recency (handle_id + kind_id as tagged-id strings, \
                       bytes_len, pinned, refcount, created_at_ms). Use it to triage \"why is my \
                       handle store at the disk-budget cap\" without ssh-ing into the machine. \
                       `max` defaults to 16, clamps to 256."
    )]
    pub async fn describe_handles(
        &self,
        Parameters(args): Parameters<DescribeHandlesArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let request = aether_kinds::HandleDescribe {
            max: args.max.unwrap_or(0),
        };
        let reply = self
            .session
            .call_one(engine_envelope(engine, HANDLE_CAP, &request))
            .await
            .map_err(internal)?;
        let Some(result) = aether_kinds::HandleDescribeResult::decode_from_bytes(&reply.payload)
        else {
            return Err(internal_msg("undecodable HandleDescribeResult"));
        };
        let to_json = |s: &aether_kinds::HandleSummary| HandleSummaryJson {
            // Handle + kind ids are tagged 64-bit ids (ADR-0064); render
            // them as the tagged-id strings the rest of the MCP wire uses.
            // Fall back to the raw decimal only if a synthetic id lacks
            // tag bits (test fixtures), so the tool never panics.
            handle_id: tagged_id::encode(s.handle_id.0)
                .unwrap_or_else(|| s.handle_id.0.to_string()),
            kind_id: tagged_id::encode(s.kind_id.0).unwrap_or_else(|| s.kind_id.0.to_string()),
            bytes_len: s.bytes_len,
            pinned: s.pinned,
            refcount: s.refcount,
            created_at_ms: s.created_at_ms,
        };
        let response = DescribeHandlesResponse {
            engine_id: args.engine_id,
            total_entries: result.total_entries,
            in_memory_entries: result.in_memory_entries,
            on_disk_entries: result.on_disk_entries,
            pinned_entries: result.pinned_entries,
            in_memory_bytes: result.in_memory_bytes,
            on_disk_bytes: result.on_disk_bytes,
            on_disk_budget_bytes: result.on_disk_budget_bytes,
            top_by_size: result.top_by_size.iter().map(to_json).collect(),
            top_by_recency: result.top_by_recency.iter().map(to_json).collect(),
        };
        json(&response)
    }

    #[tool(
        description = "Submit a computation DAG to a substrate (ADR-0047). Validation runs SYNCHRONOUSLY on this call: returns {dag_id, output_handles:[{node_id, handle_id}]} once the descriptor passes, or {error: <DagError>} immediately on a bad descriptor (cycle, unknown sink/recipient, kind-not-accepted, etc.) — no dag_id minted, nothing dispatched. Sources execute asynchronously AFTER this ack; the returned output_handles are the per-node handle ids (allocated at submit) you can stamp into downstream Ref<K> slots before their values resolve. Poll dag_status for execution state. The descriptor is JSON encoded against the aether.dag.descriptor kind schema (see describe_kinds): each Source carries a virtual `payload_path` (a filesystem path readable by this process) that submit_dag reads and substitutes into the wire `payload` bytes — so large source payloads stage to a file instead of bloating the tool call. timeout_ms (default 5000) guards a hung validator, not normal latency."
    )]
    pub async fn submit_dag(
        &self,
        Parameters(args): Parameters<SubmitDagArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        // Resolve each Source's `payload_path` virtual field into wire
        // `payload` bytes before encoding. A read failure surfaces as a
        // clean invalid-params error and never touches the wire.
        let descriptor_json = resolve_payload_paths(args.descriptor)
            .await
            .map_err(|e| McpError::invalid_params(format!("submit_dag descriptor: {e}"), None))?;
        // Encode `Submit { descriptor }` against the Submit kind schema —
        // the same encode_schema path send_mail uses for its params.
        let submit_json = serde_json::json!({ "descriptor": descriptor_json });
        let payload = aether_codec::encode_schema(&submit_json, &Submit::SCHEMA)
            .map_err(|e| McpError::invalid_params(format!("submit_dag encode: {e}"), None))?;
        let envelope = MailEnvelope {
            to: MailboxAddress {
                engine: Some(engine),
                mailbox: mailbox_id_from_name(DAG_CAP),
            },
            from: None,
            kind: Submit::ID,
            correlation_id: None,
            payload,
        };
        let timeout_ms = args.timeout_ms.unwrap_or(5000);
        let reply = match time::timeout(
            Duration::from_millis(u64::from(timeout_ms)),
            self.session.call_one(envelope),
        )
        .await
        {
            Ok(Ok(reply)) => reply,
            Ok(Err(e)) => return Err(internal(e)),
            Err(_) => {
                return Err(internal_msg(
                    "submit_dag timed out waiting for the validation verdict",
                ));
            }
        };
        match SubmitResult::decode_from_bytes(&reply.payload) {
            Some(SubmitResult::Ok {
                dag_id,
                output_handles,
            }) => {
                let handles: Vec<serde_json::Value> = output_handles
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "node_id": h.node_id.0,
                            "handle_id": tagged_id::encode(h.handle_id.0)
                                .unwrap_or_else(|| h.handle_id.0.to_string()),
                        })
                    })
                    .collect();
                json(&serde_json::json!({
                    "dag_id": tagged_id::encode(dag_id.0)
                        .unwrap_or_else(|| dag_id.0.to_string()),
                    "output_handles": handles,
                }))
            }
            Some(SubmitResult::Err { error }) => json(&serde_json::json!({ "error": error })),
            None => Err(internal_msg("undecodable SubmitResult")),
        }
    }

    #[tool(
        description = "Poll a submitted DAG's execution status by its dag_id (ADR-0047). Returns the discriminated-union variant directly: \"Pending\" (acked, no source dispatched yet — transient), {\"Running\": {progress:[{node_id, state}]}}, {\"Complete\": {outputs:[{node_id, handle_id}]}}, or {\"Failed\": {node_id, error}}. Validation failures already came back synchronously on submit_dag, so this only ever reports execution-time failures (a source/Call timeout, a malformed reply) — or, for an unknown/reaped dag_id, a Failed with error \"unknown dag …\"."
    )]
    pub async fn dag_status(
        &self,
        Parameters(args): Parameters<DagStatusArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let dag_id = parse_dag_id(&args.dag_id)?;
        let reply = self
            .session
            .call_one(engine_envelope(engine, DAG_CAP, &Status { dag_id }))
            .await
            .map_err(internal)?;
        StatusResult::decode_from_bytes(&reply.payload).map_or_else(
            || Err(internal_msg("undecodable StatusResult")),
            |result| json(&result),
        )
    }

    #[tool(
        description = "Cancel an in-flight DAG by its dag_id (ADR-0047 §5). Returns {\"cancelled\": true} for a still-running DAG (its parked downstream mail is dropped; in-flight cap calls complete server-side but their results discard), {\"cancelled\": false} if it had already completed, or an error for an unknown dag_id."
    )]
    pub async fn dag_cancel(
        &self,
        Parameters(args): Parameters<DagCancelArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let dag_id = parse_dag_id(&args.dag_id)?;
        let reply = self
            .session
            .call_one(engine_envelope(engine, DAG_CAP, &Cancel { dag_id }))
            .await
            .map_err(internal)?;
        match CancelResult::decode_from_bytes(&reply.payload) {
            Some(CancelResult::Ok { cancelled }) => {
                json(&serde_json::json!({ "cancelled": cancelled }))
            }
            Some(CancelResult::Err { error }) => Err(internal_msg(&error)),
            None => Err(internal_msg("undecodable CancelResult")),
        }
    }
}

impl Mcp {
    /// Build one `MailSpec` into an `engine = Some` envelope and route
    /// it through the hub, awaiting the substrate's terminal settle.
    async fn deliver_one(
        &self,
        descriptors: &[KindDescriptor],
        spec: MailSpec,
    ) -> anyhow::Result<()> {
        let engine = EngineId(
            Uuid::parse_str(&spec.engine_id)
                .map_err(|e| anyhow::anyhow!("engine_id is not a valid UUID: {e}"))?,
        );
        // Resolve the kind against the substrate vocabulary baked into
        // `aether-kinds` — the same descriptor set the scenario runner
        // and the embedded hub encode `send_mail` params against.
        let desc = descriptors
            .iter()
            .find(|d| d.name == spec.kind_name)
            .ok_or_else(|| anyhow::anyhow!("unknown kind: {}", spec.kind_name))?;
        let params = spec.params.unwrap_or(serde_json::Value::Null);
        let payload = aether_codec::encode_schema(&params, &desc.schema)
            .map_err(|e| anyhow::anyhow!("param encode failed: {e}"))?;
        let envelope = MailEnvelope {
            to: MailboxAddress {
                engine: Some(engine),
                mailbox: mailbox_id_from_name(&spec.recipient_name),
            },
            from: None,
            kind: KindId(kind_id_from_parts(&desc.name, &desc.schema)),
            correlation_id: None,
            payload,
        };
        self.session.call_settled(envelope).await
    }
}

#[tool_handler]
impl ServerHandler for Mcp {
    fn get_info(&self) -> ServerInfo {
        let mut server_info = Implementation::default();
        server_info.name = "aether-mcp".into();
        server_info.version = env!("CARGO_PKG_VERSION").into();
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = server_info;
        info
    }
}

/// Build a `MailEnvelope` addressed at a hub-local mailbox
/// (`engine = None`) carrying a typed kind.
fn local_envelope<K: Kind + serde::Serialize>(mailbox: &str, kind: &K) -> MailEnvelope {
    MailEnvelope {
        to: MailboxAddress::local(mailbox_id_from_name(mailbox)),
        from: None,
        kind: K::ID,
        correlation_id: None,
        payload: kind.encode_into_bytes(),
    }
}

/// Build a `MailEnvelope` addressed at a mailbox on a specific
/// substrate (`engine = Some`) carrying a typed kind — the hub routes
/// it through to that engine's proxy.
fn engine_envelope<K: Kind + serde::Serialize>(
    engine: EngineId,
    mailbox: &str,
    kind: &K,
) -> MailEnvelope {
    MailEnvelope {
        to: MailboxAddress {
            engine: Some(engine),
            mailbox: mailbox_id_from_name(mailbox),
        },
        from: None,
        kind: K::ID,
        correlation_id: None,
        payload: kind.encode_into_bytes(),
    }
}

/// Parse a UUID-string `engine_id` (from `list_engines` /
/// `spawn_substrate`) into an `EngineId`.
fn parse_engine_id(s: &str) -> Result<EngineId, McpError> {
    Uuid::parse_str(s)
        .map(EngineId)
        .map_err(|e| McpError::invalid_params(format!("engine_id is not a valid UUID: {e}"), None))
}

/// Parse a tagged mailbox-id string (`mbx-…`, ADR-0064) into a
/// `MailboxId`.
fn parse_mailbox_id(s: &str) -> Result<MailboxId, McpError> {
    tagged_id::decode_with_tag(s, Tag::Mailbox)
        .map(MailboxId)
        .map_err(|e| McpError::invalid_params(format!("mailbox_id: {e}"), None))
}

/// Parse a tagged DAG-id string (`dag-…`, ADR-0064/0065) into a
/// `DagId`.
fn parse_dag_id(s: &str) -> Result<DagId, McpError> {
    tagged_id::decode_with_tag(s, Tag::Dag)
        .map(DagId)
        .map_err(|e| McpError::invalid_params(format!("dag_id: {e}"), None))
}

/// Resolve the tool-layer `payload_path` virtual field on every `Source`
/// node of a descriptor JSON into the wire `payload: Vec<u8>` byte array.
///
/// The descriptor JSON is the externally-tagged `DagDescriptor` shape
/// `encode_schema` accepts: `nodes` is an array of `{ "Source": { … } }`
/// / `{ "Observer": { … } }` / `{ "Call": { … } }` objects. For each
/// `Source` object carrying a `payload_path` string, this reads the file
/// at that path, replaces `payload` with the file bytes as a JSON byte
/// array, and removes `payload_path`. A `Source` with an inline
/// `payload` array and no `payload_path` is left untouched. The
/// substrate never sees the path — it gets a normal `Vec<u8>` payload.
async fn resolve_payload_paths(
    mut descriptor: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let Some(nodes) = descriptor
        .get_mut("nodes")
        .and_then(serde_json::Value::as_array_mut)
    else {
        // No nodes array — let encode_schema produce the structured
        // error rather than second-guessing the shape here.
        return Ok(descriptor);
    };
    for node in nodes.iter_mut() {
        // Externally-tagged: { "Source": { … } }.
        let Some(source) = node
            .get_mut("Source")
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        let Some(path) = source
            .get("payload_path")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let path = path.to_owned();
        let bytes = fs::read(&path)
            .await
            .map_err(|e| anyhow::anyhow!("reading payload_path {path:?}: {e}"))?;
        let byte_array: Vec<serde_json::Value> =
            bytes.into_iter().map(serde_json::Value::from).collect();
        source.insert("payload".to_owned(), serde_json::Value::Array(byte_array));
        source.remove("payload_path");
    }
    Ok(descriptor)
}

/// Encode a `send_mail_traced` batch into the same `MailEnvelope`
/// shape `CaptureFrame` carries: name-level addressing + schema-encoded
/// payload. The substrate's `TraceObserver` resolves the names through
/// its registry at dispatch time. Same `resolve_payload` path
/// `encode_capture_bundle` uses, just over `TracedMailSpec` instead of
/// `CaptureMailSpec`.
fn encode_traced_bundle(
    descriptors: &[KindDescriptor],
    specs: &[TracedMailSpec],
) -> anyhow::Result<Vec<KindMailEnvelope>> {
    specs
        .iter()
        .map(|spec| {
            let desc = descriptors
                .iter()
                .find(|d| d.name == spec.kind_name)
                .ok_or_else(|| anyhow::anyhow!("unknown kind: {}", spec.kind_name))?;
            let params = spec.params.clone().unwrap_or(serde_json::Value::Null);
            let payload = aether_codec::encode_schema(&params, &desc.schema)
                .map_err(|e| anyhow::anyhow!("param encode failed for {}: {e}", spec.kind_name))?;
            Ok(KindMailEnvelope {
                recipient_name: spec.recipient_name.clone(),
                kind_name: spec.kind_name.clone(),
                payload,
                count: 1,
            })
        })
        .collect()
}

/// Encode an unwrapped raw `u64` mailbox id as the tagged-id string the
/// MCP wire surfaces (`mbx-…`, ADR-0064). Panics only if the id has
/// no tag bits set — chassis-minted ids always do.
fn mailbox_id_to_tagged(id: MailboxId) -> String {
    tagged_id::encode(id.0).expect("mailbox id is taggable")
}

fn kind_id_to_tagged(id: KindId) -> String {
    tagged_id::encode(id.0).expect("kind id is taggable")
}

fn mail_id_to_json(id: MailId) -> MailIdJson {
    MailIdJson {
        sender: mailbox_id_to_tagged(id.sender),
        correlation_id: id.correlation_id,
    }
}

fn mail_node_to_json(node: MailNodeWire) -> MailNodeJson {
    MailNodeJson {
        mail_id: mail_id_to_json(node.mail_id),
        parent: node.parent.map(mail_id_to_json),
        sender: mailbox_id_to_tagged(node.sender),
        recipient: mailbox_id_to_tagged(node.recipient),
        kind: kind_id_to_tagged(node.kind),
        t_sent: node.t_sent.0,
        t_received: node.t_received.map(|n| n.0),
        t_finished: node.t_finished.map(|n| n.0),
        thread_name: node.thread_name,
    }
}

/// Encode a `capture_frame` mail bundle: resolve each spec's kind
/// against the substrate descriptor inventory, schema-encode its
/// params, and wrap into the substrate-side `aether_kinds::MailEnvelope`
/// shape (name-level addressing + pre-encoded payload).
fn encode_capture_bundle(
    descriptors: &[KindDescriptor],
    specs: &[CaptureMailSpec],
) -> anyhow::Result<Vec<aether_kinds::MailEnvelope>> {
    specs
        .iter()
        .map(|spec| {
            let desc = descriptors
                .iter()
                .find(|d| d.name == spec.kind_name)
                .ok_or_else(|| anyhow::anyhow!("unknown kind: {}", spec.kind_name))?;
            let params = spec.params.clone().unwrap_or(serde_json::Value::Null);
            let payload = aether_codec::encode_schema(&params, &desc.schema)
                .map_err(|e| anyhow::anyhow!("param encode failed for {}: {e}", spec.kind_name))?;
            Ok(aether_kinds::MailEnvelope {
                recipient_name: spec.recipient_name.clone(),
                kind_name: spec.kind_name.clone(),
                payload,
                count: 1,
            })
        })
        .collect()
}

/// Serialize a tool result to the JSON string `rmcp` wraps as text
/// content.
fn json<T: serde::Serialize>(value: &T) -> Result<String, McpError> {
    serde_json::to_string(value).map_err(|e| McpError::internal_error(e.to_string(), None))
}

// `e` is owned because callers do `.map_err(internal)` — the closure-
// converted form needs an `FnOnce(anyhow::Error) -> McpError`.
#[allow(clippy::needless_pass_by_value)]
fn internal(e: anyhow::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

fn internal_msg(msg: &str) -> McpError {
    McpError::internal_error(msg.to_owned(), None)
}

/// Issue 963: render an `actor_logs` `LogTailResult::Err` into a
/// tool-error message that names the agent-supplied mailbox, so an
/// unregistered-mailbox query reads as "that mailbox doesn't exist"
/// rather than a bare relayed substrate string. Factored out so the
/// formatting is unit-testable without standing up a live engine.
fn actor_logs_err_message(mailbox_name: &str, error: &str) -> String {
    format!("actor_logs: mailbox \"{mailbox_name}\" — {error}")
}

/// Map ADR-0023 §4's level string to the `0..=4` byte the
/// `aether.log.*` kinds carry. Case-insensitive. Returns an
/// `invalid_params` error on unknown strings so a typoed `"Warn "`
/// surfaces at the tool boundary rather than reaching the substrate.
fn parse_level(s: &str) -> Result<u8, McpError> {
    match s.to_ascii_lowercase().as_str() {
        "trace" => Ok(0),
        "debug" => Ok(1),
        "info" => Ok(2),
        "warn" => Ok(3),
        "error" => Ok(4),
        other => Err(McpError::invalid_params(
            format!("unknown level {other:?}; expected trace|debug|info|warn|error"),
            None,
        )),
    }
}

/// Inverse of [`parse_level`]: render the `0..=4` byte back to the
/// canonical lowercase level string. Out-of-band bytes render as
/// `"info"` (matches the existing fallback in
/// `aether-capabilities::log`'s pre-issue-776 conversion).
fn level_to_str(level: u8) -> &'static str {
    match level {
        0 => "trace",
        1 => "debug",
        3 => "warn",
        4 => "error",
        // 2 is "info"; out-of-band bytes also render as "info".
        _ => "info",
    }
}

#[cfg(test)]
// Test-setup unwraps (tagged-id encode of literal ids, JSON build) panic
// on failure, which is the assertion; the DAG-tool fixtures lean on them.
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::args::{
        CaptureFrameArgs, CaptureMailSpec, DescribeComponentArgs, LoadComponentArgs, MailSpec,
        ReplaceComponentArgs, SendMailArgs, SendMailTracedArgs, SpawnSubstrateArgs,
        TerminateSubstrateArgs, TracedMailSpec,
    };
    use aether_capabilities::EngineServer;
    use aether_capabilities::rpc::{
        PeerKind, RpcServerCapability, RpcServerConfig, RpcServerHandle,
    };
    use aether_capabilities::trace::TraceObserverCapability;
    use aether_substrate::chassis::builder::{Builder, PassiveChassis};
    use aether_substrate::handle_store::HandleStore;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::Registry;

    use crate::args::ActorLogsArgs;
    use crate::args::DescribeHandlesArgs;
    use crate::test_chassis::TestChassis;
    use aether_kinds::descriptors;

    /// Boot a hub-shaped passive chassis: a forwarding
    /// `RpcServerCapability` + the engines cap + `TraceObserver` (so
    /// the `RpcServer`'s local Calls settle and close). Returns the
    /// chassis (kept alive for its dispatcher threads) and the RPC
    /// port an `RpcSession` dials.
    fn boot_hub() -> (PassiveChassis<TestChassis>, u16) {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let (outbound, _rx) = HubOutbound::attached_loopback();
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store).with_outbound(outbound));
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<TraceObserverCapability>(())
            .with_actor::<EngineServer>(())
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: PeerKind::Substrate {
                    engine_name: "test-hub".into(),
                    engine_version: "0.1.0".into(),
                    kinds: vec![],
                },
            })
            .build_passive()
            .expect("hub caps boot");
        let port = chassis
            .handle::<RpcServerHandle>()
            .expect("RpcServerHandle published")
            .local_port;
        (chassis, port)
    }

    /// Connect an `RpcSession` + wrap it in an `Mcp` against a booted
    /// hub chassis, with a fresh component cache.
    fn connect_mcp(port: u16) -> Mcp {
        let session = RpcSession::connect(&format!("127.0.0.1:{port}")).expect("session connects");
        Mcp::new(Arc::new(session), Arc::new(ComponentCache::default()))
    }

    /// `list_engines` over the RPC round-trip yields an empty array on
    /// a fresh hub — proves the whole `RpcSession` demux + the
    /// `engine = None` Call path against the real `aether.engine` cap.
    #[tokio::test]
    async fn list_engines_on_empty_hub_is_empty() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let out = mcp.list_engines().await.expect("list_engines ok");
        assert_eq!(out, "[]", "fresh hub supervises no engines");
    }

    /// `spawn_substrate` with a binary path that doesn't exist surfaces
    /// the hub's `SpawnEngineResult::Err` as a tool error.
    #[tokio::test]
    async fn spawn_substrate_missing_binary_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .spawn_substrate(Parameters(SpawnSubstrateArgs {
                binary_path: "/nonexistent/aether-substrate-does-not-exist".to_owned(),
                args: vec![],
            }))
            .await;
        assert!(result.is_err(), "a missing binary should be a tool error");
    }

    /// `terminate_substrate` with a malformed `engine_id` surfaces the
    /// hub's `TerminateEngineResult::Err` as a tool error.
    #[tokio::test]
    async fn terminate_substrate_bad_engine_id_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .terminate_substrate(Parameters(TerminateSubstrateArgs {
                engine_id: "not-a-uuid".to_owned(),
            }))
            .await;
        assert!(
            result.is_err(),
            "a malformed engine_id should be a tool error"
        );
    }

    /// `send_mail` is a best-effort batch: a bad `kind_name` and a bad
    /// `engine_id` fail locally in `deliver_one`, while a well-formed
    /// item addressed at an unknown engine round-trips to the hub and
    /// comes back a `CallSettled::Err`. Every item reports `error: ...`
    /// and none aborts its siblings.
    #[tokio::test]
    async fn send_mail_reports_per_item_errors() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let out = mcp
            .send_mail(Parameters(SendMailArgs {
                mails: vec![
                    MailSpec {
                        engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                        recipient_name: "aether.fs".to_owned(),
                        kind_name: "not.a.real.kind".to_owned(),
                        params: None,
                    },
                    MailSpec {
                        engine_id: "not-a-uuid".to_owned(),
                        recipient_name: "aether.fs".to_owned(),
                        kind_name: "aether.fs.list".to_owned(),
                        params: None,
                    },
                    MailSpec {
                        engine_id: "00000000-0000-0000-0000-000000000002".to_owned(),
                        recipient_name: "aether.fs".to_owned(),
                        kind_name: "aether.fs.list".to_owned(),
                        params: Some(serde_json::json!({ "namespace": "save", "prefix": "" })),
                    },
                ],
            }))
            .await
            .expect("send_mail returns a status array, not a tool error");
        let statuses: Vec<MailStatus> = serde_json::from_str(&out).expect("status array");
        assert_eq!(statuses.len(), 3);
        for status in &statuses {
            assert!(
                status.status.starts_with("error: "),
                "item {} should be an error: {}",
                status.index,
                status.status,
            );
        }
    }

    /// `describe_kinds` is fully local — it renders the substrate kind
    /// inventory baked into `aether-kinds`, no hub round-trip. The
    /// result is a non-empty JSON array.
    #[tokio::test]
    async fn describe_kinds_returns_the_substrate_inventory() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let out = mcp.describe_kinds().await.expect("describe_kinds ok");
        let kinds: serde_json::Value = serde_json::from_str(&out).expect("json array");
        assert!(
            kinds.as_array().is_some_and(|a| !a.is_empty()),
            "describe_kinds should list the substrate vocabulary",
        );
    }

    /// `load_component` with a binary path that doesn't exist fails at
    /// the file read, before any RPC.
    #[tokio::test]
    async fn load_component_missing_binary_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .load_component(Parameters(LoadComponentArgs {
                engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                binary_path: "/nonexistent/does-not-exist.wasm".to_owned(),
                name: None,
            }))
            .await;
        assert!(result.is_err(), "a missing binary should be a tool error");
    }

    /// `replace_component` with a malformed tagged mailbox id is
    /// rejected before any RPC.
    #[tokio::test]
    async fn replace_component_bad_mailbox_id_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .replace_component(Parameters(ReplaceComponentArgs {
                engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                mailbox_id: "not-a-tagged-id".to_owned(),
                binary_path: "/tmp/whatever.wasm".to_owned(),
                drain_timeout_ms: None,
            }))
            .await;
        assert!(
            result.is_err(),
            "a malformed mailbox_id should be a tool error"
        );
    }

    /// `send_mail_traced` with an unknown kind in the batch is
    /// rejected up front — the batch is encoded before any RPC,
    /// mirroring `capture_frame`'s all-or-fail bundle semantics.
    #[tokio::test]
    async fn send_mail_traced_bad_spec_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .send_mail_traced(Parameters(SendMailTracedArgs {
                engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                mails: vec![TracedMailSpec {
                    recipient_name: "aether.render".to_owned(),
                    kind_name: "not.a.real.kind".to_owned(),
                    params: None,
                }],
                settlement_timeout_ms: None,
            }))
            .await;
        assert!(
            result.is_err(),
            "an unknown kind in the batch should be a tool error",
        );
    }

    /// `capture_frame` with an unknown kind in the mails bundle is
    /// rejected up front — the bundle is encoded before any RPC.
    #[tokio::test]
    async fn capture_frame_bad_bundle_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .capture_frame(Parameters(CaptureFrameArgs {
                engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                mails: vec![CaptureMailSpec {
                    recipient_name: "aether.render".to_owned(),
                    kind_name: "not.a.real.kind".to_owned(),
                    params: None,
                }],
                after_mails: vec![],
            }))
            .await;
        assert!(
            result.is_err(),
            "an unknown kind in the bundle should be a tool error",
        );
    }

    /// `describe_component` reads the component cache: an empty cache
    /// errors, a seeded entry round-trips.
    #[tokio::test]
    async fn describe_component_reads_the_cache() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let engine_id = "00000000-0000-0000-0000-000000000001";
        // A real, taggable mailbox id (arbitrary u64s don't carry the
        // mailbox-domain bits `tagged_id::encode` needs).
        let mailbox = mailbox_id_from_name("aether.test.fake_component");
        let tagged = tagged_id::encode(mailbox.0).expect("mailbox id is taggable");

        // Empty cache → error.
        let miss = mcp
            .describe_component(Parameters(DescribeComponentArgs {
                engine_id: engine_id.to_owned(),
                mailbox_id: tagged.clone(),
            }))
            .await;
        assert!(
            miss.is_err(),
            "an uncached component should be a tool error"
        );

        // Seed the cache, then it round-trips.
        let engine =
            EngineId(Uuid::parse_str(engine_id).expect("test setup: engine_id is a valid uuid"));
        mcp.components
            .lock()
            .expect("test setup: component cache mutex is never poisoned")
            .insert((engine, mailbox), ComponentCapabilities::default());
        let hit = mcp
            .describe_component(Parameters(DescribeComponentArgs {
                engine_id: engine_id.to_owned(),
                mailbox_id: tagged,
            }))
            .await
            .expect("cached component describes");
        let caps: serde_json::Value = serde_json::from_str(&hit).expect("json");
        assert!(caps.get("handlers").is_some(), "capabilities shape: {hit}");
    }

    /// `parse_level` round-trips every documented spelling and rejects
    /// unknown strings — case-insensitive (`"INFO"` and `"info"` both
    /// land on `2`).
    #[test]
    fn parse_level_round_trips_documented_strings() {
        assert_eq!(
            parse_level("trace").expect("test setup: \"trace\" parses"),
            0
        );
        assert_eq!(
            parse_level("debug").expect("test setup: \"debug\" parses"),
            1
        );
        assert_eq!(parse_level("info").expect("test setup: \"info\" parses"), 2);
        assert_eq!(parse_level("warn").expect("test setup: \"warn\" parses"), 3);
        assert_eq!(
            parse_level("error").expect("test setup: \"error\" parses"),
            4
        );
        assert_eq!(
            parse_level("INFO").expect("test setup: case-insensitive \"INFO\" parses"),
            2
        );
        assert!(parse_level("verbose").is_err());
    }

    /// `level_to_str` inverts `parse_level` for in-band bytes and
    /// falls back to `"info"` for out-of-band ones (matches the
    /// pre-issue-776 conversion behaviour in `aether-capabilities`).
    #[test]
    fn level_to_str_matches_parse_level_and_falls_back_to_info() {
        for level in 0..=4u8 {
            let parsed = parse_level(level_to_str(level))
                .expect("test setup: level_to_str output round-trips through parse_level");
            assert_eq!(parsed, level);
        }
        assert_eq!(level_to_str(99), "info");
    }

    /// `actor_logs` with a malformed `engine_id` rejects up front
    /// without touching the wire.
    #[tokio::test]
    async fn actor_logs_bad_engine_id_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .actor_logs(Parameters(ActorLogsArgs {
                engine_id: "not-a-uuid".to_owned(),
                mailbox_name: "aether.audio".to_owned(),
                max: None,
                level: None,
                since: None,
            }))
            .await;
        assert!(
            result.is_err(),
            "a malformed engine_id should be a tool error"
        );
    }

    /// Issue 963: the `LogTailResult::Err` arm names the agent-
    /// supplied mailbox in the tool error. A live engine isn't needed
    /// to inject a decoded `Err` — pin the formatting at the call
    /// site's helper instead (the substrate-side synthesized-Err
    /// routing is covered in `aether-substrate`'s mailer tests).
    #[test]
    fn actor_logs_err_message_names_mailbox() {
        let msg =
            actor_logs_err_message("aether.nope", "mailbox mbx-0000-0000-0000 not registered");
        assert!(msg.contains("aether.nope"), "names the mailbox: {msg}");
        assert!(msg.contains("not registered"), "carries the cause: {msg}");
    }

    /// `actor_logs` with an unknown `level` string is rejected at
    /// the tool boundary before any RPC.
    #[tokio::test]
    async fn actor_logs_bad_level_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .actor_logs(Parameters(ActorLogsArgs {
                engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                mailbox_name: "aether.audio".to_owned(),
                max: None,
                level: Some("verbose".to_owned()),
                since: None,
            }))
            .await;
        assert!(result.is_err(), "an unknown level should be a tool error");
    }

    /// `describe_handles` with a malformed `engine_id` rejects up front
    /// without touching the wire.
    #[tokio::test]
    async fn describe_handles_bad_engine_id_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .describe_handles(Parameters(DescribeHandlesArgs {
                engine_id: "not-a-uuid".to_owned(),
                max: None,
            }))
            .await;
        assert!(
            result.is_err(),
            "a malformed engine_id should be a tool error"
        );
    }

    // DAG tools (issue 977).

    use crate::args::{DagCancelArgs, DagStatusArgs, SubmitDagArgs};
    use aether_data::with_tag;
    use aether_kinds::{DagDescriptor, Edge, Node, NodeId, Submit};
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env as std_env, fs as std_fs};

    /// Write `bytes` to a unique temp file and return its path. nextest's
    /// process-per-test isolation keeps the filename collision-free
    /// across the suite; the `pid + nanos` suffix guards within a process.
    fn stage_temp_file(tag: &str, bytes: &[u8]) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std_env::temp_dir().join(format!(
            "aether-mcp-dag-{tag}-{}-{nanos}.bin",
            process::id()
        ));
        std_fs::write(&path, bytes).expect("stage temp file");
        path
    }

    /// The tool's `payload_path` → wire-`payload` substitution + JSON
    /// encode path produces the exact same canonical bytes as a direct
    /// `#[derive(Kind)]` encode of the same descriptor with the bytes
    /// inlined. Locks against encoding skew between the two paths.
    #[tokio::test]
    async fn submit_dag_encodes_descriptor() {
        let source_mbx = mailbox_id_from_name("aether.fs");
        let observer_mbx = mailbox_id_from_name("aether.render");
        // Use real registered kind ids so the tagged-string round-trip is
        // exercised against the actual TypeId encode arm.
        let source_kind = aether_kinds::Read::ID;
        let observer_kind = aether_kinds::DrawTriangle::ID;
        let payload_bytes = vec![0x01u8, 0x02, 0x03, 0xFF, 0x00, 0x42];

        // Expected: a typed DagDescriptor with the payload inlined,
        // wrapped in Submit and encoded via the Kind derive.
        let expected_descriptor = DagDescriptor {
            version: 1,
            nodes: vec![
                Node::Source {
                    id: NodeId(0),
                    mailbox: source_mbx,
                    kind_id: source_kind,
                    payload: payload_bytes.clone(),
                },
                Node::Observer {
                    id: NodeId(1),
                    recipient: observer_mbx,
                    kind_id: observer_kind,
                },
            ],
            edges: vec![Edge {
                from: NodeId(0),
                to: NodeId(1),
                slot: 0,
            }],
        };
        let expected = Submit {
            descriptor: expected_descriptor,
        }
        .encode_into_bytes();

        // Tool path: descriptor JSON with a `payload_path` virtual field
        // on the source, externally-tagged Node variants, tagged-string
        // ids.
        let path = stage_temp_file("encode", &payload_bytes);
        // NodeId is a `#[derive(Schema)]` newtype, so encode_schema reads
        // its single tuple field as `{ "0": <u32> }` (the schema-correct
        // JSON the agent authors against `describe_kinds`).
        let descriptor_json = serde_json::json!({
            "version": 1,
            "nodes": [
                { "Source": {
                    "id": { "0": 0 },
                    "mailbox": tagged_id::encode(source_mbx.0).unwrap(),
                    "kind_id": tagged_id::encode(source_kind.0).unwrap(),
                    "payload_path": path.to_str().unwrap(),
                }},
                { "Observer": {
                    "id": { "0": 1 },
                    "recipient": tagged_id::encode(observer_mbx.0).unwrap(),
                    "kind_id": tagged_id::encode(observer_kind.0).unwrap(),
                }},
            ],
            "edges": [ { "from": { "0": 0 }, "to": { "0": 1 }, "slot": 0 } ],
        });
        let resolved = resolve_payload_paths(descriptor_json)
            .await
            .expect("payload_path resolves");
        let submit_json = serde_json::json!({ "descriptor": resolved });
        let actual =
            aether_codec::encode_schema(&submit_json, &Submit::SCHEMA).expect("encode_schema ok");

        std_fs::remove_file(&path).ok();
        assert_eq!(
            actual, expected,
            "tool path-loading + JSON encode must match the binary inline-bytes encode",
        );
    }

    /// A `Source` carrying an inline `payload` byte array (no
    /// `payload_path`) is left untouched by `resolve_payload_paths` and
    /// encodes identically to the typed form.
    #[tokio::test]
    async fn submit_dag_inline_payload_encodes() {
        let source_mbx = mailbox_id_from_name("aether.fs");
        let source_kind = aether_kinds::Read::ID;
        let payload_bytes = vec![9u8, 8, 7];
        let expected = Submit {
            descriptor: DagDescriptor {
                version: 1,
                nodes: vec![Node::Source {
                    id: NodeId(0),
                    mailbox: source_mbx,
                    kind_id: source_kind,
                    payload: payload_bytes.clone(),
                }],
                edges: vec![],
            },
        }
        .encode_into_bytes();

        let descriptor_json = serde_json::json!({
            "version": 1,
            "nodes": [
                { "Source": {
                    "id": { "0": 0 },
                    "mailbox": tagged_id::encode(source_mbx.0).unwrap(),
                    "kind_id": tagged_id::encode(source_kind.0).unwrap(),
                    "payload": payload_bytes,
                }},
            ],
            "edges": [],
        });
        let resolved = resolve_payload_paths(descriptor_json)
            .await
            .expect("inline payload untouched");
        let submit_json = serde_json::json!({ "descriptor": resolved });
        let actual =
            aether_codec::encode_schema(&submit_json, &Submit::SCHEMA).expect("encode_schema ok");
        assert_eq!(actual, expected);
    }

    /// `submit_dag` with a `payload_path` that doesn't exist returns a
    /// structured tool error before any call hits the engine.
    #[tokio::test]
    async fn submit_dag_rejects_missing_payload_path() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let source_mbx = mailbox_id_from_name("aether.fs");
        let descriptor = serde_json::json!({
            "version": 1,
            "nodes": [
                { "Source": {
                    "id": 0,
                    "mailbox": tagged_id::encode(source_mbx.0).unwrap(),
                    "kind_id": tagged_id::encode(aether_kinds::Read::ID.0).unwrap(),
                    "payload_path": "/nonexistent/aether-dag-source.bin",
                }},
            ],
            "edges": [],
        });
        let result = mcp
            .submit_dag(Parameters(SubmitDagArgs {
                engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                descriptor,
                timeout_ms: None,
            }))
            .await;
        assert!(
            result.is_err(),
            "a missing payload_path should be a tool error before any RPC",
        );
    }

    /// `dag_status` / `dag_cancel` reject a malformed (non-`dag-…`)
    /// `dag_id` at the tool boundary, before any RPC.
    #[tokio::test]
    async fn dag_status_and_cancel_reject_bad_dag_id() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let status = mcp
            .dag_status(Parameters(DagStatusArgs {
                engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                dag_id: "not-a-dag-id".to_owned(),
            }))
            .await;
        assert!(status.is_err(), "malformed dag_id is a tool error");
        let cancel = mcp
            .dag_cancel(Parameters(DagCancelArgs {
                engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                dag_id: "mbx-aaaa-aaaa-aaaa".to_owned(),
            }))
            .await;
        assert!(
            cancel.is_err(),
            "a mailbox-tagged id is not a dag id — tool error",
        );
    }

    /// `dag_status` / `dag_cancel` reject a malformed `engine_id` at the
    /// tool boundary.
    #[tokio::test]
    async fn dag_tools_reject_bad_engine_id() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let status = mcp
            .dag_status(Parameters(DagStatusArgs {
                engine_id: "not-a-uuid".to_owned(),
                dag_id: tagged_id::encode(with_tag(Tag::Dag, 1)).unwrap(),
            }))
            .await;
        assert!(status.is_err(), "malformed engine_id is a tool error");
    }
}
