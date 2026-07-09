use super::bytes::resolve_bytes_params;
use super::{KindDescriptorWire, McpError, SchemaType, internal_msg, wire};
use aether_codec::frame::max_frame_size;
use std::path::PathBuf;
use tokio::fs;

/// A component registry selector resolved to its bytes + `@actor` export
/// (ADR-0116) — the front half of `load_component` / `replace_component` /
/// the boot-manifest pre-resolution. `export` is the `module@actor`
/// selector's actor half, threaded into the forwarded `LoadComponent.export`.
/// `entry_namespace` is the first actor's `Actor::NAMESPACE` from the wasm
/// manifest (per `export!` order), used by `stage_boot_manifest` to derive the
/// expected registered name when neither `spec.name` nor an export is set.
pub(super) struct ResolvedComponent {
    pub(super) wasm: Vec<u8>,
    pub(super) export: Option<String>,
    pub(super) entry_namespace: Option<String>,
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
            let bytes = fs::read(path).await.map_err(|e| {
                McpError::invalid_params(
                    format!("{context}: reading config_path {path:?}: {e}"),
                    None,
                )
            })?;
            serde_json::from_slice(&bytes).map_err(|e| {
                McpError::invalid_params(
                    format!("{context}: parsing config_path {path:?} as JSON: {e}"),
                    None,
                )
            })?
        }
    };

    let Some(config_kind) = config_kind else {
        return Err(McpError::invalid_params(
            format!(
                "{context}: config JSON was provided but the component declares no Config kind"
            ),
            None,
        ));
    };
    let schema = wire::from_bytes::<SchemaType>(&config_kind.schema_wire).map_err(|e| {
        internal_msg(&format!(
            "{context}: decoding config schema for {}: {e}",
            config_kind.name
        ))
    })?;
    let resolved = resolve_bytes_params(value, &schema, max_frame_size())
        .await
        .map_err(|e| {
            McpError::invalid_params(
                format!(
                    "{context}: resolving config blob params for {}: {e}",
                    config_kind.name
                ),
                None,
            )
        })?;
    let bytes = aether_codec::encode_schema(&resolved, &schema).map_err(|e| {
        McpError::invalid_params(
            format!("{context}: config does not match {}: {e}", config_kind.name),
            None,
        )
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
    let module = selector
        .split_once('@')
        .map_or(selector, |(module, _)| module);
    format!("{module}@{export}")
}

/// Resolve the base name a `replicas` fan-out derives its `{base}-{index}`
/// names from (issue 2626), using the same precedence the component host
/// itself applies at load: caller `name` > `export` > entry actor
/// namespace. `None` when none of the three is available — the caller
/// turns that into a clean tool error naming what to set. Shared by
/// `stage_boot_manifest` (deriving `expected_names` to poll) and
/// `load_component` (deriving each replica's load name), so both sides of
/// a replicated load agree on what the components register as.
pub(super) fn replica_base_name(
    name: Option<&str>,
    export: Option<&str>,
    entry_namespace: Option<&str>,
) -> Option<String> {
    name.or(export).or(entry_namespace).map(str::to_owned)
}

/// Derive the `{base}-{index}` name set a `replicas` fan-out registers
/// under: every instance suffixed, no bare-name special case for index 0,
/// so `replicas: 1` differs from an omitted field only by the `-0` suffix.
pub(super) fn replica_names(base: &str, replicas: u32) -> Vec<String> {
    (0..replicas)
        .map(|index| format!("{base}-{index}"))
        .collect()
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
