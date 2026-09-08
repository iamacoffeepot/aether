use std::collections::BTreeMap;

use aether_data::{Kind, KindDescriptor, tagged_id};
use aether_inventory::kinds::{HandlersResult, ListHandlers};
use aether_kinds::{DescribeComponent, DescribeComponentResult};
use rmcp::ErrorData as McpError;

use crate::args::{
    DescribeComponentArgs, DescribeHandlersArgs, DescribeHandlersResponse, DescribeKindsArgs, DescribeKindsResponse,
    KindDetail, KindFamily, KindSummary, NativeCapHandlers, NativeHandlerJson, TransformListing,
};

use super::envelope::engine_envelope;
use super::ids::{parse_mailbox_id, static_kind_name};
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
    // A tagged id remains a local cache-only fast path. Every textual
    // address is resolved by the selected engine, which returns both the
    // real mailbox id used as the cache key and its canonical path. The
    // component host still receives the operator's original spelling so its
    // own engine-atomic name handling remains the forwarding contract.
    let (mailbox_id, forward_name) = if args.address.starts_with("mbx-") {
        (parse_mailbox_id(&args.address)?, None)
    } else {
        let (mailbox_id, _) = mcp.resolve_engine_address(engine, &args.address).await.map_err(internal)?;
        (mailbox_id, Some(args.address.clone()))
    };

    // Cache fast-path: populated by load_component / replace_component or
    // a prior name-resolved describe.
    let cached =
        mcp.components.lock().expect("component cache mutex is never poisoned").get(&(engine, mailbox_id)).cloned();
    if let Some(caps) = cached {
        return component_reply(&engine_id, &args.address, &caps, args.full);
    }

    // Cache miss. With a lineage name, ask the substrate live — this is
    // the load-bearing half: the cache is empty for a boot-loaded
    // component, but the substrate always holds the live loaded set. With
    // only a `mbx-` id there is no name to forward, so the cache was the
    // only source.
    let Some(name) = forward_name else {
        return Err(McpError::invalid_params(
            format!(
                "no component cached at {} on engine {engine_id} — address by lineage name to resolve \
                     live, or load_component / replace_component to populate this cache",
                args.address
            ),
            None,
        ));
    };
    let reply = mcp
        .session
        .call_one(engine_envelope(engine, COMPONENT_CAP, &DescribeComponent { name: name.clone() }))
        .await
        .map_err(internal)?;
    match DescribeComponentResult::decode_from_bytes(&reply.payload) {
        Some(DescribeComponentResult::Ok { capabilities }) => {
            mcp.components
                .lock()
                .expect("component cache mutex is never poisoned")
                .insert((engine, mailbox_id), capabilities.clone());
            component_reply(&engine_id, &args.address, &capabilities, args.full)
        }
        Some(DescribeComponentResult::Err { error }) => Err(internal_msg(&error)),
        None => Err(internal_msg("undecodable DescribeComponentResult")),
    }
}

/// Name the engine that answered alongside the capabilities. `engine_id` may
/// have been auto-resolved rather than named by the caller, so the reply says
/// which engine — and which address on it — the description came from.
pub(super) fn component_reply(
    engine_id: &str,
    address: &str,
    capabilities: &super::ComponentCapabilities,
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
        folded.entry(entry.namespace).or_default().push(NativeHandlerJson {
            // Input kind id rendered as the ADR-0064 tagged string,
            // falling back to a hex literal on an unencodable id.
            input_id: tagged_id::encode(entry.id.0).unwrap_or_else(|| format!("{:#x}", entry.id.0)),
            input_name: entry.name,
            // The reply kind id is the contract; resolve its name
            // best-effort from the static substrate vocabulary so
            // the In -> Out reads without a second lookup. A
            // component-defined reply kind stays `None`.
            reply_id: entry.reply.map(|id| tagged_id::encode(id.0).unwrap_or_else(|| format!("{:#x}", id.0))),
            reply_name: entry.reply.and_then(static_kind_name),
        });
    }
    let caps = folded.into_iter().map(|(namespace, handlers)| NativeCapHandlers { namespace, handlers }).collect();
    json(&DescribeHandlersResponse { engine_id, caps })
}
