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

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::mem;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

// `tokio::sync::Mutex` (the async one used by the per-engine refresh-
// collapse guard) imported under an alias so the struct-field path
// stays short — `std::sync::Mutex` is the bare `Mutex`.
use tokio::sync::Mutex as AsyncMutex;

use aether_codec::frame::max_frame_size;
use aether_data::MailId;
use aether_data::canonical::kind_id_from_parts;
use aether_data::wire;
use aether_data::{
    DagId, EngineId, HandleId, Kind, KindDescriptor, KindId, MailboxId, ScopePathError, Tag, Uuid,
    mailbox_id_from_name, mailbox_id_from_path, tagged_id, validate_scope_path,
};
use aether_data::{EnumVariant, Primitive, SchemaType};
use aether_kinds::dag::{DagDescriptor, Edge, Node, NodeId};
use aether_kinds::{
    BinarySelector, Cancel, CancelResult, CaptureFrame, CaptureFrameResult, ComponentCapabilities,
    ComponentSelector, CostTail, CostTailResult, DeathReason, FrameCheck, FrameReduction,
    ListBinaries, ListBinariesResult, ListComponents, ListComponentsResult, ListEngines,
    ListEnginesResult, ListKinds, ListKindsResult, LoadComponent, LoadResult,
    MailEnvelope as KindMailEnvelope, ReplaceComponent, ReplaceResult, ResolveComponent,
    ResolveComponentResult, SimilarityCheck, SpawnEngine, SpawnEngineResult, Status, StatusResult,
    Submit, SubmitResult, TerminateEngine, TerminateEngineResult, UploadBinary, UploadBinaryResult,
    UploadComponent, UploadComponentResult,
    trace::{
        DescribeTreeResult, DispatchTraced, DispatchTracedAck, MailNodeWire, TRACE_MAILBOX_NAME,
        TraceTail, TraceTailResult,
    },
};
use aether_rpc::rpc::{MailEnvelope, MailboxAddress};
use aether_rpc::trace_walk::TreeWalk;
use base64::Engine as _;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::args::ActorLogEntry;
use crate::args::ActorLogsArgs;
use crate::args::ActorLogsResponse;
use crate::args::{ActorCostArgs, ActorCostResponse, ActorCostRow};
use crate::args::{
    CaptureCheckSpec, CaptureFrameArgs, CaptureMailSpec, ComponentSpec, DagCancelArgs,
    DagDescriptorArg, DagStatusArgs, DeadEngineInfo, DescribeComponentArgs, DescribeHandlersArgs,
    DescribeHandlersResponse, DescribeHandlesArgs, DescribeHandlesResponse, DescribeKindsArgs,
    EngineInfo, HandleSummaryJson, KindSummary, ListBinariesArgs, ListComponentsArgs,
    ListEnginesResponse, LoadComponentArgs, MailIdJson, MailNodeJson, MailSpec, MailStatus,
    NativeCapHandlers, NativeHandlerJson, NodeArg, ReplaceComponentArgs, ReplyEventJson,
    SendMailArgs, SendMailTracedArgs, SendMailTracedResponse, SpawnSubstrateArgs, SubmitDagArgs,
    TerminateSubstrateArgs, TracedMailSpec, TransformListing, UploadBinaryArgs,
    UploadComponentArgs,
};
use crate::reverse::EngineNames;
use crate::rpc::RpcSession;
use aether_kinds::descriptors;
use aether_kinds::{
    HandlersResult, ListHandlers, Manifest, ManifestResult, Resolve, ResolveResult,
};
use base64::engine::general_purpose::STANDARD;
use std::time::Duration;
use tokio::fs;
use tokio::time;

/// Default wall-clock cap on `send_mail` / `send_mail_traced` awaiting a
/// chain to settle (issue 1242). 300s — clears a provider cap's API
/// timeout (the gemini cap's 180s, anthropic's 120s) with margin for
/// queue / dispatch / staging overhead.
const AWAIT_TIMEOUT_DEFAULT_MS: u32 = 300_000;
/// Hard ceiling on the caller-supplied await timeout (issue 1242). A
/// `settlement_timeout_ms` above this is clamped down. 600s — twice the
/// default, the locked upper bound for a legitimately-long provider call.
const AWAIT_TIMEOUT_CAP_MS: u32 = 600_000;

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
/// Mailbox name of a substrate's reverse-lookup inventory cap
/// (ADR-0088 §6) — the `aether.inventory.manifest` / `resolve` target.
const INVENTORY_CAP: &str = "aether.inventory";

/// Component receive-side capabilities, keyed by `(engine, mailbox)`.
/// Populated from `load_component` / `replace_component` replies and
/// read by `describe_component` — the forward-model stand-in for the
/// embedded hub's component registry.
pub type ComponentCache = Mutex<HashMap<(EngineId, MailboxId), ComponentCapabilities>>;

/// Per-engine reverse-lookup state, keyed by [`EngineId`] (ADR-0088 §8).
/// Each [`EngineNames`] folds that engine's served `aether.inventory`
/// manifest into a `hash → name` map plus a dynamic-resolve cache. Built
/// lazily on first need (the first id render for an engine), cached for
/// the engine's lifetime, and shared across cloned [`Mcp`] sessions —
/// statics are build-identical but dynamic instances are per-engine, so
/// the map can't be process-global. An engine that doesn't answer the
/// manifest gets an empty map (every lookup falls back to hex) rather
/// than erroring the tool.
pub type ReverseNameCache = Mutex<HashMap<EngineId, EngineNames>>;

/// Per-engine kind-encode cache (ADR-0091): a `kind_name → KindDescriptor`
/// map per engine, plus the per-engine async mutex that collapses
/// concurrent refreshes. Built lazily on first send for an engine
/// (prefilled from the substrate's static vocabulary via
/// `descriptors::all`); refreshed on encode miss via
/// `aether.inventory.kinds`. Component-defined kinds enter on the
/// first miss after `load_component`.
///
/// Two halves so the cache can be read under the synchronous `Mutex`
/// without holding the lock across the async refresh RPC: the outer
/// `descriptors` map is the read path, and `refresh_guards` holds the
/// per-engine `AsyncMutex<()>` two concurrent misses on
/// different unknown names collapse on (the second waiter awaits the
/// first's result, then retries the lookup against the freshly-
/// populated map without re-fetching).
#[derive(Default)]
pub struct KindsCache {
    /// `engine → kind_name → descriptor`. Read with the std `Mutex`
    /// uncontended on cache hits (no await inside the critical
    /// section).
    descriptors: Mutex<HashMap<EngineId, HashMap<String, KindDescriptor>>>,
    /// `engine → refresh-collapse mutex`. Looked up under
    /// `descriptors`'s lock to fetch-or-insert, then acquired
    /// out-of-band via `tokio::sync::Mutex::lock().await` so the
    /// refresh RPC doesn't pin the cache lock.
    refresh_guards: Mutex<HashMap<EngineId, Arc<AsyncMutex<()>>>>,
}

/// Per-session MCP service. `rmcp` calls the factory once per session
/// and may clone the result for concurrent tool dispatch — `session`
/// and `components` are `Arc`s, so clones share the one hub connection
/// and one component cache.
#[derive(Clone)]
pub struct Mcp {
    session: Arc<RpcSession>,
    components: Arc<ComponentCache>,
    /// Per-engine reverse-lookup maps (ADR-0088 §8), shared across cloned
    /// sessions so a manifest fetched for one tool call serves the next.
    names: Arc<ReverseNameCache>,
    /// Per-engine kind-encode cache (ADR-0091), shared across cloned
    /// sessions so a `ListKinds` refresh fetched for one tool call
    /// serves the next.
    kinds: Arc<KindsCache>,
    // The `#[tool_router]` macro stores the router instance here; it's
    // consumed by `#[tool_handler]` codegen rather than read by name, so
    // the dead-code lint fires under `-D warnings` despite the field
    // being load-bearing. (rmcp 1.7 stopped tagging the field as used.)
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl Mcp {
    /// Construct a per-session service over an established hub
    /// connection + the process-wide component, reverse-name, and
    /// kind-encode caches.
    pub fn new(
        session: Arc<RpcSession>,
        components: Arc<ComponentCache>,
        names: Arc<ReverseNameCache>,
        kinds: Arc<KindsCache>,
    ) -> Self {
        Self {
            session,
            components,
            names,
            kinds,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl Mcp {
    #[tool(
        description = "List every engine the hub currently supervises, plus a recently-died sidecar. Returns an object {engines, recently_died}: each `engines` item reports the engine_id (pass it to send_mail / terminate_substrate) and the localhost RPC port the hub assigned its substrate; each `recently_died` item reports a departed engine with why it left — reason \"terminated\" (a deliberate terminate_substrate), \"crashed\" (the substrate closed its connection), or \"evicted\" (a missed-heartbeat eviction) — plus a detail string and how long ago it left, so a clean shutdown is distinguishable from a failure."
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
                last_heartbeat_age_millis: e.last_heartbeat_age_millis,
            })
            .collect();
        let recently_died: Vec<DeadEngineInfo> = result
            .recently_died
            .into_iter()
            .map(|d| {
                let (reason, detail) = death_reason_parts(d.reason);
                DeadEngineInfo {
                    engine_id: d.engine_id,
                    rpc_port: d.rpc_port,
                    reason,
                    detail,
                    died_age_millis: d.died_age_millis,
                }
            })
            .collect();
        json(&ListEnginesResponse {
            engines,
            recently_died,
        })
    }

    #[tool(
        description = "Fork+exec a substrate binary as a child of the hub, resolved from the hub's content-addressed binary store (ADR-0115) — not a host path. Pass `selector` to pick the binary: a content `hash`, a `name@version`, or a `name` (upload_binary first if it isn't stored). Omit `selector` for `default` — the headless chassis — so a bare spawn_substrate with no arguments returns a working engine. When `selector` is omitted you may instead attribute-query with `chassis` (\"headless\"/\"desktop\"/\"hub\"), `caps` (linked-cap superset), and `target` (build triple). The hub resolves the selector to the stored bytes, materializes them to an executable temp file, assigns a free localhost RPC port (injected as AETHER_RPC_PORT), forks it, and connects a proxy. Returns the engine_id and rpc_port on success; errors if the selector resolves to no stored binary. Pass `components` (each {selector, name?, config_path?, export?}) to bring the engine up with those components already loaded in one call — each selector is a content hash, name, or module@actor resolved against the hub's component registry (ADR-0116; upload_component first). aether-mcp pre-resolves each selector to its wasm bytes, stages a temp boot-manifest the hub injects as AETHER_BOOT_MANIFEST, and the spawned substrate reads the staged wasm itself (single-host), so no follow-up load_component is needed."
    )]
    pub async fn spawn_substrate(
        &self,
        Parameters(args): Parameters<SpawnSubstrateArgs>,
    ) -> Result<String, McpError> {
        // A boot list rides in as a temp boot-manifest JSON of file paths;
        // the hub injects its path as AETHER_BOOT_MANIFEST and the
        // single-host substrate reads the staged wasm itself (issue 1776).
        // ADR-0116: each component is a registry selector, so aether-mcp
        // pre-resolves it to bytes and stages those bytes to a temp wasm
        // file the manifest points at — the substrate boot path stays
        // path-based, now fed by the registry. Hold the temp files across
        // the spawn call — the substrate reads them at boot, before the
        // spawn reply returns — then clean them up.
        let staged = if args.components.is_empty() {
            None
        } else {
            Some(self.stage_boot_manifest(&args.components).await?)
        };
        let reply = self
            .session
            .call_one(local_envelope(
                ENGINE_CAP,
                &SpawnEngine {
                    selector: BinarySelector {
                        query: args.selector,
                        chassis: args.chassis,
                        caps: args.caps,
                        target: args.target,
                    },
                    args: args.args,
                    boot_manifest: staged
                        .as_ref()
                        .map(|s| s.manifest_path.to_string_lossy().into_owned()),
                },
            ))
            .await;
        if let Some(staged) = &staged {
            // Best-effort cleanup; the substrate has already read them.
            staged.cleanup().await;
        }
        let reply = reply.map_err(internal)?;
        match SpawnEngineResult::decode_from_bytes(&reply.payload) {
            Some(SpawnEngineResult::Ok {
                engine_id,
                rpc_port,
            }) => json(&EngineInfo {
                engine_id,
                rpc_port,
                // A just-spawned engine is alive as of now.
                last_heartbeat_age_millis: 0,
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
        description = "Upload a binary into the hub's content-addressed store (ADR-0115). Pass `staged_path` — an absolute path to the binary on the fleet host — and an optional `name`. The hub reads the path itself (aether-mcp never reads the bytes — a binary is too large for the tool channel), sha256-hashes it, dedups against the store (a re-upload of identical bytes returns the same hash), forks `<binary> --describe` to capture its manifest (chassis kind, linked caps, build provenance), stores both, and points `name` (when given) at the hash. The store persists across a restart-hub. Returns {hash, name}."
    )]
    pub async fn upload_binary(
        &self,
        Parameters(args): Parameters<UploadBinaryArgs>,
    ) -> Result<String, McpError> {
        // The hub reads the staged path; aether-mcp forwards it, never
        // reading the bytes (unlike load_component).
        let reply = self
            .session
            .call_one(local_envelope(
                ENGINE_CAP,
                &UploadBinary {
                    staged_path: args.staged_path,
                    name: args.name,
                },
            ))
            .await
            .map_err(internal)?;
        match UploadBinaryResult::decode_from_bytes(&reply.payload) {
            Some(UploadBinaryResult::Ok { hash, name }) => {
                json(&serde_json::json!({ "hash": hash, "name": name }))
            }
            Some(UploadBinaryResult::Err { error }) => Err(internal_msg(&error)),
            None => Err(internal_msg("undecodable UploadBinaryResult")),
        }
    }

    #[tool(
        description = "Enumerate the hub's stored binaries (ADR-0115). Optional AND-combined filters: `chassis` (\"headless\"/\"desktop\"/\"hub\"), `caps` (keep only binaries whose linked caps are a superset of every listed cap), `target` (the build target triple). Omit all to list the whole store. Returns an array of {hash, name, manifest: {chassis, caps, git_sha, profile, target}} — the manifest each binary reported via a one-time --describe at upload time."
    )]
    pub async fn list_binaries(
        &self,
        Parameters(args): Parameters<ListBinariesArgs>,
    ) -> Result<String, McpError> {
        let reply = self
            .session
            .call_one(local_envelope(
                ENGINE_CAP,
                &ListBinaries {
                    chassis: args.chassis,
                    caps: args.caps,
                    target: args.target,
                },
            ))
            .await
            .map_err(internal)?;
        match ListBinariesResult::decode_from_bytes(&reply.payload) {
            Some(result) => json(&result.binaries),
            None => Err(internal_msg("undecodable ListBinariesResult")),
        }
    }

    #[tool(
        description = "Upload a WASM component into the hub's content-addressed store (ADR-0116). Pass `staged_path` — an absolute path to the component .wasm on the fleet host — and an optional `name` (the component's Actor::NAMESPACE is the natural one). The hub reads the path itself (aether-mcp never reads the bytes — too large for the tool channel), sha256-hashes it, dedups against the store (a re-upload of identical bytes returns the same hash), reads its manifest straight from the wasm (no execution step — exported actor namespaces, handled kind ids, #[fallback] presence, build provenance), stores both, and points `name` (when given) at the hash. The store persists across a restart-hub. Then load it by selector with load_component — the host wasm path is gone from load_component / replace_component / boot manifests, surviving only here as the upload input. Returns {hash, name}."
    )]
    pub async fn upload_component(
        &self,
        Parameters(args): Parameters<UploadComponentArgs>,
    ) -> Result<String, McpError> {
        // The hub reads the staged path; aether-mcp forwards it, never
        // reading the bytes (unlike the load_component resolve hop, which
        // pulls the bytes back from the store).
        let reply = self
            .session
            .call_one(local_envelope(
                ENGINE_CAP,
                &UploadComponent {
                    staged_path: args.staged_path,
                    name: args.name,
                },
            ))
            .await
            .map_err(internal)?;
        match UploadComponentResult::decode_from_bytes(&reply.payload) {
            Some(UploadComponentResult::Ok { hash, name }) => {
                json(&serde_json::json!({ "hash": hash, "name": name }))
            }
            Some(UploadComponentResult::Err { error }) => Err(internal_msg(&error)),
            None => Err(internal_msg("undecodable UploadComponentResult")),
        }
    }

    #[tool(
        description = "Enumerate the hub's stored components (ADR-0116). Optional AND-combined filters: `namespace` (keep only components exporting an actor with that Actor::NAMESPACE) and `handled_kind` (keep only components handling that kind, by tagged knd-… id or kind name). Omit both to list every stored component. Returns an array of {hash, name, manifest} — the manifest read straight from each wasm at upload: {namespaces, actors: [{namespace, handled_kinds, fallback}], handled_kinds, fallback, provenance}."
    )]
    pub async fn list_components(
        &self,
        Parameters(args): Parameters<ListComponentsArgs>,
    ) -> Result<String, McpError> {
        let handled_kind = match args.handled_kind.as_deref() {
            Some(s) => Some(resolve_handled_kind(s)?),
            None => None,
        };
        let reply = self
            .session
            .call_one(local_envelope(
                ENGINE_CAP,
                &ListComponents {
                    namespace: args.namespace,
                    handled_kind,
                },
            ))
            .await
            .map_err(internal)?;
        match ListComponentsResult::decode_from_bytes(&reply.payload) {
            Some(result) => json(&result.components),
            None => Err(internal_msg("undecodable ListComponentsResult")),
        }
    }

    #[tool(
        description = "Send one or more mail items to substrate mailboxes. Each item carries structured `params`, schema-encoded against the substrate kind vocabulary. Best-effort batch: per-item status is returned and one failure doesn't abort siblings. By default each item BLOCKS until its dispatch chain settles and the item's correlated reply payloads are returned in `replies` (status 'delivered'); each reply is {kind_id, kind_name, params (best-effort decode, null on miss), payload_bytes (base64 string, present only on a decode miss)}. The await cap is 600s (gated by the batch-level settlement against a slow provider cap); on timeout the item reports status 'timeout' with timed_out:true and any replies collected so far. Set fire_and_forget:true to restore non-blocking dispatch (status 'dispatched', empty replies) — use it for a fire-and-poke (e.g. a DrawTriangle before a capture_frame) or a cap that never replies."
    )]
    pub async fn send_mail(
        &self,
        Parameters(args): Parameters<SendMailArgs>,
    ) -> Result<String, McpError> {
        let fire_and_forget = args.fire_and_forget;
        let mut statuses = Vec::with_capacity(args.mails.len());
        for (index, spec) in args.mails.into_iter().enumerate() {
            let mut replies = Vec::new();
            let mut timed_out = false;
            let status = if fire_and_forget {
                match self.deliver_one_fire(spec).await {
                    Ok(()) => "dispatched".to_owned(),
                    Err(e) => format!("error: {e}"),
                }
            } else {
                // Capture the engine id and the handler's declared reply kind
                // (ADR-0109 / issue 1803) before `deliver_one` consumes the
                // spec, so `decode_reply_events` can search the per-engine kind
                // cache for component-defined reply kinds (issue 1804).
                let engine = Uuid::parse_str(&spec.engine_id).ok().map(EngineId);
                let declared_reply = engine.and_then(|e| {
                    let mbx = mailbox_id_from_path(&spec.recipient_name);
                    let cache = self
                        .components
                        .lock()
                        .expect("component cache mutex is never poisoned");
                    cache.get(&(e, mbx)).and_then(|caps| {
                        caps.handlers
                            .iter()
                            .find(|h| h.name == spec.kind_name)
                            // ADR-0112: only a single-class handler names one
                            // static reply kind to search the cache for; a
                            // manual / silent handler yields no declared kind.
                            .and_then(|h| match h.reply {
                                aether_data::ReplyContract::One(id) => Some(id),
                                _ => None,
                            })
                    })
                });
                match self.deliver_one(spec).await {
                    Ok((events, hit_timeout)) => {
                        let engine_kinds = engine
                            .map(|e| self.snapshot_engine_kinds(e))
                            .unwrap_or_default();
                        replies = decode_reply_events(&events, &engine_kinds, declared_reply);
                        timed_out = hit_timeout;
                        if hit_timeout { "timeout" } else { "delivered" }.to_owned()
                    }
                    Err(e) => format!("error: {e}"),
                }
            };
            statuses.push(MailStatus {
                index,
                status,
                replies,
                timed_out,
            });
        }
        json(&statuses)
    }

    #[tool(
        description = "Atomic batched dispatch with combined trace tree. Like send_mail but every spec lands on the engine's aether.trace mailbox under one shared chassis root, and the response returns the full trace subtree once the chain settles — no window guessing, no separate describe_tree call. By default it BLOCKS until settlement and also returns the batch's correlated reply payloads as a flat arrival-ordered `replies` list (the batch is one wire Call, so replies aren't per-item) alongside the tree; each reply is {kind_id, kind_name, params (best-effort decode, null on miss), payload_bytes (base64 string, present only on a decode miss)}. Two-call protocol behind the scenes: the substrate emits a synchronous ack with the root id, the caller waits for chain settlement on the wire collecting reply events, then issues a describe_tree against the captured root. Bad specs abort the whole batch before any mail moves (mirrors capture_frame). settlement_timeout_ms caps wall-clock wait (default 300000, max 600000); on timeout the response carries status:timeout with no root, tree, or replies. Set fire_and_forget:true to return the ack only (status:dispatched with root populated, mails/replies null) without awaiting settlement."
    )]
    pub async fn send_mail_traced(
        &self,
        Parameters(args): Parameters<SendMailTracedArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        // Encode the batch before sending — a bad spec produces a
        // clean invalid-params error and never touches the wire.
        // Same shape `CaptureFrame` carries: `Vec<MailEnvelope>` with
        // name-level addressing the substrate resolves at dispatch
        // time via `resolve_bundle`. ADR-0091: descriptors come from
        // the per-engine merged view so a component's own kinds
        // encode after `load_component`.
        let mails = self
            .encode_traced_bundle(engine, &args.mails)
            .await
            .map_err(|e| McpError::invalid_params(format!("send_mail_traced batch: {e}"), None))?;
        let timeout_ms = args
            .settlement_timeout_ms
            .unwrap_or(AWAIT_TIMEOUT_DEFAULT_MS)
            .min(AWAIT_TIMEOUT_CAP_MS);
        let dispatch_envelope =
            engine_envelope(engine, TRACE_MAILBOX_NAME, &DispatchTraced { mails });

        // Fire-and-forget: write the dispatch without awaiting the chain
        // to settle. We still need the synchronous ack's `root`, so this
        // path isn't a bare `fire` — issue the call, read the ack from
        // the (immediately-available) first reply, and skip the tree
        // walk. Bound it by the same timeout so a wedged ack doesn't hang.
        if args.fire_and_forget {
            let (events, ack_timed_out) = self
                .session
                .call_collecting(
                    dispatch_envelope,
                    Duration::from_millis(u64::from(timeout_ms)),
                )
                .await
                .map_err(internal)?;
            if ack_timed_out {
                return json(&SendMailTracedResponse {
                    status: "timeout".into(),
                    root: None,
                    mails: None,
                    in_flight: None,
                    replies: None,
                });
            }
            let root = decode_traced_ack(&events)?;
            let root_json = {
                self.ensure_names(engine).await;
                let cache = self
                    .names
                    .lock()
                    .expect("reverse-name cache mutex is never poisoned");
                mail_id_to_json(root, cache.get(&engine))
            };
            return json(&SendMailTracedResponse {
                status: "dispatched".into(),
                root: Some(root_json),
                mails: None,
                in_flight: None,
                replies: None,
            });
        }

        // Round 1: ack carries the chassis-root MailId; ReplyEnd
        // closes when the chain settles substrate-side. `call_collecting`
        // keeps every correlated `ReplyEvent` (the ack plus any cap
        // replies) instead of `call_one`'s single-event discard.
        let (events, ack_timed_out) = self
            .session
            .call_collecting(
                dispatch_envelope,
                Duration::from_millis(u64::from(timeout_ms)),
            )
            .await
            .map_err(internal)?;
        if ack_timed_out {
            return json(&SendMailTracedResponse {
                status: "timeout".into(),
                root: None,
                mails: None,
                in_flight: None,
                replies: None,
            });
        }
        let engine_kinds = self.snapshot_engine_kinds(engine);
        let replies = decode_reply_events(strip_ack(&events), &engine_kinds, None);
        let root = decode_traced_ack(&events)?;

        // Round 2: reconstruct the tree by a guided walk over the
        // per-actor trace rings (ADR-0086 Phase 3b). Seed at
        // `root.sender` (`CHASSIS_MAILBOX_ID` for this chassis-rooted
        // dispatch), follow each `Sent`'s recipient, fetch every ring
        // with one `aether.trace.tail` addressed by id — the chassis-
        // host ring answers at `CHASSIS_MAILBOX_ID`. The walk touches
        // only the actors in the tree; the rings are in-memory and the
        // chain has already settled, so each hop is microseconds. A
        // failed or undecodable per-ring reply contributes no entries —
        // the walk completes from the rings that answer.
        let mut walk = TreeWalk::new(root);
        while let Some(mailbox) = walk.next_mailbox() {
            let request = TraceTail {
                max: 0,
                since: None,
                root: Some(root),
            };
            let entries = match self
                .session
                .call_one(engine_envelope_by_id(engine, mailbox, &request))
                .await
                .ok()
                .and_then(|reply| TraceTailResult::decode_from_bytes(&reply.payload))
            {
                Some(TraceTailResult::Ok { entries, .. }) => entries,
                Some(TraceTailResult::Err { .. }) | None => Vec::new(),
            };
            walk.absorb(entries);
        }

        match walk.finish() {
            DescribeTreeResult::Ok {
                root,
                in_flight,
                mails,
            } => {
                // Reverse mailbox / kind ids to real names through the
                // engine's inventory map (ADR-0088 §8). `render_mail_nodes`
                // builds + resolves the map; the root id then renders
                // through the now-populated cache (its sender is the
                // chassis mailbox — a static name).
                let mails = self.render_mail_nodes(engine, mails).await;
                let root = {
                    let cache = self
                        .names
                        .lock()
                        .expect("reverse-name cache mutex is never poisoned");
                    mail_id_to_json(root, cache.get(&engine))
                };
                json(&SendMailTracedResponse {
                    status: "settled".into(),
                    root: Some(root),
                    mails: Some(mails),
                    in_flight: Some(in_flight),
                    replies: Some(replies),
                })
            }
            DescribeTreeResult::Err { not_found } => Err(internal_msg(&format!(
                "describe_tree: root {not_found:?} not found"
            ))),
        }
    }

    #[tool(
        description = "Load a WASM component into a substrate by registry selector (ADR-0116) — upload_component first if it isn't stored. Pass `selector`: a content hash, a name (latest upload under it), or a module@actor (the @actor half picks an exported actor type from a multi-actor module). The host wasm path is gone — the only path anywhere is the upload_component input. aether-mcp resolves the selector hub-local to the wasm bytes, forwards aether.component.load to the engine's aether.component mailbox, and awaits the LoadResult — returning {mailbox_id, name, capabilities} or an error. The component's kind vocabulary rides in the wasm's aether.kinds custom section. Pass config_path to deliver init-config bytes to a typed-config component (ADR-0090): the file must already hold the component's Config kind wire bytes — describe_component reports the expected config kind. Pass export to pick which exported actor type to instantiate from a multi-actor module (ADR-0096), named by its Actor::NAMESPACE; a module@actor selector populates it from its @actor half; omit both to load the module's entry type (the first in its export! list, and the only type a single-actor module has). The returned name + capabilities describe the selected type. Very large wasm payloads (debug builds at 15-25 MiB) may exceed the RPC framing cap — prefer release builds, or raise the cap via the AETHER_MAX_FRAME_SIZE env var (default 64 MiB, clamped at 1 GiB; issue 1271)."
    )]
    pub async fn load_component(
        &self,
        Parameters(args): Parameters<LoadComponentArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let selector = args.selector.clone();
        // ADR-0116: resolve the selector hub-local to the wasm bytes; a
        // `module@actor` selector's `@actor` half rides back as `export`.
        let resolved = self.resolve_component(&selector).await?;
        // ADR-0090 (issue 1257): read optional init-config bytes from a
        // file path (already encoded to the component's `Config` kind
        // wire shape). Absent → empty vec → the substrate hands `&[]` to
        // a `Config = ()` guest's `init`.
        let config = match args.config_path {
            Some(ref path) => fs::read(path).await.map_err(|e| {
                McpError::invalid_params(format!("reading config_path {path:?}: {e}"), None)
            })?,
            None => Vec::new(),
        };
        // An explicit `export` arg wins over the selector's `@actor` half.
        let export = args.export.or(resolved.export);
        let reply = self
            .session
            .call_one(engine_envelope(
                engine,
                COMPONENT_CAP,
                &LoadComponent {
                    wasm: resolved.wasm,
                    name: args.name,
                    config,
                    export,
                },
            ))
            .await
            .map_err(|e| frame_size_aware_error(&format!("load_component {selector:?}"), e))?;
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
        description = "Atomically replace a live component's WASM with a build resolved from a registry selector (ADR-0022 structural splice; ADR-0116 selector). Pass `selector` (hash-primary — a hash pins or rolls the component to an exact build; a name or module@actor resolves too); the host wasm path is gone, surviving only as the upload_component input. aether-mcp resolves the selector hub-local to the wasm bytes and forwards aether.component.replace to the engine's aether.component mailbox. drain_timeout_ms is accepted for wire compatibility but currently ignored. Returns the replaced component's advertised capabilities. Very large wasm payloads (debug builds at 15-25 MiB) may exceed the RPC framing cap — prefer release builds, or raise the cap via the AETHER_MAX_FRAME_SIZE env var (default 64 MiB, clamped at 1 GiB; issue 1271)."
    )]
    pub async fn replace_component(
        &self,
        Parameters(args): Parameters<ReplaceComponentArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let mailbox_id = parse_mailbox_id(&args.mailbox_id)?;
        let selector = args.selector.clone();
        // ADR-0116: resolve the selector hub-local to the replacement wasm
        // bytes (hash-primary, so a hash pins/rolls to an exact build).
        let resolved = self.resolve_component(&selector).await?;
        // ADR-0090 (issue 1257): optional init-config bytes for the
        // replacement instance, read from a file path like the load path.
        let config = match args.config_path {
            Some(ref path) => fs::read(path).await.map_err(|e| {
                McpError::invalid_params(format!("reading config_path {path:?}: {e}"), None)
            })?,
            None => Vec::new(),
        };
        let reply = self
            .session
            .call_one(engine_envelope(
                engine,
                COMPONENT_CAP,
                &ReplaceComponent {
                    mailbox_id,
                    wasm: resolved.wasm,
                    drain_timeout_ms: args.drain_timeout_ms,
                    config,
                },
            ))
            .await
            .map_err(|e| frame_size_aware_error(&format!("replace_component {selector:?}"), e))?;
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
        description = "Capture an engine's current frame as a PNG, returned inline as image content. Optionally carries two mail bundles dispatched atomically around the capture: `mails` fires before readback (state changes that should appear in the image), `after_mails` fires after (cleanup). A bad bundle entry aborts the whole capture before any mail moves. Optionally carries `checks`: substrate-side reductions (not_all_black, differs_from_background, coverage, centroid, bounding_box) scored on the exact RGBA the PNG is built from and returned as a `verdict` text block alongside the image — a one-call spawn -> drive -> assert with no caller-side PNG decode. Optionally carries `similarity`: a reference-image check (`namespace` + `reference_path` + `threshold`) the render thread scores as a normalised mean-absolute-error against the captured RGBA, returned as `similarity_score` / `similarity_pass` text blocks alongside the image."
    )]
    pub async fn capture_frame(
        &self,
        Parameters(args): Parameters<CaptureFrameArgs>,
    ) -> Result<CallToolResult, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        // Encode both bundles before sending — a bad entry produces a
        // clean invalid-params error and never touches the wire.
        // ADR-0091: descriptors come from the per-engine merged view
        // so a `capture_frame` referencing a component-defined kind
        // (e.g. an `aether.mesh.load` pre-mail) encodes correctly
        // after `load_component`.
        let mails = self
            .encode_capture_bundle(engine, &args.mails)
            .await
            .map_err(|e| {
                McpError::invalid_params(format!("capture_frame mails bundle: {e}"), None)
            })?;
        let after_mails = self
            .encode_capture_bundle(engine, &args.after_mails)
            .await
            .map_err(|e| {
                McpError::invalid_params(format!("capture_frame after_mails bundle: {e}"), None)
            })?;
        // Map the verdict request: an unknown reduction name is a clean
        // invalid-params error before the capture touches the wire.
        let checks = args
            .checks
            .iter()
            .map(capture_check)
            .collect::<Result<Vec<FrameCheck>, McpError>>()?;
        // Map the optional reference-image similarity check
        // (iamacoffeepot/aether#1780); the render thread loads the
        // reference and scores the captured RGBA against it.
        let similarity = args.similarity.as_ref().map(|s| SimilarityCheck {
            namespace: s.namespace.clone(),
            reference_path: s.reference_path.clone(),
            threshold: s.threshold,
        });
        let reply = self
            .session
            .call_one(engine_envelope(
                engine,
                RENDER_CAP,
                &CaptureFrame {
                    mails,
                    after_mails,
                    checks,
                    similarity,
                },
            ))
            .await
            .map_err(internal)?;
        match CaptureFrameResult::decode_from_bytes(&reply.payload) {
            Some(CaptureFrameResult::Ok {
                png,
                verdict,
                similarity_score,
                similarity_pass,
            }) => {
                let encoded = STANDARD.encode(&png);
                let mut content = vec![Content::image(encoded, "image/png")];
                // Surface the verdict as a JSON text block so the caller
                // reads the reductions' results without decoding the PNG
                // (iamacoffeepot/aether#1777). Absent when no `checks`
                // were requested.
                if let Some(verdict) = verdict {
                    let json = serde_json::to_string(&verdict)
                        .map_err(|e| internal_msg(&format!("verdict serialize: {e}")))?;
                    content.push(Content::text(json));
                }
                // Surface the similarity verdict as its own JSON block
                // when a `similarity` check ran (iamacoffeepot/aether#1780).
                if similarity_score.is_some() || similarity_pass.is_some() {
                    let json = serde_json::to_string(&serde_json::json!({
                        "similarity_score": similarity_score,
                        "similarity_pass": similarity_pass,
                    }))
                    .map_err(|e| internal_msg(&format!("similarity serialize: {e}")))?;
                    content.push(Content::text(json));
                }
                Ok(CallToolResult::success(content))
            }
            Some(CaptureFrameResult::Err { error }) => Err(internal_msg(&error)),
            None => Err(internal_msg("undecodable CaptureFrameResult")),
        }
    }

    #[tool(
        description = "List the substrate kind vocabulary — the static aether.* kinds aether-mcp ships with (not a per-engine query; component-defined kinds use describe_component). Default (no args) returns a compact [{name, shape}] JSON array where shape is a one-line field rendering — small enough to never trip the context cap and chunk-readable. prefix (case-sensitive starts_with) filters the listing to a kind family (e.g. \"aether.fs\" for just the fs kinds). full:true returns the full [{name, schema}] with the authoritative nested SchemaType; combine with prefix to bound the payload to the kinds a task needs."
    )]
    pub async fn describe_kinds(
        &self,
        Parameters(args): Parameters<DescribeKindsArgs>,
    ) -> Result<String, McpError> {
        let all = descriptors::all();
        let filtered: Vec<_> = if let Some(prefix) = &args.prefix {
            all.into_iter()
                .filter(|d| d.name.starts_with(prefix.as_str()))
                .collect()
        } else {
            all
        };
        if args.full {
            json(&filtered)
        } else {
            let summary: Vec<KindSummary> = filtered
                .iter()
                .map(|d| KindSummary {
                    name: d.name.clone(),
                    shape: render_shape(&d.schema),
                })
                .collect();
            json(&summary)
        }
    }

    #[tool(
        description = "List the native transforms collected at link time (ADR-0048): every #[transform] fn with its global transform_id, fully-qualified name, declared input kind ids (slot order), and output kind id. These are pure Kind -> Kind functions a DAG Transform node dispatches; this is the static inventory aether-mcp ships with (a transform set is a build-time property). Empty when no first-party transforms are linked."
    )]
    pub async fn describe_transforms(&self) -> Result<String, McpError> {
        let listing: Vec<TransformListing> = aether_data::transforms()
            .map(|t| TransformListing {
                transform_id: t.transform_id.to_string(),
                name: t.name,
                input_kind_ids: t.input_kind_ids.iter().map(ToString::to_string).collect(),
                output_kind_id: t.output_kind_id.to_string(),
            })
            .collect();
        json(&listing)
    }

    #[tool(
        description = "Describe a loaded component's receive-side capabilities (ADR-0033): the kinds it typed-handles with per-handler docs, whether it has a fallback catchall, its top-level doc, and (ADR-0090) its boot-config kind id+name when it declared a typed Config. Reads aether-mcp's component cache, populated by load_component / replace_component — describing a component aether-mcp didn't load (or after an aether-mcp restart) returns an error."
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
        description = "Describe a substrate's NATIVE chassis caps' reply contracts (ADR-0109 §5): the native analogue of describe_component. Sends aether.inventory.handlers to the engine's aether.inventory mailbox and decodes aether.inventory.handlers_result — the link-time handler manifest the #[actor] macro populates. Returns the handlers folded per mailbox namespace; each handler carries its input kind (id + name) and its reply kind id+name, so you read a native cap's In -> Out (e.g. aether.fs.read -> aether.fs.read_result) before issuing the call. reply is null for a fire-and-forget handler. Reply kind names resolve best-effort from the static substrate vocabulary; a component-defined reply kind stays null. Use describe_component for a loaded wasm component, describe_kinds for the full schema of any kind."
    )]
    pub async fn describe_handlers(
        &self,
        Parameters(args): Parameters<DescribeHandlersArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let reply = self
            .session
            .call_one(engine_envelope(engine, INVENTORY_CAP, &ListHandlers {}))
            .await
            .map_err(internal)?;
        let Some(HandlersResult { handlers }) = HandlersResult::decode_from_bytes(&reply.payload)
        else {
            return Err(internal_msg("undecodable HandlersResult"));
        };
        // Fold the flat per-handler manifest per owning namespace so each
        // native cap reads as a describe_component-style handler list. A
        // BTreeMap keeps the caps (and their handlers) in a stable order.
        let mut folded: BTreeMap<String, Vec<NativeHandlerJson>> = BTreeMap::new();
        for entry in handlers {
            folded
                .entry(entry.namespace)
                .or_default()
                .push(NativeHandlerJson {
                    // Input kind id rendered as the ADR-0064 tagged string,
                    // falling back to a hex literal on an unencodable id.
                    input_id: tagged_id::encode(entry.id.0)
                        .unwrap_or_else(|| format!("{:#x}", entry.id.0)),
                    input_name: entry.name,
                    // The reply kind id is the contract; resolve its name
                    // best-effort from the static substrate vocabulary so
                    // the In -> Out reads without a second lookup. A
                    // component-defined reply kind stays `None`.
                    reply_id: entry.reply.map(|id| {
                        tagged_id::encode(id.0).unwrap_or_else(|| format!("{:#x}", id.0))
                    }),
                    reply_name: entry.reply.and_then(static_kind_name),
                });
        }
        let caps = folded
            .into_iter()
            .map(|(namespace, handlers)| NativeCapHandlers {
                namespace,
                handlers,
            })
            .collect();
        json(&DescribeHandlersResponse {
            engine_id: args.engine_id,
            caps,
        })
    }

    #[tool(
        description = "Pull recent log entries from one actor's per-actor log ring (ADR-0081). \
                       Sends aether.log.tail to the named mailbox and decodes aether.log.tail_result. \
                       Every actor — native or wasm trampoline — serves this kind via the substrate's \
                       framework dispatch arm, so any mailbox is queryable (e.g. \"aether.audio\", \
                       \"aether.component/aether.embedded:camera\"). `max` defaults to 100 and clamps to 1000; \
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
        description = "Dump one actor's per-handler execution-cost EWMA table \
                       (iamacoffeepot/aether#1128, Phase 0 dark instrumentation). Sends \
                       aether.cost.tail to the named mailbox and decodes aether.cost.tail_result. \
                       The substrate folds (Finished − Received) from each dispatch's trace \
                       bracket into a per-handler EWMA; this reads it back — MEASURE-ONLY, the \
                       table has no scheduling effect. Every actor — native or wasm trampoline — \
                       serves this kind via the substrate's framework dispatch arm, so any mailbox \
                       is queryable. Each row carries the handler kind (id + resolved name when \
                       known), `mean_nanos` / `mad_nanos` (the EWMA mean + mean-absolute-deviation \
                       of execution time in nanos), and `samples` (folded-sample count; `0` is the \
                       neutral seed — a handler the actor declares but hasn't run yet). Pass \
                       `kind_id` (tagged `knd-…` or decimal) to filter to one handler. Use it to \
                       check whether handler costs are heterogeneous enough to warrant the \
                       cost-aware recruiter (iamacoffeepot/aether#1127)."
    )]
    pub async fn actor_cost(
        &self,
        Parameters(args): Parameters<ActorCostArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let engine_id_str = args.engine_id.clone();
        let mailbox_name = args.mailbox_name.clone();
        // Optional kind filter: accept a tagged `knd-…` id or a raw
        // decimal `u64`, matching the rest of the MCP id surface.
        let kind = match args.kind_id.as_deref() {
            Some(s) => Some(parse_kind_id(s)?),
            None => None,
        };
        let request = CostTail { kind };
        let reply = self
            .session
            .call_one(engine_envelope(engine, &args.mailbox_name, &request))
            .await
            .map_err(internal)?;
        match CostTailResult::decode_from_bytes(&reply.payload) {
            Some(CostTailResult::Ok { rows }) => {
                let response = ActorCostResponse {
                    engine_id: engine_id_str,
                    mailbox_name,
                    rows: rows
                        .into_iter()
                        .map(|r| ActorCostRow {
                            // Render the kind id as the ADR-0064 tagged
                            // string the rest of the MCP wire uses, falling
                            // back to a hex literal on an unencodable id.
                            kind_id: tagged_id::encode(r.kind_id.0)
                                .unwrap_or_else(|| format!("{:#x}", r.kind_id.0)),
                            // The substrate ships `kind_name: None` (the
                            // cost table holds ids, not names); resolve it
                            // best-effort from the static kind inventory
                            // the MCP harness ships with. Component-defined
                            // kinds stay `None`.
                            kind_name: r.kind_name.or_else(|| static_kind_name(r.kind_id)),
                            mean_nanos: r.mean_nanos,
                            mad_nanos: r.mad_nanos,
                            samples: r.samples,
                        })
                        .collect(),
                };
                json(&response)
            }
            Some(CostTailResult::Err { error }) => Err(internal_msg(&format!(
                "actor_cost: {mailbox_name} — {error}"
            ))),
            None => Err(internal_msg("undecodable CostTailResult")),
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
        // Build the wire descriptor from the typed arg, reading each
        // Source's `payload_path` into the wire `payload` bytes. A read
        // failure surfaces as a clean invalid-params error and never
        // touches the wire.
        let descriptor = build_descriptor(args.descriptor)
            .await
            .map_err(|e| McpError::invalid_params(format!("submit_dag descriptor: {e}"), None))?;
        // Encode via the kind's wire encode — ids serialize in their u64
        // form, matching the substrate's decode.
        let payload = Submit { descriptor }.encode_into_bytes();
        let envelope = MailEnvelope {
            to: MailboxAddress {
                engine: Some(engine),
                // Runtime-name routing: the out-of-process MCP harness addresses
                // the dag cap by its well-known wire name (no in-process actor
                // type to resolve through).
                #[allow(clippy::disallowed_methods)]
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
    /// it through the hub, awaiting the substrate's terminal settle and
    /// surfacing the correlated reply events (issue 1242). Returns the
    /// collected reply envelopes plus a `timed_out` flag — the await is
    /// bounded by [`AWAIT_TIMEOUT_DEFAULT_MS`] so a cap that never
    /// replies returns at the cap rather than hanging.
    async fn deliver_one(&self, spec: MailSpec) -> anyhow::Result<(Vec<MailEnvelope>, bool)> {
        let envelope = self.build_mail_envelope(spec).await?;
        let timeout = Duration::from_millis(u64::from(AWAIT_TIMEOUT_DEFAULT_MS));
        self.session.call_collecting(envelope, timeout).await
    }

    /// [`Self::deliver_one`]'s fire-and-forget twin: build the envelope
    /// and write the `Call` without awaiting any reply (issue 1242).
    async fn deliver_one_fire(&self, spec: MailSpec) -> anyhow::Result<()> {
        let envelope = self.build_mail_envelope(spec).await?;
        self.session.fire(envelope).await
    }

    /// Resolve a component registry selector hub-local to its wasm bytes +
    /// `@actor` export (ADR-0116). aether-mcp issues a `ResolveComponent` to
    /// the `aether.engine` cap (no engine route — the store is hub-level),
    /// which matches the selector to a single component and replies with the
    /// wasm bytes from its store; aether-mcp then forwards those bytes to
    /// the target substrate's `aether.component` mailbox. Shared by
    /// `load_component`, `replace_component`, and the boot-manifest
    /// pre-resolution, so the load seam stays path-free. An `Err` reply (no
    /// match, or an attribute query matching more than one component) is a
    /// clean tool error.
    async fn resolve_component(&self, selector: &str) -> Result<ResolvedComponent, McpError> {
        let reply = self
            .session
            .call_one(local_envelope(
                ENGINE_CAP,
                &ResolveComponent {
                    selector: ComponentSelector {
                        query: Some(selector.to_owned()),
                        namespace: None,
                        handled_kind: None,
                    },
                },
            ))
            .await
            .map_err(|e| frame_size_aware_error(&format!("resolve_component {selector:?}"), e))?;
        match ResolveComponentResult::decode_from_bytes(&reply.payload) {
            Some(ResolveComponentResult::Ok { wasm, export, .. }) => {
                Ok(ResolvedComponent { wasm, export })
            }
            Some(ResolveComponentResult::Err { error }) => Err(internal_msg(&error)),
            None => Err(internal_msg("undecodable ResolveComponentResult")),
        }
    }

    /// Pre-resolve a `spawn_substrate` boot list against the component
    /// registry (ADR-0116) and stage it as a temp boot-manifest JSON the
    /// hub injects as `AETHER_BOOT_MANIFEST` (issue 1776). For each spec
    /// aether-mcp resolves the selector hub-local to its wasm bytes, writes
    /// the bytes to a per-process-unique temp `.wasm`, and points the
    /// manifest entry's `wasm` at that staged path — so the substrate boot
    /// autoload path stays path-based, now fed by the registry rather than
    /// host build paths. A `module@actor` selector's `@actor` half
    /// populates the entry's `export` unless the spec set one explicitly.
    /// Returns the staged paths so the caller cleans them all up once the
    /// substrate has read them at boot.
    async fn stage_boot_manifest(
        &self,
        components: &[ComponentSpec],
    ) -> Result<StagedBootManifest, McpError> {
        use std::env;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);

        let mut wasm_paths: Vec<PathBuf> = Vec::with_capacity(components.len());
        let mut entries: Vec<serde_json::Value> = Vec::with_capacity(components.len());
        for spec in components {
            let resolved = self.resolve_component(&spec.selector).await?;
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let wasm_path =
                env::temp_dir().join(format!("aether-boot-wasm-{}-{seq}.wasm", process::id()));
            fs::write(&wasm_path, &resolved.wasm).await.map_err(|e| {
                internal_msg(&format!(
                    "staging boot wasm for selector {:?}: {e}",
                    spec.selector
                ))
            })?;
            let mut entry = serde_json::json!({ "wasm": wasm_path.to_string_lossy() });
            if let Some(name) = &spec.name {
                entry["name"] = serde_json::json!(name);
            }
            if let Some(config) = &spec.config_path {
                entry["config"] = serde_json::json!(config);
            }
            // An explicit `export` wins over the selector's `@actor` half.
            if let Some(export) = spec.export.clone().or(resolved.export) {
                entry["export"] = serde_json::json!(export);
            }
            entries.push(entry);
            wasm_paths.push(wasm_path);
        }

        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let manifest_path =
            env::temp_dir().join(format!("aether-boot-manifest-{}-{seq}.json", process::id()));
        let bytes = serde_json::to_vec(&serde_json::json!({ "components": entries }))
            .map_err(|e| internal_msg(&format!("encoding boot manifest: {e}")))?;
        fs::write(&manifest_path, bytes)
            .await
            .map_err(|e| internal_msg(&format!("staging boot manifest: {e}")))?;
        Ok(StagedBootManifest {
            manifest_path,
            wasm_paths,
        })
    }

    /// Resolve a `MailSpec` against the per-engine merged kind view
    /// (static prefill + cached `ListKinds` reply, ADR-0091) and build
    /// the `engine = Some` wire envelope — the shared front half of
    /// [`Self::deliver_one`] / [`Self::deliver_one_fire`]. A miss
    /// against an engine that has loaded a component triggers one
    /// `aether.inventory.kinds` refresh before erroring "unknown
    /// kind"; the encode then succeeds for the component's own kinds.
    async fn build_mail_envelope(&self, spec: MailSpec) -> anyhow::Result<MailEnvelope> {
        // ADR-0098/0099 wire boundary: `recipient_name` is user-controlled
        // and folds to a registry key, so cap its scope depth / byte size
        // before it reaches `mailbox_id_from_path` (the fold itself stays
        // infallible for static callers). A breach fails this mail item.
        validate_recipient_scope(&spec.recipient_name)?;
        let engine = EngineId(
            Uuid::parse_str(&spec.engine_id)
                .map_err(|e| anyhow::anyhow!("engine_id is not a valid UUID: {e}"))?,
        );
        let desc = self.lookup_descriptor(engine, &spec.kind_name).await?;
        let params = spec.params.unwrap_or(serde_json::Value::Null);
        let params = resolve_bytes_params(params, &desc.schema, max_frame_size())
            .await
            .map_err(|e| anyhow::anyhow!("resolving blob params: {e}"))?;
        let payload = aether_codec::encode_schema(&params, &desc.schema)
            .map_err(|e| anyhow::anyhow!("param encode failed: {e}"))?;
        Ok(MailEnvelope {
            to: MailboxAddress {
                engine: Some(engine),
                // ADR-0099 §4: resolve the recipient by the parse → fold,
                // so a `/`-rendered hosted / nested actor name
                // (`aether.component/aether.component/aether.embedded:camera`)
                // reaches its lineage-folded id. A root-cap name is a
                // single segment and folds to the same id `hash(name)`
                // gives.
                mailbox: mailbox_id_from_path(&spec.recipient_name),
            },
            from: None,
            kind: KindId(kind_id_from_parts(&desc.name, &desc.schema)),
            correlation_id: None,
            payload,
        })
    }

    /// Ensure `engine`'s reverse-name map is built (ADR-0088 §8). On the
    /// first need for an engine, fetch `aether.inventory.manifest` and
    /// fold it into an [`EngineNames`]; on any subsequent call the cached
    /// map is reused. An engine that doesn't answer the manifest (older
    /// build, headless without the cap, transient error) gets an empty
    /// map cached — every lookup then falls back to the hex tag rather
    /// than erroring the tool or re-fetching on every render.
    async fn ensure_names(&self, engine: EngineId) {
        if self
            .names
            .lock()
            .expect("reverse-name cache mutex is never poisoned")
            .contains_key(&engine)
        {
            return;
        }
        // Fetch outside the lock — the await must not hold a std Mutex.
        // No / undecodable reply caches an empty map so we fall back to
        // hex for this engine without re-querying every render.
        let manifest = self
            .session
            .call_one(engine_envelope(engine, INVENTORY_CAP, &Manifest {}))
            .await
            .ok()
            .and_then(|reply| ManifestResult::decode_from_bytes(&reply.payload))
            .unwrap_or_else(|| ManifestResult {
                names: Vec::new(),
                templates: Vec::new(),
            });
        let mut cache = self
            .names
            .lock()
            .expect("reverse-name cache mutex is never poisoned");
        // A concurrent session may have populated it while we awaited —
        // first writer wins, both maps are equivalent.
        cache
            .entry(engine)
            .or_insert_with(|| EngineNames::from_manifest(&manifest));
    }

    /// ADR-0091 §3 lookup → miss → refresh → retry → error flow. Look
    /// up `kind_name` in the engine's encode cache; on a miss, fetch
    /// the substrate's authoritative vocabulary via
    /// `aether.inventory.kinds` and retry. The per-engine refresh is
    /// collapsed under an async mutex so two concurrent misses on
    /// different unknown names trigger one RPC, not two.
    ///
    /// Returns the matched descriptor; errors with `unknown kind: …`
    /// after one refresh round-trip if the engine doesn't recognise
    /// the name (a typoed kind, or a kind belonging to a component
    /// that hasn't been loaded yet — distinguishable by the error
    /// type at the substrate's later dispatch attempt).
    async fn lookup_descriptor(
        &self,
        engine: EngineId,
        kind_name: &str,
    ) -> anyhow::Result<KindDescriptor> {
        // Fast path: hit on the cache as it stands. `prefill_engine`
        // populates the static `descriptors::all()` baseline on first
        // touch so the very first send for a substrate-vocab kind
        // doesn't trip a refresh.
        self.prefill_engine(engine);
        if let Some(desc) = self.cache_lookup(engine, kind_name) {
            return Ok(desc);
        }

        // Miss: take the per-engine refresh mutex, then re-check (a
        // concurrent waiter may have just refreshed) and refresh
        // ourselves if still missing.
        let guard = self.refresh_guard(engine);
        let _refresh = guard.lock().await;
        if let Some(desc) = self.cache_lookup(engine, kind_name) {
            return Ok(desc);
        }
        self.refresh_engine_kinds(engine).await;
        self.cache_lookup(engine, kind_name)
            .ok_or_else(|| anyhow::anyhow!("unknown kind: {kind_name}"))
    }

    /// Seed `engine`'s cache from the substrate's static vocabulary
    /// (`descriptors::all`) the first time the harness touches it. The
    /// static set is process-global so a second engine with the same
    /// build sees the same prefill, but the cache is keyed per-engine
    /// because component-defined kinds aren't shared across engines.
    #[allow(clippy::significant_drop_tightening)] // tight scope already
    fn prefill_engine(&self, engine: EngineId) {
        let mut cache = self
            .kinds
            .descriptors
            .lock()
            .expect("kinds-cache mutex is never poisoned");
        cache.entry(engine).or_insert_with(|| {
            descriptors::all()
                .into_iter()
                .map(|d| (d.name.clone(), d))
                .collect()
        });
    }

    /// Synchronous cache hit/miss check — no await, holds the std
    /// `Mutex` only across the cloning lookup.
    fn cache_lookup(&self, engine: EngineId, kind_name: &str) -> Option<KindDescriptor> {
        let cache = self
            .kinds
            .descriptors
            .lock()
            .expect("kinds-cache mutex is never poisoned");
        cache.get(&engine).and_then(|m| m.get(kind_name).cloned())
    }

    /// Fetch-or-create the per-engine refresh mutex. The mutex is
    /// wrapped in an `Arc` so we can drop the cache lock before
    /// awaiting; the only writers to `refresh_guards` are this
    /// function itself, so it's a small concurrent-insert with no
    /// upstream contention.
    fn refresh_guard(&self, engine: EngineId) -> Arc<AsyncMutex<()>> {
        let mut guards = self
            .kinds
            .refresh_guards
            .lock()
            .expect("kinds-cache refresh-guards mutex is never poisoned");
        guards
            .entry(engine)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Issue `ListKinds` against the engine's `aether.inventory`
    /// mailbox and replace this engine's cache with the reply (folded
    /// over the static prefill — a component's own kinds layer on top
    /// of the substrate baseline, neither side wins exclusively).
    /// A failed RPC / undecodable reply leaves the cache untouched so
    /// the caller's retry surfaces the original "unknown kind" miss
    /// rather than a transient RPC error.
    async fn refresh_engine_kinds(&self, engine: EngineId) {
        let Some(ListKindsResult { kinds }) = self
            .session
            .call_one(engine_envelope(engine, INVENTORY_CAP, &ListKinds {}))
            .await
            .ok()
            .and_then(|reply| ListKindsResult::decode_from_bytes(&reply.payload))
        else {
            return;
        };

        // Decode each `schema_postcard` back into a `SchemaType` via
        // `wire::from_bytes`; an entry whose schema fails to decode is
        // dropped (the substrate's wire form is canonical, so a decode
        // failure is a substrate / aether-data version mismatch —
        // better to skip the entry than panic the tool call).
        let fresh: Vec<KindDescriptor> = kinds
            .into_iter()
            .filter_map(|wire| {
                let schema = wire::from_bytes::<SchemaType>(&wire.schema_postcard).ok()?;
                Some(KindDescriptor {
                    name: wire.name,
                    schema,
                })
            })
            .collect();

        // Hold the cache lock only for the merge; the await above is
        // already complete, so the `MutexGuard`'s significant `Drop`
        // doesn't span any await point.
        self.merge_into_engine_cache(engine, fresh);
    }

    /// Merge `fresh` into `engine`'s cache map, replacing any prior
    /// entries with the same name. Factored out of `refresh_engine_kinds`
    /// so the cache lock is acquired in a tight scope — no other state
    /// hangs off the same critical section, and no await crosses the
    /// guard.
    #[allow(clippy::significant_drop_tightening)] // tight scope already
    fn merge_into_engine_cache(&self, engine: EngineId, fresh: Vec<KindDescriptor>) {
        let mut cache = self
            .kinds
            .descriptors
            .lock()
            .expect("kinds-cache mutex is never poisoned");
        let map = cache.entry(engine).or_default();
        for desc in fresh {
            map.insert(desc.name.clone(), desc);
        }
    }

    /// Snapshot the per-engine kind descriptor map for reply decoding
    /// (issue 1804). Returns a clone of `engine`'s `name → descriptor`
    /// map, or an empty map when the cache hasn't been seeded for that
    /// engine yet. The engine cache is prefilled from the static substrate
    /// vocabulary on the first `build_mail_envelope` call (`prefill_engine`),
    /// so an empty snapshot only arises on paths that never encoded a kind
    /// for this engine (e.g. a broken `engine_id` before `deliver_one` errored).
    fn snapshot_engine_kinds(&self, engine: EngineId) -> HashMap<String, KindDescriptor> {
        self.kinds
            .descriptors
            .lock()
            .expect("kinds-cache mutex is never poisoned")
            .get(&engine)
            .cloned()
            .unwrap_or_default()
    }

    /// Batch-resolve `ids` that the engine's static map missed via
    /// `aether.inventory.resolve` and fold the answers (positive *and*
    /// negative) into the per-engine dynamic cache. A no-op when `ids` is
    /// empty or the resolve call fails — the unresolved ids then render
    /// as hex tags. `ids` are raw `u64`s; the resolve wire takes tagged
    /// strings, so each is encoded for the request and decoded back for
    /// the cache key.
    async fn resolve_dynamic(&self, engine: EngineId, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        let tagged: Vec<String> = ids.iter().filter_map(|id| tagged_id::encode(*id)).collect();
        if tagged.is_empty() {
            return;
        }
        let Some(ResolveResult { resolved }) = self
            .session
            .call_one(engine_envelope(
                engine,
                INVENTORY_CAP,
                &Resolve { ids: tagged },
            ))
            .await
            .ok()
            .and_then(|reply| ResolveResult::decode_from_bytes(&reply.payload))
        else {
            return;
        };
        let mut cache = self
            .names
            .lock()
            .expect("reverse-name cache mutex is never poisoned");
        if let Some(names) = cache.get_mut(&engine) {
            for entry in resolved {
                if let Ok(id) = tagged_id::decode(&entry.id) {
                    names.cache_resolved(id, entry.name);
                }
            }
        }
    }

    /// Reverse-render every id in a settled trace tree to a real name
    /// (ADR-0088 §8). Builds the engine's reverse map if needed, collects
    /// the ids that the static map misses, resolves them in one batched
    /// `aether.inventory.resolve` query, then renders each `MailNodeWire`
    /// through the now-populated map — falling back to the ADR-0064 hex
    /// tag for any id that resolves to nothing. `Handle` / `Dag` ids stay
    /// hex (they never enter the reverse map).
    async fn render_mail_nodes(
        &self,
        engine: EngineId,
        nodes: Vec<MailNodeWire>,
    ) -> Vec<MailNodeJson> {
        self.ensure_names(engine).await;

        // Collect the mailbox / kind / thread ids that the static map
        // misses, so one batched resolve covers the whole tree.
        let mut misses: Vec<u64> = Vec::new();
        {
            let cache = self
                .names
                .lock()
                .expect("reverse-name cache mutex is never poisoned");
            if let Some(names) = cache.get(&engine) {
                for node in &nodes {
                    for id in node_reversible_ids(node) {
                        if names.needs_resolve(id) {
                            misses.push(id);
                        }
                    }
                }
            }
        }
        misses.sort_unstable();
        misses.dedup();
        self.resolve_dynamic(engine, &misses).await;

        let cache = self
            .names
            .lock()
            .expect("reverse-name cache mutex is never poisoned");
        match cache.get(&engine) {
            Some(names) => nodes
                .into_iter()
                .map(|node| mail_node_to_json(node, Some(names)))
                .collect(),
            // No map for this engine (shouldn't happen post-ensure, but
            // be defensive): render every id as a hex tag.
            None => nodes
                .into_iter()
                .map(|n| mail_node_to_json(n, None))
                .collect(),
        }
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

/// Render a [`SchemaType`] as a one-line human-readable shape string —
/// the compact form `describe_kinds` returns by default. The rendering is
/// intentionally lossy (names only, not discriminants or `repr_c`) and is
/// enough to build `send_mail` params for simple kinds without fetching
/// the full schema. Depth is capped at 6 (`…` past that) per CLAUDE.md's
/// recursion rule; schema depth is structurally bounded by the vocabulary
/// but the cap is cheap insurance against pathological nesting.
fn render_shape(ty: &SchemaType) -> String {
    fn render(ty: &SchemaType, depth: u8) -> String {
        if depth > 6 {
            return "\u{2026}".to_owned();
        }
        match ty {
            SchemaType::Unit => "{}".to_owned(),
            SchemaType::Bool => "bool".to_owned(),
            SchemaType::Scalar(p) => match p {
                Primitive::U8 => "u8",
                Primitive::U16 => "u16",
                Primitive::U32 => "u32",
                Primitive::U64 => "u64",
                Primitive::I8 => "i8",
                Primitive::I16 => "i16",
                Primitive::I32 => "i32",
                Primitive::I64 => "i64",
                Primitive::F32 => "f32",
                Primitive::F64 => "f64",
            }
            .to_owned(),
            SchemaType::String => "String".to_owned(),
            SchemaType::Bytes => "Bytes".to_owned(),
            SchemaType::Option(inner) => format!("Option<{}>", render(inner, depth + 1)),
            SchemaType::Vec(inner) => format!("Vec<{}>", render(inner, depth + 1)),
            SchemaType::Ref(inner) => format!("Ref<{}>", render(inner, depth + 1)),
            SchemaType::Array { element, len } => {
                format!("[{}; {}]", render(element, depth + 1), len)
            }
            SchemaType::Struct { fields, .. } => {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|f| format!("{}: {}", f.name, render(&f.ty, depth + 1)))
                    .collect();
                format!("{{ {} }}", parts.join(", "))
            }
            SchemaType::Enum { variants } => {
                let parts: Vec<String> = variants
                    .iter()
                    .map(|v| match v {
                        EnumVariant::Unit { name, .. } => name.to_string(),
                        EnumVariant::Tuple { name, fields, .. } => {
                            let inner: Vec<String> =
                                fields.iter().map(|f| render(f, depth + 1)).collect();
                            format!("{}({})", name, inner.join(", "))
                        }
                        EnumVariant::Struct { name, fields, .. } => {
                            let inner: Vec<String> = fields
                                .iter()
                                .map(|f| format!("{}: {}", f.name, render(&f.ty, depth + 1)))
                                .collect();
                            format!("{} {{ {} }}", name, inner.join(", "))
                        }
                    })
                    .collect();
                parts.join(" | ")
            }
            SchemaType::Map { key, value } => {
                format!(
                    "Map<{}, {}>",
                    render(key, depth + 1),
                    render(value, depth + 1)
                )
            }
            SchemaType::TypeId(id) => {
                if *id == MailboxId::TYPE_ID {
                    "MailboxId".to_owned()
                } else if *id == KindId::TYPE_ID {
                    "KindId".to_owned()
                } else if *id == HandleId::TYPE_ID {
                    "HandleId".to_owned()
                } else {
                    format!("TypeId({id:#x})")
                }
            }
        }
    }
    render(ty, 0)
}

/// ADR-0098/0099 input hygiene: reject a `recipient_name` whose
/// `/`-rendered scope path exceeds the depth or byte caps before it
/// folds to a `MailboxId`. The MCP `send_mail` surface is the wire
/// boundary for user-controlled names, so the aggregate-key guard lands
/// here; [`aether_data::mailbox_id_from_path`] stays infallible for
/// static callers.
fn validate_recipient_scope(recipient_name: &str) -> anyhow::Result<()> {
    let segments: Vec<&str> = recipient_name.split('/').collect();
    validate_scope_path(&segments).map_err(|e| match e {
        ScopePathError::TooDeep { limit } => {
            anyhow::anyhow!("recipient_name has more than {limit} scope segments")
        }
        ScopePathError::TooLong { limit } => {
            anyhow::anyhow!("recipient_name exceeds the {limit}-byte scope-path cap")
        }
    })
}

/// A component registry selector resolved to its bytes + `@actor` export
/// (ADR-0116) — the front half of `load_component` / `replace_component` /
/// the boot-manifest pre-resolution. `export` is the `module@actor`
/// selector's actor half, threaded into the forwarded `LoadComponent.export`.
struct ResolvedComponent {
    wasm: Vec<u8>,
    export: Option<String>,
}

/// The temp files a `stage_boot_manifest` wrote (ADR-0116): the
/// boot-manifest JSON the hub injects as `AETHER_BOOT_MANIFEST` plus the
/// staged component `.wasm` files it points at. The substrate reads them
/// at boot, before the spawn reply returns; the spawn caller
/// [`cleanup`](StagedBootManifest::cleanup)s them once it has.
struct StagedBootManifest {
    manifest_path: PathBuf,
    wasm_paths: Vec<PathBuf>,
}

impl StagedBootManifest {
    /// Best-effort remove the staged manifest + every staged wasm file.
    /// The substrate has already read them at boot by the time the spawn
    /// reply returns, so a removal failure is harmless.
    async fn cleanup(&self) {
        let _ = fs::remove_file(&self.manifest_path).await;
        for path in &self.wasm_paths {
            let _ = fs::remove_file(path).await;
        }
    }
}

/// Build a `MailEnvelope` addressed at a hub-local mailbox
/// (`engine = None`) carrying a typed kind.
fn local_envelope<K: Kind>(mailbox: &str, kind: &K) -> MailEnvelope {
    MailEnvelope {
        to: MailboxAddress::local(mailbox_id_from_path(mailbox)),
        from: None,
        kind: K::ID,
        correlation_id: None,
        payload: kind.encode_into_bytes(),
    }
}

/// Build a `MailEnvelope` addressed at a mailbox on a specific
/// substrate (`engine = Some`) carrying a typed kind — the hub routes
/// it through to that engine's proxy.
fn engine_envelope<K: Kind>(engine: EngineId, mailbox: &str, kind: &K) -> MailEnvelope {
    engine_envelope_by_id(engine, mailbox_id_from_path(mailbox), kind)
}

/// Like [`engine_envelope`] but addresses the recipient by
/// [`MailboxId`] directly. The trace-tree guided walk (ADR-0086 Phase
/// 3b) discovers recipients as ids embedded in `Sent` events, never as
/// names — a `MailboxId` is a one-way name hash, so there's no name to
/// reconstruct.
fn engine_envelope_by_id<K: Kind>(engine: EngineId, mailbox: MailboxId, kind: &K) -> MailEnvelope {
    MailEnvelope {
        to: MailboxAddress {
            engine: Some(engine),
            mailbox,
        },
        from: None,
        kind: K::ID,
        correlation_id: None,
        payload: kind.encode_into_bytes(),
    }
}

/// Map a `capture_frame` check spec onto a wire [`FrameCheck`],
/// resolving the reduction name. An unknown name is an invalid-params
/// error so a typo aborts the capture cleanly before it reaches the
/// wire (iamacoffeepot/aether#1777).
fn capture_check(spec: &CaptureCheckSpec) -> Result<FrameCheck, McpError> {
    let reduction = match spec.reduction.as_str() {
        "not_all_black" => FrameReduction::NotAllBlack,
        "differs_from_background" => FrameReduction::DiffersFromBackground,
        "coverage" => FrameReduction::Coverage,
        "centroid" => FrameReduction::Centroid,
        "bounding_box" => FrameReduction::BoundingBox,
        other => {
            return Err(McpError::invalid_params(
                format!(
                    "capture_frame check: unknown reduction {other:?}; expected one of \
                     not_all_black, differs_from_background, coverage, centroid, bounding_box"
                ),
                None,
            ));
        }
    };
    Ok(FrameCheck {
        reduction,
        tolerance: spec.tolerance,
        background: spec.background,
    })
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

/// Parse a kind-id string for the `actor_cost` filter: a tagged
/// `knd-…` id (ADR-0064) or a raw decimal `u64`. The raw form is
/// accepted because a cost row's id round-trips back through this
/// filter and a caller may paste a non-tagged synthetic id.
fn parse_kind_id(s: &str) -> Result<KindId, McpError> {
    if let Ok(id) = tagged_id::decode_with_tag(s, Tag::Kind) {
        return Ok(KindId(id));
    }
    s.parse::<u64>().map(KindId).map_err(|_| {
        McpError::invalid_params(
            format!("kind_id: not a tagged `knd-…` id or a decimal u64: {s:?}"),
            None,
        )
    })
}

/// Resolve a `handled_kind` filter token (ADR-0116 `list_components`) to a
/// [`KindId`]: a tagged `knd-…` id or a decimal `u64` resolves directly;
/// otherwise the token is a kind name resolved against the static substrate
/// vocabulary (`describe_kinds`'s source). An unknown name is an
/// invalid-params error.
fn resolve_handled_kind(s: &str) -> Result<KindId, McpError> {
    if let Ok(id) = tagged_id::decode_with_tag(s, Tag::Kind) {
        return Ok(KindId(id));
    }
    if let Ok(id) = s.parse::<u64>() {
        return Ok(KindId(id));
    }
    descriptors::all()
        .into_iter()
        .find(|d| d.name == s)
        .map(|d| KindId(kind_id_from_parts(&d.name, &d.schema)))
        .ok_or_else(|| {
            McpError::invalid_params(
                format!("handled_kind: not a tagged `knd-…` id, a decimal u64, or a known kind name: {s:?}"),
                None,
            )
        })
}

/// Best-effort resolve a [`KindId`] to its name from the static kind
/// inventory the MCP harness ships with (`describe_kinds`'s source).
/// Component-defined kinds aren't in the inventory and return `None`.
/// Cold path — recomputes the inventory's ids on each call; the cost
/// dump is a diagnostic, not a hot loop.
fn static_kind_name(id: KindId) -> Option<String> {
    descriptors::all()
        .into_iter()
        .find(|d| kind_id_from_parts(&d.name, &d.schema) == id.0)
        .map(|d| d.name)
}

/// Transcode the correlated reply envelopes a `call_collecting` returned
/// into the MCP wire shape (issue 1242). Per envelope: the tagged kind
/// id, the best-effort kind name, and the best-effort `decode_schema` of
/// the payload against the matching descriptor (`None` on an unknown kind
/// or a decode miss). Decode priority (issue 1804): per-engine kind cache
/// (`engine_kinds`, includes component-defined kinds populated from
/// `ListKinds`) keyed by `declared_reply` (the handler's
/// `HandlerCapability.reply` from ADR-0109) when it matches the envelope
/// kind, then a general engine-cache scan for traced batches where the
/// per-reply handler isn't known, then the static substrate vocabulary,
/// then base64. On a clean decode the raw bytes are omitted (issue 1246).
/// Order is preserved — arrival order.
fn decode_reply_events(
    envelopes: &[MailEnvelope],
    engine_kinds: &HashMap<String, KindDescriptor>,
    declared_reply: Option<KindId>,
) -> Vec<ReplyEventJson> {
    let static_descriptors = descriptors::all();
    envelopes
        .iter()
        .map(|env| {
            // Resolve the schema for this reply envelope. Tier 1: engine
            // cache targeted by the declared reply kind — this is the path
            // that decodes component-defined reply kinds to `params` rather
            // than base64. Tier 2: general engine-cache scan — covers
            // send_mail_traced batches where `declared_reply` is None and
            // any handler in the batch may have replied. Tier 3: static
            // substrate vocabulary — the fallback for native chassis cap
            // replies not yet in the engine cache. Tier 4: base64.
            let descriptor: Option<KindDescriptor> = declared_reply
                .filter(|&dr| dr == env.kind)
                .and_then(|dr| {
                    engine_kinds
                        .values()
                        .find(|d| kind_id_from_parts(&d.name, &d.schema) == dr.0)
                        .cloned()
                })
                .or_else(|| {
                    engine_kinds
                        .values()
                        .find(|d| kind_id_from_parts(&d.name, &d.schema) == env.kind.0)
                        .cloned()
                })
                .or_else(|| {
                    static_descriptors
                        .iter()
                        .find(|d| kind_id_from_parts(&d.name, &d.schema) == env.kind.0)
                        .cloned()
                });
            let kind_name = descriptor.as_ref().map(|d| d.name.clone());
            let (params, payload_bytes) = descriptor
                .as_ref()
                .and_then(|d| {
                    // Render reply `Bytes` fields back to readable text /
                    // base64 (issue 1944): the strict decoder emits a byte
                    // array, the MCP front projects it for the caller.
                    aether_codec::decode_schema(&env.payload, &d.schema)
                        .ok()
                        .map(|v| render_bytes_reply(v, &d.schema))
                })
                .map_or_else(
                    // Decode miss: base64 the raw payload as the fallback
                    // (the only signal when `params` is `null`).
                    || (None, Some(STANDARD.encode(&env.payload))),
                    // Clean decode: `params` is the surfacing; omit the
                    // raw bytes so they aren't duplicated as an int-array.
                    |v| (Some(v), None),
                );
            ReplyEventJson {
                // Render the kind id as the ADR-0064 tagged string the
                // rest of the MCP wire uses, falling back to a hex
                // literal on an unencodable (non-kind-domain) id.
                kind_id: tagged_id::encode(env.kind.0)
                    .unwrap_or_else(|| format!("{:#x}", env.kind.0)),
                kind_name,
                params,
                payload_bytes,
            }
        })
        .collect()
}

/// Decode the `DispatchTracedAck` from a `send_mail_traced` ack call's
/// collected events (issue 1242). The synchronous ack is the *first*
/// reply event the trace cap emits on the dispatch cid; later events are
/// downstream cap replies handled separately. An absent or undecodable
/// ack, or an `Err` ack, is a tool error.
fn decode_traced_ack(events: &[MailEnvelope]) -> Result<MailId, McpError> {
    let ack_env = events
        .first()
        .ok_or_else(|| internal_msg("send_mail_traced: no ack reply from the trace cap"))?;
    let ack = DispatchTracedAck::decode_from_bytes(&ack_env.payload)
        .ok_or_else(|| internal_msg("undecodable DispatchTracedAck"))?;
    match ack {
        DispatchTracedAck::Ok { root } => Ok(root),
        DispatchTracedAck::Err { error } => Err(internal_msg(&format!(
            "send_mail_traced dispatch failed: {error}"
        ))),
    }
}

/// The collected `send_mail_traced` events minus the leading ack (the
/// `DispatchTracedAck` [`decode_traced_ack`] consumes), leaving the flat
/// list of downstream cap replies to surface as `replies` (issue 1242).
fn strip_ack(events: &[MailEnvelope]) -> &[MailEnvelope] {
    events.get(1..).unwrap_or(&[])
}

/// Build the wire [`DagDescriptor`] from the typed tool arg, reading each
/// `Source`'s `payload_path` into the wire `payload` bytes. The wire kind
/// never learns about filesystem paths — the path is a tool-layer
/// convenience resolved here; `payload_path` takes precedence over an
/// inline `payload`. The tagged-string ids were already parsed into their
/// typed `MailboxId` / `KindId` / `TransformId` form during arg
/// deserialization, so this is a straight move + a file read.
async fn build_descriptor(arg: DagDescriptorArg) -> anyhow::Result<DagDescriptor> {
    let mut nodes = Vec::with_capacity(arg.nodes.len());
    for node in arg.nodes {
        let wire = match node {
            NodeArg::Source {
                id,
                mailbox,
                kind_id,
                payload_path,
                payload,
            } => {
                let payload = match payload_path {
                    Some(path) => fs::read(&path)
                        .await
                        .map_err(|e| anyhow::anyhow!("reading payload_path {path:?}: {e}"))?,
                    None => payload.unwrap_or_default(),
                };
                Node::Source {
                    id: NodeId(id),
                    mailbox,
                    kind_id,
                    payload,
                }
            }
            NodeArg::Transform {
                id,
                transform_id,
                output_kind_id,
                timeout_ms,
            } => Node::Transform {
                id: NodeId(id),
                transform_id,
                output_kind_id,
                timeout_ms,
            },
            NodeArg::Call {
                id,
                recipient,
                kind_id,
            } => Node::Call {
                id: NodeId(id),
                recipient,
                kind_id,
            },
            NodeArg::Observer {
                id,
                recipient,
                kind_id,
            } => Node::Observer {
                id: NodeId(id),
                recipient,
                kind_id,
            },
        };
        nodes.push(wire);
    }
    let edges = arg
        .edges
        .into_iter()
        .map(|e| Edge {
            from: NodeId(e.from),
            to: NodeId(e.to),
            slot: e.slot,
        })
        .collect();
    Ok(DagDescriptor {
        version: arg.version,
        nodes,
        edges,
    })
}

impl Mcp {
    /// Encode a `send_mail_traced` batch into the same `MailEnvelope`
    /// shape `CaptureFrame` carries: name-level addressing + schema-
    /// encoded payload. The substrate's `TraceObserver` resolves the
    /// names through its registry at dispatch time. Same lookup path
    /// `encode_capture_bundle` uses (per-engine merged view, ADR-0091),
    /// just over `TracedMailSpec` instead of `CaptureMailSpec`.
    async fn encode_traced_bundle(
        &self,
        engine: EngineId,
        specs: &[TracedMailSpec],
    ) -> anyhow::Result<Vec<KindMailEnvelope>> {
        let mut out = Vec::with_capacity(specs.len());
        for spec in specs {
            let desc = self.lookup_descriptor(engine, &spec.kind_name).await?;
            let params = spec.params.clone().unwrap_or(serde_json::Value::Null);
            let params = resolve_bytes_params(params, &desc.schema, max_frame_size())
                .await
                .map_err(|e| {
                    anyhow::anyhow!("resolving blob params for {}: {e}", spec.kind_name)
                })?;
            let payload = aether_codec::encode_schema(&params, &desc.schema)
                .map_err(|e| anyhow::anyhow!("param encode failed for {}: {e}", spec.kind_name))?;
            out.push(KindMailEnvelope {
                recipient_name: spec.recipient_name.clone(),
                kind_name: spec.kind_name.clone(),
                payload,
                count: 1,
            });
        }
        Ok(out)
    }
}

/// Render a raw `u64` mailbox / kind / thread id to its display string
/// (ADR-0088 §8): the engine's real name when `names` resolves it, else
/// the ADR-0064 tagged-id string (`mbx-…` / `knd-…` / `thr-…`), else a
/// hex literal if the tag bits are unencodable. `names == None` (no
/// reverse map for the engine) renders the tag directly — the unchanged
/// pre-inventory output.
fn render_id(id: u64, names: Option<&EngineNames>) -> String {
    names.map_or_else(
        || tagged_id::encode(id).unwrap_or_else(|| format!("{id:#x}")),
        |names| names.render(id),
    )
}

/// Reverse-render a [`MailboxId`] through the engine's name map (or the
/// hex tag on a miss / no map). Chassis-minted ids always carry tag bits,
/// so the hex fallback never reaches the `{:#x}` arm in practice.
fn mailbox_id_to_tagged(id: MailboxId, names: Option<&EngineNames>) -> String {
    render_id(id.0, names)
}

fn kind_id_to_tagged(id: KindId, names: Option<&EngineNames>) -> String {
    render_id(id.0, names)
}

fn mail_id_to_json(id: MailId, names: Option<&EngineNames>) -> MailIdJson {
    MailIdJson {
        sender: mailbox_id_to_tagged(id.sender, names),
        correlation_id: id.correlation_id,
    }
}

fn mail_node_to_json(node: MailNodeWire, names: Option<&EngineNames>) -> MailNodeJson {
    MailNodeJson {
        mail_id: mail_id_to_json(node.mail_id, names),
        parent: node.parent.map(|p| mail_id_to_json(p, names)),
        sender: mailbox_id_to_tagged(node.sender, names),
        recipient: mailbox_id_to_tagged(node.recipient, names),
        kind: kind_id_to_tagged(node.kind, names),
        t_construct_start: node.t_construct_start.0,
        t_sent: node.t_sent.0,
        t_received: node.t_received.map(|n| n.0),
        t_finished: node.t_finished.map(|n| n.0),
        thread_name: node.thread_name,
    }
}

/// The mailbox / kind / thread ids in one `MailNodeWire` that reverse
/// through the inventory (ADR-0088 §8): the two mailbox endpoints, the
/// kind, and both `MailId` senders. `correlation_id` is a `Uuid`, not a
/// tagged id, so it's excluded. Thread ids ride in `thread_name` already
/// resolved substrate-side, so they aren't re-resolved here.
fn node_reversible_ids(node: &MailNodeWire) -> Vec<u64> {
    let mut ids = vec![
        node.sender.0,
        node.recipient.0,
        node.kind.0,
        node.mail_id.sender.0,
    ];
    if let Some(parent) = &node.parent {
        ids.push(parent.sender.0);
    }
    ids
}

impl Mcp {
    /// Encode a `capture_frame` mail bundle: resolve each spec's kind
    /// against the per-engine merged view (ADR-0091, static prefill +
    /// cached `ListKinds` reply), schema-encode its params, and wrap
    /// into the substrate-side `aether_kinds::MailEnvelope` shape
    /// (name-level addressing + pre-encoded payload).
    async fn encode_capture_bundle(
        &self,
        engine: EngineId,
        specs: &[CaptureMailSpec],
    ) -> anyhow::Result<Vec<aether_kinds::MailEnvelope>> {
        let mut out = Vec::with_capacity(specs.len());
        for spec in specs {
            let desc = self.lookup_descriptor(engine, &spec.kind_name).await?;
            let params = spec.params.clone().unwrap_or(serde_json::Value::Null);
            let params = resolve_bytes_params(params, &desc.schema, max_frame_size())
                .await
                .map_err(|e| {
                    anyhow::anyhow!("resolving blob params for {}: {e}", spec.kind_name)
                })?;
            let payload = aether_codec::encode_schema(&params, &desc.schema)
                .map_err(|e| anyhow::anyhow!("param encode failed for {}: {e}", spec.kind_name))?;
            out.push(aether_kinds::MailEnvelope {
                recipient_name: spec.recipient_name.clone(),
                kind_name: spec.kind_name.clone(),
                payload,
                count: 1,
            });
        }
        Ok(out)
    }
}

/// Serialize a tool result to the JSON string `rmcp` wraps as text
/// content.
fn json<T: serde::Serialize>(value: &T) -> Result<String, McpError> {
    serde_json::to_string(value).map_err(|e| McpError::internal_error(e.to_string(), None))
}

/// Flatten a wire [`DeathReason`] into the `(reason, detail)` pair the
/// `list_engines` tool renders: a short tag plus the variant's detail
/// string (empty for the clean `Terminated` case). Flat over a tagged
/// JSON enum so an LLM consumer reads the cause without a nested match.
fn death_reason_parts(reason: DeathReason) -> (String, String) {
    match reason {
        DeathReason::Terminated => ("terminated".to_owned(), String::new()),
        DeathReason::Crashed { detail } => ("crashed".to_owned(), detail),
        DeathReason::Evicted { detail } => ("evicted".to_owned(), detail),
    }
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

/// iamacoffeepot/aether#1271: tools that ship potentially-large
/// payloads through the RPC framing (currently `load_component` /
/// `replace_component`) surface a `FrameTooLarge` / `EncodeTooLarge`
/// failure as `invalid_params` rather than `internal_error`. The
/// payload is a client-controllable input (the user picked the wasm
/// path), and the actionable remediation — build the release wasm,
/// raise `AETHER_MAX_FRAME_SIZE` — is specific to the caller. Falls
/// through to `internal` for every other shape.
///
/// Detection is by substring of the error chain because the structured
/// `RpcError` rides under `anyhow::Error` (the session's `call_once`
/// formats the wire error with `{e:?}` into a string; the encode-side
/// classifier formats `RpcClientError::Frame(...)` with `{e}`). Both
/// shapes embed the literal `frame too large` / `encoded frame too
/// large` strings the codec / RPC error variants produce.
#[allow(clippy::needless_pass_by_value)]
fn frame_size_aware_error(context: &str, e: anyhow::Error) -> McpError {
    let text = e.to_string();
    if text.contains("frame too large")
        || text.contains("encoded frame too large")
        || text.contains("FrameTooLarge")
        || text.contains("EncodeTooLarge")
    {
        return McpError::invalid_params(
            format!(
                "{context}: payload exceeds the RPC framing cap — typically because the supplied \
                 wasm is a debug build. Build the release wasm (target/wasm32-unknown-unknown/\
                 release/*.wasm) or raise the cap via the AETHER_MAX_FRAME_SIZE env var. \
                 Underlying: {text}",
            ),
            None,
        );
    }
    McpError::internal_error(text, None)
}

/// Blob-embed preprocessor (issue 1944). The wire codec is strict and
/// canonical — a `SchemaType::Bytes` param encodes only from a JSON byte
/// array — so the consumer-facing ergonomics live here, in the MCP front
/// that already owns the JSON params before schema-encoding them. Walk
/// `value` alongside `schema` and, at every `Bytes` node, resolve a
/// `$`-sigil embed object into the canonical byte array `encode_schema`
/// accepts. A literal `[…]` array passes straight through (back-compat);
/// a one-key embed object expands — `{"$file": path}` reads the file on
/// the harness host and inlines its bytes, `{"$base64": s}` decodes,
/// `{"$text": s}` UTF-8-encodes. Any other shape at a `Bytes` node, or an
/// unknown `$`-tag, errors. Recursion depth is bounded by the
/// compile-time kind schema, not by user-controlled runtime data, so it
/// mirrors `encode_schema`'s own recursive walk; only the arms that can
/// carry a `Bytes` leaf (`Struct` / `Option` / `Vec` / `Array` / `Map`)
/// descend, every other value passes through untouched. `max_file_bytes`
/// is the RPC frame cap a `{"$file"}` read is guarded against; the
/// production call sites pass `max_frame_size()`.
fn resolve_bytes_params<'a>(
    value: serde_json::Value,
    schema: &'a SchemaType,
    max_file_bytes: usize,
) -> Pin<Box<dyn Future<Output = anyhow::Result<serde_json::Value>> + Send + 'a>> {
    use serde_json::Value;
    Box::pin(async move {
        match schema {
            SchemaType::Bytes => resolve_bytes_embed(value, max_file_bytes).await,
            SchemaType::Option(inner) => {
                if value.is_null() {
                    Ok(value)
                } else {
                    resolve_bytes_params(value, inner, max_file_bytes).await
                }
            }
            SchemaType::Vec(inner) => match value {
                Value::Array(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        out.push(resolve_bytes_params(item, inner, max_file_bytes).await?);
                    }
                    Ok(Value::Array(out))
                }
                other => Ok(other),
            },
            SchemaType::Array { element, .. } => match value {
                Value::Array(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        out.push(resolve_bytes_params(item, element, max_file_bytes).await?);
                    }
                    Ok(Value::Array(out))
                }
                other => Ok(other),
            },
            SchemaType::Struct { fields, .. } => match value {
                Value::Object(mut map) => {
                    for field in fields.iter() {
                        if let Some(slot) = map.remove(&*field.name) {
                            let resolved =
                                resolve_bytes_params(slot, &field.ty, max_file_bytes).await?;
                            map.insert(field.name.to_string(), resolved);
                        }
                    }
                    Ok(Value::Object(map))
                }
                other => Ok(other),
            },
            SchemaType::Map {
                value: value_schema,
                ..
            } => match value {
                Value::Object(mut map) => {
                    let keys: Vec<String> = map.keys().cloned().collect();
                    for key in keys {
                        if let Some(slot) = map.remove(&key) {
                            let resolved =
                                resolve_bytes_params(slot, value_schema, max_file_bytes).await?;
                            map.insert(key, resolved);
                        }
                    }
                    Ok(Value::Object(map))
                }
                other => Ok(other),
            },
            // Scalars, String, Enum, Ref, TypeId, Unit, Bool: no `Bytes`
            // leaf is reachable through the embed grammar, so pass through.
            _ => Ok(value),
        }
    })
}

/// Resolve a single `Bytes`-node value into the canonical JSON byte
/// array (issue 1944). A literal `[…]` array passes through; a one-key
/// `$`-sigil object expands into bytes; anything else errors. `{"$file"}`
/// is guarded above `max_file_bytes` (the RPC frame cap) so a blob too
/// large to ride in mail errors with a pointer to the staged-path
/// mechanism rather than being silently inlined.
async fn resolve_bytes_embed(
    value: serde_json::Value,
    max_file_bytes: usize,
) -> anyhow::Result<serde_json::Value> {
    use serde_json::Value;
    let obj = match value {
        // Canonical form — already a byte array. Back-compat passthrough.
        Value::Array(_) => return Ok(value),
        Value::Object(map) => map,
        _ => anyhow::bail!(
            "a Bytes field accepts a byte array or a single $-sigil embed \
             ($file / $base64 / $text)"
        ),
    };
    if obj.len() != 1 {
        anyhow::bail!(
            "a Bytes embed object must have exactly one $-sigil key \
             ($file / $base64 / $text)"
        );
    }
    let (key, body) = obj.into_iter().next().expect("len == 1");
    let bytes: Vec<u8> = match key.as_str() {
        "$file" => {
            let path = body
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("$file value must be a string path"))?;
            let bytes = fs::read(path)
                .await
                .map_err(|e| anyhow::anyhow!("$file: reading {path:?}: {e}"))?;
            if bytes.len() > max_file_bytes {
                anyhow::bail!(
                    "$file {path:?} is {} bytes, over the {max_file_bytes}-byte RPC frame cap; a \
                     blob this large must stage as a hub-read path (ADR-0115/0116), not inline \
                     into mail",
                    bytes.len()
                );
            }
            bytes
        }
        "$base64" => {
            let s = body
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("$base64 value must be a string"))?;
            STANDARD
                .decode(s)
                .map_err(|e| anyhow::anyhow!("$base64: invalid base64: {e}"))?
        }
        "$text" => {
            let s = body
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("$text value must be a string"))?;
            s.as_bytes().to_vec()
        }
        other => anyhow::bail!(
            "a Bytes field accepts a byte array or a single $-sigil embed; got {other:?} \
             (expected $file / $base64 / $text)"
        ),
    };
    Ok(Value::Array(bytes.into_iter().map(Value::from).collect()))
}

/// Reply-side mirror of [`resolve_bytes_params`] (issue 1944). The strict
/// decoder emits a `Bytes` field as a JSON byte array; render it back to
/// a bare string when the bytes are valid UTF-8 (the read-back-as-text
/// ergonomic), else to `{"base64": …}`. Walks `schema` to reach a `Bytes`
/// leaf nested in a composite, every other value untouched.
fn render_bytes_reply(value: serde_json::Value, schema: &SchemaType) -> serde_json::Value {
    use serde_json::Value;
    match schema {
        SchemaType::Bytes => render_bytes_leaf(value),
        SchemaType::Option(inner) => {
            if value.is_null() {
                value
            } else {
                render_bytes_reply(value, inner)
            }
        }
        SchemaType::Vec(inner) => match value {
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .map(|v| render_bytes_reply(v, inner))
                    .collect(),
            ),
            other => other,
        },
        SchemaType::Array { element, .. } => match value {
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .map(|v| render_bytes_reply(v, element))
                    .collect(),
            ),
            other => other,
        },
        SchemaType::Struct { fields, .. } => match value {
            Value::Object(mut map) => {
                for field in fields.iter() {
                    if let Some(slot) = map.get_mut(&*field.name) {
                        let taken = mem::take(slot);
                        *slot = render_bytes_reply(taken, &field.ty);
                    }
                }
                Value::Object(map)
            }
            other => other,
        },
        SchemaType::Map {
            value: value_schema,
            ..
        } => match value {
            Value::Object(mut map) => {
                for slot in map.values_mut() {
                    let taken = mem::take(slot);
                    *slot = render_bytes_reply(taken, value_schema);
                }
                Value::Object(map)
            }
            other => other,
        },
        _ => value,
    }
}

/// Render one decoded `Bytes` value — a JSON array of byte numbers — to a
/// bare string (valid UTF-8) or a `{"base64": …}` object (binary). A
/// value that isn't the array-of-bytes shape is returned untouched.
fn render_bytes_leaf(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    let Value::Array(items) = &value else {
        return value;
    };
    let mut bytes = Vec::with_capacity(items.len());
    for item in items {
        match item.as_u64().and_then(|n| u8::try_from(n).ok()) {
            Some(b) => bytes.push(b),
            None => return value,
        }
    }
    str::from_utf8(&bytes).map_or_else(
        |_| {
            let mut obj = serde_json::Map::new();
            obj.insert("base64".to_owned(), Value::String(STANDARD.encode(&bytes)));
            Value::Object(obj)
        },
        |s| Value::String(s.to_owned()),
    )
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
// Test fixtures derive taggable mailbox ids by name to exercise the
// tagged-string wire round-trip — reference id derivation, not sibling-cap
// addressing.
#[allow(clippy::disallowed_methods)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::args::{
        CaptureFrameArgs, CaptureMailSpec, ComponentSpec, DescribeComponentArgs, DescribeKindsArgs,
        LoadComponentArgs, MailSpec, ReplaceComponentArgs, SendMailArgs, SendMailTracedArgs,
        SpawnSubstrateArgs, TerminateSubstrateArgs, TracedMailSpec,
    };
    use aether_capabilities::rpc::{
        PeerKind, RpcServerCapability, RpcServerConfig, RpcServerHandle,
    };
    use aether_capabilities::trace::TraceDispatchCapability;
    use aether_capabilities::{EngineConfig, EngineServer};
    use aether_substrate::chassis::builder::{Builder, PassiveChassis};
    use aether_substrate::handle_store::HandleStore;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::Registry;

    use crate::args::ActorLogsArgs;
    use crate::args::DescribeHandlesArgs;
    use crate::test_chassis::TestChassis;
    use aether_kinds::descriptors;

    #[test]
    fn recipient_scope_normal_name_passes() {
        // A `/`-rendered hosted-actor name is within both caps.
        validate_recipient_scope("aether.component/aether.embedded:camera")
            .expect("a two-segment hosted-actor name is under the scope caps");
    }

    #[test]
    fn recipient_scope_over_depth_rejected() {
        // One segment past `MAX_SCOPE_PATH_DEPTH`.
        let name = (0..=aether_data::MAX_SCOPE_PATH_DEPTH)
            .map(|i| format!("seg{i}"))
            .collect::<Vec<_>>()
            .join("/");
        assert!(validate_recipient_scope(&name).is_err());
    }

    #[test]
    fn recipient_scope_over_bytes_rejected() {
        // A single segment longer than the byte cap (depth stays 1).
        let name = "a".repeat(aether_data::MAX_SCOPE_PATH_BYTES + 1);
        assert!(validate_recipient_scope(&name).is_err());
    }

    /// A single huge cap so the embed tests aren't tripping the oversize
    /// guard — the oversize test passes a deliberately tiny cap instead.
    const NO_CAP: usize = usize::MAX;

    /// One-field `{ blob: Bytes }` struct schema for the nested-Bytes
    /// embed / render tests.
    fn blob_struct_schema() -> SchemaType {
        use aether_data::NamedField;
        SchemaType::Struct {
            fields: vec![NamedField {
                name: "blob".into(),
                ty: SchemaType::Bytes,
            }]
            .into(),
            repr_c: false,
        }
    }

    /// Write `bytes` to a unique temp file for the `$file` embed tests.
    /// The `std_env` / `std_fs` aliases avoid shadowing the module's
    /// `tokio::fs`; same pattern as `stage_temp_file`.
    fn stage_blob_file(tag: &str, bytes: &[u8]) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std_env::temp_dir().join(format!(
            "aether-mcp-blob-{tag}-{}-{nanos}.bin",
            process::id()
        ));
        std_fs::write(&path, bytes).expect("stage blob temp file");
        path
    }

    #[tokio::test]
    async fn resolve_bytes_text_embed() {
        let out = resolve_bytes_params(
            serde_json::json!({"$text": "hi"}),
            &SchemaType::Bytes,
            NO_CAP,
        )
        .await
        .expect("$text resolves");
        assert_eq!(out, serde_json::json!([104, 105]));
    }

    #[tokio::test]
    async fn resolve_bytes_base64_embed() {
        // "aGk=" is base64 for "hi".
        let out = resolve_bytes_params(
            serde_json::json!({"$base64": "aGk="}),
            &SchemaType::Bytes,
            NO_CAP,
        )
        .await
        .expect("$base64 resolves");
        assert_eq!(out, serde_json::json!([104, 105]));
    }

    #[tokio::test]
    async fn resolve_bytes_array_passthrough() {
        // A literal byte array is the canonical form and passes straight
        // through untouched.
        let out = resolve_bytes_params(serde_json::json!([1, 2, 3]), &SchemaType::Bytes, NO_CAP)
            .await
            .expect("array passthrough");
        assert_eq!(out, serde_json::json!([1, 2, 3]));
    }

    #[tokio::test]
    async fn resolve_bytes_file_embed() {
        let path = stage_blob_file("read", b"hi");
        let out = resolve_bytes_params(
            serde_json::json!({"$file": path.to_str().expect("utf-8 temp path")}),
            &SchemaType::Bytes,
            NO_CAP,
        )
        .await
        .expect("$file resolves");
        assert_eq!(out, serde_json::json!([104, 105]));
        std_fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn resolve_bytes_file_oversize_errors() {
        // A 32-byte file against a 16-byte cap trips the oversize guard.
        let path = stage_blob_file("oversize", &[0u8; 32]);
        let err = resolve_bytes_params(
            serde_json::json!({"$file": path.to_str().expect("utf-8 temp path")}),
            &SchemaType::Bytes,
            16,
        )
        .await
        .expect_err("oversize $file must error");
        assert!(err.to_string().contains("over the"), "got: {err}");
        std_fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn resolve_bytes_unknown_sigil_tag_errors() {
        let err = resolve_bytes_params(
            serde_json::json!({"$weird": "x"}),
            &SchemaType::Bytes,
            NO_CAP,
        )
        .await
        .expect_err("unknown $-tag must error");
        let _ = err;
    }

    #[tokio::test]
    async fn resolve_bytes_non_sigil_object_errors() {
        // A single-key object whose key carries no `$` sigil is data, not a
        // directive — it errors at the Bytes node.
        let err =
            resolve_bytes_params(serde_json::json!({"file": "x"}), &SchemaType::Bytes, NO_CAP)
                .await
                .expect_err("non-$ object must error");
        let _ = err;
    }

    #[tokio::test]
    async fn resolve_bytes_nested_in_struct() {
        let out = resolve_bytes_params(
            serde_json::json!({"blob": {"$text": "hi"}}),
            &blob_struct_schema(),
            NO_CAP,
        )
        .await
        .expect("nested Bytes resolves");
        assert_eq!(out, serde_json::json!({"blob": [104, 105]}));
    }

    #[test]
    fn render_bytes_reply_utf8_to_string() {
        let out = render_bytes_reply(serde_json::json!([104, 105]), &SchemaType::Bytes);
        assert_eq!(out, serde_json::json!("hi"));
    }

    #[test]
    fn render_bytes_reply_binary_to_base64() {
        // 0xff 0xfe is not valid UTF-8 → base64 object.
        let out = render_bytes_reply(serde_json::json!([255, 254]), &SchemaType::Bytes);
        assert_eq!(out, serde_json::json!({"base64": "//4="}));
    }

    #[test]
    fn render_bytes_reply_nested_in_struct() {
        let out = render_bytes_reply(
            serde_json::json!({"blob": [104, 105]}),
            &blob_struct_schema(),
        );
        assert_eq!(out, serde_json::json!({"blob": "hi"}));
    }

    #[test]
    fn certifier_transforms_linked_into_mcp_inventory() {
        // `describe_transforms` reads the LOCAL `aether_data::transforms()`
        // inventory baked into the mcp binary at link time — not the
        // engine's. The reachability certifier transforms reach it only
        // because aether-mcp declares a dependency edge on aether-labyrinth
        // that no mcp source references (issue 1908). The `inventory` crate
        // drops a fully-unreferenced dependency's submissions, so this
        // guards the edge: drop the dep and describe_transforms silently
        // stops listing the reachability transforms — no compile error.
        assert!(
            aether_data::transforms().any(|t| t.name.ends_with("::solve")),
            "the aether-labyrinth `solve` transform must be in the mcp link-time \
             inventory; a dropped dependency edge silently de-registers it",
        );
    }

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
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<EngineServer>(EngineConfig::default())
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
    /// hub chassis, with fresh component, reverse-name, and kind-encode
    /// caches.
    fn connect_mcp(port: u16) -> Mcp {
        let session = RpcSession::connect(&format!("127.0.0.1:{port}")).expect("session connects");
        Mcp::new(
            Arc::new(session),
            Arc::new(ComponentCache::default()),
            Arc::new(ReverseNameCache::default()),
            Arc::new(KindsCache::default()),
        )
    }

    /// Hub-shape chassis with `InventoryCapability` installed and a
    /// caller-supplied descriptor registered against the bench's
    /// `Registry` — emulating the post-`load_component` state where
    /// a component's own kind is in the substrate's vocab but not in
    /// `descriptors::all()`. Used by ADR-0091's end-to-end check that
    /// the MCP encode path picks the registered kind up via
    /// `aether.inventory.kinds`.
    fn boot_hub_with_inventory(extras: &[KindDescriptor]) -> (PassiveChassis<TestChassis>, u16) {
        use aether_capabilities::InventoryCapability;

        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        for d in extras {
            // Component-defined kinds enter the substrate's `Registry`
            // via `ComponentHostCapability::handle_load` →
            // `register_or_match_all`; here we shortcut that with a
            // direct register so the test doesn't need a real wasm
            // load lifecycle (the ADR-0091 surface under test is the
            // *projection*, not the loader).
            let _ = registry.register_kind_with_descriptor(d.clone());
        }
        let (outbound, _rx) = HubOutbound::attached_loopback();
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store).with_outbound(outbound));
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<EngineServer>(EngineConfig::default())
            // The inventory cap pulls `Arc::clone(ctx.mailer().registry())`
            // in `init`, so it sees the same `Registry` we just wrote
            // the extra kinds into.
            .with_actor::<InventoryCapability>(())
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

    /// `list_engines` over the RPC round-trip yields an object with empty
    /// `engines` / `recently_died` arrays on a fresh hub — proves the
    /// whole `RpcSession` demux + the `engine = None` Call path against
    /// the real `aether.engine` cap, and the issue-1906 output shape.
    #[tokio::test]
    async fn list_engines_on_empty_hub_is_empty() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let out = mcp.list_engines().await.expect("list_engines ok");
        assert_eq!(
            out, "{\"engines\":[],\"recently_died\":[]}",
            "fresh hub supervises no engines and has no recent deaths",
        );
    }

    /// `spawn_substrate` with a selector that resolves to no stored binary
    /// surfaces the hub's `SpawnEngineResult::Err` as a tool error (the
    /// store is empty on a fresh hub).
    #[tokio::test]
    async fn spawn_substrate_missing_binary_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .spawn_substrate(Parameters(SpawnSubstrateArgs {
                selector: Some("nonexistent-hash-or-name".to_owned()),
                chassis: None,
                caps: vec![],
                target: None,
                args: vec![],
                components: vec![],
            }))
            .await;
        assert!(
            result.is_err(),
            "an unresolvable selector should be a tool error"
        );
    }

    /// A `spawn_substrate` boot list whose component selector resolves to
    /// no stored component fails the spawn as a tool error before any fork
    /// (ADR-0116): aether-mcp pre-resolves each selector via
    /// `ResolveComponent`, and a miss aborts the staging. The store is
    /// empty on a fresh hub, so any selector is a miss.
    #[tokio::test]
    async fn spawn_substrate_unresolvable_component_selector_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .spawn_substrate(Parameters(SpawnSubstrateArgs {
                selector: None,
                chassis: None,
                caps: vec![],
                target: None,
                args: vec![],
                components: vec![ComponentSpec {
                    selector: "no-such-component".to_owned(),
                    name: None,
                    config_path: None,
                    export: None,
                }],
            }))
            .await;
        assert!(
            result.is_err(),
            "an unresolvable component selector should abort the spawn as a tool error",
        );
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
                fire_and_forget: false,
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
    /// default (compact) result is a non-empty JSON array of `{name,shape}`
    /// objects.
    #[tokio::test]
    async fn describe_kinds_returns_the_substrate_inventory() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let out = mcp
            .describe_kinds(Parameters(DescribeKindsArgs {
                prefix: None,
                full: false,
            }))
            .await
            .expect("describe_kinds ok");
        let kinds: serde_json::Value = serde_json::from_str(&out).expect("json array");
        let arr = kinds.as_array().expect("result is a JSON array");
        assert!(
            !arr.is_empty(),
            "describe_kinds should list the substrate vocabulary"
        );
        let first = &arr[0];
        assert!(
            first.get("name").is_some() && first.get("shape").is_some(),
            "compact entry must carry name and shape, got: {first}",
        );
        assert!(
            first.get("schema").is_none(),
            "compact entry must not carry schema, got: {first}",
        );
    }

    /// `describe_kinds(prefix="aether.fs")` narrows the array to only the
    /// fs kinds — every returned name starts with the prefix.
    #[tokio::test]
    async fn describe_kinds_prefix_narrows_results() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let out = mcp
            .describe_kinds(Parameters(DescribeKindsArgs {
                prefix: Some("aether.fs".to_owned()),
                full: false,
            }))
            .await
            .expect("describe_kinds ok");
        let arr: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
        assert!(
            !arr.is_empty(),
            "aether.fs prefix should match at least one kind"
        );
        for entry in &arr {
            let name = entry["name"].as_str().expect("name is a string");
            assert!(
                name.starts_with("aether.fs"),
                "entry name {name:?} should start with \"aether.fs\"",
            );
        }
    }

    /// `describe_kinds(full=true)` returns objects with a `schema` key
    /// (the full nested `SchemaType`) and no `shape` key.
    #[tokio::test]
    async fn describe_kinds_full_returns_schema_key() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let out = mcp
            .describe_kinds(Parameters(DescribeKindsArgs {
                prefix: Some("aether.fs".to_owned()),
                full: true,
            }))
            .await
            .expect("describe_kinds ok");
        let arr: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
        assert!(
            !arr.is_empty(),
            "aether.fs prefix should match at least one kind"
        );
        for entry in &arr {
            assert!(
                entry.get("schema").is_some(),
                "full entry must carry schema, got: {entry}",
            );
            assert!(
                entry.get("shape").is_none(),
                "full entry must not carry shape, got: {entry}",
            );
        }
    }

    /// `describe_kinds(prefix="zzz.does.not.exist")` returns an empty
    /// array — not an error.
    #[tokio::test]
    async fn describe_kinds_nonmatching_prefix_returns_empty() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let out = mcp
            .describe_kinds(Parameters(DescribeKindsArgs {
                prefix: Some("zzz.does.not.exist".to_owned()),
                full: false,
            }))
            .await
            .expect("describe_kinds returns ok even with no matches");
        let arr: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
        assert!(
            arr.is_empty(),
            "non-matching prefix should return empty array, got {arr:?}"
        );
    }

    /// `load_component` with a selector that resolves to no stored
    /// component is a tool error: the hub-local `ResolveComponent` misses
    /// on the empty store (ADR-0116).
    #[tokio::test]
    async fn load_component_unresolvable_selector_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .load_component(Parameters(LoadComponentArgs {
                engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                selector: "no-such-component".to_owned(),
                name: None,
                config_path: None,
                export: None,
            }))
            .await;
        assert!(
            result.is_err(),
            "an unresolvable selector should be a tool error",
        );
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
                selector: "any-selector".to_owned(),
                drain_timeout_ms: None,
                config_path: None,
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
                fire_and_forget: false,
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
                checks: vec![],
                similarity: None,
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

        // Seed the cache with a handler that declares a `-> R` reply
        // contract (ADR-0109). `describe_component` surfaces the `reply`
        // kind id verbatim through serde, so a caller reads `In -> Out`
        // before issuing the call.
        let engine =
            EngineId(Uuid::parse_str(engine_id).expect("test setup: engine_id is a valid uuid"));
        let seeded = ComponentCapabilities {
            handlers: vec![aether_kinds::HandlerCapability {
                id: KindId(0x11),
                name: "test.request".to_owned(),
                doc: None,
                reply: aether_data::ReplyContract::One(KindId(0x22)),
            }],
            ..ComponentCapabilities::default()
        };
        mcp.components
            .lock()
            .expect("test setup: component cache mutex is never poisoned")
            .insert((engine, mailbox), seeded);
        let hit = mcp
            .describe_component(Parameters(DescribeComponentArgs {
                engine_id: engine_id.to_owned(),
                mailbox_id: tagged,
            }))
            .await
            .expect("cached component describes");
        let caps: serde_json::Value = serde_json::from_str(&hit).expect("json");
        assert!(caps.get("handlers").is_some(), "capabilities shape: {hit}");
        assert!(
            !caps["handlers"][0]["reply"].is_null(),
            "the handler's ADR-0109 reply contract is surfaced: {hit}"
        );
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

    /// iamacoffeepot/aether#1128: `actor_cost` with a malformed
    /// `engine_id` rejects at the tool boundary without touching the
    /// wire (mirrors `actor_logs_bad_engine_id_is_tool_error`).
    #[tokio::test]
    async fn actor_cost_bad_engine_id_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .actor_cost(Parameters(ActorCostArgs {
                engine_id: "not-a-uuid".to_owned(),
                mailbox_name: "aether.audio".to_owned(),
                kind_id: None,
            }))
            .await;
        assert!(
            result.is_err(),
            "a malformed engine_id should be a tool error"
        );
    }

    /// iamacoffeepot/aether#1128: `actor_cost`'s `kind_id` filter
    /// accepts a tagged `knd-…` id and a raw decimal, and rejects
    /// gibberish.
    #[test]
    fn parse_kind_id_accepts_tagged_and_decimal() {
        let tagged = tagged_id::encode(with_tag(Tag::Kind, 42)).expect("encodes a kind id");
        assert!(parse_kind_id(&tagged).is_ok(), "tagged knd- id parses");
        assert_eq!(
            parse_kind_id("12345").expect("decimal parses").0,
            12345,
            "raw decimal u64 parses",
        );
        assert!(parse_kind_id("not-an-id").is_err(), "gibberish rejected");
    }

    /// iamacoffeepot/aether#1128: `static_kind_name` resolves a known
    /// substrate kind's id back to its name and misses on a stranger.
    #[test]
    fn static_kind_name_resolves_known_substrate_kind() {
        let log_tail = KindId(<aether_kinds::LogTail as Kind>::ID.0);
        assert_eq!(
            static_kind_name(log_tail).as_deref(),
            Some(aether_kinds::LogTail::NAME),
            "a substrate kind resolves to its name",
        );
        assert_eq!(
            static_kind_name(KindId(0xDEAD_BEEF_DEAD_BEEF)),
            None,
            "an unknown id has no static name",
        );
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

    /// The typed-arg path (`DagDescriptorArg` deserialize + `payload_path`
    /// file read + native `encode_into_bytes`) produces the exact same
    /// canonical bytes as a direct `#[derive(Kind)]` encode of the same
    /// descriptor with the bytes inlined. Locks against encoding skew.
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

        // Tool path: typed descriptor with a `payload_path` virtual field
        // on the source, externally-tagged variants, tagged-string ids,
        // and plain-integer node ids (no `{ "0": n }` schema wrapping).
        let path = stage_temp_file("encode", &payload_bytes);
        let descriptor_json = serde_json::json!({
            "version": 1,
            "nodes": [
                { "Source": {
                    "id": 0,
                    "mailbox": tagged_id::encode(source_mbx.0).unwrap(),
                    "kind_id": tagged_id::encode(source_kind.0).unwrap(),
                    "payload_path": path.to_str().unwrap(),
                }},
                { "Observer": {
                    "id": 1,
                    "recipient": tagged_id::encode(observer_mbx.0).unwrap(),
                    "kind_id": tagged_id::encode(observer_kind.0).unwrap(),
                }},
            ],
            "edges": [ { "from": 0, "to": 1, "slot": 0 } ],
        });
        let arg: DagDescriptorArg =
            serde_json::from_value(descriptor_json).expect("descriptor deserializes");
        let actual = Submit {
            descriptor: build_descriptor(arg).await.expect("payload_path resolves"),
        }
        .encode_into_bytes();

        std_fs::remove_file(&path).ok();
        assert_eq!(
            actual, expected,
            "typed tool path + native encode must match the binary inline-bytes encode",
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
                    "id": 0,
                    "mailbox": tagged_id::encode(source_mbx.0).unwrap(),
                    "kind_id": tagged_id::encode(source_kind.0).unwrap(),
                    "payload": payload_bytes,
                }},
            ],
            "edges": [],
        });
        let arg: DagDescriptorArg =
            serde_json::from_value(descriptor_json).expect("descriptor deserializes");
        let actual = Submit {
            descriptor: build_descriptor(arg).await.expect("inline payload builds"),
        }
        .encode_into_bytes();
        assert_eq!(actual, expected);
    }

    /// A cast-only kind — `#[repr(C)]` + `bytemuck::Pod`, no
    /// `serde::Serialize` impl (`LifecycleSubscribe`) — rides the send
    /// builders, which bound `K: Kind` (not `K: Kind + Serialize`) and
    /// encode via the descriptor-aware `encode_into_bytes`. The payload
    /// is the kind's cast image (length == `size_of`, distinct from a
    /// wire varint encode of these `u64`s) and round-trips through
    /// `Kind::decode_from_bytes`. Compiling at all is the bound check:
    /// the old `serde::Serialize` bound would reject this kind.
    #[test]
    fn send_builders_encode_a_cast_only_kind() {
        let mail = aether_kinds::LifecycleSubscribe {
            stage: u64::MAX,
            mailbox: 0x0102_0304_0506_0708,
        };
        let cast_bytes = mail.encode_into_bytes();
        assert_eq!(
            cast_bytes.len(),
            size_of::<aether_kinds::LifecycleSubscribe>(),
            "cast image is the fixed struct size, not a wire varint encode",
        );

        let local = local_envelope("aether.lifecycle", &mail);
        assert_eq!(local.kind, aether_kinds::LifecycleSubscribe::ID);
        assert_eq!(local.payload, cast_bytes);
        assert_eq!(
            aether_kinds::LifecycleSubscribe::decode_from_bytes(&local.payload),
            Some(mail),
        );

        let engine = EngineId(Uuid::from_u128(0x1232_dead_beef));
        let by_id = engine_envelope_by_id(engine, mailbox_id_from_name("aether.lifecycle"), &mail);
        assert_eq!(by_id.kind, aether_kinds::LifecycleSubscribe::ID);
        assert_eq!(by_id.payload, cast_bytes);
        assert_eq!(
            aether_kinds::LifecycleSubscribe::decode_from_bytes(&by_id.payload),
            Some(mail),
        );
    }

    /// `submit_dag` with a `payload_path` that doesn't exist returns a
    /// structured tool error before any call hits the engine.
    #[tokio::test]
    async fn submit_dag_rejects_missing_payload_path() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let source_mbx = mailbox_id_from_name("aether.fs");
        let descriptor: DagDescriptorArg = serde_json::from_value(serde_json::json!({
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
        }))
        .expect("descriptor deserializes");
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

    /// Issue 1242 / 1246: `decode_reply_events` transcodes a correlated
    /// reply into the MCP wire shape — a known substrate kind decodes to
    /// its name + params, and on a clean decode the raw bytes are
    /// omitted (issue 1246, no int-array duplicate). This is the
    /// surfacing the await-by-default change adds; the decode is the
    /// reusable core both tools share.
    #[test]
    fn decode_reply_events_decodes_known_substrate_kind() {
        // Pick a real substrate kind out of the static inventory and
        // round-trip a params object through `encode_schema` into the
        // reply envelope the substrate would have produced.
        let descriptors = descriptors::all();
        let desc = descriptors
            .iter()
            .find(|d| d.name == "aether.fs.list")
            .expect("aether.fs.list is in the static vocabulary");
        let params = serde_json::json!({ "namespace": "save", "prefix": "" });
        let payload =
            aether_codec::encode_schema(&params, &desc.schema).expect("encode list params");
        let kind = KindId(kind_id_from_parts(&desc.name, &desc.schema));
        let reply = MailEnvelope {
            to: MailboxAddress::local(mailbox_id_from_name("aether.fs")),
            from: None,
            kind,
            correlation_id: Some(7),
            payload,
        };

        // Empty engine-kinds map → falls through to the static vocabulary.
        let decoded = decode_reply_events(&[reply], &HashMap::new(), None);
        assert_eq!(decoded.len(), 1, "one reply in, one out");
        let only = &decoded[0];
        assert_eq!(
            only.kind_name.as_deref(),
            Some("aether.fs.list"),
            "the known kind resolves to its name",
        );
        assert_eq!(
            only.params.as_ref(),
            Some(&params),
            "params decode back to the original JSON",
        );
        assert!(
            only.payload_bytes.is_none(),
            "a clean decode omits the raw bytes (issue 1246)",
        );
        assert!(
            only.kind_id.starts_with("knd-"),
            "the kind id renders as the ADR-0064 tagged string: {}",
            only.kind_id,
        );
    }

    /// Issue 1242 / 1246: an unknown / undecodable reply kind never
    /// fails the surfacing — `params` is `null`, `kind_name` is `null`,
    /// and the raw bytes are still returned, now base64-encoded (the
    /// disconnected-engine fallback contract).
    #[test]
    fn decode_reply_events_falls_back_on_unknown_kind() {
        let reply = MailEnvelope {
            to: MailboxAddress::local(MailboxId(1)),
            from: None,
            kind: KindId(0xDEAD_BEEF_DEAD_BEEF),
            correlation_id: None,
            payload: vec![1, 2, 3],
        };
        // No engine-kinds entry, no declared reply → falls through to base64.
        let decoded = decode_reply_events(&[reply], &HashMap::new(), None);
        assert_eq!(decoded.len(), 1);
        let only = &decoded[0];
        assert_eq!(only.kind_name, None, "an unknown kind has no name");
        assert_eq!(only.params, None, "an unknown kind doesn't decode");
        assert_eq!(
            only.payload_bytes.as_deref(),
            Some("AQID"),
            "raw bytes survive as base64 (issue 1246)",
        );
    }

    /// Issue 1246: a clean-decode reply serializes to JSON with no
    /// `payload_bytes` key at all — the `skip_serializing_if` guard
    /// against the redundant-int-array regression this issue fixes.
    #[test]
    fn clean_decode_reply_omits_payload_bytes_key_in_json() {
        let descriptors = descriptors::all();
        let desc = descriptors
            .iter()
            .find(|d| d.name == "aether.fs.list")
            .expect("aether.fs.list is in the static vocabulary");
        let params = serde_json::json!({ "namespace": "save", "prefix": "" });
        let payload =
            aether_codec::encode_schema(&params, &desc.schema).expect("encode list params");
        let kind = KindId(kind_id_from_parts(&desc.name, &desc.schema));
        let reply = MailEnvelope {
            to: MailboxAddress::local(mailbox_id_from_name("aether.fs")),
            from: None,
            kind,
            correlation_id: Some(7),
            payload,
        };

        // Empty engine-kinds map → falls through to the static vocabulary.
        let decoded = decode_reply_events(&[reply], &HashMap::new(), None);
        let json = serde_json::to_value(&decoded[0]).expect("reply serializes");
        let obj = json.as_object().expect("reply is a JSON object");
        assert!(
            !obj.contains_key("payload_bytes"),
            "a clean decode omits the payload_bytes key entirely: {json}",
        );
        assert!(obj.contains_key("params"), "params is still present");
    }

    /// Issue 1804: `decode_reply_events` decodes a reply whose kind is
    /// component-defined (not in `descriptors::all()`) when the engine
    /// kind cache carries the schema and the handler's declared reply kind
    /// matches the envelope (ADR-0109). This is the core gap the issue
    /// closes: a `send_mail` reply for a component-defined kind should
    /// surface `params`, not base64.
    #[test]
    fn decode_reply_events_decodes_component_defined_reply_via_engine_cache() {
        use aether_data::{KindDescriptor, SchemaType};

        // A component-defined reply kind — not in `descriptors::all()`.
        let reply_kind = KindDescriptor {
            name: "test.component.reply".to_owned(),
            schema: SchemaType::String,
        };
        let reply_kind_id = KindId(kind_id_from_parts(&reply_kind.name, &reply_kind.schema));

        // Encode a value against the component-defined schema, as the
        // substrate handler would produce.
        let value = serde_json::Value::String("hello from component".to_owned());
        let payload =
            aether_codec::encode_schema(&value, &reply_kind.schema).expect("encode reply value");

        let envelope = MailEnvelope {
            to: MailboxAddress::local(mailbox_id_from_name("aether.test.component")),
            from: None,
            kind: reply_kind_id,
            correlation_id: Some(1),
            payload,
        };

        // Pre-condition: the static vocabulary doesn't carry this kind, so
        // without the engine cache the decode would fall through to base64.
        assert!(
            !descriptors::all().iter().any(|d| d.name == reply_kind.name),
            "test invariant: the component kind must not be in the static vocabulary",
        );

        // Build an engine-kinds map as `load_component` / `ListKinds` would
        // populate it, and supply the handler's declared reply kind.
        let mut engine_kinds = HashMap::new();
        engine_kinds.insert(reply_kind.name.clone(), reply_kind);

        let decoded = decode_reply_events(&[envelope], &engine_kinds, Some(reply_kind_id));
        assert_eq!(decoded.len(), 1);
        let only = &decoded[0];
        assert_eq!(
            only.params.as_ref(),
            Some(&value),
            "component-defined reply kind decodes to params via engine cache",
        );
        assert!(
            only.payload_bytes.is_none(),
            "a clean decode omits the raw bytes",
        );
        assert_eq!(
            only.kind_name.as_deref(),
            Some("test.component.reply"),
            "the component-defined kind name is surfaced from the engine cache",
        );
    }

    /// Issue 1804: the base64 fallback is unchanged when neither the engine
    /// kind cache nor the static vocabulary carries the reply kind, even
    /// when `declared_reply` is `Some`. Covers fire-and-forget / unknown-
    /// sender replies that never had a registered schema.
    #[test]
    fn decode_reply_events_base64_fallback_when_kind_absent_from_all_caches() {
        let absent_kind_id = KindId(0xC0FF_EE00_C0FF_EE00);
        let envelope = MailEnvelope {
            to: MailboxAddress::local(MailboxId(2)),
            from: None,
            kind: absent_kind_id,
            correlation_id: None,
            payload: vec![0xAB, 0xCD],
        };
        // Declared reply matches the envelope but the engine cache is empty.
        let decoded = decode_reply_events(&[envelope], &HashMap::new(), Some(absent_kind_id));
        assert_eq!(decoded.len(), 1);
        let only = &decoded[0];
        assert_eq!(only.params, None, "absent kind doesn't decode to params");
        assert!(
            only.payload_bytes.is_some(),
            "absent kind surfaces as base64 fallback",
        );
    }

    /// ADR-0091 issue 1232 (end-to-end): a kind registered in the
    /// substrate's `Registry` — emulating the post-`load_component`
    /// state for a component-defined kind like `aether.mesh.load` —
    /// flows through `InventoryCapability`'s `ListKinds` projection
    /// onto the wire, lands in the harness's per-engine encode cache,
    /// and the next `send_mail` encodes correctly. This is the
    /// forcing-function path the issue calls out: a kind NOT in
    /// `descriptors::all()` becomes encodable the moment the substrate
    /// holds it.
    ///
    /// Test addresses the engines cap with `engine = None` (the hub
    /// fixture's local dispatch path) so the round-trip closes against
    /// the same chassis without needing a separately-routed engine
    /// proxy; the cache machinery under test is engine-keyed but
    /// engine-agnostic at the RPC layer.
    #[tokio::test]
    async fn lookup_descriptor_picks_up_a_post_load_kind_via_inventory() {
        use aether_data::{KindDescriptor, SchemaType};

        // The component-defined kind in this scenario: present in the
        // substrate's `Registry` but not in `descriptors::all()`.
        let component_kind = KindDescriptor {
            name: "aether.test.component_defined_kind".to_owned(),
            schema: SchemaType::String,
        };

        let extras = vec![component_kind.clone()];
        let (_chassis, port) = boot_hub_with_inventory(&extras);
        let session = RpcSession::connect(&format!("127.0.0.1:{port}")).expect("session connects");
        let mcp = Mcp::new(
            Arc::new(session),
            Arc::new(ComponentCache::default()),
            Arc::new(ReverseNameCache::default()),
            Arc::new(KindsCache::default()),
        );

        // Pre-condition: the static prefill does NOT carry the
        // component's kind. (If a future change accidentally promotes
        // it to native, the test surfaces immediately rather than
        // silently bypassing the cache-refresh path.)
        assert!(
            !descriptors::all()
                .iter()
                .any(|d| d.name == component_kind.name),
            "test invariant: the component kind must not be in the static descriptors",
        );

        // Address the hub's local `aether.inventory` via the engines-
        // cap path: the hub-fixture's RPC server routes
        // `engine = Some(uuid)` envelopes through the engines cap,
        // which knows no matching engine and warn-drops. To exercise
        // the cache against the local cap, route as a local Call
        // by stamping `engine = None`. We bypass `lookup_descriptor`'s
        // `engine_envelope` here because the test fixture is hub-
        // shaped (the engines cap doesn't proxy to a separate
        // substrate); in production the hub forwards to the engine
        // and the engine answers via its local `aether.inventory`.
        let reply = mcp
            .session
            .call_one(local_envelope(INVENTORY_CAP, &ListKinds {}))
            .await
            .expect("aether.inventory.kinds reply");
        let result =
            ListKindsResult::decode_from_bytes(&reply.payload).expect("ListKindsResult decodes");
        // The reply must include the registered component kind with a
        // schema that decodes back to the originally registered shape
        // — the wire path the harness's cache reads from.
        let entry = result
            .kinds
            .iter()
            .find(|k| k.name == component_kind.name)
            .unwrap_or_else(|| {
                panic!(
                    "ListKindsResult should include the registered component kind; \
                     got {:?}",
                    result.kinds.iter().map(|k| &k.name).collect::<Vec<_>>(),
                )
            });
        let decoded_schema: SchemaType =
            wire::from_bytes(&entry.schema_postcard).expect("schema_postcard decodes");
        assert!(
            matches!(decoded_schema, SchemaType::String),
            "the registered schema round-trips through the wire",
        );

        // Now drive the harness's encode path directly. Seed the
        // per-engine cache the way a real refresh would (engine id is
        // synthetic; the cache is engine-keyed so any uuid suffices
        // for this assertion), then verify `build_mail_envelope`
        // encodes a `MailSpec` against the component kind without
        // ever consulting `descriptors::all()`. This is the surface
        // the production `send_mail` reaches for after a
        // `load_component` populates the cache via the same wire
        // path the assertion above exercised.
        let engine = EngineId(Uuid::from_u128(0x1232_dead_beef));
        // Seed the per-engine cache the way `refresh_engine_kinds` would
        // on a hit — the cache merge helper is the single writer.
        mcp.merge_into_engine_cache(engine, vec![component_kind.clone()]);
        let envelope = mcp
            .build_mail_envelope(MailSpec {
                engine_id: engine.0.to_string(),
                recipient_name: "aether.embedded:test".to_owned(),
                kind_name: component_kind.name.clone(),
                params: Some(serde_json::Value::String("hello".to_owned())),
            })
            .await
            .expect("build_mail_envelope encodes the component-defined kind");
        // The schema-encoded payload for a `SchemaType::String` is the
        // wire-codec string shape; decoding back via the same schema
        // must yield the original JSON value.
        let decoded = aether_codec::decode_schema(&envelope.payload, &component_kind.schema)
            .expect("payload decodes against the cached schema");
        assert_eq!(
            decoded,
            serde_json::Value::String("hello".to_owned()),
            "the encoded payload round-trips through aether_codec against the live schema",
        );
        assert_eq!(
            envelope.kind,
            KindId(kind_id_from_parts(
                &component_kind.name,
                &component_kind.schema
            )),
            "envelope kind id matches the live KindId of the component-defined kind",
        );
    }

    /// Issue 1242: `fire_and_forget: true` is non-blocking — a
    /// well-formed item is dispatched without awaiting any reply, so the
    /// call returns `status: "dispatched"` with empty `replies` well
    /// under the await timeout, even against an unknown engine (the
    /// server's eventual error `ReplyEnd` is dropped as an unrouted
    /// frame, never awaited). Contrast `delivered`, which blocks on
    /// settlement.
    #[tokio::test]
    async fn send_mail_fire_and_forget_is_non_blocking() {
        use std::time::Instant;

        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let started = Instant::now();
        let out = mcp
            .send_mail(Parameters(SendMailArgs {
                mails: vec![MailSpec {
                    // A well-formed item to an engine the hub doesn't
                    // supervise: the dispatch chain never settles with a
                    // reply, so a blocking call would wait — fire-and-
                    // forget returns at once.
                    engine_id: "00000000-0000-0000-0000-000000000099".to_owned(),
                    recipient_name: "aether.fs".to_owned(),
                    kind_name: "aether.fs.list".to_owned(),
                    params: Some(serde_json::json!({ "namespace": "save", "prefix": "" })),
                }],
                fire_and_forget: true,
            }))
            .await
            .expect("send_mail returns a status array");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "fire-and-forget must not block on settlement",
        );
        let statuses: Vec<MailStatus> = serde_json::from_str(&out).expect("status array");
        assert_eq!(statuses.len(), 1);
        assert_eq!(
            statuses[0].status, "dispatched",
            "fire-and-forget reports dispatched, not delivered",
        );
        assert!(
            statuses[0].replies.is_empty(),
            "fire-and-forget carries no replies",
        );
        assert!(!statuses[0].timed_out, "dispatch is not a timeout");
    }

    /// `render_shape` on a struct kind produces a `{ field: type, … }`
    /// one-liner. Using `aether.fs.write` as a representative struct kind —
    /// it has named fields with known types.
    #[test]
    fn render_shape_struct_kind() {
        use aether_kinds::descriptors;
        let write = descriptors::all()
            .into_iter()
            .find(|d| d.name == "aether.fs.write")
            .expect("aether.fs.write in the substrate vocabulary");
        let shape = render_shape(&write.schema);
        assert!(
            shape.starts_with("{ ") && shape.ends_with(" }"),
            "struct shape should be {{ field: type, … }}, got: {shape:?}",
        );
        assert!(
            shape.contains("namespace") && shape.contains("path"),
            "aether.fs.write shape should mention namespace and path, got: {shape:?}",
        );
    }

    /// `render_shape` on a unit/fieldless kind produces `{}`.
    #[test]
    fn render_shape_unit_kind() {
        let shape = render_shape(&SchemaType::Unit);
        assert_eq!(shape, "{}", "unit schema should render as {{}}");
    }

    /// `render_shape` on an enum kind produces `Var1 | Var2(…) | …`
    /// with variants separated by ` | `.
    #[test]
    fn render_shape_enum_kind() {
        use aether_data::{EnumVariant, SchemaType as ST};
        use std::borrow::Cow;
        let schema = ST::Enum {
            variants: Cow::Borrowed(&[
                EnumVariant::Unit {
                    name: Cow::Borrowed("Off"),
                    discriminant: 0,
                },
                EnumVariant::Tuple {
                    name: Cow::Borrowed("On"),
                    discriminant: 1,
                    fields: Cow::Borrowed(&[ST::Bool]),
                },
            ]),
        };
        let shape = render_shape(&schema);
        assert_eq!(
            shape, "Off | On(bool)",
            "enum shape should be Var1 | Var2(inner)"
        );
    }
}
