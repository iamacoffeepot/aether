use super::bytes::resolve_bytes_params;
use super::envelope::{engine_envelope, local_envelope};
use super::ids::{parse_engine_id, parse_mailbox_id, resolve_handled_kind, static_kind_name};
use super::render::{frame_size_aware_error, internal, internal_msg, json, project_capabilities};
use super::{COMPONENT_CAP, FLEET_CAP, Mcp};
use crate::args::{
    ListBinariesArgs, ListComponentsArgs, LoadComponentArgs, ReplaceComponentArgs, UploadBinaryArgs,
    UploadComponentArgs,
};
use aether_codec::frame::max_frame_size;
use aether_data::{EngineId, Kind, SchemaType, wire};
use aether_kinds::{
    BinaryEntry, ComponentCapabilities, ComponentEntry, KindDescriptorWire, ListComponentBinaries,
    ListComponentBinariesResult, ListEngineBinaries, ListEngineBinariesResult, LoadComponent, LoadResult,
    ReplaceComponent, ReplaceResult, UploadBinary, UploadBinaryResult, UploadComponent, UploadComponentResult,
};
use rmcp::ErrorData as McpError;
use serde::Serialize;
use std::path::PathBuf;
use tokio::fs;

/// A component registry selector resolved to its bytes + `@actor` export
/// (ADR-0116) — the front half of `load_component` / `replace_component` /
/// the boot-manifest pre-resolution. `export` is the `module@actor`
/// selector's actor half, threaded into the forwarded `LoadComponent.export`.
/// `default_namespace` is the first actor's `Actor::NAMESPACE` from the wasm
/// manifest (per `export!` order), used by `stage_boot_manifest` to derive the
/// expected registered name when neither `spec.name` nor an export is set.
pub(super) struct ResolvedComponent {
    pub(super) wasm: Vec<u8>,
    pub(super) export: Option<String>,
    pub(super) default_namespace: Option<String>,
    pub(super) config_kind: Option<KindDescriptorWire>,
}

/// The temp files a `stage_boot_manifest` wrote (ADR-0116): the
/// boot-manifest JSON the hub injects as `AETHER_BOOT_MANIFEST` plus the
/// staged component `.wasm` files it points at. The substrate reads them
/// at boot, before the spawn reply returns; the spawn caller
/// [`cleanup`](StagedBootManifest::cleanup)s them once it has.
/// `expected_names` carries the full `aether.component/aether.embedded:{ns}`
/// lineage address computed for each spec, used by `spawn_substrate` to poll
/// readiness by identity rather than count.
pub(super) struct StagedBootManifest {
    pub(super) manifest_path: PathBuf,
    pub(super) wasm_paths: Vec<PathBuf>,
    pub(super) config_paths: Vec<PathBuf>,
    pub(super) expected_names: Vec<String>,
}

impl StagedBootManifest {
    /// Best-effort remove the staged manifest + every staged wasm file.
    /// The substrate has already read them at boot by the time the spawn
    /// reply returns, so a removal failure is harmless.
    pub(super) async fn cleanup(&self) {
        let _ = fs::remove_file(&self.manifest_path).await;
        for path in &self.wasm_paths {
            let _ = fs::remove_file(path).await;
        }
        for path in &self.config_paths {
            let _ = fs::remove_file(path).await;
        }
    }
}

/// Resolve an optional component init-config source (`config` inline JSON or
/// `config_path` JSON file) and schema-encode it to the component's declared
/// Config kind. No source returns `None`; a source for a no-config component is
/// a tool error.
pub(super) async fn component_config_bytes(
    config_kind: Option<&KindDescriptorWire>,
    config: Option<serde_json::Value>,
    config_path: Option<&str>,
    context: &str,
) -> Result<Option<Vec<u8>>, McpError> {
    let value = match (config, config_path) {
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => {
            return Err(McpError::invalid_params(
                format!("{context}: set only one of `config` or `config_path`"),
                None,
            ));
        }
        (Some(value), None) => value,
        (None, Some(path)) => {
            let bytes = fs::read(path)
                .await
                .map_err(|e| McpError::invalid_params(format!("{context}: reading config_path {path:?}: {e}"), None))?;
            if bytes.len() > max_frame_size() {
                return Err(McpError::invalid_params(
                    format!(
                        "{context}: config_path {path:?} is {} bytes, over the {}-byte RPC frame cap; a \
                         blob this large must stage as a hub-read path (ADR-0115/0116), not inline into mail",
                        bytes.len(),
                        max_frame_size()
                    ),
                    None,
                ));
            }
            serde_json::from_slice(&bytes).map_err(|e| {
                McpError::invalid_params(format!("{context}: parsing config_path {path:?} as JSON: {e}"), None)
            })?
        }
    };

    let Some(config_kind) = config_kind else {
        return Err(McpError::invalid_params(
            format!("{context}: config JSON was provided but the component declares no Config kind"),
            None,
        ));
    };
    let schema = wire::from_bytes::<SchemaType>(&config_kind.schema_wire)
        .map_err(|e| internal_msg(&format!("{context}: decoding config schema for {}: {e}", config_kind.name)))?;
    let resolved = resolve_bytes_params(value, &schema, max_frame_size()).await.map_err(|e| {
        McpError::invalid_params(format!("{context}: resolving config blob params for {}: {e}", config_kind.name), None)
    })?;
    let bytes = aether_codec::encode_schema(&resolved, &schema).map_err(|e| {
        McpError::invalid_params(format!("{context}: config does not match {}: {e}", config_kind.name), None)
    })?;
    Ok(Some(bytes))
}

/// Return `true` once every name in `want` is present in `actual`.
/// Used by `wait_for_loaded_components` to drive identity-based readiness
/// polling: the count variant (`actual.len() >= want.len()`) is insufficient
/// because a baseline trampoline or an unrequested component can satisfy a
/// count while a requested component is still absent.
pub(super) fn components_all_loaded(want: &[String], actual: &[String]) -> bool {
    want.iter().all(|w| actual.iter().any(|a| a == w))
}

/// Fold an explicit `export` argument into the hub-local component resolve
/// selector so the resolve reply's config descriptor matches the actor type
/// that will instantiate. If the selector already carries `module@actor`, the
/// explicit export wins by replacing the actor half.
pub(super) fn selector_with_explicit_export(selector: &str, export: Option<&str>) -> String {
    let Some(export) = export else {
        return selector.to_owned();
    };
    let module = selector.split_once('@').map_or(selector, |(module, _)| module);
    format!("{module}@{export}")
}

/// Resolve the base name a `replicas` fan-out derives its `{base}-{index}`
/// names from (issue 2626), using the same precedence the component host
/// itself applies at load: caller `name` > `export` > default actor
/// namespace. `None` when none of the three is available — the caller
/// turns that into a clean tool error naming what to set. Shared by
/// `stage_boot_manifest` (deriving `expected_names` to poll) and
/// `load_component` (deriving each replica's load name), so both sides of
/// a replicated load agree on what the components register as.
pub(super) fn replica_base_name(
    name: Option<&str>,
    export: Option<&str>,
    default_namespace: Option<&str>,
) -> Option<String> {
    name.or(export).or(default_namespace).map(str::to_owned)
}

/// Derive the `{base}-{index}` name set a `replicas` fan-out registers
/// under: every instance suffixed, no bare-name special case for index 0,
/// so `replicas: 1` differs from an omitted field only by the `-0` suffix.
pub(super) fn replica_names(base: &str, replicas: u32) -> Vec<String> {
    (0..replicas).map(|index| format!("{base}-{index}")).collect()
}

pub(super) fn replicas_reply(
    capabilities: &ComponentCapabilities,
    instances: &[serde_json::Value],
    full: bool,
) -> Result<String, McpError> {
    json(&serde_json::json!({
        "capabilities": project_capabilities(capabilities, full),
        "instances": instances,
    }))
}

/// Hard ceiling on `replicas` for one `load_component` fan-out (issue 3006
/// review). Bounds allocation of the name list and sequential load
/// dispatches before an absurd caller value can OOM or hang the tool.
/// Mirrors the style of `actor_logs`' caller-supplied `max` clamp (default
/// ring-sized, hard ceiling) — tool-layer protection, not a substrate
/// protocol constant.
pub(super) const MAX_REPLICAS: u32 = 256;

#[derive(Serialize)]
pub(super) struct StoreListingResponse<T> {
    entries: Vec<T>,
    total_matched: u32,
    shown: u32,
    truncated: bool,
    notice: Option<String>,
}

#[derive(Serialize)]
pub(super) struct BinaryStoreEntry {
    hash: String,
    name: Option<String>,
    manifest: BinaryStoreManifest,
}

/// The `list_binaries` projection of a stored [`BinaryManifest`] (ADR-0162).
/// The config surface — `env_keys` / `argv_flags` — is rendered as counts, not
/// full lists: a full-stack chassis carries dozens of each, so inlining them per
/// entry across a listing would bloat the tool output without a caller need. The
/// full sets live in the hub's store, captured under the content hash; a
/// consumer that needs them (spawn-side validation) reads the store directly.
/// `chassis` / `caps` / build provenance stay full — they are small and the
/// existing listing already surfaced them.
#[derive(Serialize)]
struct BinaryStoreManifest {
    chassis: String,
    caps: Vec<String>,
    git_sha: String,
    profile: String,
    target: String,
    env_key_count: usize,
    argv_flag_count: usize,
}

#[derive(Serialize)]
pub(super) struct ComponentStoreEntry {
    hash: String,
    name: Option<String>,
    manifest: ComponentStoreManifest,
}

#[derive(Serialize)]
struct ComponentStoreManifest {
    namespaces: Vec<String>,
    actors: Vec<ComponentStoreActor>,
    fallback: bool,
    provenance: String,
    default_entry: Option<String>,
}

#[derive(Serialize)]
struct ComponentStoreActor {
    namespace: String,
    handled_kinds: Vec<String>,
    fallback: bool,
}

pub(super) fn store_listing_response<T>(entries: Vec<T>, total_matched: u32) -> StoreListingResponse<T> {
    let shown = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    let truncated = shown < total_matched;
    let notice = truncated.then(|| {
        format!(
            "Showing {shown} of {total_matched} matching store entries; request a larger explicit `limit` to retrieve more"
        )
    });
    StoreListingResponse { entries, total_matched, shown, truncated, notice }
}

pub(super) fn binary_listing_response(result: ListEngineBinariesResult) -> StoreListingResponse<BinaryStoreEntry> {
    store_listing_response(result.binaries.into_iter().map(project_binary_store_entry).collect(), result.total_matched)
}

fn project_binary_store_entry(entry: BinaryEntry) -> BinaryStoreEntry {
    BinaryStoreEntry {
        hash: entry.hash,
        name: entry.name,
        manifest: BinaryStoreManifest {
            chassis: entry.manifest.chassis,
            caps: entry.manifest.caps,
            git_sha: entry.manifest.git_sha,
            profile: entry.manifest.profile,
            target: entry.manifest.target,
            env_key_count: entry.manifest.env_keys.len(),
            argv_flag_count: entry.manifest.argv_flags.len(),
        },
    }
}

pub(super) fn component_listing_response(
    result: ListComponentBinariesResult,
) -> StoreListingResponse<ComponentStoreEntry> {
    store_listing_response(
        result.components.into_iter().map(project_component_store_entry).collect(),
        result.total_matched,
    )
}

fn project_component_store_entry(entry: ComponentEntry) -> ComponentStoreEntry {
    ComponentStoreEntry {
        hash: entry.hash,
        name: entry.name,
        manifest: ComponentStoreManifest {
            namespaces: entry.manifest.namespaces,
            actors: entry
                .manifest
                .actors
                .into_iter()
                .map(|actor| ComponentStoreActor {
                    namespace: actor.namespace,
                    handled_kinds: actor
                        .handled_kinds
                        .into_iter()
                        .map(|kind| static_kind_name(kind).unwrap_or_else(|| kind.to_string()))
                        .collect(),
                    fallback: actor.fallback,
                })
                .collect(),
            fallback: entry.manifest.fallback,
            provenance: entry.manifest.provenance,
            default_entry: entry.manifest.default_entry,
        },
    }
}

/// Reject `replicas: 0` (ADR-0090 §4 posture: a bad known value is a hard
/// error, not a silent no-op) before it reaches any load dispatch.
pub(super) fn reject_zero_replicas(replicas: Option<u32>, selector: &str) -> Result<(), McpError> {
    if replicas == Some(0) {
        return Err(McpError::invalid_params(
            format!("component {selector:?}: replicas must be at least 1 (got 0)"),
            None,
        ));
    }
    Ok(())
}

/// Reject `load_component` replicas outside `1..=MAX_REPLICAS` before any
/// dispatch or name-list allocation.
pub(super) fn reject_replicas_out_of_range(replicas: Option<u32>, selector: &str) -> Result<(), McpError> {
    reject_zero_replicas(replicas, selector)?;
    let Some(n) = replicas else {
        return Ok(());
    };
    if n > MAX_REPLICAS {
        return Err(McpError::invalid_params(
            format!("component {selector:?}: replicas must be at most {MAX_REPLICAS} (got {n})"),
            None,
        ));
    }
    Ok(())
}

pub(super) async fn upload_binary(mcp: &Mcp, args: UploadBinaryArgs) -> Result<String, McpError> {
    // The hub reads the staged path; aether-mcp forwards it, never
    // reading the bytes (unlike load_component).
    let reply = mcp
        .session
        .call_one(local_envelope(FLEET_CAP, &UploadBinary { staged_path: args.staged_path, name: args.name }))
        .await
        .map_err(internal)?;
    match UploadBinaryResult::decode_from_bytes(&reply.payload) {
        Some(UploadBinaryResult::Ok { hash, name }) => json(&serde_json::json!({ "hash": hash, "name": name })),
        Some(UploadBinaryResult::Err { error }) => Err(internal_msg(&error)),
        None => Err(internal_msg("undecodable UploadBinaryResult")),
    }
}

pub(super) async fn list_binaries(mcp: &Mcp, args: ListBinariesArgs) -> Result<String, McpError> {
    let reply = mcp
        .session
        .call_one(local_envelope(
            FLEET_CAP,
            &ListEngineBinaries {
                chassis: args.chassis,
                caps: args.caps,
                target: args.target,
                limit: args.limit,
                include_history: args.include_history,
            },
        ))
        .await
        .map_err(internal)?;
    ListEngineBinariesResult::decode_from_bytes(&reply.payload).map_or_else(
        || Err(internal_msg("undecodable ListEngineBinariesResult")),
        |result| json(&binary_listing_response(result)),
    )
}

pub(super) async fn upload_component(mcp: &Mcp, args: UploadComponentArgs) -> Result<String, McpError> {
    // The hub reads the staged path; aether-mcp forwards it, never
    // reading the bytes (unlike the load_component resolve hop, which
    // pulls the bytes back from the store).
    let reply = mcp
        .session
        .call_one(local_envelope(FLEET_CAP, &UploadComponent { staged_path: args.staged_path, name: args.name }))
        .await
        .map_err(internal)?;
    match UploadComponentResult::decode_from_bytes(&reply.payload) {
        Some(UploadComponentResult::Ok { hash, name }) => json(&serde_json::json!({ "hash": hash, "name": name })),
        Some(UploadComponentResult::Err { error }) => Err(internal_msg(&error)),
        None => Err(internal_msg("undecodable UploadComponentResult")),
    }
}

pub(super) async fn list_components(mcp: &Mcp, args: ListComponentsArgs) -> Result<String, McpError> {
    let handled_kind = match args.handled_kind.as_deref() {
        Some(s) => Some(resolve_handled_kind(s)?),
        None => None,
    };
    let reply = mcp
        .session
        .call_one(local_envelope(
            FLEET_CAP,
            &ListComponentBinaries {
                namespace: args.namespace,
                handled_kind,
                limit: args.limit,
                include_history: args.include_history,
            },
        ))
        .await
        .map_err(internal)?;
    ListComponentBinariesResult::decode_from_bytes(&reply.payload).map_or_else(
        || Err(internal_msg("undecodable ListComponentBinariesResult")),
        |result| json(&component_listing_response(result)),
    )
}

pub(super) async fn load_component(mcp: &Mcp, args: LoadComponentArgs) -> Result<String, McpError> {
    let engine = parse_engine_id(&args.engine_id)?;
    let selector = selector_with_explicit_export(&args.selector, args.export.as_deref());
    reject_replicas_out_of_range(args.replicas, &selector)?;
    // ADR-0116: resolve the selector hub-local to the wasm bytes; a
    // `module@actor` selector's `@actor` half rides back as `export`.
    let resolved = mcp.resolve_component(&selector).await?;
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
        return load_single_component(mcp, engine, &selector, resolved.wasm, args.name, config, export, args.full)
            .await;
    };

    // issue 2626: loop the single-load dispatch N times, one shared
    // wasm/config, naming each instance in the same precedence order
    // `stage_boot_manifest` derives `expected_names` in.
    let base = replica_base_name(args.name.as_deref(), export.as_deref(), resolved.default_namespace.as_deref())
        .ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "component {selector:?}: cannot determine a base name for `replicas` \
                     (no `name`, `export`, or default actor namespace in the wasm manifest); \
                     set `name` or `export`"
                ),
                None,
            )
        })?;

    // Shared capabilities (identical wasm) projected once; instances list
    // drops the N-fold capabilities echo (issue 3006). Cache still stores
    // full caps per instance so describe_component --full works.
    let mut instances = Vec::with_capacity(replicas as usize);
    let mut shared_caps = None;
    for (index, name) in replica_names(&base, replicas).into_iter().enumerate() {
        let reply = mcp
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
            .map_err(|e| frame_size_aware_error(&format!("load_component {selector:?} replica {index}"), e))?;
        match LoadResult::decode_from_bytes(&reply.payload) {
            Some(LoadResult::Ok { mailbox_id, name, capabilities }) => {
                mcp.components
                    .lock()
                    .expect("component cache mutex is never poisoned")
                    .insert((engine, mailbox_id), capabilities.clone());
                if shared_caps.is_none() {
                    shared_caps = Some(capabilities);
                }
                instances.push(serde_json::json!({
                    "mailbox_id": mailbox_id,
                    "name": name,
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
    replicas_reply(
        &shared_caps.expect("replicas >= 1: loop either populated shared_caps or returned early"),
        &instances,
        args.full,
    )
}

/// Preserve the original single-instance response shape while keeping the
/// replica orchestration in [`load_component`] focused on fan-out.
// `full` is the issue-3006 projection flag; keeps the single-load path's
// existing positional args rather than a new options struct.
#[allow(clippy::too_many_arguments)]
async fn load_single_component(
    mcp: &Mcp,
    engine: EngineId,
    selector: &str,
    wasm: Vec<u8>,
    name: Option<String>,
    config: Vec<u8>,
    export: Option<String>,
    full: bool,
) -> Result<String, McpError> {
    let reply = mcp
        .session
        .call_one(engine_envelope(engine, COMPONENT_CAP, &LoadComponent { wasm, name, config, export }))
        .await
        .map_err(|e| frame_size_aware_error(&format!("load_component {selector:?}"), e))?;
    match LoadResult::decode_from_bytes(&reply.payload) {
        Some(LoadResult::Ok { mailbox_id, name, capabilities }) => {
            mcp.components
                .lock()
                .expect("component cache mutex is never poisoned")
                .insert((engine, mailbox_id), capabilities.clone());
            json(&serde_json::json!({
                "mailbox_id": mailbox_id,
                "name": name,
                "capabilities": project_capabilities(&capabilities, full),
            }))
        }
        Some(LoadResult::Err { error }) => Err(internal_msg(&error)),
        None => Err(internal_msg("undecodable LoadResult")),
    }
}

pub(super) async fn replace_component(mcp: &Mcp, args: ReplaceComponentArgs) -> Result<String, McpError> {
    let engine = parse_engine_id(&args.engine_id)?;
    let mailbox_id = parse_mailbox_id(&args.mailbox_id)?;
    let selector = selector_with_explicit_export(&args.selector, args.export.as_deref());
    // ADR-0116: resolve the selector hub-local to the replacement wasm
    // bytes (hash-primary, so a hash pins/rolls to an exact build).
    let resolved = mcp.resolve_component(&selector).await?;
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
    let reply = mcp
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
            mcp.components
                .lock()
                .expect("component cache mutex is never poisoned")
                .insert((engine, mailbox_id), capabilities.clone());
            json(&project_capabilities(&capabilities, args.full))
        }
        Some(ReplaceResult::Err { error }) => Err(internal_msg(&error)),
        None => Err(internal_msg("undecodable ReplaceResult")),
    }
}
