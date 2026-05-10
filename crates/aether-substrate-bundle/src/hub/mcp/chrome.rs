//! Chrome trace event format (a.k.a. "trace event format" / "Catapult")
//! converter for the substrate's `TraceObserverCapability` state. Issue
//! iamacoffeepot/aether#728 / ADR-0080 Phase 3.
//!
//! Pure function over [`aether_kinds::trace::DescribeTreeResult`] +
//! a kind-id → name lookup. The MCP `dump_trace_chrome` tool builds
//! the lookup from the engine's handshake descriptor list, calls
//! [`render_chrome_trace`], and either returns the JSON inline or
//! writes it to disk.
//!
//! Output format reference:
//! <https://docs.google.com/document/d/1CvAClvFfyA5R-PhYUmn5OOQtYMH4h6I0nSsKchNAySU/preview>
//!
//! Per mail with `t_received` and `t_finished` populated, one
//! `ph:"X"` (complete) event covering the receive→finish interval on
//! the recipient's lane. Per mail with a parent (and where both
//! `parent.t_finished` and `self.t_received` are set), one
//! `ph:"s"` / `ph:"f"` flow pair so chrome://tracing draws a causal
//! arrow. Mails missing timestamps (orphan or in-flight at query
//! time) are skipped — they remain inspectable via `describe_tree`.

use std::collections::HashMap;

use aether_data::MailId;
use aether_kinds::trace::{DescribeTreeResult, MailNodeWire};
use serde_json::{Value, json};

/// Render a `DescribeTreeResult` into Chrome trace event format JSON.
/// Returns the serialized document as a `String`.
///
/// `kind_names` maps the raw `KindId` u64 → human-readable kind name
/// for the chrome event `name` field. Missing entries fall back to
/// the tagged-string id (`knd-XXXX-XXXX-XXXX`).
pub(super) fn render_chrome_trace(
    result: &DescribeTreeResult,
    kind_names: &HashMap<u64, String>,
) -> Result<String, serde_json::Error> {
    let events = build_events(result, kind_names);
    serde_json::to_string_pretty(&json!({ "traceEvents": events }))
}

fn build_events(result: &DescribeTreeResult, kind_names: &HashMap<u64, String>) -> Vec<Value> {
    let mails = match result {
        DescribeTreeResult::Ok { mails, .. } => mails,
        DescribeTreeResult::Err { not_found } => {
            // Empty trace doc with a metadata event explaining the
            // missing root — chrome://tracing surfaces metadata
            // events as a top-level note so the agent sees why the
            // file is empty.
            return vec![json!({
                "ph": "M",
                "name": "process_name",
                "cat": "metadata",
                "pid": format_mail_id(*not_found),
                "args": {
                    "name": format!("describe_tree returned not_found for {}", format_mail_id(*not_found))
                },
            })];
        }
    };

    // Index for parent-edge lookup.
    let by_id: HashMap<MailId, &MailNodeWire> = mails.iter().map(|n| (n.mail_id, n)).collect();

    let mut events: Vec<Value> = Vec::with_capacity(mails.len() * 2);
    for node in mails {
        // Skip mails missing timestamps (orphan Sent or in-flight at
        // query time). They stay inspectable via `describe_tree`.
        let (Some(t_received), Some(t_finished)) = (node.t_received, node.t_finished) else {
            continue;
        };
        let kind_name = kind_names
            .get(&node.kind.0)
            .cloned()
            .unwrap_or_else(|| node.kind.to_string());
        events.push(json!({
            "ph": "X",
            "name": kind_name,
            "cat": "mail",
            "ts": ns_to_us(t_received.0),
            "dur": ns_to_us(t_finished.0.saturating_sub(t_received.0)),
            "pid": node.recipient.to_string(),
            "tid": 0,
            "args": {
                "mail_id": format_mail_id(node.mail_id),
                "sender": node.sender.to_string(),
                "parent": node.parent.map(format_mail_id),
                "root": format_mail_id(root_of(node, &by_id)),
                "kind_id": node.kind.to_string(),
            },
        }));

        // Flow arrow: requires the parent's t_finished and this
        // node's t_received. The flow id ties the start ("s") and
        // finish ("f") events into one arrow on chrome://tracing.
        if let Some(parent_id) = node.parent
            && let Some(parent) = by_id.get(&parent_id)
            && let Some(parent_finished) = parent.t_finished
        {
            let flow_id = format_mail_id(node.mail_id);
            events.push(json!({
                "ph": "s",
                "name": "flow",
                "cat": "flow",
                "id": flow_id.clone(),
                "ts": ns_to_us(parent_finished.0),
                "pid": parent.recipient.to_string(),
                "tid": 0,
            }));
            events.push(json!({
                "ph": "f",
                "name": "flow",
                "cat": "flow",
                "id": flow_id,
                "ts": ns_to_us(t_received.0),
                "pid": node.recipient.to_string(),
                "tid": 0,
                "bp": "e",
            }));
        }
    }
    events
}

/// MailId composite → compact string for chrome event `id` and `args`
/// fields. Format: `<sender_tagged_id>:<correlation_id>` (e.g.
/// `mbx-XXXX-XXXX-XXXX:42`). Stable enough to round-trip and
/// human-readable enough to spot-check in chrome://tracing.
fn format_mail_id(id: MailId) -> String {
    format!("{}:{}", id.sender, id.correlation_id)
}

/// Convert a nanosecond value to microseconds for chrome's
/// `ts`/`dur` fields (chrome's standard time unit).
fn ns_to_us(nanos: u64) -> u64 {
    nanos / 1_000
}

/// Walk the parent chain to find the root mail. Falls back to the
/// node's own `mail_id` if the parent reference is broken (parent
/// not in the result set — possible if eviction trimmed an
/// ancestor).
fn root_of(node: &MailNodeWire, by_id: &HashMap<MailId, &MailNodeWire>) -> MailId {
    let mut current = node;
    while let Some(parent_id) = current.parent {
        match by_id.get(&parent_id) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    current.mail_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::tagged_id::Tag;
    use aether_data::{KindId, MailboxId, with_tag};
    use aether_kinds::trace::Nanos;

    fn mid(sender_body: u64, cid: u64) -> MailId {
        MailId {
            sender: MailboxId(with_tag(Tag::Mailbox, sender_body)),
            correlation_id: cid,
        }
    }

    /// Issue 728: the converter emits one complete event per mail
    /// with both timestamps populated, plus one flow pair per
    /// parent edge.
    #[test]
    fn render_emits_complete_and_flow_events() {
        let root_id = mid(0xAA, 1);
        let child_id = mid(0xBB, 2);
        let result = DescribeTreeResult::Ok {
            root: root_id,
            in_flight: 0,
            mails: vec![
                MailNodeWire {
                    mail_id: root_id,
                    parent: None,
                    sender: MailboxId(with_tag(Tag::Mailbox, 0xAA)),
                    recipient: MailboxId(with_tag(Tag::Mailbox, 0xCC)),
                    kind: KindId(with_tag(Tag::Kind, 0xDEAD)),
                    t_sent: Nanos(1_000),
                    t_received: Some(Nanos(2_000)),
                    t_finished: Some(Nanos(5_000)),
                },
                MailNodeWire {
                    mail_id: child_id,
                    parent: Some(root_id),
                    sender: MailboxId(with_tag(Tag::Mailbox, 0xCC)),
                    recipient: MailboxId(with_tag(Tag::Mailbox, 0xEE)),
                    kind: KindId(with_tag(Tag::Kind, 0xBEEF)),
                    t_sent: Nanos(3_000),
                    t_received: Some(Nanos(6_000)),
                    t_finished: Some(Nanos(9_000)),
                },
            ],
        };

        let mut kind_names = HashMap::new();
        kind_names.insert(with_tag(Tag::Kind, 0xDEAD), "test.tick".to_owned());

        let json = render_chrome_trace(&result, &kind_names).expect("render");
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let events = parsed["traceEvents"].as_array().unwrap();
        // Two complete events + two flow events (s + f for the one
        // parent edge) = 4 total.
        assert_eq!(events.len(), 4, "got: {events:#?}");

        let phs: Vec<&str> = events.iter().map(|e| e["ph"].as_str().unwrap()).collect();
        assert_eq!(phs.iter().filter(|p| **p == "X").count(), 2);
        assert_eq!(phs.iter().filter(|p| **p == "s").count(), 1);
        assert_eq!(phs.iter().filter(|p| **p == "f").count(), 1);

        // Root event uses the kind name from the lookup; child falls
        // back to the tagged kind id (not in the lookup).
        let names: Vec<&str> = events
            .iter()
            .filter(|e| e["ph"] == "X")
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"test.tick"), "names: {names:?}");
        assert!(
            names.iter().any(|n| n.starts_with("knd-")),
            "child should fall back to tagged kind id: {names:?}"
        );

        // Microsecond conversion: t_received=2000ns → ts=2us,
        // dur=(5000-2000)/1000 = 3us.
        let root_complete = events
            .iter()
            .find(|e| e["ph"] == "X" && e["name"] == "test.tick")
            .unwrap();
        assert_eq!(root_complete["ts"], 2);
        assert_eq!(root_complete["dur"], 3);

        // Flow pair: child mail_id ties the s/f arrow.
        let child_mail_str = format_mail_id(child_id);
        let s = events.iter().find(|e| e["ph"] == "s").unwrap();
        let f = events.iter().find(|e| e["ph"] == "f").unwrap();
        assert_eq!(s["id"], child_mail_str);
        assert_eq!(f["id"], child_mail_str);
        // s anchors at parent.t_finished (5000ns -> 5us), f at
        // child.t_received (6000ns -> 6us).
        assert_eq!(s["ts"], 5);
        assert_eq!(f["ts"], 6);
    }

    /// Mails without `t_received` / `t_finished` are skipped (no
    /// complete event, no flow). They remain inspectable via
    /// `describe_tree` for in-flight diagnostics.
    #[test]
    fn render_skips_mails_missing_timestamps() {
        let result = DescribeTreeResult::Ok {
            root: mid(0xAA, 1),
            in_flight: 1,
            mails: vec![MailNodeWire {
                mail_id: mid(0xAA, 1),
                parent: None,
                sender: MailboxId(with_tag(Tag::Mailbox, 0xAA)),
                recipient: MailboxId(with_tag(Tag::Mailbox, 0xCC)),
                kind: KindId(with_tag(Tag::Kind, 0xDEAD)),
                t_sent: Nanos(1_000),
                t_received: None,
                t_finished: None,
            }],
        };
        let json = render_chrome_trace(&result, &HashMap::new()).expect("render");
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert!(
            parsed["traceEvents"].as_array().unwrap().is_empty(),
            "in-flight mail must not produce events"
        );
    }

    /// `Err::not_found` produces a trace doc with a single metadata
    /// event noting the missing root, so chrome://tracing displays
    /// the diagnostic instead of a blank file.
    #[test]
    fn render_not_found_emits_metadata_doc() {
        let missing = mid(0xFF, 99);
        let result = DescribeTreeResult::Err { not_found: missing };
        let json = render_chrome_trace(&result, &HashMap::new()).expect("render");
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let events = parsed["traceEvents"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["ph"], "M");
        let arg_name = events[0]["args"]["name"].as_str().unwrap();
        assert!(
            arg_name.contains("not_found"),
            "metadata should mention not_found: {arg_name}"
        );
    }
}
