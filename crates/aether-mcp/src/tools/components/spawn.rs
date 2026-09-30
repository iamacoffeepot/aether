//! The `spawn` tool: ask for instances of a published type (ADR-0241 §9).
//! The name an instance takes decides the answer: a live name answers with
//! that instance, an absent one stands it up.

use super::super::describe::published_type_capabilities;
use super::super::envelope::engine_envelope;
use super::super::render::{frame_size_aware_error, internal, internal_msg, json, project_capabilities};
use super::super::{COMPONENT_CAP, Mcp};
use super::{component_config_bytes, reject_key_with_replicas, reject_replicas_out_of_range, replicas_reply};
use crate::args::SpawnArgs;
use aether_data::{EngineId, ErasedActorPath, Kind};
use aether_kinds::{ComponentCapabilities, Spawn, SpawnResult};
use rmcp::ErrorData as McpError;

/// The spawns one tool call sends: `replicas` spawns of `namespace` with no
/// key, or one spawn keyed by `key`, each beneath `parent` and built with
/// `config`.
pub(in crate::tools) struct SpawnRequest {
    pub(in crate::tools) namespace: String,
    pub(in crate::tools) key: Option<String>,
    pub(in crate::tools) parent: Option<ErasedActorPath>,
    pub(in crate::tools) config: Vec<u8>,
    pub(in crate::tools) replicas: Option<u32>,
}

pub(in crate::tools) async fn spawn(mcp: &Mcp, args: SpawnArgs) -> Result<String, McpError> {
    let (engine, engine_id) = mcp.resolve_engine(args.engine_id.as_deref()).await?;
    reject_replicas_out_of_range(args.replicas, &args.namespace)?;
    reject_key_with_replicas(args.key.as_deref(), args.replicas, &args.namespace)?;
    let context = format!("spawn {:?}", args.namespace);

    // Only a supplied config needs the type's config kind: its id and name
    // come from the published type's surface, its schema from the engine's
    // kind vocabulary (refreshed on a miss).
    let config_kind = if args.config.is_some() || args.config_path.is_some() {
        match published_type_capabilities(mcp, engine, &args.namespace).await?.config {
            Some(config) => Some(mcp.lookup_descriptor(engine, &config.name).await.map_err(internal)?),
            None => None,
        }
    } else {
        None
    };
    let config = component_config_bytes(config_kind.as_ref(), args.config, args.config_path.as_deref(), &context)
        .await?
        .unwrap_or_default();
    let parent = match args.parent.as_deref() {
        Some(parent) => Some(mcp.resolve_engine_path(engine, parent).await.map_err(internal)?),
        None => None,
    };

    let request = SpawnRequest { namespace: args.namespace, key: args.key, parent, config, replicas: args.replicas };
    spawn_instances(mcp, engine, &engine_id, request, args.full).await
}

/// Send `request`'s spawns one at a time, recording each answered instance in
/// the component cache. One spawn answers with the instance's address,
/// state, and capabilities; a `replicas` fan-out answers with one shared
/// capabilities block and each instance's address and state. A failure
/// after k spawns names k: those instances stay live.
pub(in crate::tools) async fn spawn_instances(
    mcp: &Mcp,
    engine: EngineId,
    engine_id: &str,
    request: SpawnRequest,
    full: bool,
) -> Result<String, McpError> {
    let SpawnRequest { namespace, key, parent, config, replicas } = request;
    let count = replicas.unwrap_or(1);
    let context = format!("spawn {namespace:?}");

    let mut instances = Vec::with_capacity(count as usize);
    let mut shared_caps: Option<ComponentCapabilities> = None;
    for index in 0..count {
        let spawn =
            Spawn { namespace: namespace.clone(), key: key.clone(), parent: parent.clone(), config: config.clone() };
        let reply = mcp
            .session
            .call_one(engine_envelope(engine, COMPONENT_CAP, &spawn))
            .await
            .map_err(|e| frame_size_aware_error(&context, e))?;
        let (path, capabilities, state) = match SpawnResult::decode_from_bytes(&reply.payload) {
            Some(SpawnResult::Spawned { path, capabilities }) => (path, capabilities, "spawned"),
            Some(SpawnResult::Live { path, capabilities }) => (path, capabilities, "live"),
            Some(SpawnResult::Err { error }) if replicas.is_some() => {
                return Err(internal_msg(&format!(
                    "{context} instance {index} of {count} failed: {error} ({index} of {count} instances spawned \
                     before this failure; they stay live)"
                )));
            }
            Some(SpawnResult::Err { error }) => return Err(internal_msg(&format!("{context}: {error}"))),
            None => return Err(internal_msg("undecodable SpawnResult")),
        };
        mcp.components.record_spawned(engine, &namespace, path.clone(), &capabilities);
        if replicas.is_none() {
            return json(&serde_json::json!({
                "engine_id": engine_id,
                "address": path.to_string(),
                "state": state,
                "capabilities": project_capabilities(&capabilities, full),
            }));
        }
        instances.push(serde_json::json!({ "address": path.to_string(), "state": state }));
        shared_caps.get_or_insert(capabilities);
    }
    replicas_reply(
        engine_id,
        &shared_caps.expect("replicas >= 1: the loop either filled shared_caps or returned early"),
        &instances,
        full,
    )
}
