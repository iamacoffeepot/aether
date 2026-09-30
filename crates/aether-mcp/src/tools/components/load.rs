//! The `load_component` tool: publish a stored module, then spawn one of its
//! types (ADR-0241 §9). A replicated load publishes once and spawns N times.

use super::super::Mcp;
use super::super::render::internal_msg;
use super::publish::publish_module;
use super::spawn::{SpawnRequest, spawn_instances};
use super::{
    component_config_bytes, reject_key_with_replicas, reject_replicas_out_of_range, selector_with_explicit_export,
};
use crate::args::LoadComponentArgs;
use aether_kinds::PublishedType;
use rmcp::ErrorData as McpError;

pub(in crate::tools) async fn load_component(mcp: &Mcp, args: LoadComponentArgs) -> Result<String, McpError> {
    let (engine, engine_id) = mcp.resolve_engine(args.engine_id.as_deref()).await?;
    let selector = selector_with_explicit_export(&args.selector, args.namespace.as_deref());
    reject_replicas_out_of_range(args.replicas, &selector)?;
    reject_key_with_replicas(args.key.as_deref(), args.replicas, &selector)?;
    // ADR-0116: resolve the selector hub-local to the wasm bytes; a
    // `module@actor` selector's `@actor` half rides back as `export`.
    let resolved = mcp.resolve_component(&selector).await?;
    // An explicit `namespace` wins over the selector's `@actor` half; with
    // neither, only a module exporting one type names its type.
    let namespace = match args.namespace.or(resolved.export) {
        Some(namespace) => namespace,
        None => match resolved.exports.as_slice() {
            [sole] => sole.clone(),
            exports => {
                return Err(McpError::invalid_params(
                    format!(
                        "load_component {selector:?}: the module exports {} types ({exports:?}); name the one to \
                         spawn with `namespace` or a `module@actor` selector",
                        exports.len()
                    ),
                    None,
                ));
            }
        },
    };
    let context = format!("load_component {selector:?}");
    let config =
        component_config_bytes(resolved.config_kind.as_ref(), args.config, args.config_path.as_deref(), &context)
            .await?
            .unwrap_or_default();

    let types = publish_module(mcp, engine, &context, resolved.wasm, Vec::new()).await?;
    let bound = bound_namespace(&types, &namespace).ok_or_else(|| {
        let published: Vec<&str> = types.iter().map(|published| published.namespace.as_str()).collect();
        internal_msg(&format!("{context}: the publish bound no type {namespace:?}; it bound {published:?}"))
    })?;
    let request =
        SpawnRequest { namespace: bound.to_owned(), key: args.key, parent: None, config, replicas: args.replicas };
    spawn_instances(mcp, engine, &engine_id, request, args.full).await
}

/// The name a publish bound the declared `namespace` under: `namespace`
/// itself, or `namespace.<hash>` for a content-addressed module (ADR-0241
/// §3), so a spawn names what the engine published without recomputing the
/// hash.
pub(in crate::tools) fn bound_namespace<'a>(types: &'a [PublishedType], namespace: &str) -> Option<&'a str> {
    let content_addressed = |published: &str| {
        published
            .strip_prefix(namespace)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
    };
    types
        .iter()
        .find(|published| published.namespace == namespace)
        .or_else(|| types.iter().find(|published| content_addressed(&published.namespace)))
        .map(|published| published.namespace.as_str())
}
