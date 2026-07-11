use super::bytes::{render_bytes_reply, reply_inline_max_bytes};
use super::{
    EngineId, Kind, KindDescriptor, KindId, MailEnvelope, MailId, Mcp, McpError, NamedMail, ReplyEventJson,
    ReplyProjection, TracedMailSpec, descriptors, internal_msg, kind_id_from_parts, tagged_id,
};
use aether_kinds::trace::DispatchTracedAck;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use std::collections::HashMap;

/// Transcode the correlated reply envelopes a `call_collecting` returned
/// into the MCP wire shape (issue 1242). Per envelope: the tagged kind
/// id, the best-effort kind name, and the best-effort `decode_schema` of
/// the payload against the matching descriptor (`None` on an unknown kind
/// or a decode miss). Decode priority (issue 1804): per-engine kind cache
/// (`engine_kinds`, includes component-defined kinds populated from
/// `ListKinds`) keyed by `declared_reply` (the handler's
/// `HandlerCapability.reply` from ADR-0109) when it matches the envelope
/// kind, then a general engine-cache scan for traced batches where the
/// per-reply handler isn't known, then the static substrate vocabulary,
/// then base64. On a clean decode the raw bytes are omitted (issue 1246).
/// Order is preserved — arrival order.
pub(super) fn decode_reply_events(
    envelopes: &[MailEnvelope],
    engine_kinds: &HashMap<String, KindDescriptor>,
    declared_reply: Option<KindId>,
) -> Vec<ReplyEventJson> {
    let static_descriptors = descriptors::all();
    // Resolve the reply-`Bytes` spill threshold once for the whole batch
    // (issue 2108) and thread it through every projection.
    let inline_max = reply_inline_max_bytes();
    envelopes
        .iter()
        .map(|env| {
            // Resolve the schema for this reply envelope. Tier 1: engine
            // cache targeted by the declared reply kind — this is the path
            // that decodes component-defined reply kinds to `params` rather
            // than base64. Tier 2: general engine-cache scan — covers
            // send_mail_traced batches where `declared_reply` is None and
            // any handler in the batch may have replied. Tier 3: static
            // substrate vocabulary — the fallback for native chassis cap
            // replies not yet in the engine cache. Tier 4: base64.
            let descriptor: Option<KindDescriptor> = declared_reply
                .filter(|&dr| dr == env.kind)
                .and_then(|dr| engine_kinds.values().find(|d| kind_id_from_parts(&d.name, &d.schema) == dr.0).cloned())
                .or_else(|| {
                    engine_kinds.values().find(|d| kind_id_from_parts(&d.name, &d.schema) == env.kind.0).cloned()
                })
                .or_else(|| {
                    static_descriptors.iter().find(|d| kind_id_from_parts(&d.name, &d.schema) == env.kind.0).cloned()
                });
            let kind_name = descriptor.as_ref().map(|d| d.name.clone());
            let (params, payload_bytes) = descriptor
                .as_ref()
                .and_then(|d| {
                    // Render reply `Bytes` fields back to readable text /
                    // base64 (issue 1944): the strict decoder emits a byte
                    // array, the MCP front projects it for the caller.
                    aether_codec::decode_schema(&env.payload, &d.schema)
                        .ok()
                        .map(|v| render_bytes_reply(v, &d.schema, inline_max))
                })
                .map_or_else(
                    // Decode miss: base64 the raw payload as the fallback
                    // (the only signal when `params` is `null`).
                    || (None, Some(STANDARD.encode(&env.payload))),
                    // Clean decode: `params` is the surfacing; omit the
                    // raw bytes so they aren't duplicated as an int-array.
                    |v| (Some(v), None),
                );
            ReplyEventJson {
                // Render the kind id as the ADR-0064 tagged string the
                // rest of the MCP wire uses, falling back to a hex
                // literal on an unencodable (non-kind-domain) id.
                kind_id: tagged_id::encode(env.kind.0).unwrap_or_else(|| format!("{:#x}", env.kind.0)),
                kind_name,
                params,
                payload_bytes,
            }
        })
        .collect()
}

/// Best-effort recognition of the common decoded error shapes the MCP front
/// can identify without component-specific semantics. Matching is deliberately
/// exact so successful names containing `err` (for example `terra` or
/// `deferred`) are not projected as failures.
pub(super) fn is_error_reply(reply: &ReplyEventJson) -> bool {
    let params_is_error = match reply.params.as_ref() {
        Some(serde_json::Value::String(variant)) => variant == "Err",
        Some(serde_json::Value::Object(object)) => object.len() == 1 && object.contains_key("Err"),
        _ => false,
    };
    if params_is_error {
        return true;
    }

    reply.kind_name.as_deref().is_some_and(|name| {
        let final_segment = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
        final_segment == "err"
            || final_segment == "error"
            || final_segment.ends_with("_err")
            || final_segment.ends_with("_error")
    })
}

/// Project a complete decoded reply stream without changing arrival order.
/// Terminal selection is index-based so a final reply that is itself an error
/// is retained once rather than deduplicated by payload equality.
pub(super) fn project_replies(replies: Vec<ReplyEventJson>, projection: ReplyProjection) -> Vec<ReplyEventJson> {
    match projection {
        ReplyProjection::All => replies,
        ReplyProjection::None => replies.into_iter().filter(is_error_reply).collect(),
        ReplyProjection::Terminal => {
            let terminal = replies.len().checked_sub(1);
            replies
                .into_iter()
                .enumerate()
                .filter_map(|(index, reply)| (Some(index) == terminal || is_error_reply(&reply)).then_some(reply))
                .collect()
        }
    }
}

/// Decode the `DispatchTracedAck` from a `send_mail_traced` ack call's
/// collected events (issue 1242). The synchronous ack is the *first*
/// reply event the trace cap emits on the dispatch cid; later events are
/// downstream cap replies handled separately. An absent or undecodable
/// ack, or an `Err` ack, is a tool error.
pub(super) fn decode_traced_ack(events: &[MailEnvelope]) -> Result<MailId, McpError> {
    let ack_env = events.first().ok_or_else(|| internal_msg("send_mail_traced: no ack reply from the trace cap"))?;
    let ack = DispatchTracedAck::decode_from_bytes(&ack_env.payload)
        .ok_or_else(|| internal_msg("undecodable DispatchTracedAck"))?;
    match ack {
        DispatchTracedAck::Ok { root } => Ok(root),
        DispatchTracedAck::Err { error } => Err(internal_msg(&format!("send_mail_traced dispatch failed: {error}"))),
    }
}

/// The collected `send_mail_traced` events minus the leading ack (the
/// `DispatchTracedAck` [`decode_traced_ack`] consumes), leaving the flat
/// list of downstream cap replies to surface as `replies` (issue 1242).
pub(super) fn strip_ack(events: &[MailEnvelope]) -> &[MailEnvelope] {
    events.get(1..).unwrap_or(&[])
}
impl Mcp {
    /// Encode a `send_mail_traced` batch into the same `NamedMail`
    /// shape `CaptureFrame` carries: name-level addressing + schema-
    /// encoded payload. The substrate's `TraceObserver` resolves the
    /// names through its registry at dispatch time. Same lookup path
    /// `encode_capture_bundle` uses (per-engine merged view, ADR-0091),
    /// just over `TracedMailSpec` instead of `CaptureMailSpec`.
    pub(super) async fn encode_traced_bundle(
        &self,
        engine: EngineId,
        specs: &[TracedMailSpec],
    ) -> anyhow::Result<Vec<NamedMail>> {
        self.encode_named_mail_bundle(engine, specs).await
    }
}
