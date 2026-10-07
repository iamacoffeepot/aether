//! The component tools: the hub store (`store`), the component host's three
//! doors, `publish`, `spawn`, and `unpublish` (ADR-0241 §9, ADR-0250 §5),
//! `load_component` as publish then spawn (`load`), and the capability cache
//! they fill (`cache`).

use super::bytes::resolve_bytes_params;
use super::render::{json, project_capabilities};
use aether_codec::frame::max_frame_size;
use aether_data::KindDescriptor;
use aether_kinds::ComponentCapabilities;
use rmcp::ErrorData as McpError;
use std::path::PathBuf;
use tokio::fs;

pub(super) mod cache;
pub(super) mod load;
pub(super) mod publish;
pub(super) mod spawn;
pub(super) mod store;
pub(super) mod unpublish;

/// A component registry selector resolved to its bytes (ADR-0116) — the
/// front half of `publish` / `load_component` and the boot-manifest
/// pre-resolution. `export` is the `module@actor` selector's actor half;
/// `exports` names every type the module's manifest declares, which a
/// namespace-less load reads to find a sole export. `config_kind` is the
/// selected type's config descriptor (the module's sole export when the
/// selector names none).
pub(super) struct ResolvedComponent {
    pub(super) wasm: Vec<u8>,
    pub(super) export: Option<String>,
    pub(super) exports: Vec<String>,
    pub(super) config_kind: Option<KindDescriptor>,
}

/// The temp files a `stage_boot_manifest` wrote (ADR-0116): the
/// boot-manifest JSON the hub addresses to the child as `--boot-manifest`
/// argv plus the staged component `.wasm` files it points at. The substrate reads them
/// at boot, before the spawn reply returns; the spawn caller
/// [`cleanup`](StagedBootManifest::cleanup)s them once it has.
pub(super) struct StagedBootManifest {
    pub(super) manifest_path: PathBuf,
    pub(super) wasm_paths: Vec<PathBuf>,
    pub(super) config_paths: Vec<PathBuf>,
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
    config_kind: Option<&KindDescriptor>,
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
    let resolved = resolve_bytes_params(value, &config_kind.schema, max_frame_size()).await.map_err(|e| {
        McpError::invalid_params(format!("{context}: resolving config blob params for {}: {e}", config_kind.name), None)
    })?;
    let bytes = aether_codec::encode_schema(&resolved, &config_kind.schema).map_err(|e| {
        McpError::invalid_params(format!("{context}: config does not match {}: {e}", config_kind.name), None)
    })?;
    Ok(Some(bytes))
}

/// Fold an explicit `namespace` argument into the hub-local component resolve
/// selector so the resolve reply's config descriptor matches the actor type
/// that will instantiate. If the selector already carries `module@actor`, the
/// explicit namespace wins by replacing the actor half.
pub(super) fn selector_with_explicit_export(selector: &str, export: Option<&str>) -> String {
    let Some(export) = export else {
        return selector.to_owned();
    };
    let module = selector.split_once('@').map_or(selector, |(module, _)| module);
    format!("{module}@{export}")
}

pub(super) fn replicas_reply(
    engine_id: &str,
    capabilities: &ComponentCapabilities,
    instances: &[serde_json::Value],
    full: bool,
) -> Result<String, McpError> {
    json(&serde_json::json!({
        "engine_id": engine_id,
        "capabilities": project_capabilities(capabilities, full),
        "instances": instances,
    }))
}

/// Hard ceiling on `replicas` for one spawn fan-out (issue 3006 review).
/// Bounds the sequential spawn dispatches before an absurd caller value can
/// hang the tool. Mirrors the style of `actor_logs`' caller-supplied `max`
/// clamp (default ring-sized, hard ceiling) — tool-layer protection, not a
/// substrate protocol constant.
pub(super) const MAX_REPLICAS: u32 = 256;

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

/// Reject replicas outside `1..=MAX_REPLICAS` before any dispatch.
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

/// Refuse a `key` together with `replicas`: a fan-out draws a counter key
/// per instance, so one caller key cannot name them all.
pub(super) fn reject_key_with_replicas(
    key: Option<&str>,
    replicas: Option<u32>,
    selector: &str,
) -> Result<(), McpError> {
    if let (Some(key), Some(_)) = (key, replicas) {
        return Err(McpError::invalid_params(
            format!(
                "component {selector:?}: `key` ({key:?}) cannot be combined with `replicas`; each replica takes a \
                 counter key"
            ),
            None,
        ));
    }
    Ok(())
}
