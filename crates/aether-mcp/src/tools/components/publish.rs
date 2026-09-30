//! The `publish` tool: bind every namespace a stored module exports on one
//! engine (ADR-0241 §3, §9). A publish of a successor republishes every live
//! instance of those namespaces as one group, which is how code is replaced.

use super::super::envelope::engine_envelope;
use super::super::render::{frame_size_aware_error, internal, internal_msg, json, project_capabilities};
use super::super::{COMPONENT_CAP, Mcp};
use super::cache::leaf_namespace;
use super::component_config_bytes;
use crate::args::{InstanceConfigArgs, PublishArgs};
use aether_data::{Blob, EngineId, Kind};
use aether_kinds::{InstanceConfig, Publish, PublishResult, PublishedType};
use rmcp::ErrorData as McpError;

pub(in crate::tools) async fn publish(mcp: &Mcp, args: PublishArgs) -> Result<String, McpError> {
    let (engine, engine_id) = mcp.resolve_engine(args.engine_id.as_deref()).await?;
    let PublishArgs { selector, configs, full, .. } = args;
    if let Some((_, actor)) = selector.split_once('@') {
        return Err(McpError::invalid_params(
            format!(
                "publish {selector:?}: a publish binds every namespace the module exports, so it names no actor \
                 ({actor:?}); pass the module selector alone"
            ),
            None,
        ));
    }
    // ADR-0116: resolve the selector hub-local to the module's wasm bytes
    // (hash-primary, so a hash pins or rolls to an exact build).
    let resolved = mcp.resolve_component(&selector).await?;

    let mut encoded = Vec::with_capacity(configs.len());
    for InstanceConfigArgs { address, config, config_path } in configs {
        let context = format!("publish {selector:?} config for {address:?}");
        // The engine resolves the operator's spelling to the canonical
        // lineage; its leaf names the instance's type (ADR-0241 §5), and
        // resolving `module@type` answers that type's config kind in the
        // successor module, the kind its config is encoded to.
        let path = mcp.resolve_engine_path(engine, &address).await.map_err(internal)?;
        let typed = mcp.resolve_component(&format!("{selector}@{}", leaf_namespace(&path))).await?;
        let config = component_config_bytes(typed.config_kind.as_ref(), config, config_path.as_deref(), &context)
            .await?
            .ok_or_else(|| {
                McpError::invalid_params(format!("{context}: set one of `config` or `config_path`"), None)
            })?;
        encoded.push(InstanceConfig { path, config });
    }

    let types = publish_module(mcp, engine, &format!("publish {selector:?}"), resolved.wasm, encoded).await?;
    let types: Vec<_> = types
        .iter()
        .map(|published| {
            serde_json::json!({
                "namespace": published.namespace,
                "capabilities": project_capabilities(&published.capabilities, full),
            })
        })
        .collect();
    json(&serde_json::json!({ "engine_id": engine_id, "types": types }))
}

/// Send one `Publish` of `wasm` to `engine`'s component host and record the
/// types it bound in the component cache. `context` names the caller in an
/// error.
pub(in crate::tools) async fn publish_module(
    mcp: &Mcp,
    engine: EngineId,
    context: &str,
    wasm: Vec<u8>,
    configs: Vec<InstanceConfig>,
) -> Result<Vec<PublishedType>, McpError> {
    let reply = mcp
        .session
        .call_one(engine_envelope(engine, COMPONENT_CAP, &Publish { code: Blob::from(wasm), configs }))
        .await
        .map_err(|e| frame_size_aware_error(context, e))?;
    match PublishResult::decode_from_bytes(&reply.payload) {
        Some(PublishResult::Ok { types }) => {
            mcp.components.record_published(engine, &types);
            Ok(types)
        }
        Some(PublishResult::Err { error }) => Err(internal_msg(&format!("{context}: {error}"))),
        None => Err(internal_msg("undecodable PublishResult")),
    }
}
