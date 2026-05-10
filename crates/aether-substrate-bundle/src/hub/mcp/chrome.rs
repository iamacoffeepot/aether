//! Chrome trace event format (a.k.a. "trace event format" / "Catapult")
//! converter for the substrate's `TraceObserverCapability` state. Issue
//! iamacoffeepot/aether#728 / ADR-0080 Phase 3.
//!
//! Pure function over [`aether_kinds::trace::DescribeTreeResult`] +
//! a kind-id → name lookup + a mailbox-id → (name, category) lookup
//! (issue iamacoffeepot/aether#731). The MCP `dump_trace_chrome` tool
//! builds both lookups from the engine record's `kinds` + `mailboxes`
//! caches, calls [`render_chrome_trace`], and either returns the JSON
//! inline or writes it to disk.
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

use aether_data::{MailId, MailboxCategory, MailboxId};
use aether_kinds::trace::{DescribeTreeResult, MailNodeWire};
use serde_json::{Value, json};

/// Per-engine mailbox lookup the chrome renderer uses to swap raw
/// tagged ids for category-prefixed names (issue
/// iamacoffeepot/aether#731). Keyed on the raw `MailboxId` u64 so
/// the call site can pre-strip the tag once instead of per-event.
/// `None` category means "we know the name but the substrate didn't
/// classify it" — render the bare name without a prefix so the
/// failure mode stays visible.
pub(super) type MailboxLookup = HashMap<u64, (String, Option<MailboxCategory>)>;

/// Render a `DescribeTreeResult` into Chrome trace event format JSON.
/// Returns the serialized document as a `String`.
///
/// `kind_names` maps the raw `KindId` u64 → human-readable kind name
/// for the chrome event `name` field. Resolved entries render as
/// `kind:NAME`; missing entries fall back to the tagged-string id
/// (`knd-XXXX-XXXX-XXXX`) with no prefix so unresolved ids are
/// visually distinct from resolved-but-empty.
///
/// `mailbox_names` (issue iamacoffeepot/aether#731) does the same for
/// `MailboxId` fields (`pid`, `args.sender`, `args.parent`,
/// `args.root`). Resolved entries render as `<prefix>:<name>` per the
/// `MailboxCategory` table:
///
/// | Category          | Prefix      |
/// |-------------------|-------------|
/// | `Actor`           | `actor:`    |
/// | `Trampoline`      | `actor:`    |
/// | `BroadcastSink`   | `sink:`     |
/// | `ChassisSentinel` | `chassis:`  |
/// | (None)            | (no prefix) |
///
/// Unknown ids fall back to the raw `mbx-XXXX-XXXX-XXXX` tagged form
/// so the unresolved case stays visible.
pub(super) fn render_chrome_trace(
    result: &DescribeTreeResult,
    kind_names: &HashMap<u64, String>,
    mailbox_names: &MailboxLookup,
) -> Result<String, serde_json::Error> {
    let events = build_events(result, kind_names, mailbox_names);
    serde_json::to_string_pretty(&json!({ "traceEvents": events }))
}

fn build_events(
    result: &DescribeTreeResult,
    kind_names: &HashMap<u64, String>,
    mailbox_names: &MailboxLookup,
) -> Vec<Value> {
    let mails = match result {
        DescribeTreeResult::Ok { mails, .. } => mails,
        DescribeTreeResult::Err { not_found } => {
            // Empty trace doc with a metadata event explaining the
            // missing root — chrome://tracing surfaces metadata
            // events as a top-level note so the agent sees why the
            // file is empty.
            let label = format_mail_id(*not_found, mailbox_names);
            return vec![json!({
                "ph": "M",
                "name": "process_name",
                "cat": "metadata",
                "pid": label,
                "args": {
                    "name": format!("describe_tree returned not_found for {label}")
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
        events.push(json!({
            "ph": "X",
            "name": format_kind_label(node.kind.0, kind_names),
            "cat": "mail",
            "ts": ns_to_us(t_received.0),
            "dur": ns_to_us(t_finished.0.saturating_sub(t_received.0)),
            "pid": format_mailbox_label(node.recipient, mailbox_names),
            "tid": 0,
            "args": {
                "mail_id": format_mail_id(node.mail_id, mailbox_names),
                "sender": format_mailbox_label(node.sender, mailbox_names),
                "parent": node.parent.map(|p| format_mail_id(p, mailbox_names)),
                "root": format_mail_id(root_of(node, &by_id), mailbox_names),
                "kind_id": format_kind_label(node.kind.0, kind_names),
            },
        }));

        // Flow arrow: ties the parent's processing slice to this
        // mail's processing slice via a unique flow id (`s` start +
        // `f` finish events). Both endpoints carry `bp: "e"` so
        // Perfetto / chrome://tracing bind them to the *enclosing*
        // slice on the same pid rather than searching for a future /
        // past slice — that searching default is what flagged earlier
        // anchors as `flow_invalid_id` when the s timestamp landed
        // exactly at the parent slice's `t_finished` boundary
        // (outside the half-open `[t_received, t_finished)` slice
        // range, with no future slice to bind to).
        //
        // Anchor the `s` at `node.t_sent` — the moment the parent
        // sent this mail. By construction `t_sent` falls within the
        // parent's processing window `[t_received, t_finished)`, so
        // the s event lands inside the parent's slice. The `f` lands
        // at the child's `t_received`, which is the start of the
        // child's own slice — inside it by definition. Both pairs of
        // (pid, ts) now sit inside concrete slices, which is what
        // Perfetto's flow-binder expects.
        if let Some(parent_id) = node.parent
            && let Some(parent) = by_id.get(&parent_id)
            && parent.t_finished.is_some()
        {
            let flow_id = format_mail_id(node.mail_id, mailbox_names);
            events.push(json!({
                "ph": "s",
                "name": "flow",
                "cat": "flow",
                "id": flow_id.clone(),
                "ts": ns_to_us(node.t_sent.0),
                "pid": format_mailbox_label(parent.recipient, mailbox_names),
                "tid": 0,
                "bp": "e",
            }));
            events.push(json!({
                "ph": "f",
                "name": "flow",
                "cat": "flow",
                "id": flow_id,
                "ts": ns_to_us(t_received.0),
                "pid": format_mailbox_label(node.recipient, mailbox_names),
                "tid": 0,
                "bp": "e",
            }));
        }
    }
    events
}

/// Render a `MailboxId` as `<prefix>:<name>` when the lookup resolves
/// the id, or as the raw tagged `mbx-XXXX-XXXX-XXXX` form when it
/// doesn't. Issue iamacoffeepot/aether#731 — keeping the unresolved
/// case visually distinct from resolved-with-no-prefix lets agents
/// spot inventory drift at a glance.
fn format_mailbox_label(id: MailboxId, lookup: &MailboxLookup) -> String {
    match lookup.get(&id.0) {
        Some((name, Some(category))) => format!("{}:{}", category_prefix(*category), name),
        Some((name, None)) => name.clone(),
        None => id.to_string(),
    }
}

/// Mirror of [`format_mailbox_label`] for `KindId` u64 values. Kinds
/// only have one category, so the prefix is uniformly `kind:`.
fn format_kind_label(kind_id: u64, kind_names: &HashMap<u64, String>) -> String {
    match kind_names.get(&kind_id) {
        Some(name) => format!("kind:{name}"),
        None => aether_data::KindId(kind_id).to_string(),
    }
}

fn category_prefix(c: MailboxCategory) -> &'static str {
    // Trampoline folds under `actor:` per the issue 731 spec — agents
    // think of trampolines as just another actor; if they ever need
    // the distinction the variant survives in the wire and a future
    // PR can split this into `trampoline:` without churning callers.
    match c {
        MailboxCategory::Actor | MailboxCategory::Trampoline => "actor",
        MailboxCategory::BroadcastSink => "sink",
        MailboxCategory::ChassisSentinel => "chassis",
    }
}

/// MailId composite → compact string for chrome event `id` and `args`
/// fields. Format: `<sender_label>#<correlation_id>` — the sender
/// component is resolved through [`format_mailbox_label`] (so it
/// carries a category prefix when known), and the correlation id is
/// appended after `#`. The `#` separator (rather than another `:`)
/// keeps the correlation_id visually distinct from the type-prefix
/// separator already inside the sender label (e.g.
/// `actor:aether.input#2489` reads as "actor `aether.input`, mail
/// 2489" without `:` doing double duty).
fn format_mail_id(id: MailId, mailbox_names: &MailboxLookup) -> String {
    format!(
        "{}#{}",
        format_mailbox_label(id.sender, mailbox_names),
        id.correlation_id
    )
}

/// Convert a nanosecond value to fractional microseconds for chrome's
/// `ts`/`dur` fields (chrome's standard time unit).
///
/// Returns `f64` so sub-microsecond handlers (the camera component's
/// per-tick `aether.camera` send takes <1us in release builds) keep
/// their non-zero duration after the unit conversion. Integer-floored
/// 0-duration `ph:"X"` events render as invisible slices in Perfetto,
/// which then makes incoming flow arrows look like they point at
/// nothing.
fn ns_to_us(nanos: u64) -> f64 {
    (nanos as f64) / 1_000.0
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
    /// parent edge. Issue 731 amendment: with a non-empty mailbox
    /// lookup, `pid` / `args.sender` / `args.parent` / `args.root`
    /// + the top-level `name` carry category-prefixed labels.
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

        // Two of the three mailbox ids are categorised; the third
        // (0xEE) is left out so the unresolved fallback path is
        // exercised on the same render call.
        let mut mailbox_names: MailboxLookup = HashMap::new();
        mailbox_names.insert(
            with_tag(Tag::Mailbox, 0xAA),
            (
                "aether.chassis".to_owned(),
                Some(MailboxCategory::ChassisSentinel),
            ),
        );
        mailbox_names.insert(
            with_tag(Tag::Mailbox, 0xCC),
            ("aether.input".to_owned(), Some(MailboxCategory::Actor)),
        );

        let json = render_chrome_trace(&result, &kind_names, &mailbox_names).expect("render");
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let events = parsed["traceEvents"].as_array().unwrap();
        // Two complete events + two flow events (s + f for the one
        // parent edge) = 4 total.
        assert_eq!(events.len(), 4, "got: {events:#?}");

        let phs: Vec<&str> = events.iter().map(|e| e["ph"].as_str().unwrap()).collect();
        assert_eq!(phs.iter().filter(|p| **p == "X").count(), 2);
        assert_eq!(phs.iter().filter(|p| **p == "s").count(), 1);
        assert_eq!(phs.iter().filter(|p| **p == "f").count(), 1);

        // Root event: kind resolved → `kind:test.tick`; recipient
        // resolved → `actor:aether.input`; sender resolved →
        // `chassis:aether.chassis`.
        let root_complete = events
            .iter()
            .find(|e| e["ph"] == "X" && e["name"] == "kind:test.tick")
            .expect("root complete event with kind: prefix");
        assert_eq!(root_complete["pid"], "actor:aether.input");
        assert_eq!(root_complete["args"]["sender"], "chassis:aether.chassis");
        assert_eq!(root_complete["args"]["parent"], serde_json::Value::Null);
        // Root's mail_id renders as `<sender_label>#<cid>` =
        // `chassis:aether.chassis#1`. Same for `root` (root of root
        // is itself). The `#` separator keeps `:` reserved for the
        // type prefix; reading `chassis:aether.chassis#1` as "the
        // chassis sentinel's mail 1" stays unambiguous even when
        // names contain colons (trampolines have a `:NAME` suffix).
        assert_eq!(root_complete["args"]["mail_id"], "chassis:aether.chassis#1");
        assert_eq!(root_complete["args"]["root"], "chassis:aether.chassis#1");
        assert_eq!(root_complete["args"]["kind_id"], "kind:test.tick");

        // Child event: kind NOT in the lookup → falls back to raw
        // `knd-...` (no `kind:` prefix). Recipient (0xEE) NOT in the
        // lookup → falls back to raw `mbx-...`. Parent's sender
        // (0xAA) is resolved → parent renders prefixed.
        let child_complete = events
            .iter()
            .find(|e| e["ph"] == "X" && e["pid"].as_str().unwrap_or("").starts_with("mbx-"))
            .expect("child complete event with raw mbx- pid");
        let child_kind_name = child_complete["name"].as_str().unwrap();
        assert!(
            child_kind_name.starts_with("knd-"),
            "unresolved kind should fall back to raw tagged id (no kind: prefix): {child_kind_name}"
        );
        // Child's sender (0xCC) IS resolved → `actor:aether.input`.
        assert_eq!(child_complete["args"]["sender"], "actor:aether.input");
        // Parent reference points at root_id, whose sender (0xAA) is
        // resolved → `chassis:aether.chassis#1`.
        assert_eq!(child_complete["args"]["parent"], "chassis:aether.chassis#1");
        // Root walks to root_id again.
        assert_eq!(child_complete["args"]["root"], "chassis:aether.chassis#1");

        // Microsecond conversion (fractional): t_received=2000ns →
        // ts=2.0us, dur=(5000-2000)/1000.0 = 3.0us. f64 so
        // sub-microsecond handlers stay non-zero (and so visible
        // in Perfetto) after the unit conversion.
        assert_eq!(root_complete["ts"], 2.0);
        assert_eq!(root_complete["dur"], 3.0);

        // Flow pair: child mail_id ties the s/f arrow. Same render
        // path on both sides means the ids match without extra
        // care.
        let child_mail_str = format_mail_id(child_id, &mailbox_names);
        // 0xBB sender is unresolved → raw mbx-... — flow id reflects
        // that without a prefix, but matches itself, which is all
        // chrome needs.
        let s = events.iter().find(|e| e["ph"] == "s").unwrap();
        let f = events.iter().find(|e| e["ph"] == "f").unwrap();
        assert_eq!(s["id"], child_mail_str);
        assert_eq!(f["id"], child_mail_str);
        // s anchors at child.t_sent (3000ns -> 3.0us, inside
        // parent's [2000, 5000) processing slice), f at
        // child.t_received (6000ns -> 6.0us, the start of the
        // child's own slice). Both endpoints sit inside concrete
        // slices so Perfetto's flow-binder doesn't flag them as
        // flow_invalid_id.
        assert_eq!(s["ts"], 3.0);
        assert_eq!(f["ts"], 6.0);
        // Both endpoints carry `bp: "e"` so the binder uses the
        // enclosing slice instead of searching for a past / future
        // one.
        assert_eq!(s["bp"], "e");
        assert_eq!(f["bp"], "e");
        // Flow event pids resolve through the same lookup. s.pid is
        // parent.recipient (0xCC) → `actor:aether.input`; f.pid is
        // node.recipient (0xEE) → unresolved raw.
        assert_eq!(s["pid"], "actor:aether.input");
        assert!(f["pid"].as_str().unwrap().starts_with("mbx-"));
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
        let json = render_chrome_trace(&result, &HashMap::new(), &HashMap::new()).expect("render");
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
        let json = render_chrome_trace(&result, &HashMap::new(), &HashMap::new()).expect("render");
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

    /// Sub-microsecond handlers (the camera component's per-tick
    /// `aether.camera` send takes <1us in release builds) must keep
    /// their non-zero duration after the ns→us unit conversion.
    /// Integer-floored 0-duration `ph:"X"` events render as
    /// invisible slices in Perfetto, which then makes the inbound
    /// flow arrow look like it points at empty space (the user-
    /// reported regression on the first #731 smoke).
    #[test]
    fn render_preserves_subus_duration_as_fractional() {
        let id = mid(0xAA, 1);
        let result = DescribeTreeResult::Ok {
            root: id,
            in_flight: 0,
            mails: vec![MailNodeWire {
                mail_id: id,
                parent: None,
                sender: MailboxId(with_tag(Tag::Mailbox, 0xAA)),
                recipient: MailboxId(with_tag(Tag::Mailbox, 0xAA)),
                kind: KindId(with_tag(Tag::Kind, 0x99)),
                t_sent: Nanos(1_000),
                // 500ns handler — under the 1us floor.
                t_received: Some(Nanos(2_000)),
                t_finished: Some(Nanos(2_500)),
            }],
        };
        let json = render_chrome_trace(&result, &HashMap::new(), &HashMap::new())
            .expect("render");
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let evt = &parsed["traceEvents"][0];
        // dur survives as 0.5us — non-zero, so Perfetto draws the
        // slice.
        assert_eq!(evt["dur"], 0.5);
        assert_eq!(evt["ts"], 2.0);
    }

    /// Issue iamacoffeepot/aether#731: a known-name mailbox with
    /// `category: None` renders the bare name (no prefix) — that
    /// signals "we know what it is but the substrate didn't
    /// classify it" and is a different failure mode from the
    /// completely-unresolved raw `mbx-...` fallback.
    #[test]
    fn render_uncategorised_mailbox_emits_bare_name() {
        let id = mid(0x77, 5);
        let mut mailbox_names: MailboxLookup = HashMap::new();
        mailbox_names.insert(
            with_tag(Tag::Mailbox, 0x77),
            ("user_thing".to_owned(), None),
        );

        let result = DescribeTreeResult::Ok {
            root: id,
            in_flight: 0,
            mails: vec![MailNodeWire {
                mail_id: id,
                parent: None,
                sender: MailboxId(with_tag(Tag::Mailbox, 0x77)),
                recipient: MailboxId(with_tag(Tag::Mailbox, 0x77)),
                kind: KindId(with_tag(Tag::Kind, 0x44)),
                t_sent: Nanos(1_000),
                t_received: Some(Nanos(2_000)),
                t_finished: Some(Nanos(3_000)),
            }],
        };
        let json = render_chrome_trace(&result, &HashMap::new(), &mailbox_names).expect("render");
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let evt = &parsed["traceEvents"][0];
        // Bare name, no prefix.
        assert_eq!(evt["pid"], "user_thing");
        assert_eq!(evt["args"]["sender"], "user_thing");
        // Mail id retains the bare-name + correlation_id form,
        // separated by `#`.
        assert_eq!(evt["args"]["mail_id"], "user_thing#5");
    }
}
