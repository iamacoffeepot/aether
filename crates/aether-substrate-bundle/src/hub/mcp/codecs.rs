//! Schema-driven encoding and decoding for the MCP tool surface.
//! Thin adapter layer between the tool-level JSON surface and the
//! wire bytes the engine reads and writes:
//!
//! - `deliver_one` / `resolve_payload` / `encode_capture_bundle` go
//!   outbound (tools → engine).
//! - `decode_inbound` goes the other way for `receive_mail` (engine →
//!   tools, ADR-0020).
//!
//! Every function here is pure data transformation against an
//! `EngineRecord`'s declared kind descriptors — no `HubState`, no
//! business logic, no concurrency primitives beyond the outbound
//! `mail_tx`.

use crate::hub::wire::{EngineId, HubToEngine, MailFrame, SessionToken, Uuid};
use aether_codec::{decode_schema, encode_schema};
use aether_data::{KindDescriptor, SchemaType};

use crate::hub::registry::{EngineRecord, EngineRegistry};

use super::args::MailSpec;

/// Resolve one `MailSpec` against the registry, encode its payload,
/// and push to the engine's mail channel. Returns a human-readable
/// error string rather than `Err` so the batch driver can emit it as
/// a per-mail status without losing sibling success.
pub(super) async fn deliver_one(
    spec: MailSpec,
    engines: &EngineRegistry,
    sender: SessionToken,
) -> Result<(), String> {
    let uuid = Uuid::parse_str(&spec.engine_id)
        .map_err(|e| format!("engine_id is not a valid UUID: {e}"))?;
    let id = EngineId(uuid);
    let record = engines
        .get(&id)
        .ok_or_else(|| format!("unknown engine_id {}", spec.engine_id))?;

    let payload = resolve_payload(&spec, &record)?;

    let frame = HubToEngine::Mail(MailFrame {
        recipient_name: spec.recipient_name,
        kind_name: spec.kind_name,
        payload,
        count: spec.count,
        sender,
        // ADR-0042: MCP `send_mail` doesn't expose correlation
        // today — tooling can still invoke sync-like flows by
        // manually matching on echoed namespace + path per
        // ADR-0041, and we can widen MCP later when a concrete
        // need surfaces.
        correlation_id: 0,
    });
    record
        .mail_tx
        .send(frame)
        .await
        .map_err(|_| "engine disconnected".to_owned())
}

/// Decide the wire bytes for a mail by looking up the kind's
/// descriptor and feeding `params` through `encode_schema`. ADR-0019:
/// every kind has a schema; absent params is only legal when the
/// schema is `Unit`.
pub(super) fn resolve_payload(spec: &MailSpec, record: &EngineRecord) -> Result<Vec<u8>, String> {
    let desc = find_kind(record, &spec.kind_name)
        .ok_or_else(|| format!("kind {:?} has no descriptor on this engine", spec.kind_name))?;
    match (&spec.params, &desc.schema) {
        (None, SchemaType::Unit) => Ok(Vec::new()),
        (None, _) => Err(format!(
            "kind {:?} requires `params` (only Unit kinds may omit them)",
            spec.kind_name
        )),
        (Some(p), schema) => encode_schema(p, schema).map_err(|e| e.to_string()),
    }
}

pub(super) fn find_kind<'a>(record: &'a EngineRecord, name: &str) -> Option<&'a KindDescriptor> {
    record.kinds.iter().find(|k| k.name == name)
}

/// Encode each `MailSpec` in a `capture_frame` bundle against the
/// engine's descriptors, producing the JSON shape the `CaptureFrame`
/// kind's schema expects (`{mails: [{recipient_name, kind_name,
/// payload, count}]}`'s `mails` array — returned here as a JSON
/// array ready to be slotted under the outer `mails` key).
///
/// Abort-on-first-failure: a single bad envelope aborts the whole
/// bundle, matching the substrate's atomic-dispatch guarantee. This
/// also short-circuits the tool before it touches the engine wire.
pub(super) fn encode_capture_bundle(
    specs: &[MailSpec],
    record: &EngineRecord,
) -> Result<Vec<serde_json::Value>, String> {
    let mut out = Vec::with_capacity(specs.len());
    for (i, spec) in specs.iter().enumerate() {
        let payload = resolve_payload(spec, record)
            .map_err(|e| format!("envelope[{i}] ({}): {e}", spec.kind_name))?;
        let payload_bytes: Vec<serde_json::Value> = payload
            .into_iter()
            .map(|b| serde_json::Value::Number(b.into()))
            .collect();
        out.push(serde_json::json!({
            "recipient_name": spec.recipient_name,
            "kind_name": spec.kind_name,
            "payload": payload_bytes,
            "count": spec.count,
        }));
    }
    Ok(out)
}

/// Decode an inbound observation payload against the originating
/// engine's kind descriptor (ADR-0020). Returns the structured `params`
/// on success; on any failure (engine no longer in the registry, kind
/// not declared, decode error) returns `(None, Some(reason))` so the
/// agent sees both the bytes and a human-readable explanation. Lookup
/// failures are treated as decode failures rather than tool errors —
/// the rest of the batch should still drain.
pub(super) fn decode_inbound(
    engine_id: &EngineId,
    kind_name: &str,
    payload: &[u8],
    engines: &EngineRegistry,
) -> (Option<serde_json::Value>, Option<String>) {
    let Some(record) = engines.get(engine_id) else {
        return (
            None,
            Some(format!(
                "engine {} no longer connected; cannot resolve schema",
                engine_id.0
            )),
        );
    };
    let Some(desc) = find_kind(&record, kind_name) else {
        return (
            None,
            Some(format!(
                "kind {kind_name:?} has no descriptor on this engine"
            )),
        );
    };
    match decode_schema(payload, &desc.schema) {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e.to_string())),
    }
}
