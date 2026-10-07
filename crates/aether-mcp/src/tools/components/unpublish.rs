//! The `unpublish` tool: withdraw one published namespace on one engine
//! (ADR-0250 §5). Dropping the namespace's live instances comes first — the
//! host refuses an unpublish while one still runs, naming it.

use super::super::envelope::engine_envelope;
use super::super::render::{frame_size_aware_error, internal_msg, json};
use super::super::{COMPONENT_CAP, Mcp};
use crate::args::UnpublishArgs;
use aether_data::Kind;
use aether_kinds::{Unpublish, UnpublishResult};
use rmcp::ErrorData as McpError;

pub(in crate::tools) async fn unpublish(mcp: &Mcp, args: UnpublishArgs) -> Result<String, McpError> {
    let (engine, engine_id) = mcp.resolve_engine(args.engine_id.as_deref()).await?;
    let context = format!("unpublish {:?}", args.namespace);
    let reply = mcp
        .session
        .call_one(engine_envelope(engine, COMPONENT_CAP, &Unpublish { namespace: args.namespace }))
        .await
        .map_err(|e| frame_size_aware_error(&context, e))?;
    match UnpublishResult::decode_from_bytes(&reply.payload) {
        Some(UnpublishResult::Ok { namespace }) => {
            mcp.components.forget_namespace(engine, &namespace);
            json(&serde_json::json!({ "engine_id": engine_id, "namespace": namespace }))
        }
        Some(UnpublishResult::Err { error }) => Err(internal_msg(&format!("{context}: {error}"))),
        None => Err(internal_msg("undecodable UnpublishResult")),
    }
}
