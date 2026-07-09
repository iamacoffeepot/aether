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
//! queries the live engine's `aether.inventory.kinds` mailbox so it
//! surfaces capability-owned and component-defined kinds, falling back to
//! the static substrate baseline only when no engine is reachable.
//! `describe_component` answers locally from a component-capability cache
//! populated by `load_component` / `replace_component`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

// `tokio::sync::Mutex` (the async one used by the per-engine refresh-
// collapse guard) imported under an alias so the struct-field path
// stays short — `std::sync::Mutex` is the bare `Mutex`.
use tokio::sync::Mutex as AsyncMutex;

use aether_capabilities::rpc::{MailEnvelope, MailboxAddress};
use aether_capabilities::trace::walk::TreeWalk;
use aether_codec::frame::max_frame_size;
use aether_data::MailId;
use aether_data::canonical::kind_id_from_parts;
use aether_data::wire;
use aether_data::{
    EngineId, Kind, KindDescriptor, KindId, MailboxId, ScopePathError, Tag, Uuid,
    mailbox_id_from_path, tagged_id, validate_scope_path,
};
use aether_data::{EnumVariant, Primitive, SchemaType};
use aether_kinds::{
    BinarySelector, CaptureFrame, CaptureFrameResult, ComponentCapabilities, ComponentSelector,
    CostTail, CostTailResult, DeathReason, DescribeComponent, DescribeComponentResult, FrameCheck,
    FrameRect, FrameReduction, KindDescriptorWire, ListComponentBinaries,
    ListComponentBinariesResult, ListComponents, ListComponentsResult, ListEngineBinaries,
    ListEngineBinariesResult, ListEngines, ListEnginesResult, ListKinds, ListKindsResult,
    LoadComponent, LoadResult, NamedMail, ReplaceComponent, ReplaceResult, ResolveComponent,
    ResolveComponentResult, SimilarityCheck, SpawnEngine, SpawnEngineResult, TerminateEngine,
    TerminateEngineResult, UploadBinary, UploadBinaryResult, UploadComponent,
    UploadComponentResult,
    trace::{
        DescribeTreeResult, DispatchTraced, MailNodeWire, TRACE_MAILBOX_NAME, TraceTail,
        TraceTailResult,
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
use crate::args::{ActorCostArgs, ActorCostResponse, ActorCostRow};
use crate::args::{
    CaptureCheckSpec, CaptureFrameArgs, CaptureMailSpec, ComponentSpec, DeadEngineInfo,
    DescribeComponentArgs, DescribeHandlersArgs, DescribeHandlersResponse, DescribeKindsArgs,
    EngineInfo, KindSummary, ListBinariesArgs, ListComponentsArgs, ListEnginesResponse,
    LoadComponentArgs, MailIdJson, MailNodeJson, MailSpec, MailStatus, NativeCapHandlers,
    NativeHandlerJson, ReplaceComponentArgs, ReplyEventJson, SendMailArgs, SendMailTracedArgs,
    SendMailTracedResponse, SpawnSubstrateArgs, TerminateSubstrateArgs, TracedMailSpec,
    TransformListing, UploadBinaryArgs, UploadComponentArgs,
};
use crate::reverse::EngineNames;
use crate::rpc::RpcSession;
use aether_kinds::descriptors;
use aether_kinds::{
    HandlersResult, ListHandlers, Manifest, ManifestResult, Resolve, ResolveResult,
};
use base64::engine::general_purpose::STANDARD;
use std::time::Duration;
use tokio::time;

mod bytes;
mod capture;
mod components;
mod envelope;
mod ids;
mod logs_cost;
#[cfg(test)]
mod loopback;
mod render;
mod reply;
mod state;

trait NamedMailSpec: Sync {
    fn recipient_name(&self) -> &str;
    fn kind_name(&self) -> &str;
    fn params(&self) -> Option<&serde_json::Value>;
}

impl NamedMailSpec for CaptureMailSpec {
    fn recipient_name(&self) -> &str {
        &self.recipient_name
    }

    fn kind_name(&self) -> &str {
        &self.kind_name
    }

    fn params(&self) -> Option<&serde_json::Value> {
        self.params.as_ref()
    }
}

impl NamedMailSpec for TracedMailSpec {
    fn recipient_name(&self) -> &str {
        &self.recipient_name
    }

    fn kind_name(&self) -> &str {
        &self.kind_name
    }

    fn params(&self) -> Option<&serde_json::Value> {
        self.params.as_ref()
    }
}

#[cfg(test)]
use self::bytes::{render_bytes_leaf_in, render_bytes_reply, resolve_bytes_params};
use self::capture::capture_check;
use self::components::{
    component_config_bytes, components_all_loaded, reject_zero_replicas, replica_base_name,
    replica_names, selector_with_explicit_export,
};
use self::envelope::{
    engine_envelope, engine_envelope_by_id, local_envelope, validate_recipient_scope,
};
use self::ids::{
    mail_id_to_json, parse_engine_id, parse_kind_id, parse_mailbox_id, resolve_handled_kind,
    static_kind_name,
};
use self::logs_cost::{actor_logs_err_message, level_to_str, parse_level};
#[cfg(test)]
use self::loopback::{RouteInventorySink, RouteLoopbackConfig};
use self::render::{
    death_reason_parts, frame_size_aware_error, internal, internal_msg, json, render_shape,
};
use self::reply::{decode_reply_events, decode_traced_ack, strip_ack};
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
        description = "List every engine the hub currently supervises, plus a recently-died sidecar. Returns an object {engines, recently_died}: each `engines` item reports the engine_id (pass it to send_mail / terminate_substrate) and the localhost RPC port the hub assigned its substrate; each `recently_died` item reports a departed engine with why it left — reason \"terminated\" (a deliberate terminate_substrate), \"crashed\" (the substrate closed its connection), \"evicted\" (a missed-heartbeat eviction), or \"spawn_failed\" (a spawn that never connected — the substrate failed to come up) — plus a detail string and how long ago it left, so a clean shutdown is distinguishable from a failure."
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
        description = "Fork+exec a substrate binary as a child of the hub, resolved from the hub's content-addressed binary store (ADR-0115) — not a host path. Pass `selector` to pick the binary: a content `hash`, a `name@version`, or a `name` (upload_binary first if it isn't stored). Omit `selector` for `default` — the headless chassis — so a bare spawn_substrate with no arguments returns a working engine. When `selector` is omitted you may instead attribute-query with `chassis` (\"headless\"/\"desktop\"/\"hub\"), `caps` (linked-cap superset), and `target` (build triple). The hub resolves the selector to the stored bytes, materializes them to an executable temp file, assigns a free localhost RPC port (injected as AETHER_RPC_PORT), forks it, and connects a proxy. Returns the engine_id and rpc_port on success; errors if the selector resolves to no stored binary or the substrate fails to come up. A spawn that fails after the hub allocated an engine_id carries that id in the error (and records a matching spawn_failed entry in list_engines.recently_died), so you can correlate and reap rather than guess. Pass `components` (each {selector, name?, config?, config_path?, export?, replicas?}) to bring the engine up with those components already loaded in one call — each selector is a content hash, name, or module@actor resolved against the hub's component registry (ADR-0116; upload_component first). `config` is inline JSON and `config_path` is a JSON file path; aether-mcp schema-encodes either one to the component's Config kind, stages the resulting bytes next to the staged wasm, and writes a boot-manifest the hub injects as AETHER_BOOT_MANIFEST. The spawned substrate reads the staged wasm/config byte files itself (single-host), so no follow-up load_component is needed. Set replicas: N on an entry (issue 2626) to fan it out into N instances at boot from one spec, one shared config — each named {base}-{index} for index in 0..N (base = name > export > entry actor namespace) — and the readiness wait gates on every instance; pairs with #[router(shared)] (ADR-0136) to scale an HTTP handler to N instances with no hand-named entries. replicas: 0 is a tool error."
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
        let info = match SpawnEngineResult::decode_from_bytes(&reply.payload) {
            Some(SpawnEngineResult::Ok {
                engine_id,
                rpc_port,
            }) => EngineInfo {
                engine_id,
                rpc_port,
                // A just-spawned engine is alive as of now.
                last_heartbeat_age_millis: 0,
            },
            Some(SpawnEngineResult::Err { engine_id, error }) => {
                // Carry the allocated engine_id (when the failure came
                // after the hub minted one) so the caller can correlate
                // the failed spawn against its `recently_died` entry and
                // reap it rather than guessing.
                let message = match engine_id {
                    Some(id) => format!(
                        "{error} (engine_id {id} — see this id's spawn_failed entry in list_engines.recently_died)"
                    ),
                    None => error,
                };
                return Err(internal_msg(&message));
            }
            None => return Err(internal_msg("undecodable SpawnEngineResult")),
        };

        // The spawn reply returns once the proxy connects, before any
        // boot-manifest autoload (ADR-0116) settles — those loads are
        // fire-and-forget and emit no completion signal. When components
        // were requested, poll the engine's loaded-components query (issue
        // 2020) until every requested component's lineage name is present in
        // the live trampoline set, so the tool hands back a genuinely-ready
        // engine rather than one mid-boot. Identity-based polling catches both
        // baseline contamination (a pre-existing trampoline satisfying a count)
        // and wrong-set false positives (a different component registering while
        // a requested one stalls).
        if let Some(ref staged) = staged
            && !staged.expected_names.is_empty()
        {
            self.wait_for_loaded_components(&info.engine_id, &staged.expected_names)
                .await?;
        }

        json(&info)
    }

    /// Poll the spawned engine's `aether.component.list` query (issue 2020)
    /// until every name in `want_names` appears in `result.names`, or the
    /// bounded budget elapses. The boot autoload is async and signalless, so
    /// this turns the readiness wait into a deterministic identity-poll of the
    /// live trampoline set rather than a fixed post-spawn sleep. A timeout is a
    /// clear tool error naming the specific components still missing.
    async fn wait_for_loaded_components(
        &self,
        engine_id: &str,
        want_names: &[String],
    ) -> Result<(), McpError> {
        const POLL_INTERVAL: Duration = Duration::from_millis(100);
        const BUDGET: Duration = Duration::from_secs(30);

        let engine = parse_engine_id(engine_id)?;
        let deadline = time::Instant::now() + BUDGET;
        loop {
            let reply = self
                .session
                .call_one(engine_envelope(
                    engine,
                    "aether.component",
                    &ListComponents {},
                ))
                .await
                .map_err(internal)?;
            let Some(result) = ListComponentsResult::decode_from_bytes(&reply.payload) else {
                return Err(internal_msg("undecodable ListComponentsResult"));
            };
            if components_all_loaded(want_names, &result.names) {
                return Ok(());
            }
            if time::Instant::now() >= deadline {
                let missing: Vec<&str> = want_names
                    .iter()
                    .filter(|w| !result.names.iter().any(|n| n == *w))
                    .map(String::as_str)
                    .collect();
                return Err(internal_msg(&format!(
                    "spawned engine did not load all boot components within {}s: \
                     still missing: {missing:?}",
                    BUDGET.as_secs(),
                )));
            }
            time::sleep(POLL_INTERVAL).await;
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
                &ListEngineBinaries {
                    chassis: args.chassis,
                    caps: args.caps,
                    target: args.target,
                },
            ))
            .await
            .map_err(internal)?;
        match ListEngineBinariesResult::decode_from_bytes(&reply.payload) {
            Some(result) => json(&result.binaries),
            None => Err(internal_msg("undecodable ListEngineBinariesResult")),
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
                &ListComponentBinaries {
                    namespace: args.namespace,
                    handled_kind,
                },
            ))
            .await
            .map_err(internal)?;
        match ListComponentBinariesResult::decode_from_bytes(&reply.payload) {
            Some(result) => json(&result.components),
            None => Err(internal_msg("undecodable ListComponentBinariesResult")),
        }
    }

    #[tool(
        description = "Send one or more mail items to substrate mailboxes. Each item carries structured `params`, schema-encoded against the substrate kind vocabulary. Best-effort batch: per-item status is returned and one failure doesn't abort siblings. By default each item BLOCKS until its dispatch chain settles and the item's correlated reply payloads are returned in `replies` (status 'delivered'); each reply is {kind_id, kind_name, params (best-effort decode, null on miss), payload_bytes (base64 string, present only on a decode miss)}. The await cap is 600s (gated by the batch-level settlement against a slow provider cap); on timeout the item reports status 'timeout' with timed_out:true and any replies collected so far. Set fire_and_forget:true to restore non-blocking dispatch (status 'dispatched', empty replies) — use it for a fire-and-poke (e.g. a DrawTriangle before a capture_frame) or a cap that never replies. For `Bytes`-typed fields in `params`, pass a byte array (`[…]`, canonical) or one `$`-sigil embed: `$file` (reads a file on the harness host), `$base64` (decodes), or `$text` (UTF-8-encodes)."
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
                            // ADR-0112 / ADR-0134: a single-class handler names
                            // one static reply kind and a multi-class handler
                            // names its element kind — both are what a driver
                            // decodes, so search the cache for either. A manual
                            // / silent handler yields no declared kind.
                            .and_then(|h| match h.reply {
                                aether_data::ReplyContract::One(id)
                                | aether_data::ReplyContract::Multi(id) => Some(id),
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
        // Same shape `CaptureFrame` carries: `Vec<NamedMail>` with
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
        description = "Load a WASM component into a substrate by registry selector (ADR-0116) — upload_component first if it isn't stored. Pass `selector`: a content hash, a name (latest upload under it), or a module@actor (the @actor half picks an exported actor type from a multi-actor module). The host wasm path is gone — the only path anywhere is the upload_component input. aether-mcp resolves the selector hub-local to the wasm bytes, forwards aether.component.load to the engine's aether.component mailbox, and awaits the LoadResult — returning {mailbox_id, name, capabilities} or an error. The component's kind vocabulary rides in the wasm's aether.kinds custom section. Pass config (inline JSON) or config_path (path to a JSON file) to deliver init-config to a typed-config component (ADR-0090): aether-mcp schema-encodes the JSON to the component's Config kind before forwarding bytes — describe_component reports the expected config kind. Pass export to pick which exported actor type to instantiate from a multi-actor module (ADR-0096), named by its Actor::NAMESPACE; a module@actor selector populates it from its @actor half; omit both to load the module's entry type (the first in its export! list, and the only type a single-actor module has). The returned name + capabilities describe the selected type. Pass replicas: N (issue 2626) to load N instances of this selector in one call — each named {base}-{index} (base = name > export > entry actor namespace) — returning {\"components\": [{mailbox_id, name, capabilities}, ...]} instead of the single-load shape; pairs with #[router(shared)] (ADR-0136) to scale an HTTP handler to N instances with no hand-written registration. A mid-loop failure reports which replica failed and how many loaded before it — already-loaded replicas stay live, the same as N manual calls. replicas: 0 is a tool error. Very large wasm payloads (debug builds at 15-25 MiB) may exceed the RPC framing cap — prefer release builds, or raise the cap via the AETHER_MAX_FRAME_SIZE env var (default 64 MiB, clamped at 1 GiB; issue 1271)."
    )]
    pub async fn load_component(
        &self,
        Parameters(args): Parameters<LoadComponentArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let selector = selector_with_explicit_export(&args.selector, args.export.as_deref());
        reject_zero_replicas(args.replicas, &selector)?;
        // ADR-0116: resolve the selector hub-local to the wasm bytes; a
        // `module@actor` selector's `@actor` half rides back as `export`.
        let resolved = self.resolve_component(&selector).await?;
        let config = component_config_bytes(
            resolved.config_kind.as_ref(),
            args.config,
            args.config_path.as_deref(),
            &format!("load_component {selector:?}"),
        )
        .await?
        .unwrap_or_default();
        // An explicit `export` arg wins over the selector's `@actor` half.
        let export = args.export.or(resolved.export);

        let Some(replicas) = args.replicas else {
            // Today's exact single-load path, unmodified.
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
            return match LoadResult::decode_from_bytes(&reply.payload) {
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
            };
        };

        // issue 2626: loop the single-load dispatch N times, one shared
        // wasm/config, naming each instance in the same precedence order
        // `stage_boot_manifest` derives `expected_names` in.
        let base = replica_base_name(
            args.name.as_deref(),
            export.as_deref(),
            resolved.entry_namespace.as_deref(),
        )
        .ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "component {selector:?}: cannot determine a base name for `replicas` \
                     (no `name`, `export`, or entry actor namespace in the wasm manifest); \
                     set `name` or `export`"
                ),
                None,
            )
        })?;

        let mut loaded = Vec::with_capacity(replicas as usize);
        for (index, name) in replica_names(&base, replicas).into_iter().enumerate() {
            let reply = self
                .session
                .call_one(engine_envelope(
                    engine,
                    COMPONENT_CAP,
                    &LoadComponent {
                        wasm: resolved.wasm.clone(),
                        name: Some(name),
                        config: config.clone(),
                        export: export.clone(),
                    },
                ))
                .await
                .map_err(|e| {
                    frame_size_aware_error(
                        &format!("load_component {selector:?} replica {index}"),
                        e,
                    )
                })?;
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
                    loaded.push(serde_json::json!({
                        "mailbox_id": mailbox_id,
                        "name": name,
                        "capabilities": capabilities,
                    }));
                }
                Some(LoadResult::Err { error }) => {
                    return Err(internal_msg(&format!(
                        "load_component {selector:?} replica {index} of {replicas} failed: {error} \
                         ({index} of {replicas} replicas loaded before this failure; already-loaded \
                         replicas stay live, the same as N manual load_component calls)"
                    )));
                }
                None => return Err(internal_msg("undecodable LoadResult")),
            }
        }
        json(&serde_json::json!({ "components": loaded }))
    }

    #[tool(
        description = "Atomically replace a live component's WASM with a build resolved from a registry selector (ADR-0022 structural splice; ADR-0116 selector). Pass `selector` (hash-primary — a hash pins or rolls a component to an exact build; a name or module@actor resolves too); the host wasm path is gone, surviving only as the upload_component input. aether-mcp resolves the selector hub-local to the wasm bytes and forwards aether.component.replace to the engine's aether.component mailbox. drain_timeout_ms is accepted for wire compatibility but currently ignored. Pass config (inline JSON) or config_path (path to a JSON file) to deliver init-config to a typed-config replacement; aether-mcp schema-encodes the JSON to the replacement component's Config kind before forwarding bytes. Pass export to instantiate a specific exported actor type from a multi-actor replacement module (ADR-0096), named by its Actor::NAMESPACE; a module@actor selector populates it from its @actor half. Omit export to reuse the actor type the trampoline currently hosts — not necessarily the module entry — preserving today's replace behaviour; an export the replacement module doesn't declare comes back as an error. Returns the replaced component's advertised capabilities. Very large wasm payloads (debug builds at 15-25 MiB) may exceed the RPC framing cap — prefer release builds, or raise the cap via the AETHER_MAX_FRAME_SIZE env var (default 64 MiB, clamped at 1 GiB; issue 1271)."
    )]
    pub async fn replace_component(
        &self,
        Parameters(args): Parameters<ReplaceComponentArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        let mailbox_id = parse_mailbox_id(&args.mailbox_id)?;
        let selector = selector_with_explicit_export(&args.selector, args.export.as_deref());
        // ADR-0116: resolve the selector hub-local to the replacement wasm
        // bytes (hash-primary, so a hash pins/rolls to an exact build).
        let resolved = self.resolve_component(&selector).await?;
        let config = component_config_bytes(
            resolved.config_kind.as_ref(),
            args.config,
            args.config_path.as_deref(),
            &format!("replace_component {selector:?}"),
        )
        .await?
        .unwrap_or_default();
        // ADR-0096: an explicit `export` arg wins over the selector's
        // `@actor` half; `None` reuses the actor type the trampoline
        // currently hosts.
        let export = args.export.or(resolved.export);
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
                    export,
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
        description = "Capture an engine's current frame as a PNG, returned inline as image content. Optionally carries two mail bundles dispatched atomically around the capture: `mails` fires before readback (state changes that should appear in the image), `after_mails` fires after (cleanup). A bad bundle entry aborts the whole capture before any mail moves. Optionally carries `checks`: substrate-side reductions (not_all_black, differs_from_background, coverage, centroid, bounding_box) scored on the exact RGBA the PNG is built from and returned as a `verdict` text block alongside the image — a one-call spawn -> drive -> assert with no caller-side PNG decode. Each check entry optionally carries `region` ({min_x, min_y, max_x, max_y}) to restrict the reduction to the frame-clamped intersection of that rect instead of the whole frame — e.g. asserting a widget's fill lands inside its own screen rect rather than folding the whole scene into one number; coverage then divides by the clamped region's pixel count, and centroid/bounding_box still report absolute frame coordinates. Omit `region` to score the whole frame (the prior behavior). Optionally carries `similarity`: a reference-image check (`namespace` + `reference_path` + `threshold`) the render thread scores as a normalised mean-absolute-error against the captured RGBA, returned as `similarity_score` / `similarity_pass` text blocks alongside the image."
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
        // (e.g. an `aether.kit.mesh.load` pre-mail) encodes correctly
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
        description = "List the substrate kind vocabulary — both the static aether.* kinds and the engine's live capability + component kinds. engine_id selects which engine to query; omit it and the tool auto-resolves the sole supervised engine (the common single-substrate harness); with zero or many engines and no engine_id it returns the static substrate baseline. Default (no args) returns a compact [{name, shape}] JSON array where shape is a one-line field rendering — small and chunk-readable. prefix (case-sensitive starts_with) filters the listing to a kind family (e.g. \"aether.fs\" for just the fs kinds). full:true returns the full [{name, schema}] with the authoritative nested SchemaType; combine with prefix to bound the payload to the kinds a task needs."
    )]
    pub async fn describe_kinds(
        &self,
        Parameters(args): Parameters<DescribeKindsArgs>,
    ) -> Result<String, McpError> {
        // Resolve the target engine: explicit engine_id wins; when absent,
        // auto-resolve the sole supervised engine (the single-substrate
        // harness used in dogfood runs) so a bare describe_kinds() covers
        // that case without requiring the caller to know the engine_id.
        let engine = if let Some(id) = &args.engine_id {
            Some(parse_engine_id(id)?)
        } else {
            let reply = self
                .session
                .call_one(local_envelope(ENGINE_CAP, &ListEngines {}))
                .await
                .map_err(internal)?;
            let result = ListEnginesResult::decode_from_bytes(&reply.payload)
                .ok_or_else(|| internal_msg("undecodable ListEnginesResult"))?;
            // Auto-resolve only when exactly one engine is supervised;
            // zero or many is ambiguous — degrade to the static baseline.
            if result.engines.len() == 1 {
                result
                    .engines
                    .into_iter()
                    .next()
                    .map(|e| EngineId(Uuid::parse_str(&e.engine_id).unwrap_or_default()))
            } else {
                None
            }
        };

        // When an engine is in play, prefill its cache from the static
        // baseline then refresh from the live inventory.  The merged
        // snapshot (static ∪ capability-owned ∪ component-defined) is the
        // authoritative source.  When no engine resolves, fall back to the
        // static baseline unchanged.
        let descriptors: Vec<KindDescriptor> = if let Some(e) = engine {
            self.prefill_engine(e);
            self.refresh_engine_kinds(e).await;
            self.snapshot_engine_kinds(e).into_values().collect()
        } else {
            descriptors::all()
        };

        let filtered: Vec<_> = if let Some(prefix) = &args.prefix {
            descriptors
                .into_iter()
                .filter(|d| d.name.starts_with(prefix.as_str()))
                .collect()
        } else {
            descriptors
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
        description = "Describe a loaded component's receive-side capabilities (ADR-0033): the kinds it typed-handles with per-handler docs, whether it has a fallback catchall, its top-level doc, and (ADR-0090) its boot-config kind id+name when it declared a typed Config. Address the component by its lineage name (the aether.component/aether.embedded:NAME address spawn_substrate / list_components / load_component hand back) — a name resolves live against the substrate, so a boot-manifest-loaded component is fully introspectable without a prior load_component and survives an aether-mcp restart or an in-place replace_component. A tagged mbx- id is also accepted as a local cache fast-path."
    )]
    pub async fn describe_component(
        &self,
        Parameters(args): Parameters<DescribeComponentArgs>,
    ) -> Result<String, McpError> {
        let engine = parse_engine_id(&args.engine_id)?;
        // Resolve the input to a cache key (and, when it is a lineage name,
        // the forwardable name). A `mbx-` id parses directly; anything else
        // is a lineage name folded the same way send_mail resolves a
        // recipient and the substrate's `registry.lookup` resolves it
        // (`mailbox_id_from_path`), so the cache key agrees across all three.
        let (mailbox_id, forward_name) = if args.component.starts_with("mbx-") {
            (parse_mailbox_id(&args.component)?, None)
        } else {
            (
                mailbox_id_from_path(&args.component),
                Some(args.component.clone()),
            )
        };

        // Cache fast-path: populated by load_component / replace_component or
        // a prior name-resolved describe.
        let cached = self
            .components
            .lock()
            .expect("component cache mutex is never poisoned")
            .get(&(engine, mailbox_id))
            .cloned();
        if let Some(caps) = cached {
            return json(&caps);
        }

        // Cache miss. With a lineage name, ask the substrate live — this is
        // the load-bearing half: the cache is empty for a boot-loaded
        // component, but the substrate always holds the live loaded set. With
        // only a `mbx-` id there is no name to forward, so the cache was the
        // only source.
        let Some(name) = forward_name else {
            return Err(McpError::invalid_params(
                format!(
                    "no component cached at {} on engine {} — address by lineage name to resolve \
                     live, or load_component / replace_component to populate this cache",
                    args.component, args.engine_id
                ),
                None,
            ));
        };
        let reply = self
            .session
            .call_one(engine_envelope(
                engine,
                COMPONENT_CAP,
                &DescribeComponent { name: name.clone() },
            ))
            .await
            .map_err(internal)?;
        match DescribeComponentResult::decode_from_bytes(&reply.payload) {
            Some(DescribeComponentResult::Ok { capabilities }) => {
                self.components
                    .lock()
                    .expect("component cache mutex is never poisoned")
                    .insert((engine, mailbox_id), capabilities.clone());
                json(&capabilities)
            }
            Some(DescribeComponentResult::Err { error }) => Err(internal_msg(&error)),
            None => Err(internal_msg("undecodable DescribeComponentResult")),
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
                       \"aether.component/aether.embedded:aether.camera\"). `max` defaults to 100 and clamps to 1000; \
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

#[cfg(test)]
// Test-setup unwraps (tagged-id encode of literal ids, JSON build) panic
// on failure, which is the assertion.
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
    use aether_data::{mailbox_id_from_name, mailbox_id_from_path, with_tag};
    use aether_substrate::chassis::builder::{Builder, PassiveChassis};
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::Registry;
    use std::path::PathBuf;
    use std::process;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env as std_env, fs as std_fs};

    use crate::args::ActorLogsArgs;
    use aether_kinds::descriptors;
    use aether_substrate::testing::TestChassis;

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

    /// Small typed-config-shaped schema for component config encoding tests.
    fn config_struct_schema() -> SchemaType {
        use aether_data::NamedField;
        SchemaType::Struct {
            fields: vec![
                NamedField {
                    name: "seed".into(),
                    ty: SchemaType::Scalar(Primitive::U32),
                },
                NamedField {
                    name: "label".into(),
                    ty: SchemaType::String,
                },
            ]
            .into(),
            repr_c: false,
        }
    }

    fn config_kind(schema: &SchemaType) -> KindDescriptorWire {
        KindDescriptorWire {
            id: KindId(kind_id_from_parts("test.config", schema)),
            name: "test.config".to_owned(),
            schema_wire: wire::to_vec(schema).expect("SchemaType wire-encodes"),
        }
    }

    /// Write `bytes` to a unique temp file for the `$file` embed tests.
    /// The `std_env` / `std_fs` aliases avoid shadowing the module's
    /// `tokio::fs`.
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

    #[tokio::test]
    async fn component_config_inline_json_encodes_to_schema_bytes() {
        let schema = config_struct_schema();
        let kind = config_kind(&schema);
        let bytes = component_config_bytes(
            Some(&kind),
            Some(serde_json::json!({"seed": 7, "label": "demo"})),
            None,
            "test",
        )
        .await
        .expect("config encodes")
        .expect("source present");
        let decoded = aether_codec::decode_schema(&bytes, &schema).expect("config decodes");
        assert_eq!(decoded, serde_json::json!({"seed": 7, "label": "demo"}));
    }

    #[tokio::test]
    async fn component_config_path_is_json_and_encodes() {
        let path = stage_blob_file("config-json", br#"{"seed":9,"label":"from-file"}"#);
        let schema = config_struct_schema();
        let kind = config_kind(&schema);
        let bytes = component_config_bytes(
            Some(&kind),
            None,
            Some(path.to_str().expect("utf-8 temp path")),
            "test",
        )
        .await
        .expect("config_path JSON encodes")
        .expect("source present");
        let decoded = aether_codec::decode_schema(&bytes, &schema).expect("config decodes");
        assert_eq!(
            decoded,
            serde_json::json!({"seed": 9, "label": "from-file"})
        );
        std_fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn component_config_rejects_both_sources() {
        let schema = config_struct_schema();
        let kind = config_kind(&schema);
        let err = component_config_bytes(
            Some(&kind),
            Some(serde_json::json!({"seed": 7, "label": "demo"})),
            Some("/tmp/ignored.json"),
            "test",
        )
        .await
        .expect_err("both config sources must be rejected");
        assert!(
            err.to_string().contains("set only one"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn component_config_rejects_no_config_component() {
        let err = component_config_bytes(
            None,
            Some(serde_json::json!({"seed": 7, "label": "demo"})),
            None,
            "test",
        )
        .await
        .expect_err("config for no-config component must be rejected");
        assert!(
            err.to_string().contains("declares no Config kind"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn component_config_field_mismatch_is_invalid_params() {
        let schema = config_struct_schema();
        let kind = config_kind(&schema);
        let err = component_config_bytes(
            Some(&kind),
            Some(serde_json::json!({"seed": 7, "label": "demo", "extra": true})),
            None,
            "test",
        )
        .await
        .expect_err("field mismatch must be rejected");
        assert!(
            err.to_string().contains("does not match"),
            "unexpected error: {err}"
        );
    }

    /// A spill threshold so high nothing in these projection tests trips it —
    /// they assert the under-threshold UTF-8 / base64 ladder, unchanged.
    const NO_SPILL: usize = usize::MAX;

    #[test]
    fn render_bytes_reply_utf8_to_string() {
        let out = render_bytes_reply(serde_json::json!([104, 105]), &SchemaType::Bytes, NO_SPILL);
        assert_eq!(out, serde_json::json!("hi"));
    }

    #[test]
    fn render_bytes_reply_binary_to_base64() {
        // 0xff 0xfe is not valid UTF-8 → base64 object.
        let out = render_bytes_reply(serde_json::json!([255, 254]), &SchemaType::Bytes, NO_SPILL);
        assert_eq!(out, serde_json::json!({"base64": "//4="}));
    }

    #[test]
    fn render_bytes_reply_nested_in_struct() {
        let out = render_bytes_reply(
            serde_json::json!({"blob": [104, 105]}),
            &blob_struct_schema(),
            NO_SPILL,
        );
        assert_eq!(out, serde_json::json!({"blob": "hi"}));
    }

    /// Minimal `Result<Ok { bytes: Bytes }, Err>`-shaped enum schema — a
    /// stand-in for `aether.fs.read_result` that pins the enum-nested-Bytes
    /// regression (issue 2103).
    fn read_result_schema() -> SchemaType {
        use aether_data::{EnumVariant, NamedField};
        SchemaType::Enum {
            variants: vec![
                EnumVariant::Struct {
                    name: "Ok".into(),
                    discriminant: 0,
                    fields: vec![NamedField {
                        name: "bytes".into(),
                        ty: SchemaType::Bytes,
                    }]
                    .into(),
                },
                EnumVariant::Unit {
                    name: "Err".into(),
                    discriminant: 1,
                },
            ]
            .into(),
        }
    }

    #[test]
    fn render_bytes_reply_enum_struct_variant_utf8() {
        // `{"Ok": {"bytes": [104, 105]}}` → `{"Ok": {"bytes": "hi"}}`.
        // This is the `aether.fs.read_result` shape — the primary advertised
        // example of the bytes-render feature (issue 2103).
        let out = render_bytes_reply(
            serde_json::json!({"Ok": {"bytes": [104, 105]}}),
            &read_result_schema(),
            NO_SPILL,
        );
        assert_eq!(out, serde_json::json!({"Ok": {"bytes": "hi"}}));
    }

    #[test]
    fn render_bytes_reply_enum_struct_variant_binary() {
        // Binary bytes inside a struct variant render to a base64 object.
        let out = render_bytes_reply(
            serde_json::json!({"Ok": {"bytes": [255, 254]}}),
            &read_result_schema(),
            NO_SPILL,
        );
        assert_eq!(
            out,
            serde_json::json!({"Ok": {"bytes": {"base64": "//4="}}})
        );
    }

    #[test]
    fn render_bytes_reply_enum_unit_variant_passthrough() {
        // `"Err"` is a bare-string Unit variant — no payload, passes through.
        let out = render_bytes_reply(serde_json::json!("Err"), &read_result_schema(), NO_SPILL);
        assert_eq!(out, serde_json::json!("Err"));
    }

    #[test]
    fn render_bytes_reply_enum_unknown_tag_passthrough() {
        // An unrecognised tag passes through untouched — the walker is
        // best-effort and must never drop data.
        let out = render_bytes_reply(
            serde_json::json!({"Unknown": {"x": 1}}),
            &read_result_schema(),
            NO_SPILL,
        );
        assert_eq!(out, serde_json::json!({"Unknown": {"x": 1}}));
    }

    /// A unique scratch directory under the system temp dir, so reply-spill
    /// tests never litter the real temp dir with `aether-reply-*.bin` files.
    fn reply_scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir =
            std_env::temp_dir().join(format!("aether-reply-test-{tag}-{}-{nanos}", process::id()));
        std_fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn render_bytes_leaf_over_threshold_spills_to_file() {
        // A reply Bytes leaf over the threshold spills to a host temp file and
        // renders as `{"file": <path>}`; the file is present and byte-equal.
        let dir = reply_scratch_dir("over-threshold");
        let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let json: Vec<serde_json::Value> = payload.iter().map(|b| serde_json::json!(b)).collect();
        let out = render_bytes_leaf_in(serde_json::Value::Array(json), 1024, &dir);
        let file = out
            .get("file")
            .and_then(|v| v.as_str())
            .expect("over-threshold leaf renders as a {\"file\": …} reference");
        let on_disk = std_fs::read(file).expect("spilled file is present on disk");
        assert_eq!(on_disk, payload, "spilled bytes match the input");
        std_fs::remove_file(file).ok();
        std_fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_bytes_leaf_under_threshold_utf8_to_string() {
        let dir = reply_scratch_dir("under-utf8");
        let out = render_bytes_leaf_in(serde_json::json!([104, 105]), 1024, &dir);
        assert_eq!(out, serde_json::json!("hi"));
        // Nothing should have been written.
        assert!(
            std_fs::read_dir(&dir)
                .expect("scratch dir")
                .next()
                .is_none()
        );
        std_fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_bytes_leaf_under_threshold_binary_to_base64() {
        let dir = reply_scratch_dir("under-binary");
        // 0xff 0xfe is not valid UTF-8 and is under the threshold → base64.
        let out = render_bytes_leaf_in(serde_json::json!([255, 254]), 1024, &dir);
        assert_eq!(out, serde_json::json!({"base64": "//4="}));
        assert!(
            std_fs::read_dir(&dir)
                .expect("scratch dir")
                .next()
                .is_none()
        );
        std_fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_bytes_leaf_spill_io_failure_falls_back_to_base64() {
        // A spill dir that doesn't exist makes `std::fs::write` fail; the leaf
        // must fall through to the in-band rendering rather than error or drop
        // data. 0xff bytes are non-UTF-8 → the fallback is base64.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let missing = std_env::temp_dir().join(format!(
            "aether-reply-test-missing-{}-{nanos}",
            process::id()
        ));
        let payload: Vec<u8> = vec![0xffu8; 64];
        let json: Vec<serde_json::Value> = payload.iter().map(|b| serde_json::json!(b)).collect();
        let out = render_bytes_leaf_in(serde_json::Value::Array(json), 8, &missing);
        assert_eq!(
            out,
            serde_json::json!({"base64": STANDARD.encode(&payload)})
        );
        assert!(
            !missing.exists(),
            "the missing spill dir must not be created by the fallback"
        );
    }

    #[test]
    fn render_bytes_reply_threads_threshold_to_leaf() {
        // End-to-end: the threshold threaded through `render_bytes_reply`
        // reaches the leaf and triggers a spill. (Writes to the real temp dir
        // since the public entry uses `env::temp_dir()`; cleaned up below.)
        let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let json: Vec<serde_json::Value> = payload.iter().map(|b| serde_json::json!(b)).collect();
        let out = render_bytes_reply(serde_json::Value::Array(json), &SchemaType::Bytes, 1024);
        let file = out
            .get("file")
            .and_then(|v| v.as_str())
            .expect("threaded threshold spills the leaf to a file reference");
        let on_disk = std_fs::read(file).expect("spilled file is present on disk");
        assert_eq!(on_disk, payload);
        std_fs::remove_file(file).ok();
    }

    #[tokio::test]
    async fn resolve_bytes_nested_in_enum_struct_variant() {
        // A `$text` embed inside an enum struct variant resolves to a byte
        // array — the request-side mirror of the render regression.
        let out = resolve_bytes_params(
            serde_json::json!({"Ok": {"bytes": {"$text": "hi"}}}),
            &read_result_schema(),
            NO_CAP,
        )
        .await
        .expect("$text embed nested in enum struct variant resolves");
        assert_eq!(out, serde_json::json!({"Ok": {"bytes": [104, 105]}}));
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
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
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
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
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

    /// Hub-shape chassis whose `aether.engine` mailbox is a
    /// [`RouteInventorySink`] loopback (issue 2672) rather than the real
    /// `EngineServer`, so the harness's `engine = Some`
    /// `aether.inventory.kinds` refresh RPC lands locally and returns
    /// `reply`. `calls` counts the refreshes the sink fielded, so a test
    /// can assert the refresh-and-retry fired exactly once (no loop).
    /// Unlike `boot_hub_with_inventory` this installs no `EngineServer`
    /// (which would warn/settle-err an `engine = Some` for an unregistered
    /// engine) and no `InventoryCapability` (the sink answers `ListKinds`
    /// from the canned reply directly).
    fn boot_hub_with_route_loopback(
        reply: ListKindsResult,
        calls: Arc<AtomicUsize>,
    ) -> (PassiveChassis<TestChassis>, u16) {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let (outbound, _rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<RouteInventorySink>(RouteLoopbackConfig { reply, calls })
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

    /// One-field `{ button: String }` struct schema — the widened shape a
    /// kind gains in place (issue 2672); the narrow shape is the empty
    /// struct `narrow_struct_schema`.
    fn widened_struct_schema() -> SchemaType {
        use aether_data::NamedField;
        SchemaType::Struct {
            fields: vec![NamedField {
                name: "button".into(),
                ty: SchemaType::String,
            }]
            .into(),
            repr_c: false,
        }
    }

    /// The empty-struct schema a widened kind had *before* it gained a
    /// field — the stale shape the harness holds cached (issue 2672).
    fn narrow_struct_schema() -> SchemaType {
        SchemaType::Struct {
            fields: vec![].into(),
            repr_c: false,
        }
    }

    /// Build the single-entry `ListKindsResult` a `RouteInventorySink`
    /// serves for `name` at `schema` — the wire projection the real
    /// inventory cap performs (issue 2672).
    fn canned_kinds_reply(name: &str, schema: &SchemaType) -> ListKindsResult {
        use aether_kinds::KindDescriptorWire;
        ListKindsResult {
            kinds: vec![KindDescriptorWire {
                id: KindId(kind_id_from_parts(name, schema)),
                name: name.to_owned(),
                schema_wire: wire::to_vec(schema).expect("SchemaType wire-encodes"),
            }],
        }
    }

    /// Issue 2672: a kind widened in place (same name, a new field) —
    /// the harness holds the stale narrow descriptor, so the name
    /// resolves to a cache hit and `lookup_descriptor` never refreshes.
    /// The field-mismatch `encode_schema` failure must itself trigger the
    /// `aether.inventory.kinds` refresh-and-retry, so a `send_mail` that
    /// supplies the new field succeeds against the widened schema. The
    /// last correctness gap in ADR-0091's lazy-on-miss cache.
    #[tokio::test]
    async fn resolve_and_encode_refreshes_on_field_mismatch() {
        use aether_data::KindDescriptor;

        let name = "aether.test.widened_kind";
        let widened = widened_struct_schema();

        // The live engine's vocabulary carries the widened shape.
        let calls = Arc::new(AtomicUsize::new(0));
        let (_chassis, port) =
            boot_hub_with_route_loopback(canned_kinds_reply(name, &widened), Arc::clone(&calls));
        let mcp = connect_mcp(port);

        // Pre-seed the per-engine cache with the STALE narrow shape, so
        // the name is a cache hit (no unknown-kind-miss refresh) — only
        // the encode failure can drive the refresh.
        let engine = EngineId(Uuid::from_u128(0x2672_dead_beef));
        mcp.merge_into_engine_cache(
            engine,
            vec![KindDescriptor {
                name: name.to_owned(),
                schema: narrow_struct_schema(),
            }],
        );

        // Params carrying the new field: rejected by the narrow cached
        // schema, accepted by the widened live one.
        let params = serde_json::json!({ "button": "left" });
        let (desc, payload) = mcp
            .resolve_and_encode(engine, name, params.clone())
            .await
            .expect("field-mismatch encode failure refreshes and retries");

        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "the field-mismatch triggered exactly one refresh RPC",
        );
        assert_eq!(
            desc.schema, widened,
            "resolve_and_encode returns the fresh (widened) descriptor",
        );
        let decoded = aether_codec::decode_schema(&payload, &widened)
            .expect("payload decodes against the widened schema");
        assert_eq!(
            decoded, params,
            "the new field round-trips through the refreshed schema",
        );
    }

    /// Issue 2672: the refresh-and-retry is bounded to exactly one
    /// refresh — when the fresh vocabulary *still* rejects the params (a
    /// field that isn't in the live schema either, not an in-place
    /// widening), `resolve_and_encode` surfaces the error after a single
    /// refresh rather than looping. The tripwire for the "retry-once"
    /// invariant.
    #[tokio::test]
    async fn resolve_and_encode_retry_is_bounded_to_one_refresh() {
        use aether_data::KindDescriptor;

        let name = "aether.test.narrow_kind";

        // The live vocabulary is *also* narrow — the refresh changes
        // nothing, so the re-encode fails identically.
        let calls = Arc::new(AtomicUsize::new(0));
        let (_chassis, port) = boot_hub_with_route_loopback(
            canned_kinds_reply(name, &narrow_struct_schema()),
            Arc::clone(&calls),
        );
        let mcp = connect_mcp(port);

        let engine = EngineId(Uuid::from_u128(0x2672_beef_cafe));
        mcp.merge_into_engine_cache(
            engine,
            vec![KindDescriptor {
                name: name.to_owned(),
                schema: narrow_struct_schema(),
            }],
        );

        let params = serde_json::json!({ "button": "left" });
        let result = mcp.resolve_and_encode(engine, name, params).await;

        assert!(
            result.is_err(),
            "a field the fresh vocab still lacks surfaces an error, not a hang",
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "the retry refreshed exactly once — no loop",
        );
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
                    config: None,
                    config_path: None,
                    export: None,
                    replicas: None,
                }],
            }))
            .await;
        assert!(
            result.is_err(),
            "an unresolvable component selector should abort the spawn as a tool error",
        );
    }

    /// `spawn_substrate` rejects `replicas: 0` on a boot-list component
    /// entry (issue 2626, ADR-0090 §4 posture) before any selector
    /// resolution or fork — a bad known value is a hard tool error, never
    /// a silent zero-instance no-op.
    #[tokio::test]
    async fn spawn_substrate_replicas_zero_is_tool_error() {
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
                    selector: "irrelevant".to_owned(),
                    name: None,
                    config: None,
                    config_path: None,
                    export: None,
                    replicas: Some(0),
                }],
            }))
            .await;
        assert!(
            result.is_err(),
            "replicas: 0 must be a tool error, not a silent no-op",
        );
    }

    /// `components_all_loaded` checks membership, not count. The wrong-set
    /// false positive: `actual` has one name (satisfying a count-`>= 1` check)
    /// but it is NOT the name in `want` — membership returns false. This is the
    /// regression the count-based `wait_for_loaded_components` would silently
    /// pass: a non-requested trampoline (B) registers while the requested
    /// component (A) stalls, and the count hits the threshold before A is up.
    /// After the identity-based fix, only A's presence in `actual` satisfies
    /// the check.
    #[test]
    fn components_all_loaded_wrong_set_is_not_ready() {
        let want = vec!["aether.component/aether.embedded:wanted".to_owned()];
        let actual = vec!["aether.component/aether.embedded:other".to_owned()];
        assert!(
            !components_all_loaded(&want, &actual),
            "a non-requested trampoline present while the requested one is absent \
             must not satisfy the identity check (count-based would pass)",
        );
    }

    /// `components_all_loaded` returns true once every wanted name is present,
    /// and handles the empty-want case (no components requested → trivially
    /// ready).
    #[test]
    fn components_all_loaded_exact_match_is_ready() {
        let want = vec![
            "aether.component/aether.embedded:alpha".to_owned(),
            "aether.component/aether.embedded:beta".to_owned(),
        ];
        let actual = vec![
            "aether.component/aether.embedded:baseline".to_owned(),
            "aether.component/aether.embedded:alpha".to_owned(),
            "aether.component/aether.embedded:beta".to_owned(),
        ];
        assert!(
            components_all_loaded(&want, &actual),
            "both wanted names present (alongside an extra baseline) should be ready",
        );
        assert!(
            components_all_loaded(&[], &[]),
            "empty want is trivially ready",
        );
    }

    /// `components_all_loaded` is false when only a subset of the wanted names
    /// is present — a stalled-requested case where one component comes up but
    /// another does not.
    #[test]
    fn components_all_loaded_partial_match_is_not_ready() {
        let want = vec![
            "aether.component/aether.embedded:alpha".to_owned(),
            "aether.component/aether.embedded:stalled".to_owned(),
        ];
        let actual = vec!["aether.component/aether.embedded:alpha".to_owned()];
        assert!(
            !components_all_loaded(&want, &actual),
            "only one of two wanted names present means the engine is not yet ready",
        );
    }

    /// `replica_base_name` follows the same precedence the component host
    /// applies at load: caller `name` wins over `export`, which wins over
    /// the entry actor namespace — the bug this catches is a fan-out base
    /// name that disagrees with what an unreplicated load would resolve to.
    #[test]
    fn replica_base_name_follows_name_export_namespace_precedence() {
        assert_eq!(
            replica_base_name(Some("caller"), Some("export-ns"), Some("entry-ns")),
            Some("caller".to_owned()),
        );
        assert_eq!(
            replica_base_name(None, Some("export-ns"), Some("entry-ns")),
            Some("export-ns".to_owned()),
        );
        assert_eq!(
            replica_base_name(None, None, Some("entry-ns")),
            Some("entry-ns".to_owned()),
        );
        assert_eq!(replica_base_name(None, None, None), None);
    }

    /// `replica_names` suffixes every instance — no bare-name special case
    /// for index 0 — so `replicas: 1` differs from an omitted field only by
    /// the `-0` suffix.
    #[test]
    fn replica_names_suffixes_every_instance() {
        assert_eq!(
            replica_names("handler", 3),
            vec!["handler-0", "handler-1", "handler-2"],
        );
        assert_eq!(replica_names("handler", 1), vec!["handler-0"]);
    }

    /// `reject_zero_replicas` is a hard tool error on `replicas: 0` (ADR-0090
    /// §4 posture — a bad known value aborts loudly, not a silent no-op) and
    /// passes through any other value, including `None`.
    #[test]
    fn reject_zero_replicas_rejects_only_zero() {
        assert!(reject_zero_replicas(Some(0), "sel").is_err());
        assert!(reject_zero_replicas(Some(1), "sel").is_ok());
        assert!(reject_zero_replicas(None, "sel").is_ok());
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

    /// `describe_kinds` with no `engine_id` and an empty hub returns the
    /// substrate static inventory.  The default (compact) result is a
    /// non-empty JSON array of `{name,shape}` objects.
    #[tokio::test]
    async fn describe_kinds_returns_the_substrate_inventory() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let out = mcp
            .describe_kinds(Parameters(DescribeKindsArgs {
                engine_id: None,
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
                engine_id: None,
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
                engine_id: None,
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
                engine_id: None,
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

    /// Regression guard for the `describe_kinds` live-path bug (issue 2420):
    /// a component-defined kind that is absent from `descriptors::all()`
    /// appears in the `describe_kinds` output when an `engine_id` is supplied
    /// and the per-engine cache holds that kind.
    ///
    /// Why this catches the bug: on buggy `main` `describe_kinds` returns
    /// `descriptors::all()` regardless of `engine_id`, so the
    /// component-defined kind never appears. On fixed code the function reads
    /// the engine's cache (prefill + `refresh_engine_kinds` + snapshot), which
    /// is pre-seeded here with the component kind before the call.
    ///
    /// Note on the test-binary link hazard: `aether-capabilities` is a
    /// dev-dependency, so `descriptors::all()` in the *test* binary already
    /// materialises the capability families (`aether.fs.*`, `aether.audio.*`,
    /// …) that the *production* binary omits.  A test that only asserts
    /// `aether.fs.*` appears would pass on buggy `main` in this binary.
    /// Asserting a *component-defined* kind (not a capability family) avoids
    /// that hazard: component kinds are inherently absent from
    /// `descriptors::all()` in both the production and the test binary.
    ///
    // Tripwire: describe_kinds surfaces an engine's live kinds, not just the
    // link-time set.
    #[tokio::test]
    async fn describe_kinds_live_path_surfaces_component_defined_kind() {
        use aether_data::{KindDescriptor, SchemaType};

        let component_kind = KindDescriptor {
            name: "test.issue_2420.uniquely_named_kind".to_owned(),
            schema: SchemaType::String,
        };

        // Pre-condition: absent from the static vocabulary in both the
        // production and the test binary — ensures the assertion below
        // can only pass if describe_kinds reads the engine cache.
        assert!(
            !descriptors::all()
                .iter()
                .any(|d| d.name == component_kind.name),
            "test invariant: the component kind must not be in descriptors::all()",
        );

        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);

        // Use a synthetic but well-formed engine UUID so parse_engine_id
        // accepts it; the hub doesn't supervise it, so refresh_engine_kinds
        // fails silently (ok().and_then() path), leaving the pre-seeded
        // entry intact.
        let engine = EngineId(Uuid::from_u128(0x2420_dead_beef));
        let engine_id_str = engine.0.to_string();

        // Pre-seed the per-engine cache as load_component / refresh_engine_kinds
        // would after a component with this kind is loaded.
        mcp.merge_into_engine_cache(engine, vec![component_kind.clone()]);

        let out = mcp
            .describe_kinds(Parameters(DescribeKindsArgs {
                engine_id: Some(engine_id_str),
                prefix: None,
                full: false,
            }))
            .await
            .expect("describe_kinds ok with engine_id");
        let arr: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
        assert!(
            arr.iter()
                .any(|e| e["name"].as_str() == Some(&component_kind.name)),
            "describe_kinds must surface the component-defined kind from the engine cache; \
             got names: {:?}",
            arr.iter()
                .filter_map(|e| e["name"].as_str())
                .collect::<Vec<_>>(),
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
                config: None,
                config_path: None,
                export: None,
                replicas: None,
            }))
            .await;
        assert!(
            result.is_err(),
            "an unresolvable selector should be a tool error",
        );
    }

    /// `load_component` rejects `replicas: 0` (issue 2626, ADR-0090 §4
    /// posture) before it ever resolves the selector — a bad known value
    /// is a hard tool error, never a silent zero-instance no-op.
    #[tokio::test]
    async fn load_component_replicas_zero_is_tool_error() {
        let (_chassis, port) = boot_hub();
        let mcp = connect_mcp(port);
        let result = mcp
            .load_component(Parameters(LoadComponentArgs {
                engine_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                selector: "irrelevant".to_owned(),
                name: None,
                config: None,
                config_path: None,
                export: None,
                replicas: Some(0),
            }))
            .await;
        assert!(
            result.is_err(),
            "replicas: 0 must be a tool error, not a silent no-op",
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
                config: None,
                config_path: None,
                export: None,
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

        // Empty cache, addressed by `mbx-` id → error (no name to forward
        // live, so the cache is the only source).
        let miss = mcp
            .describe_component(Parameters(DescribeComponentArgs {
                engine_id: engine_id.to_owned(),
                component: tagged.clone(),
            }))
            .await;
        assert!(
            miss.is_err(),
            "an uncached component addressed by id should be a tool error"
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
                component: tagged,
            }))
            .await
            .expect("cached component describes");
        let caps: serde_json::Value = serde_json::from_str(&hit).expect("json");
        assert!(caps.get("handlers").is_some(), "capabilities shape: {hit}");
        assert!(
            !caps["handlers"][0]["reply"].is_null(),
            "the handler's ADR-0109 reply contract is surfaced: {hit}"
        );

        // Name-addressed describe resolves the lineage name to the SAME
        // cache key the substrate registers under (`mailbox_id_from_path`,
        // the fold `registry.lookup` uses), so a cache seeded under that key
        // is found by name without a `mbx-` id. This is the MCP half of the
        // boot-manifest path; the live substrate forward-on-miss is covered
        // end-to-end by FleetBench (it needs a real loaded component).
        let lineage = "aether.component/aether.embedded:fake_component";
        let by_name_key = mailbox_id_from_path(lineage);
        let named_caps = ComponentCapabilities {
            handlers: vec![aether_kinds::HandlerCapability {
                id: KindId(0x33),
                name: "test.by_name".to_owned(),
                doc: None,
                reply: aether_data::ReplyContract::None,
            }],
            ..ComponentCapabilities::default()
        };
        mcp.components
            .lock()
            .expect("test setup: component cache mutex is never poisoned")
            .insert((engine, by_name_key), named_caps);
        let by_name = mcp
            .describe_component(Parameters(DescribeComponentArgs {
                engine_id: engine_id.to_owned(),
                component: lineage.to_owned(),
            }))
            .await
            .expect("name-addressed describe resolves to the cached caps");
        let by_name_json: serde_json::Value = serde_json::from_str(&by_name).expect("json");
        assert_eq!(
            by_name_json["handlers"][0]["name"], "test.by_name",
            "a lineage name resolves to the substrate-consistent cache key: {by_name}"
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
    /// state for a component-defined kind like `aether.kit.mesh.load` —
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
            wire::from_bytes(&entry.schema_wire).expect("schema_wire decodes");
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
