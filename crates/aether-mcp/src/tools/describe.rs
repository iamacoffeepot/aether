use std::collections::BTreeMap;

use aether_data::{EngineId, Kind, KindDescriptor, ReplyContract, tagged_id};
use aether_inventory::kinds::{HandlersResult, ListHandlers};
use aether_kinds::{ComponentCapabilities, DescribeComponent, DescribeComponentResult};
use rmcp::ErrorData as McpError;

use crate::args::{
    DescribeComponentArgs, DescribeHandlersArgs, DescribeHandlersResponse, DescribeKindsArgs, DescribeKindsResponse,
    KindDetail, KindFamily, KindSummary, NativeCapHandlers, NativeHandlerJson, TransformListing,
};

use super::envelope::engine_envelope;
use super::ids::static_kind_name;
use super::render::{internal, internal_msg, json, project_capabilities, render_shape};
use super::{COMPONENT_CAP, INVENTORY_CAP, Mcp};

pub(super) async fn describe_kinds(mcp: &Mcp, args: DescribeKindsArgs) -> Result<String, McpError> {
    let (engine, engine_id) = mcp.resolve_engine(args.engine_id.as_deref()).await?;

    if args.names.is_some() && (args.families || args.prefix.is_some()) {
        return Err(McpError::invalid_params(
            "names cannot be combined with families or prefix; it is an exclusive exact-name selector",
            None,
        ));
    }
    if args.detail == KindDetail::Schema && !args.families && args.names.is_none() && args.prefix.is_none() {
        return Err(McpError::invalid_params(
            "bare detail:\"schema\" is not allowed; select kinds with names or prefix, or request a families digest",
            None,
        ));
    }

    // Prefill the engine's cache from the static baseline, then refresh
    // from its live inventory. The merged snapshot (static ∪
    // capability-owned ∪ component-defined) is the authoritative source;
    // a failed refresh leaves the prefilled baseline rather than erroring.
    mcp.prefill_engine(engine);
    mcp.refresh_engine_kinds(engine).await;
    let descriptors: Vec<KindDescriptor> = mcp.snapshot_engine_kinds(engine).into_values().collect();

    if args.families {
        let mut counts = BTreeMap::<String, usize>::new();
        for descriptor in descriptors
            .iter()
            .filter(|descriptor| args.prefix.as_ref().is_none_or(|prefix| descriptor.name.starts_with(prefix.as_str())))
        {
            let family = descriptor
                .name
                .rsplit_once('.')
                .map_or(descriptor.name.as_str(), |(namespace, _)| namespace)
                .to_owned();
            *counts.entry(family).or_default() += 1;
        }
        let families = counts.into_iter().map(|(family, count)| KindFamily { family, count }).collect();
        return json(&DescribeKindsResponse { engine_id, kinds: None, families: Some(families) });
    }

    let filtered: Vec<_> = if let Some(names) = &args.names {
        descriptors.into_iter().filter(|descriptor| names.iter().any(|name| name == &descriptor.name)).collect()
    } else if let Some(prefix) = &args.prefix {
        descriptors.into_iter().filter(|d| d.name.starts_with(prefix.as_str())).collect()
    } else {
        descriptors
    };
    let kinds = match args.detail {
        KindDetail::Schema => serde_json::to_value(&filtered),
        KindDetail::Shape => serde_json::to_value(
            filtered
                .iter()
                .map(|d| KindSummary { name: d.name.clone(), shape: render_shape(&d.schema) })
                .collect::<Vec<_>>(),
        ),
    }
    .map_err(|error| internal_msg(&format!("describe_kinds projection: {error}")))?;

    json(&DescribeKindsResponse { engine_id, kinds: Some(kinds), families: None })
}

pub(super) fn describe_transforms() -> Result<String, McpError> {
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

pub(super) async fn describe_component(mcp: &Mcp, args: DescribeComponentArgs) -> Result<String, McpError> {
    let (engine, engine_id) = mcp.resolve_engine(args.engine_id.as_deref()).await?;
    match (args.address, args.namespace) {
        (Some(address), None) => {
            let capabilities = instance_capabilities(mcp, engine, &address).await?;
            component_reply(&engine_id, &address, &capabilities, args.full)
        }
        (None, Some(namespace)) => {
            let capabilities = published_type_capabilities(mcp, engine, &namespace).await?;
            json(&serde_json::json!({
                "engine_id": engine_id,
                "namespace": namespace,
                "capabilities": project_capabilities(&capabilities, args.full),
            }))
        }
        _ => Err(McpError::invalid_params(
            "describe_component takes exactly one of `address` (a live instance) or `namespace` (a published type)",
            None,
        )),
    }
}

/// The surface of the live instance at `address`. Every address is resolved
/// by the selected engine, which returns the canonical path used as the
/// cache key; a miss asks the component host live, which still receives the
/// operator's original spelling so its own engine-atomic name handling
/// remains the forwarding contract. The cache is empty for a boot-loaded
/// component, but the substrate always holds the live loaded set.
async fn instance_capabilities(mcp: &Mcp, engine: EngineId, address: &str) -> Result<ComponentCapabilities, McpError> {
    let canonical = mcp.resolve_engine_path(engine, address).await.map_err(internal)?;
    if let Some(capabilities) = mcp.components.instance(engine, &canonical) {
        return Ok(capabilities);
    }
    let capabilities = forward_describe(mcp, engine, address).await?;
    mcp.components.record_instance(engine, canonical, capabilities.clone());
    Ok(capabilities)
}

/// The surface of the type published as `namespace`, from the cache a
/// publish or spawn filled, else from the component host, which answers a
/// published namespace from its published module whether or not an
/// instance of it is live (ADR-0241 §3).
pub(super) async fn published_type_capabilities(
    mcp: &Mcp,
    engine: EngineId,
    namespace: &str,
) -> Result<ComponentCapabilities, McpError> {
    if let Some(capabilities) = mcp.components.published_type(engine, namespace) {
        return Ok(capabilities);
    }
    let capabilities = forward_describe(mcp, engine, namespace).await?;
    mcp.components.record_type(engine, namespace, capabilities.clone());
    Ok(capabilities)
}

async fn forward_describe(mcp: &Mcp, engine: EngineId, name: &str) -> Result<ComponentCapabilities, McpError> {
    let reply = mcp
        .session
        .call_one(engine_envelope(engine, COMPONENT_CAP, &DescribeComponent { name: name.to_owned() }))
        .await
        .map_err(internal)?;
    match DescribeComponentResult::decode_from_bytes(&reply.payload) {
        Some(DescribeComponentResult::Ok { capabilities }) => Ok(capabilities),
        Some(DescribeComponentResult::Err { error }) => Err(internal_msg(&error)),
        None => Err(internal_msg("undecodable DescribeComponentResult")),
    }
}

/// Name the engine that answered alongside the capabilities. `engine_id` may
/// have been auto-resolved rather than named by the caller, so the reply says
/// which engine — and which address on it — the description came from.
fn component_reply(
    engine_id: &str,
    address: &str,
    capabilities: &ComponentCapabilities,
    full: bool,
) -> Result<String, McpError> {
    json(&serde_json::json!({
        "engine_id": engine_id,
        "address": address,
        "capabilities": project_capabilities(capabilities, full),
    }))
}

pub(super) async fn describe_handlers(mcp: &Mcp, args: DescribeHandlersArgs) -> Result<String, McpError> {
    let (engine, engine_id) = mcp.resolve_engine(args.engine_id.as_deref()).await?;
    let reply =
        mcp.session.call_one(engine_envelope(engine, INVENTORY_CAP, &ListHandlers {})).await.map_err(internal)?;
    let Some(HandlersResult { handlers }) = HandlersResult::decode_from_bytes(&reply.payload) else {
        return Err(internal_msg("undecodable HandlersResult"));
    };
    // Fold the flat per-handler manifest per owning namespace so each
    // native cap reads as a describe_component-style handler list. A
    // BTreeMap keeps the caps (and their handlers) in a stable order.
    let mut folded: BTreeMap<String, Vec<NativeHandlerJson>> = BTreeMap::new();
    for entry in handlers {
        // The reply kind id is the contract; resolve its name
        // best-effort from the static substrate vocabulary so the
        // In -> Out reads without a second lookup. A component-defined
        // reply kind stays `None`. Only a `One` row names a kind: a
        // manual handler replies at run time with no declared kind.
        let (reply_class, reply_id) = match entry.reply {
            ReplyContract::None => ("none", None),
            ReplyContract::One(id) => ("one", Some(id)),
            ReplyContract::Manual => ("manual", None),
        };
        folded.entry(entry.namespace).or_default().push(NativeHandlerJson {
            // Input kind id rendered as the ADR-0064 tagged string,
            // falling back to a hex literal on an unencodable id.
            input_id: tagged_id::encode(entry.id.0).unwrap_or_else(|| format!("{:#x}", entry.id.0)),
            input_name: entry.name,
            reply_class,
            reply_id: reply_id.map(|id| tagged_id::encode(id.0).unwrap_or_else(|| format!("{:#x}", id.0))),
            reply_name: reply_id.and_then(static_kind_name),
        });
    }
    let caps = folded.into_iter().map(|(namespace, handlers)| NativeCapHandlers { namespace, handlers }).collect();
    json(&DescribeHandlersResponse { engine_id, caps })
}
