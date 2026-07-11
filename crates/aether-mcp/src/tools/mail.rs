use std::time::Duration;

use aether_capabilities::trace::walk::TreeWalk;
use aether_data::{EngineId, Kind, MailId, Uuid, mailbox_id_from_path};
use aether_kinds::trace::{DescribeTreeResult, DispatchTraced, TRACE_MAILBOX_NAME, TraceTail, TraceTailResult};
use rmcp::ErrorData as McpError;

use crate::args::{MailStatus, ReplyEventJson, SendMailArgs, SendMailTracedArgs, SendMailTracedResponse};

use super::envelope::{engine_envelope, engine_envelope_by_id};
use super::ids::{mail_id_to_json, parse_engine_id, render_compact_tree};
use super::render::{internal, internal_msg, json};
use super::reply::{decode_reply_events, decode_traced_ack, project_replies, strip_ack};
use super::{AWAIT_TIMEOUT_CAP_MS, AWAIT_TIMEOUT_DEFAULT_MS, Mcp};

pub(super) async fn send_mail(mcp: &Mcp, args: SendMailArgs) -> Result<String, McpError> {
    let fire_and_forget = args.fire_and_forget;
    let reply_projection = args.replies;
    let mut statuses = Vec::with_capacity(args.mails.len());
    for (index, spec) in args.mails.into_iter().enumerate() {
        let mut replies = Vec::new();
        let mut timed_out = false;
        let status = if fire_and_forget {
            match mcp.deliver_one_fire(spec).await {
                Ok(()) => "dispatched".to_owned(),
                Err(e) => format!("error: {e}"),
            }
        } else {
            // Capture the engine id and the handler's declared reply kind
            // (ADR-0109 / issue 1803) before `deliver_one` consumes the
            // spec, so `decode_reply_events` can search the per-engine kind
            // cache for component-defined reply kinds (issue 1804).
            let engine = Uuid::parse_str(&spec.engine_id).ok().map(EngineId);
            let declared_reply = engine.and_then(|e| {
                let mbx = mailbox_id_from_path(&spec.recipient_name);
                let cache = mcp.components.lock().expect("component cache mutex is never poisoned");
                cache.get(&(e, mbx)).and_then(|caps| {
                    caps.handlers
                        .iter()
                        .find(|h| h.name == spec.kind_name)
                        // ADR-0112 / ADR-0134: a single-class handler names
                        // one static reply kind and a multi-class handler
                        // names its element kind — both are what a driver
                        // decodes, so search the cache for either. A manual
                        // / silent handler yields no declared kind.
                        .and_then(|h| match h.reply {
                            aether_data::ReplyContract::One(id) | aether_data::ReplyContract::Multi(id) => Some(id),
                            _ => None,
                        })
                })
            });
            match mcp.deliver_one(spec).await {
                Ok((events, hit_timeout)) => {
                    let engine_kinds = engine.map(|e| mcp.snapshot_engine_kinds(e)).unwrap_or_default();
                    replies =
                        project_replies(decode_reply_events(&events, &engine_kinds, declared_reply), reply_projection);
                    timed_out = hit_timeout;
                    if hit_timeout {
                        "timeout"
                    } else {
                        "delivered"
                    }
                    .to_owned()
                }
                Err(e) => format!("error: {e}"),
            }
        };
        statuses.push(MailStatus { index, status, replies, timed_out });
    }
    json(&statuses)
}

pub(super) async fn send_mail_traced(mcp: &Mcp, args: SendMailTracedArgs) -> Result<String, McpError> {
    let engine = parse_engine_id(&args.engine_id)?;
    // Encode the batch before sending — a bad spec produces a
    // clean invalid-params error and never touches the wire.
    // Same shape `CaptureFrame` carries: `Vec<NamedMail>` with
    // name-level addressing the substrate resolves at dispatch
    // time via `resolve_bundle`. ADR-0091: descriptors come from
    // the per-engine merged view so a component's own kinds
    // encode after `load_component`.
    let mails = mcp
        .encode_traced_bundle(engine, &args.mails)
        .await
        .map_err(|e| McpError::invalid_params(format!("send_mail_traced batch: {e}"), None))?;
    let timeout_ms = args.settlement_timeout_ms.unwrap_or(AWAIT_TIMEOUT_DEFAULT_MS).min(AWAIT_TIMEOUT_CAP_MS);
    let dispatch_envelope = engine_envelope(engine, TRACE_MAILBOX_NAME, &DispatchTraced { mails });

    // Fire-and-forget: write the dispatch without awaiting the chain
    // to settle. We still need the synchronous ack's `root`, so this
    // path isn't a bare `fire` — issue the call, read the ack from
    // the (immediately-available) first reply, and skip the tree
    // walk. Bound it by the same timeout so a wedged ack doesn't hang.
    if args.fire_and_forget {
        let (events, ack_timed_out) = mcp
            .session
            .call_collecting(dispatch_envelope, Duration::from_millis(u64::from(timeout_ms)))
            .await
            .map_err(internal)?;
        if ack_timed_out {
            return json(&SendMailTracedResponse {
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
        let root_json = {
            mcp.ensure_names(engine).await;
            let cache = mcp.names.lock().expect("reverse-name cache mutex is never poisoned");
            mail_id_to_json(root, cache.get(&engine))
        };
        return json(&SendMailTracedResponse {
            status: "dispatched".into(),
            root: Some(root_json),
            mails: None,
            tree: None,
            node_count: None,
            in_flight: None,
            replies: None,
        });
    }

    // Round 1: ack carries the chassis-root MailId; ReplyEnd
    // closes when the chain settles substrate-side. `call_collecting`
    // keeps every correlated `ReplyEvent` (the ack plus any cap
    // replies) instead of `call_one`'s single-event discard.
    let (events, ack_timed_out) = mcp
        .session
        .call_collecting(dispatch_envelope, Duration::from_millis(u64::from(timeout_ms)))
        .await
        .map_err(internal)?;
    if ack_timed_out {
        return json(&SendMailTracedResponse {
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
    let replies = decode_reply_events(strip_ack(&events), &engine_kinds, None);
    let root = decode_traced_ack(&events)?;

    finish_traced_dispatch(mcp, engine, root, replies, args.full).await
}

async fn finish_traced_dispatch(
    mcp: &Mcp,
    engine: EngineId,
    root: MailId,
    replies: Vec<ReplyEventJson>,
    full: bool,
) -> Result<String, McpError> {
    // Round 2: reconstruct the tree by a guided walk over the
    // per-actor trace rings (ADR-0086 Phase 3b). Seed at
    // `root.sender` (`CHASSIS_MAILBOX_ID` for this chassis-rooted
    // dispatch), follow each `Sent`'s recipient, fetch every ring
    // with one `aether.trace.tail` addressed by id — the chassis-
    // host ring answers at `CHASSIS_MAILBOX_ID`. The walk touches
    // only the actors in the tree; the rings are in-memory and the
    // chain has already settled, so each hop is microseconds. A
    // failed or undecodable per-ring reply contributes no entries —
    // the walk completes from the rings that answer.
    let mut walk = TreeWalk::new(root);
    while let Some(mailbox) = walk.next_mailbox() {
        let request = TraceTail { max: 0, since: None, root: Some(root) };
        let entries = match mcp
            .session
            .call_one(engine_envelope_by_id(engine, mailbox, &request))
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
            // Reverse mailbox / kind ids to real names through the
            // engine's inventory map (ADR-0088 §8). `render_mail_nodes`
            // builds + resolves the map; the root id then renders
            // through the now-populated cache (its sender is the
            // chassis mailbox — a static name).
            let mails = mcp.render_mail_nodes(engine, mails).await;
            let node_count = mails.len();
            let (mails, tree) = if full {
                (Some(mails), None)
            } else {
                let tree = render_compact_tree(&mails);
                (None, Some(tree))
            };
            let root = {
                let cache = mcp.names.lock().expect("reverse-name cache mutex is never poisoned");
                mail_id_to_json(root, cache.get(&engine))
            };
            json(&SendMailTracedResponse {
                status: "settled".into(),
                root: Some(root),
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
