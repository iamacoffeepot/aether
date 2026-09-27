use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::time::Duration;

use aether_data::{EngineId, ErasedActorPath, Kind};
use aether_kinds::trace::{
    DescribeTreeResult, DispatchTraced, TRACE_MAILBOX_NAME, TraceMailId, TraceTail, TraceTailResult,
};
use aether_trace::walk::TreeWalk;
use rmcp::ErrorData as McpError;

use crate::args::{
    MailSpec, MailStatus, ReplyEventJson, ReplyProjection, SendMailArgs, SendMailTracedArgs, SendMailTracedResponse,
    TraceShape,
};

use super::envelope::{engine_envelope, engine_envelope_to};
use super::ids::{mail_id_to_json, render_compact_tree};
use super::render::{internal, internal_msg, json};
use super::reply::{decode_reply_events, decode_traced_ack, project_replies, strip_ack};
use super::reply_format::{ReplyFormat, resolve_reply_format};
use super::{AWAIT_TIMEOUT_CAP_MILLIS, AWAIT_TIMEOUT_DEFAULT_MILLIS, Mcp};

pub(super) async fn send_mail(mcp: &Mcp, mut args: SendMailArgs) -> Result<String, McpError> {
    let fire_and_forget = args.fire_and_forget;
    let reply_projection = args.replies;
    let formats = match args.format.as_ref() {
        Some(raw) => Some(resolve_item_formats(mcp, &mut args.mails, raw).await?),
        None => None,
    };

    let mut statuses = Vec::with_capacity(args.mails.len());
    for (index, spec) in args.mails.into_iter().enumerate() {
        let status = if fire_and_forget {
            let status = match mcp.deliver_one_fire(spec).await {
                Ok(()) => "dispatched".to_owned(),
                Err(e) => format!("error: {e}"),
            };
            MailStatus { index, status, replies: Vec::new(), timed_out: false }
        } else {
            let format = formats.as_ref().and_then(|formats| formats.for_item(index));
            settle_mail_item(mcp, index, spec, reply_projection, format).await
        };
        statuses.push(status);
    }
    json(&statuses)
}

/// A `send_mail` batch's validated `format` masks: one per distinct engine
/// the batch names, since a kind name resolves per engine, plus each item's
/// engine.
struct ItemFormats {
    item_engines: Vec<EngineId>,
    by_engine: HashMap<EngineId, ReplyFormat>,
}

impl ItemFormats {
    fn for_item(&self, index: usize) -> Option<&ReplyFormat> {
        self.item_engines.get(index).and_then(|engine| self.by_engine.get(engine))
    }
}

/// The `format` pre-pass: resolve every item's engine, pin the item to that
/// resolved id so dispatch uses exactly the engine its mask was validated on,
/// and validate the mask once per distinct engine. Any failure refuses the
/// whole call before the first item is prepared, so no mail moves.
async fn resolve_item_formats(
    mcp: &Mcp,
    mails: &mut [MailSpec],
    raw: &serde_json::Map<String, serde_json::Value>,
) -> Result<ItemFormats, McpError> {
    let mut item_engines = Vec::with_capacity(mails.len());
    let mut by_engine = HashMap::new();
    for (index, spec) in mails.iter_mut().enumerate() {
        let (engine, engine_id) = mcp.resolve_engine(spec.engine_id.as_deref()).await.map_err(|error| {
            McpError::invalid_params(format!("send_mail format: item {index}: {}", error.message), None)
        })?;
        spec.engine_id = Some(engine_id);
        if let Entry::Vacant(slot) = by_engine.entry(engine) {
            slot.insert(
                resolve_reply_format(mcp, engine, raw)
                    .await
                    .map_err(|error| McpError::invalid_params(format!("send_mail {error}"), None))?,
            );
        }
        item_engines.push(engine);
    }
    Ok(ItemFormats { item_engines, by_engine })
}

/// Settle one mail item the `send_mail` way: dispatch it, await the
/// chain, decode + project the correlated replies, and fold any
/// transport / encode failure into the item's status string rather than
/// a call error. Shared by `send_mail`'s settled path and
/// `spawn_substrate`'s post-boot init bundle (issue 3580).
pub(super) async fn settle_mail_item(
    mcp: &Mcp,
    index: usize,
    spec: MailSpec,
    reply_projection: ReplyProjection,
    format: Option<&ReplyFormat>,
) -> MailStatus {
    let mut replies = Vec::new();
    let mut timed_out = false;
    let status = match mcp.deliver_one(spec).await {
        Ok(delivered) => {
            // The prepared direct path carries the engine's canonical lineage
            // forward, so a short-path spelling consults the same
            // component-capability cache entry as its canonical spelling.
            let declared_reply = ErasedActorPath::new(&delivered.canonical_recipient).ok().and_then(|canonical| {
                let cache = mcp.components.lock().expect("component cache mutex is never poisoned");
                cache.get(&(delivered.engine, canonical)).and_then(|caps| {
                    caps.handlers.iter().find(|handler| handler.name == delivered.kind_name).and_then(|handler| {
                        match handler.reply {
                            aether_data::ReplyContract::One(id) => Some(id),
                            _ => None,
                        }
                    })
                })
            });
            let engine_kinds = mcp.snapshot_engine_kinds(delivered.engine);
            replies = project_replies(
                decode_reply_events(&delivered.events, &engine_kinds, declared_reply, format),
                reply_projection,
            );
            timed_out = delivered.timed_out;
            if delivered.timed_out {
                "timeout"
            } else {
                "delivered"
            }
            .to_owned()
        }
        Err(e) => format!("error: {e}"),
    };
    MailStatus { index, status, replies, timed_out }
}

pub(super) async fn send_mail_traced(mcp: &Mcp, args: SendMailTracedArgs) -> Result<String, McpError> {
    let (engine, engine_id) = mcp.resolve_engine(args.engine_id.as_deref()).await?;
    // Validate the reply mask against this engine's kinds before the batch
    // is encoded, so a bad mask refuses the call before any mail moves.
    let format = match args.format.as_ref() {
        Some(raw) => Some(
            resolve_reply_format(mcp, engine, raw)
                .await
                .map_err(|error| McpError::invalid_params(format!("send_mail_traced {error}"), None))?,
        ),
        None => None,
    };
    // Encode the batch before sending — a bad spec produces a
    // clean invalid-params error and never touches the wire.
    // Same shape `CaptureFrame` carries: `Vec<NamedMail>` with
    // `ErasedActorPath` recipients the substrate proves once, before any
    // item moves, via `accept_bundle`. ADR-0091: descriptors come from
    // the per-engine merged view so a component's own kinds
    // encode after `load_component`.
    let mails = mcp
        .encode_mail_bundle(engine, &args.mails)
        .await
        .map_err(|e| McpError::invalid_params(format!("send_mail_traced batch: {e}"), None))?;
    let timeout_millis =
        args.settlement_timeout_millis.unwrap_or(AWAIT_TIMEOUT_DEFAULT_MILLIS).min(AWAIT_TIMEOUT_CAP_MILLIS);
    let dispatch_envelope = engine_envelope(engine, TRACE_MAILBOX_NAME, &DispatchTraced { mails });

    // Fire-and-forget: write the dispatch without awaiting the chain
    // to settle. We still need the synchronous ack's `root`, so this
    // path isn't a bare `fire` — issue the call, read the ack from
    // the (immediately-available) first reply, and skip the tree
    // walk. Bound it by the same timeout so a wedged ack doesn't hang.
    if args.fire_and_forget {
        let (events, ack_timed_out) = mcp
            .session
            .call_collecting(dispatch_envelope, Duration::from_millis(u64::from(timeout_millis)))
            .await
            .map_err(internal)?;
        if ack_timed_out {
            return json(&SendMailTracedResponse {
                engine_id,
                status: "timeout".into(),
                root: None,
                mails: None,
                tree: None,
                node_count: None,
                in_flight: None,
                replies: None,
            });
        }
        let root = decode_traced_ack(&events)?;
        return json(&SendMailTracedResponse {
            engine_id,
            status: "dispatched".into(),
            root: Some(mail_id_to_json(&root)),
            mails: None,
            tree: None,
            node_count: None,
            in_flight: None,
            replies: None,
        });
    }

    // Round 1: ack carries the root's trace identity; ReplyEnd
    // closes when the chain settles substrate-side. `call_collecting`
    // keeps every correlated `ReplyEvent` (the ack plus any cap
    // replies) instead of `call_one`'s single-event discard.
    let (events, ack_timed_out) = mcp
        .session
        .call_collecting(dispatch_envelope, Duration::from_millis(u64::from(timeout_millis)))
        .await
        .map_err(internal)?;
    if ack_timed_out {
        return json(&SendMailTracedResponse {
            engine_id,
            status: "timeout".into(),
            root: None,
            mails: None,
            tree: None,
            node_count: None,
            in_flight: None,
            replies: None,
        });
    }
    let engine_kinds = mcp.snapshot_engine_kinds(engine);
    let replies = decode_reply_events(strip_ack(&events), &engine_kinds, None, format.as_ref());
    let root = decode_traced_ack(&events)?;

    finish_traced_dispatch(mcp, engine, engine_id, root, replies, args.trace).await
}

pub(super) async fn finish_traced_dispatch(
    mcp: &Mcp,
    engine: EngineId,
    engine_id: String,
    root: TraceMailId,
    replies: Vec<ReplyEventJson>,
    trace: TraceShape,
) -> Result<String, McpError> {
    // Round 2: reconstruct the tree by a guided walk over the
    // per-actor trace rings (ADR-0086 Phase 3b). The export names every
    // actor by its canonical path, so the walk hands out paths and each
    // ring is tailed with `aether.trace.tail` addressed by that path, with
    // no id to resolve first. The walk touches only the actors in the
    // tree; the rings are in-memory and the chain has already settled, so
    // each hop is microseconds. A failed or undecodable per-ring reply
    // contributes no entries — the walk completes from the rings that
    // answer.
    let request = TraceTail { max: 0, since: None, root: Some(root.clone()) };
    let mut walk = TreeWalk::new(root);
    while let Some(path) = walk.next_actor() {
        let entries = match mcp
            .session
            .call_one(engine_envelope_to(engine, path, &request))
            .await
            .ok()
            .and_then(|reply| TraceTailResult::decode_from_bytes(&reply.payload))
        {
            Some(TraceTailResult::Ok { entries, .. }) => entries,
            Some(TraceTailResult::Err { .. }) | None => Vec::new(),
        };
        walk.absorb(entries);
    }

    match walk.finish() {
        DescribeTreeResult::Ok { root, in_flight, mails } => {
            // Reverse kind ids to real names through the engine's
            // inventory map (ADR-0088 §8); actors already carry paths.
            let mails = mcp.render_mail_nodes(engine, mails).await;
            let node_count = mails.len();
            let (mails, tree) = match trace {
                TraceShape::Nodes => (Some(mails), None),
                TraceShape::Tree => (None, Some(render_compact_tree(&mails))),
            };
            json(&SendMailTracedResponse {
                engine_id,
                status: "settled".into(),
                root: Some(mail_id_to_json(&root)),
                mails,
                tree,
                node_count: Some(node_count),
                in_flight: Some(in_flight),
                replies: Some(replies),
            })
        }
        DescribeTreeResult::Err { not_found } => {
            Err(internal_msg(&format!("describe_tree: root {not_found:?} not found")))
        }
    }
}
