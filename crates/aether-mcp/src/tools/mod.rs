//! The `aether-mcp` tool surface: the per-session [`Mcp`] service, its
//! `#[tool_router]` impl, and the `ServerHandler` (issue 763 P5b/P5c).
//!
//! Each tool translates to RPC `Call`s over the shared [`RpcSession`].
//! Engine-management tools (`list_engines`, `spawn_substrate`,
//! `terminate_substrate`) address the hub's own `aether.fleet` cap
//! (`engine = None`, dispatched locally on the hub); the per-engine
//! tools (`send_mail`, `load_component`, `replace_component`,
//! `capture_frame`) address a specific substrate (`engine = Some`),
//! which the hub routes through to the matching proxy. `describe_kinds`
//! queries the live engine's `aether.inventory.kinds` mailbox so it
//! surfaces capability-owned and component-defined kinds, falling back to
//! the static substrate baseline only when no engine is reachable.
//! `describe_component` answers locally from a component-capability cache
//! populated by `load_component` / `replace_component`.

use std::collections::HashMap;
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
    EngineId, Kind, KindDescriptor, KindId, MailboxId, ScopePathError, Tag, Uuid, mailbox_id_from_name, tagged_id,
    validate_scope_path,
};
use aether_data::{EnumVariant, Primitive, SchemaType};
use aether_inventory::kinds::{ListKinds, ListKindsResult, ResolveAddress, ResolveAddressResult};
#[cfg(test)]
use aether_kinds::KindDescriptorWire;
use aether_kinds::{
    ComponentCapabilities, ComponentSelector, DeathReason, FallbackCapability, HandlerCapability, NamedMail,
    ResolveComponent, ResolveComponentResult, trace::MailNodeWire,
};
use aether_rpc::{MailEnvelope, MailboxAddress};
#[cfg(test)]
use base64::Engine as _;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::args::ActorCostArgs;
use crate::args::ActorLogsArgs;
use crate::args::{
    ApplyTerrainBrushArgs, CaptureFrameArgs, CollectFailureEvidenceArgs, CommitTerrainProposalArgs, ComponentSpec,
    DescribeComponentArgs, DescribeHandlersArgs, DescribeKindsArgs, DiscardTerrainProposalArgs, EngineMailSpec,
    ListBinariesArgs, ListComponentsArgs, ListEnginesArgs, LoadComponentArgs, MailIdJson, MailNodeJson, MailSpec,
    ProposeTerrainEditArgs, ReplaceComponentArgs, ReplyEventJson, ReplyProjection, RunTerrainAutomatonArgs,
    SendMailArgs, SendMailTracedArgs, SetTerrainProposalPreviewArgs, SpawnSubstrateArgs, TerminateSubstrateArgs,
    TerrainEditorArgs, TerrainMarksArgs, UploadBinaryArgs, UploadComponentArgs,
};
use crate::reverse::EngineNames;
use crate::rpc::RpcSession;
use aether_inventory::kinds::{Manifest, ManifestResult, Resolve, ResolveResult};
use aether_kinds::descriptors;
#[cfg(test)]
use base64::engine::general_purpose::STANDARD;
#[cfg(test)]
use std::time::Duration;

mod bytes;
mod capture;
mod components;
mod describe;
mod engine;
mod envelope;
mod failure_evidence;
mod ids;
mod logs_cost;
mod mail;
mod render;
mod reply;
mod state;
mod terrain;

#[cfg(test)]
use self::bytes::{
    RESPONSE_SUMMARY_MAX_BYTES, render_bytes_leaf_in, render_bytes_reply, reply_inline_max_bytes_from,
    resolve_bytes_params, response_inline_max_bytes_from, spill_oversized_response_in, summarize_response,
};
#[cfg(test)]
use self::capture::{
    CaptureImageDimensions, CaptureImageOptions, capture_content, downsample_rgba, resize_target_dimensions,
    resolve_capture_image_options, rgba_offset, save_capture_png,
};
#[cfg(test)]
use self::components::{MAX_REPLICAS, components_all_loaded, reject_replicas_out_of_range, replicas_reply};
use self::components::{
    component_config_bytes, reject_zero_replicas, replica_base_name, replica_names, selector_with_explicit_export,
};
use self::envelope::{engine_envelope, local_envelope, validate_recipient_scope};
#[cfg(test)]
use self::failure_evidence::{
    FAILURE_EVIDENCE_LOG_ENTRIES, FailureEvidenceQuery, FailureEvidenceSource, FailureEvidenceValue, MAX_ADDRESS_BYTES,
    MAX_FAILURE_EVIDENCE_ACTORS, MAX_FAILURE_EVIDENCE_COMPONENTS, MAX_FAILURE_EVIDENCE_KINDS, MAX_KIND_NAME_BYTES,
    MAX_OPERATION_BYTES, MAX_PRIMARY_ERROR_BYTES, collect_failure_evidence_with_source, failure_evidence_capture_args,
    failure_evidence_result_with_spill, project_failure_evidence_capture, select_failure_evidence_fleet,
    validate_failure_evidence_args,
};
#[cfg(test)]
use self::ids::{parse_kind_id, render_compact_tree, static_kind_name};
#[cfg(test)]
use self::logs_cost::{actor_logs_err_message, level_to_str, parse_level};
#[cfg(test)]
use self::render::project_capabilities;
#[cfg(test)]
use self::render::render_shape;
use self::render::{frame_size_aware_error, internal_msg};
#[cfg(test)]
use self::reply::{decode_reply_events, is_error_reply, project_replies};
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
const FLEET_CAP: &str = "aether.fleet";
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
        Self { session, components, names, kinds, tool_router: Self::tool_router() }
    }
}

fn guard_response_size(tool: &str, result: Result<String, McpError>) -> Result<String, McpError> {
    result.map(|body| bytes::spill_oversized_response(tool, body))
}

#[tool_router]
impl Mcp {
    #[tool(
        description = "List every engine the hub currently supervises, plus a recently-died sidecar. Returns an object {engines, recently_died}: each `engines` item reports the engine_id (pass it to send_mail / terminate_substrate) and the localhost RPC port the hub assigned its substrate; each `recently_died` item reports a departed engine with why it left — reason \"terminated\" (a deliberate terminate_substrate), \"crashed\" (the substrate closed its connection), \"evicted\" (a missed-heartbeat eviction), or \"spawn_failed\" (a spawn that never connected — the substrate failed to come up) — plus a detail string and how long ago it left, so a clean shutdown is distinguishable from a failure. Pass `show` to filter which lists come back so a routine liveness check doesn't pay the context cost of the whole fleet history: \"alive\" returns only {engines}, \"dead\" returns only {recently_died}, and \"all\" (the default when omitted) returns both. The unasked list is absent from the reply JSON, not present-but-empty. Any other `show` value is a tool error naming the three accepted values."
    )]
    pub async fn list_engines(&self, Parameters(args): Parameters<ListEnginesArgs>) -> Result<String, McpError> {
        guard_response_size("list_engines", engine::list_engines(self, args).await)
    }

    #[tool(
        description = "Fork+exec a substrate binary as a child of the hub, resolved from the hub's content-addressed binary store (ADR-0115) — not a host path. Pass `selector` to pick the binary: a content `hash`, a `name@version`, or a `name` (upload_binary first if it isn't stored). Omit `selector` to select the stored `default` headless chassis; the standard tunnel helper stages it, but a bare spawn returns a selector-resolution error when no matching artifact exists. When `selector` is omitted you may instead attribute-query with `chassis` (\"headless\"/\"desktop\"/\"hub\"), `caps` (linked-cap superset), and `target` (build triple). The hub resolves the selector to the stored bytes, materializes them to an executable temp file, assigns a free localhost RPC port (addressed to the child as --rpc-port argv, ADR-0162; the child's environment is constructed from an allowlist at fork, never inherited, so no AETHER_* key crosses), forks it, and connects a proxy. Returns the engine_id and rpc_port on success; errors if the selector resolves to no stored binary or the substrate fails to come up. A spawn that fails after the hub allocated an engine_id carries that id in the error (and records a matching spawn_failed entry in list_engines.recently_died), so you can correlate and reap rather than guess. Pass `components` (each {selector, name?, config?, config_path?, export?, replicas?}) to bring the engine up with those components already loaded in one call — each selector is a content hash, name, or module@actor resolved against the hub's component registry (ADR-0116; upload_component first). `config` is inline JSON and `config_path` is a JSON file path; aether-mcp schema-encodes either one to the component's Config kind, stages the resulting bytes next to the staged wasm, and writes a boot-manifest the hub addresses to the child as --boot-manifest argv. The spawned substrate reads the staged wasm/config byte files itself (single-host), so no follow-up load_component is needed. Set replicas: N on an entry (issue 2626) to fan it out into N instances at boot from one spec, one shared config — each named {base}-{index} for index in 0..N (base = name > export > default actor namespace) — and readiness waits for every expected lineage string to appear. That check does not deduplicate colliding expected names or compare live bytes with the requested selector, so give every boot instance a unique derived name and describe or safely probe each one after spawn. This pairs with #[router(shared)] (ADR-0136) to scale an HTTP handler to N instances with no hand-named entries. replicas: 0 is a tool error. Pass `mails` (each {recipient_name, kind_name, params?} — a send_mail item without engine_id) to dispatch init mail once the engine and every boot component are ready, so world bring-up — create a camera, load a mesh, seed state — completes in this same call: each item is settled like a send_mail item (chain awaited, terminal replies kept) and reported per-item in the response's `mails` list, so a failed init surfaces here rather than at first observation. Items encode after boot against the live engine's merged kind view, so boot components' own kinds resolve; a bad entry becomes that item's error status and aborts neither the spawn nor its siblings. Keep capture_frame.mails for frame-scoped placement at observation time, not initialization. With no `mails` the response stays the bare engine-info shape."
    )]
    pub async fn spawn_substrate(&self, Parameters(args): Parameters<SpawnSubstrateArgs>) -> Result<String, McpError> {
        guard_response_size("spawn_substrate", engine::spawn_substrate(self, args).await)
    }

    #[tool(
        description = "Terminate a substrate the hub supervises. The hub forwards the request to the engine's proxy, which SIGKILLs the child process and self-shuts-down."
    )]
    pub async fn terminate_substrate(
        &self,
        Parameters(args): Parameters<TerminateSubstrateArgs>,
    ) -> Result<String, McpError> {
        guard_response_size("terminate_substrate", engine::terminate_substrate(self, args).await)
    }

    #[tool(
        description = "Upload a binary into the hub's content-addressed store (ADR-0115). Pass `staged_path` — an absolute path to the binary on the fleet host — and an optional `name`. The hub reads the path itself (aether-mcp never reads the bytes — a binary is too large for the tool channel), sha256-hashes it, dedups against the store (a re-upload of identical bytes returns the same hash), forks `<binary> --describe` to capture its manifest (chassis kind, linked caps, build provenance), stores both, and points `name` (when given) at the hash. The store persists across a restart-hub. Returns {hash, name}."
    )]
    pub async fn upload_binary(&self, Parameters(args): Parameters<UploadBinaryArgs>) -> Result<String, McpError> {
        guard_response_size("upload_binary", components::upload_binary(self, args).await)
    }

    #[tool(
        description = "Enumerate the hub's stored binaries (ADR-0115). Optional AND-combined filters: `chassis` (\"headless\"/\"desktop\"/\"hub\"), `caps` (keep only binaries whose linked caps are a superset of every listed cap), and `target` (the build target triple). By default returns the newest 20 name-pointed registry entries; set `include_history: true` to include unnamed historical hashes and set `limit` explicitly to change the cap (`0` returns no entries but preserves the count). Returns {entries: [{hash, name, manifest: {chassis, caps, git_sha, profile, target}}], total_matched, shown, truncated, notice}. To retrieve a complete matched history, first call with `include_history: true`, then repeat with `limit` equal to `total_matched`."
    )]
    pub async fn list_binaries(&self, Parameters(args): Parameters<ListBinariesArgs>) -> Result<String, McpError> {
        guard_response_size("list_binaries", components::list_binaries(self, args).await)
    }

    #[tool(
        description = "Upload a WASM component into the hub's content-addressed store (ADR-0116). Pass `staged_path` — an absolute path to the component .wasm on the fleet host — and an optional `name` (the component's Actor::NAMESPACE is the natural one). The hub reads the path itself (aether-mcp never reads the bytes — too large for the tool channel), sha256-hashes it, dedups against the store (a re-upload of identical bytes returns the same hash), reads its manifest straight from the wasm (no execution step — exported actor namespaces, handled kind ids, #[fallback] presence, build provenance), stores both, and points `name` (when given) at the hash. The store persists across a restart-hub. Then load it by selector with load_component — the host wasm path is gone from load_component / replace_component / boot manifests, surviving only here as the upload input. Returns {hash, name}."
    )]
    pub async fn upload_component(
        &self,
        Parameters(args): Parameters<UploadComponentArgs>,
    ) -> Result<String, McpError> {
        guard_response_size("upload_component", components::upload_component(self, args).await)
    }

    #[tool(
        description = "Enumerate the hub's stored component registry (ADR-0116). Optional AND-combined filters: `namespace` (keep only components exporting an actor with that Actor::NAMESPACE) and `handled_kind` (keep only components handling that kind, by tagged knd-… id or kind name). By default returns the newest 20 name-pointed registry entries; set `include_history: true` to include unnamed historical hashes and set `limit` explicitly to change the cap (`0` returns no entries but preserves the count). Returns {entries: [{hash, name, manifest: {namespaces, actors: [{namespace, handled_kinds, fallback}], fallback, provenance, default_entry}}], total_matched, shown, truncated, notice}. Actor handled kinds render as readable static names with tagged knd-… fallbacks; the redundant manifest-wide handled-kind union is omitted. To retrieve a complete matched history, first call with `include_history: true`, then repeat with `limit` equal to `total_matched`. These are stored artifacts, not loaded component lineage addresses. There is no live-instance list: retain names/mailbox ids from load_component, or derive boot lineage names from an explicit boot spec and the stored manifest, then use describe_component by lineage name."
    )]
    pub async fn list_components(&self, Parameters(args): Parameters<ListComponentsArgs>) -> Result<String, McpError> {
        guard_response_size("list_components", components::list_components(self, args).await)
    }

    #[tool(
        description = "Send one or more mail items to substrate mailboxes. `recipient_name` accepts a canonical lineage, an unambiguous ADR-0166 `root.namespace://relative` abbreviation, or a tagged mbx- id; textual addresses are resolved for liveness by the selected engine and are never hashed or alias-cached by aether-mcp. Each item carries structured `params`, schema-encoded against the substrate kind vocabulary. Best-effort batch: per-item status is returned and one failure doesn't abort siblings. By default each item BLOCKS until its dispatch chain settles (status 'delivered'). The batch-level `replies` projection defaults to `terminal`, which returns the last arrival-ordered correlated reply plus every recognized error; `none` suppresses non-errors, and `all` restores the complete decoded stream. Recognition is exact for decoded `Err` variants and final kind-name error segments/suffixes. Each retained reply is {kind_id, kind_name, params (best-effort decode, null on miss), payload_bytes (base64 string, present only on a decode miss)}. The await cap is fixed at 300s per item; on timeout the item reports status 'timeout' with timed_out:true and the projected replies collected so far. Set fire_and_forget:true to await recipient resolution, then fire the application mail without awaiting its settlement (status 'dispatched', empty replies regardless of projection). For `Bytes`-typed request fields, pass a byte array (`[…]`, canonical) or one `$`-sigil embed: `$file`, `$base64`, or `$text`. Decoded reply `Bytes` leaves over 16 KiB spill to a host file; a still-oversized complete result then passes through the generic 32 KiB whole-response guard, so explicit `all` never truncates."
    )]
    pub async fn send_mail(&self, Parameters(args): Parameters<SendMailArgs>) -> Result<String, McpError> {
        guard_response_size("send_mail", mail::send_mail(self, args).await)
    }

    #[tool(
        description = "Atomic batched dispatch with a combined trace tree. Every spec lands on the engine's aether.trace mailbox under one shared chassis root; bad specs abort the batch before any mail moves. By default it BLOCKS until settlement and returns `mails:null`, a compact one-line-per-node `tree`, and `node_count`, plus the batch's complete flat arrival-ordered `replies` list. Compact line indentation follows causal parentage and each line names sender → recipient, kind, and handler duration (or `in-flight`). Pass `full:true` to restore the complete `mails` node vector; full mode omits `tree` and carries the same `node_count`. Neither mode truncates: a serialized result over the generic 32 KiB whole-response threshold spills complete to a host file. Behind the scenes the substrate acks the root, the caller waits for settlement while collecting replies, then performs the guided trace walk and resolves names. settlement_timeout_ms caps wall-clock wait (default 300000, max 600000); timeout has no root or replies and omits `tree`/`node_count`. Set fire_and_forget:true to return the ack only (status:dispatched with root populated, mails/replies null and projection fields omitted)."
    )]
    pub async fn send_mail_traced(&self, Parameters(args): Parameters<SendMailTracedArgs>) -> Result<String, McpError> {
        guard_response_size("send_mail_traced", mail::send_mail_traced(self, args).await)
    }

    #[tool(
        description = "Manage revisioned terrain marks through a loaded MarkBook component. `mark_book_mailbox` MUST be the exact `LoadResult.name` returned by load_component (normally `aether.component/aether.embedded:<load-name>`), not an actor namespace, mailbox id, selector, or inferred default; use load_component / describe_component to discover it. Operations map exactly as follows: create -> aether.kit.mark.create / MarkCreateResult, update -> aether.kit.mark.update / MarkUpdateResult, delete -> aether.kit.mark.delete / MarkDeleteResult, get -> aether.kit.mark.get / MarkGetResult, list -> aether.kit.mark.list / MarkListResult. Results preserve every domain rejection and render MarkId as the named `{value}` record."
    )]
    pub async fn terrain_marks(&self, Parameters(args): Parameters<TerrainMarksArgs>) -> Result<String, McpError> {
        guard_response_size("terrain_marks", terrain::terrain_marks(self, args).await)
    }

    #[tool(
        description = "Drive a loaded TerraEditor through semantic selection and mark commands. `terra_mailbox` MUST be the exact TerraEditor `LoadResult.name` returned by load_component (normally `aether.component/aether.embedded:<load-name>`); use load_component / describe_component to discover it. The TerraEditor must already be loaded with `TerraConfig { mark_book_mailbox: <MailboxId> }`. set_selection, toggle_selection, clear_selection, create_mark, move_selection, relabel_selection, and delete_selection send the matching aether.kit.terra.* kind and return TerraCommandResult unchanged; query sends aether.kit.terra.query and returns TerraQueryResult."
    )]
    pub async fn terrain_editor(&self, Parameters(args): Parameters<TerrainEditorArgs>) -> Result<String, McpError> {
        guard_response_size("terrain_editor", terrain::terrain_editor(self, args).await)
    }

    #[tool(
        description = "Apply one bounded terrain brush through a loaded WorldView. `world_mailbox` and `mark_book_mailbox` MUST be the exact WorldView and MarkBook `LoadResult.name` strings returned by load_component, never namespaces, mailbox ids, selectors, or inferred defaults; use load_component / describe_component to discover them. Always preflights source through aether.kit.mark.get, including explicit geometry; missing, stale, id-mismatched, or non-Path source_mark geometry sends no world mutation. Sends aether.kit.world.apply_brush and returns the exact OperatorResult, including Failed as ordinary domain output."
    )]
    pub async fn apply_terrain_brush(
        &self,
        Parameters(args): Parameters<ApplyTerrainBrushArgs>,
    ) -> Result<String, McpError> {
        guard_response_size("apply_terrain_brush", terrain::apply_terrain_brush(self, args).await)
    }

    #[tool(
        description = "Run one bounded terrain automaton through a loaded WorldView. `world_mailbox` and `mark_book_mailbox` MUST be the exact WorldView and MarkBook `LoadResult.name` strings returned by load_component, never namespaces, mailbox ids, selectors, or inferred defaults; use load_component / describe_component to discover them. Always preflights source through aether.kit.mark.get, including explicit geometry; source_mark accepts only Point and converts octimeters to cells with negative-safe arithmetic flooring. Sends aether.kit.world.run_automaton and returns the exact OperatorResult, including Failed as ordinary domain output."
    )]
    pub async fn run_terrain_automaton(
        &self,
        Parameters(args): Parameters<RunTerrainAutomatonArgs>,
    ) -> Result<String, McpError> {
        guard_response_size("run_terrain_automaton", terrain::run_terrain_automaton(self, args).await)
    }

    #[tool(
        description = "Stage one of the exact eight WorldView proposal operations: set_chunk, set_cell_points, set_cell_heights, stamp_polygon, stamp_disc, stamp_hexagon, apply_brush, or run_automaton. `world_mailbox` MUST be the exact WorldView `LoadResult.name` returned by load_component; operator variants also require the exact MarkBook `LoadResult.name` and perform the same mandatory MarkGet revision/geometry preflight as the immediate tools. Use load_component / describe_component for discovery. Sends aether.kit.world.propose and returns ProposalResult unchanged, including StagedProposalLimitReached, UnknownProposal, StaleProposal, NoTouchedChunks, and ProposalIdExhausted as domain Rejected values."
    )]
    pub async fn propose_terrain_edit(
        &self,
        Parameters(args): Parameters<ProposeTerrainEditArgs>,
    ) -> Result<String, McpError> {
        guard_response_size("propose_terrain_edit", terrain::propose_terrain_edit(self, args).await)
    }

    #[tool(
        description = "Commit a staged terrain proposal. `world_mailbox` MUST be the exact loaded WorldView `LoadResult.name` returned by load_component, not a namespace, mailbox id, selector, or inferred default; use load_component / describe_component to discover it. Sends aether.kit.world.commit_proposal and returns the exact ProposalResult, including domain Rejected variants."
    )]
    pub async fn commit_terrain_proposal(
        &self,
        Parameters(args): Parameters<CommitTerrainProposalArgs>,
    ) -> Result<String, McpError> {
        guard_response_size("commit_terrain_proposal", terrain::commit_terrain_proposal(self, args).await)
    }

    #[tool(
        description = "Discard a fresh or stale staged terrain proposal. `world_mailbox` MUST be the exact loaded WorldView `LoadResult.name` returned by load_component, not a namespace, mailbox id, selector, or inferred default; use load_component / describe_component to discover it. Sends aether.kit.world.discard_proposal and returns the exact ProposalResult, including domain Rejected variants."
    )]
    pub async fn discard_terrain_proposal(
        &self,
        Parameters(args): Parameters<DiscardTerrainProposalArgs>,
    ) -> Result<String, McpError> {
        guard_response_size("discard_terrain_proposal", terrain::discard_terrain_proposal(self, args).await)
    }

    #[tool(
        description = "Select or clear the rendered terrain-proposal preview; `proposal_id: null` clears it. `world_mailbox` MUST be the exact loaded WorldView `LoadResult.name` returned by load_component, not a namespace, mailbox id, selector, or inferred default; use load_component / describe_component to discover it. Sends aether.kit.world.set_proposal_preview and returns the exact ProposalResult, including domain Rejected variants."
    )]
    pub async fn set_terrain_proposal_preview(
        &self,
        Parameters(args): Parameters<SetTerrainProposalPreviewArgs>,
    ) -> Result<String, McpError> {
        guard_response_size("set_terrain_proposal_preview", terrain::set_terrain_proposal_preview(self, args).await)
    }

    #[tool(
        description = "Load a WASM component into a substrate by registry selector (ADR-0116) — upload_component first if it isn't stored. Pass `selector`: a content hash, a name (latest upload under it), or a module@actor (the @actor half picks an exported actor type from a multi-actor module). The host wasm path is gone — the only path anywhere is the upload_component input. aether-mcp resolves the selector hub-local to the wasm bytes, forwards aether.component.load to the engine's aether.component mailbox, and awaits the LoadResult — returning {mailbox_id, name, capabilities} or an error. The component's kind vocabulary rides in the wasm's aether.kinds custom section. Pass config (inline JSON) or config_path (path to a JSON file) to deliver init-config to a typed-config component (ADR-0090): aether-mcp schema-encodes the JSON to the component's Config kind before forwarding bytes — describe_component reports the expected config kind. Pass export to pick which exported actor type to instantiate from a multi-actor module (ADR-0096), named by its Actor::NAMESPACE; a module@actor selector populates it from its @actor half. Omit both only when the module declares a default: the sole type in a single-actor module or the type selected with export!(default = …). A defaultless multi-actor export!(A, B, …) requires an explicit export and otherwise returns LoadResult::Err. The returned name + capabilities describe the selected type. Pass replicas: N in 1..=256 (issue 2626) to load N instances of this selector in one call — each named {base}-{index} (base = name > export > default actor namespace) — returning {capabilities, instances: [{mailbox_id, name}, ...]} (one shared capabilities block; issue 3006) instead of the single-load shape; pairs with #[router(shared)] (ADR-0136) to scale an HTTP handler to N instances with no hand-written registration. Handler/component docs default to the first rustdoc line; pass full: true for complete rustdoc strings. A mid-loop failure reports which replica failed and how many loaded before it — already-loaded replicas stay live, the same as N manual calls. replicas outside the accepted range are a tool error. Very large wasm payloads (debug builds at 15-25 MiB) may exceed the RPC framing cap — prefer release builds, or raise the cap via the AETHER_MAX_FRAME_SIZE env var (default 64 MiB, clamped at 1 GiB; issue 1271)."
    )]
    pub async fn load_component(&self, Parameters(args): Parameters<LoadComponentArgs>) -> Result<String, McpError> {
        guard_response_size("load_component", components::load_component(self, args).await)
    }

    #[tool(
        description = "Replace the WASM behind a live component's stable mailbox binding with a build resolved from a registry selector (ADR-0038 structural splice; ADR-0116 selector). Pass `selector` (hash-primary — a hash pins or rolls a component to an exact build; a name or module@actor resolves too); the host wasm path is gone, surviving only as the upload_component input. aether-mcp resolves the selector hub-local to the wasm bytes and forwards aether.component.replace to the engine's aether.component mailbox. drain_timeout_ms is accepted for wire compatibility but currently ignored. Failure is phase-dependent rather than rollback-atomic: validation errors preserve the old instance, but after replacement takes the old guest an instantiation error can leave the trampoline empty, and a rehydrate error leaves the new guest installed while returning Err. Observe the component before recovery or retry. Pass config (inline JSON) or config_path (path to a JSON file) to deliver init-config to a typed-config replacement; aether-mcp schema-encodes the JSON to the replacement component's Config kind before forwarding bytes. Pass export to instantiate a specific exported actor type from a multi-actor replacement module (ADR-0096), named by its Actor::NAMESPACE; a module@actor selector populates it from its @actor half. Omit export to reuse the actor type the trampoline currently hosts — not necessarily the module entry — preserving today's replace behaviour; an export the replacement module doesn't declare comes back as an error. On success returns the replacement's advertised capabilities (docs default to the first rustdoc line; full: true for complete rustdoc). Very large wasm payloads (debug builds at 15-25 MiB) may exceed the RPC framing cap — prefer release builds, or raise the cap via the AETHER_MAX_FRAME_SIZE env var (default 64 MiB, clamped at 1 GiB; issue 1271)."
    )]
    pub async fn replace_component(
        &self,
        Parameters(args): Parameters<ReplaceComponentArgs>,
    ) -> Result<String, McpError> {
        guard_response_size("replace_component", components::replace_component(self, args).await)
    }

    #[tool(
        description = "Capture an engine's current frame as a PNG. Requires `window_id` — the engine window to capture, as the tagged `mbx-…` string `aether.window.list` reports (or the id a window-create reply returned). It is a string because a real window id is an ADR-0099 lineage fold near 2^60, which a JSON number cannot carry to a client that parses numbers as doubles; a decimal `u64` is still accepted for a synthetic or harness-scale id. Desktop capture never guesses a primary, focused, or current window, so this field has no default and omitting it is a deserialize error, not a fallback. The optional inline image defaults to a 768-pixel inclusive long-edge ceiling, never upscales, and preserves aspect ratio within integer-pixel quantization. Pass `scale` as a finite factor in (0.0, 1.0] for an explicit proportional reduction, then `max_dimension` (> 0) to clamp the scaled long edge; they compose in that order. Optionally carries two mail bundles dispatched atomically around the capture: `mails` fires before readback (state changes that should appear in the image), `after_mails` fires after (cleanup). A bad bundle entry aborts the whole capture before any mail moves. Optionally carries `checks`: substrate-side reductions (not_all_black, differs_from_background, coverage, centroid, bounding_box) scored on the exact full-resolution RGBA the PNG is built from and returned as a `verdict` text block. Checks-driven captures omit the image by default; `include_image` explicitly overrides that default, while plain and similarity-only captures include it. Each check entry optionally carries `region` ({min_x, min_y, max_x, max_y}) to restrict the reduction to the frame-clamped intersection of that rect instead of the whole frame — e.g. asserting a widget's fill lands inside its own screen rect rather than folding the whole scene into one number; coverage then divides by the clamped region's pixel count, and centroid/bounding_box still report absolute frame coordinates. Omit `region` to score the whole frame (the prior behavior). Optionally carries `similarity`: a reference-image check (`namespace` + `reference_path` + `threshold`) the render thread scores as a normalised mean-absolute-error against the captured RGBA, returned as `similarity_score` / `similarity_pass` text blocks. Optionally carries `save_path`: an absolute harness-host path to persist the original full-resolution PNG bytes — missing parent directories are created, an existing file at the path is overwritten. A relative save_path is rejected up front as invalid_params, before the capture touches the wire. On success the result carries a `{\"saved\": {\"path\": …, \"bytes\": N}}` text block; a write failure (e.g. an unwritable path) never fails the call and carries `{\"saved\": {\"error\": …}}` instead."
    )]
    pub async fn capture_frame(
        &self,
        Parameters(args): Parameters<CaptureFrameArgs>,
    ) -> Result<CallToolResult, McpError> {
        capture::capture_frame(self, args).await
    }

    #[tool(
        description = "Collect a bounded, non-mutating live-engine failure bundle while preserving the caller's required `primary_error`. Requires one engine id and explicit selectors: at most 8 actor lineage names, 8 component lineage names, 16 exact kind names, plus an optional exact `frame.window_id`. Selectors are validated, sorted, and deduplicated before any observation. The result records the selected live/recently-dead fleet row, full schemas for selected kinds, full component descriptions, and for each actor a log tail capped at 100 entries plus its complete cost table. Each observation has a 3-second cap and records success, error, timeout, or budget_exhausted without preventing later observations; the whole bundle has a 15-second budget. Optional frame capture returns the structured JSON text block first and a bounded inline PNG second. This tool never sends mail, replays or retries the failed operation, runs checks, reads a reference image, writes a host file, changes lifecycle state, or performs cleanup. Oversized JSON uses the standard whole-response spill projection."
    )]
    pub async fn collect_failure_evidence(
        &self,
        Parameters(args): Parameters<CollectFailureEvidenceArgs>,
    ) -> Result<CallToolResult, McpError> {
        failure_evidence::collect_failure_evidence(self, args).await
    }

    #[tool(
        description = "List a best-effort substrate kind snapshot. engine_id selects an engine: the tool prefills static aether.* kinds and attempts to merge that engine's live capability + component kinds. A failed or undecodable live refresh currently leaves the static/prior cached snapshot and still succeeds, so pair the result with a fresh fleet read and a bounded live probe when reachability or freshness matters. Omit engine_id and the tool auto-resolves the sole supervised engine; with zero or many engines it returns the static baseline. Default (no selectors) returns a compact [{name, shape}] JSON array where shape is a one-line field rendering. A complete response over the generic 32 KiB inline guard is returned losslessly as {file, bytes, summary}; read file for the array. families:true returns a sorted [{family, count}] digest, may combine with prefix to digest a subtree, and ignores full. names selects an exact set of kind names and may combine only with full. prefix is a case-sensitive starts_with selector. Selection precedence is families > names > prefix > bare; combining names with families or prefix is an error. full:true returns schema-exact [{name, schema}] entries for the returned snapshot, while bare full:true is refused."
    )]
    pub async fn describe_kinds(&self, Parameters(args): Parameters<DescribeKindsArgs>) -> Result<String, McpError> {
        guard_response_size("describe_kinds", describe::describe_kinds(self, args).await)
    }

    #[tool(
        description = "List the native transforms collected at link time (ADR-0048): every #[transform] fn with its global transform_id, fully-qualified name, declared input kind ids (slot order), and output kind id. These are pure Kind -> Kind functions available to registered consumers such as the aether.fs fetch-fold path; this is the static inventory aether-mcp ships with (a transform set is a build-time property), not an engine query. Empty when no first-party transforms are linked."
    )]
    pub async fn describe_transforms(&self) -> Result<String, McpError> {
        guard_response_size("describe_transforms", describe::describe_transforms())
    }

    #[tool(
        description = "Describe a loaded component's receive-side capabilities (ADR-0033): the kinds it typed-handles with per-handler docs, whether it has a fallback catchall, its top-level doc, and (ADR-0090) its boot-config kind id+name when it declared a typed Config. Per-handler and component docs default to the first rustdoc line; pass full: true for the complete rustdoc strings (issue 3006). Address the component by its full lineage name or an unambiguous ADR-0166 abbreviation; the selected engine resolves textual addresses to its live mailbox id and canonical path before the cache lookup. load_component returns the canonical name. spawn_substrate returns engine information only, so callers booting components should give each an explicit name or derive and retain its lineage from the boot spec and stored manifest. list_components reports stored artifacts, never live lineages. A tagged mbx- id is accepted only as a local cache fast-path."
    )]
    pub async fn describe_component(
        &self,
        Parameters(args): Parameters<DescribeComponentArgs>,
    ) -> Result<String, McpError> {
        guard_response_size("describe_component", describe::describe_component(self, args).await)
    }

    #[tool(
        description = "Describe a substrate's NATIVE chassis caps' reply contracts (ADR-0109 §5): the native analogue of describe_component. Sends aether.inventory.handlers to the engine's aether.inventory mailbox and decodes aether.inventory.handlers_result — the link-time handler manifest the #[actor] macro populates. Returns the handlers folded per mailbox namespace; each handler carries its input kind (id + name) and its reply kind id+name, so you read a native cap's In -> Out (e.g. aether.fs.read -> aether.fs.read_result) before issuing the call. reply is null for a fire-and-forget handler. Reply kind names resolve best-effort from the static substrate vocabulary; a component-defined reply kind stays null. Use describe_component for a loaded wasm component, describe_kinds for the full schema of any kind."
    )]
    pub async fn describe_handlers(
        &self,
        Parameters(args): Parameters<DescribeHandlersArgs>,
    ) -> Result<String, McpError> {
        guard_response_size("describe_handlers", describe::describe_handlers(self, args).await)
    }

    #[tool(description = "Pull recent log entries from one actor's per-actor log ring (ADR-0081). \
                       Sends aether.log.tail to the named mailbox and decodes aether.log.tail_result. \
                       Every actor — native or wasm trampoline — serves this kind via the substrate's \
                       framework dispatch arm, so any mailbox is queryable (e.g. \"aether.audio\", \
                       \"aether.component/aether.embedded:aether.camera\"). `max` defaults to 100 and clamps to 1000; \
                       pass `level` (`trace|debug|info|warn|error`) for server-side filtering; pass \
                       `contains` for a case-sensitive server-side message substring filter; pass \
                       `since` (the prior call's `next_since`) to walk past already-seen entries without \
                       double-reading. `truncated_before` in the reply is `Some(seq)` when the ring \
                       evicted entries the caller hadn't seen yet (the lowest sequence still held). \
                       Aggregate across actors by calling this tool once per mailbox client-side.")]
    pub async fn actor_logs(&self, Parameters(args): Parameters<ActorLogsArgs>) -> Result<String, McpError> {
        guard_response_size("actor_logs", logs_cost::actor_logs(self, args).await)
    }

    #[tool(description = "Dump one actor's per-handler execution-cost EWMA table \
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
                       cost-aware recruiter (iamacoffeepot/aether#1127).")]
    pub async fn actor_cost(&self, Parameters(args): Parameters<ActorCostArgs>) -> Result<String, McpError> {
        guard_response_size("actor_cost", logs_cost::actor_cost(self, args).await)
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
mod test_support;

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::unwrap_used)]
mod tests;
