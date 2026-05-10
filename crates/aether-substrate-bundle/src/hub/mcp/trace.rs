//! Trace renderer for the substrate's `TraceObserverCapability` state.
//! Issue iamacoffeepot/aether#728 / ADR-0080 Phase 3. The MCP
//! `dump_trace` tool ([`super::tools`]) is the only consumer.
//!
//! Output format today is the Chrome trace event format (a.k.a.
//! "trace event format" / "Catapult"), which Perfetto, chrome://tracing,
//! and speedscope all read natively — see
//! <https://docs.google.com/document/d/1CvAClvFfyA5R-PhYUmn5OOQtYMH4h6I0nSsKchNAySU/preview>.
//! The module name is intentionally generic so a future renderer
//! (pprof, perfetto-protobuf, …) can land as a sibling without
//! re-shuffling the path; the `dump_trace` tool would gain a `format`
//! param and dispatch.
//!
//! Pure function over [`aether_kinds::trace::DescribeTreeResult`] +
//! a kind-id → name lookup + a mailbox-id → (name, category) lookup
//! (issue iamacoffeepot/aether#731). The tool builds both lookups
//! from the engine record's `kinds` + `mailboxes` caches, calls
//! [`render`], and either returns the JSON inline or writes it to
//! disk.
//!
//! Per mail with `t_received` and `t_finished` populated, one
//! `ph:"X"` (complete) event covering the receive→finish interval on
//! the recipient's lane. Per mail with a parent (and where both
//! `parent.t_finished` and `self.t_received` are set), one
//! `ph:"s"` / `ph:"f"` flow pair so the trace viewer draws a causal
//! arrow. Mails missing timestamps (orphan or in-flight at query
//! time) are skipped — they remain inspectable via `describe_tree`.
//!
//! Issue iamacoffeepot/aether#734: pids and tids are emitted as
//! integers, not strings — Perfetto's importer auto-hashes string
//! pids (showing `<label> <hashed_pid>` decoration in lane headers);
//! integers go through the `process_name` / `thread_name` metadata
//! events as the only display-name binding. `process_name` events
//! bind each integer pid to the resolved actor label; `thread_name`
//! events bind each integer tid to the OS thread name captured at
//! the dispatcher's receive hook. Tids start past the pid range so
//! Perfetto's "tid==pid means main-thread" Linux heuristic never
//! fires.

use std::collections::HashMap;

use aether_data::{MailId, MailboxCategory, MailboxId};
use aether_kinds::trace::{DescribeTreeResult, MailNodeWire};
use serde_json::{Value, json};

/// Per-engine mailbox lookup the renderer uses to swap raw tagged
/// ids for category-prefixed names (issue iamacoffeepot/aether#731).
/// Keyed on the raw `MailboxId` u64 so the call site can pre-strip
/// the tag once instead of per-event. `None` category means "we know
/// the name but the substrate didn't classify it" — render the bare
/// name without a prefix so the failure mode stays visible.
pub(super) type MailboxLookup = HashMap<u64, (String, Option<MailboxCategory>)>;

/// Render a `DescribeTreeResult` into Chrome trace event format JSON.
/// Returns the serialized document as a `String`.
///
/// `kind_names` maps the raw `KindId` u64 → human-readable kind name
/// for the event `name` field. Resolved entries render as
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
pub(super) fn render(
    result: &DescribeTreeResult,
    kind_names: &HashMap<u64, String>,
    mailbox_names: &MailboxLookup,
) -> Result<String, serde_json::Error> {
    match result {
        DescribeTreeResult::Ok { mails, .. } => render_mails(mails, kind_names, mailbox_names),
        DescribeTreeResult::Err { not_found } => render_not_found(*not_found, mailbox_names),
    }
}

/// Issue 735: render a flat list of [`MailNodeWire`]s (the
/// `dump_trace_window` shape — no `root` / `in_flight` context, just
/// the mails that fell within the requested window). The body is the
/// post-`build_events` pipeline lifted out of [`render`] so the
/// describe-tree path and the describe-window path share the
/// downstream sort + integer-pid/tid-rewrite + metadata emission
/// logic without forcing the window-side caller to fabricate an
/// unused root / in_flight tuple.
pub(super) fn render_mails(
    mails: &[MailNodeWire],
    kind_names: &HashMap<u64, String>,
    mailbox_names: &MailboxLookup,
) -> Result<String, serde_json::Error> {
    let mut events = build_events(mails, kind_names, mailbox_names);
    // Sort by `ts` ascending. Some chrome-trace importers (Perfetto
    // included) tolerate unsorted streams but emit warnings; sorting
    // here removes that whole category of noise without callers
    // noticing.
    events.sort_by(|a, b| {
        let a_ts = a.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        let b_ts = b.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
        a_ts.partial_cmp(&b_ts).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Issue 734: assign integer pids per unique label so Perfetto
    // doesn't append a hashed-pid suffix to each lane. The first
    // unique label seen in event order gets pid = 1, the second 2,
    // etc. — stable within one render. The intermediate `pid` field
    // produced by `build_events` is a string label; we rewrite it in
    // place to the assigned integer below and emit one `process_name`
    // M event per (pid, label) pair.
    let mut pid_for_label: HashMap<String, u32> = HashMap::new();
    let mut next_pid: u32 = 1;
    for e in &events {
        if let Some(pid_label) = e.get("pid").and_then(Value::as_str)
            && !pid_for_label.contains_key(pid_label)
        {
            pid_for_label.insert(pid_label.to_owned(), next_pid);
            next_pid += 1;
        }
    }

    // Issue 734: assign integer tids per unique thread name, starting
    // *past* the pid range. Perfetto inherits the Linux convention
    // "thread's tid equals its process's pid means main thread" and
    // decorates such rows with a `main-thread` tag in its UI; if our
    // tid integers shared the 1..N range with pids, every actor whose
    // thread happened to land on the matching tid would render as
    // "main-thread" (visual noise — the substrate has no Linux-style
    // process-main concept). Offsetting tids past the pid range
    // sidesteps the heuristic without losing per-thread row distinction.
    let mut tid_for_thread: HashMap<String, u32> = HashMap::new();
    let mut next_tid: u32 = next_pid;
    for e in &events {
        if let Some(tn) = e.get("_thread_name").and_then(Value::as_str)
            && !tid_for_thread.contains_key(tn)
        {
            tid_for_thread.insert(tn.to_owned(), next_tid);
            next_tid += 1;
        }
    }

    // Track unique (pid, tid, thread_name) tuples so we emit one
    // `thread_name` M event per actually-seen pair. A `Pooled`-style
    // future scheduler that routes one actor across multiple threads
    // would surface here as multiple tids per pid, all bound by their
    // own M event.
    let mut thread_name_pairs: std::collections::BTreeSet<(u32, u32, String)> =
        std::collections::BTreeSet::new();

    // Resolve string pid/tid to integers in place. Strip the
    // `_thread_name` stash — it was an internal carrier between
    // `build_events` and this resolution pass; chrome doesn't read
    // unknown fields but they're noise in the dump.
    for e in &mut events {
        let pid_label = e.get("pid").and_then(Value::as_str).map(|s| s.to_owned());
        let thread_name = e
            .get("_thread_name")
            .and_then(Value::as_str)
            .map(|s| s.to_owned());
        if let Some(label) = &pid_label
            && let Some(&pid) = pid_for_label.get(label.as_str())
        {
            e["pid"] = Value::Number(pid.into());
            if let Some(tn) = &thread_name
                && let Some(&tid) = tid_for_thread.get(tn.as_str())
            {
                e["tid"] = Value::Number(tid.into());
                thread_name_pairs.insert((pid, tid, tn.clone()));
            }
        }
        if let Some(obj) = e.as_object_mut() {
            obj.remove("_thread_name");
        }
    }

    // Emit `process_name` M events sorted by pid for stable output.
    let mut process_name_pairs: Vec<(&String, u32)> =
        pid_for_label.iter().map(|(k, v)| (k, *v)).collect();
    process_name_pairs.sort_by_key(|(_, pid)| *pid);
    let mut metadata: Vec<Value> = Vec::with_capacity(process_name_pairs.len() + events.len());
    for (label, pid) in process_name_pairs {
        metadata.push(json!({
            "ph": "M",
            "name": "process_name",
            "cat": "metadata",
            "pid": pid,
            "args": { "name": label },
        }));
    }
    // `thread_name` M events follow — Perfetto reads them after the
    // matching `process_name` for a clean lane / row binding.
    for (pid, tid, name) in &thread_name_pairs {
        metadata.push(json!({
            "ph": "M",
            "name": "thread_name",
            "cat": "metadata",
            "pid": *pid,
            "tid": *tid,
            "args": { "name": name },
        }));
    }
    metadata.extend(events);
    serde_json::to_string_pretty(&json!({ "traceEvents": metadata }))
}

/// Renders the `not_found` Err arm of [`DescribeTreeResult`] as a
/// trace doc with a single metadata event explaining the empty
/// payload. Pulled out of `render` for symmetry with `render_mails` —
/// the window-side path never produces this shape (Phase 3 surfaces
/// `Err::too_many` as an MCP error before reaching the renderer at
/// all).
fn render_not_found(
    not_found: MailId,
    mailbox_names: &MailboxLookup,
) -> Result<String, serde_json::Error> {
    let label = format_mail_id(not_found, mailbox_names);
    // Two-event shape preserved from the pre-refactor pipeline: the
    // post-build_events resolver auto-emitted one `process_name` M
    // event per unique pid label, on top of the diagnostic `process_name`
    // M event itself. Agents that scanned the trace doc for the
    // `args.name = "...not_found..."` line still find it here.
    let metadata = vec![
        json!({
            "ph": "M",
            "name": "process_name",
            "cat": "metadata",
            "pid": 1,
            "args": { "name": label.clone() },
        }),
        json!({
            "ph": "M",
            "name": "process_name",
            "cat": "metadata",
            "pid": 1,
            "args": {
                "name": format!("describe_tree returned not_found for {label}")
            },
        }),
    ];
    serde_json::to_string_pretty(&json!({ "traceEvents": metadata }))
}

fn build_events(
    mails: &[MailNodeWire],
    kind_names: &HashMap<u64, String>,
    mailbox_names: &MailboxLookup,
) -> Vec<Value> {
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
            // Issue 734: stash the thread name captured at the
            // dispatcher's receive hook. `render` rewrites
            // `pid` to an integer + assigns a per-thread tid + emits
            // the matching `process_name` / `thread_name` M events,
            // then strips this field. Underscore prefix marks it as
            // an internal carrier (chrome ignores unknown fields, but
            // the prefix flags it for human readers of intermediate
            // dumps).
            "_thread_name": node.thread_name,
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
                // Issue 734: the s endpoint lives on the parent's
                // (pid, tid) — bind to the parent's thread name so
                // resolution lands the flow on the correct row.
                "_thread_name": parent.thread_name,
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
                // Issue 734: the f endpoint lives on the child's
                // (pid, tid) — bind to the child's thread name.
                "_thread_name": node.thread_name,
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
                    thread_name: Some("aether-root-aether.input".to_owned()),
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
                    thread_name: Some(
                        "aether-instanced-aether.component.trampoline:cam".to_owned(),
                    ),
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

        let json = render(&result, &kind_names, &mailbox_names).expect("render");
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let all_events = parsed["traceEvents"].as_array().unwrap();
        // Filter out the `process_name` / `thread_name` metadata
        // auto-prepend so existing X/s/f assertions stay focused on the
        // trace shape; resolution happens via `pid_label_for`.
        let events: Vec<&Value> = all_events.iter().filter(|e| e["ph"] != "M").collect();
        // Two complete events + two flow events (s + f for the one
        // parent edge) = 4 total.
        assert_eq!(events.len(), 4, "got: {events:#?}");

        let phs: Vec<&str> = events.iter().map(|e| e["ph"].as_str().unwrap()).collect();
        assert_eq!(phs.iter().filter(|p| **p == "X").count(), 2);
        assert_eq!(phs.iter().filter(|p| **p == "s").count(), 1);
        assert_eq!(phs.iter().filter(|p| **p == "f").count(), 1);

        // Root event: kind resolved → `kind:test.tick`; recipient
        // resolved → `actor:aether.input`; sender resolved →
        // `chassis:aether.chassis`. Issue 734: pids are integers now;
        // resolve back through the `process_name` M metadata for the
        // human-readable assertion.
        let root_complete = events
            .iter()
            .find(|e| e["ph"] == "X" && e["name"] == "kind:test.tick")
            .expect("root complete event with kind: prefix");
        assert_eq!(
            pid_label_for(all_events, root_complete["pid"].as_u64().unwrap()),
            Some("actor:aether.input".to_owned())
        );
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
        // Issue 734: the root's recipient ran on `aether-root-aether
        // .input` per the fixture; tid resolves through the
        // `thread_name` M metadata bound to (pid, tid).
        assert_eq!(
            thread_label_for(
                all_events,
                root_complete["pid"].as_u64().unwrap(),
                root_complete["tid"].as_u64().unwrap(),
            ),
            Some("aether-root-aether.input".to_owned())
        );

        // Child event: kind NOT in the lookup → falls back to raw
        // `knd-...` (no `kind:` prefix). Recipient (0xEE) NOT in the
        // lookup → falls back to raw `mbx-...`. Parent's sender
        // (0xAA) is resolved → parent renders prefixed.
        let child_complete = events
            .iter()
            .find(|e| {
                e["ph"] == "X"
                    && pid_label_for(all_events, e["pid"].as_u64().unwrap_or(0))
                        .map(|s| s.starts_with("mbx-"))
                        .unwrap_or(false)
            })
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
        // Issue 734: the child's recipient ran on the camera
        // trampoline thread per the fixture.
        assert_eq!(
            thread_label_for(
                all_events,
                child_complete["pid"].as_u64().unwrap(),
                child_complete["tid"].as_u64().unwrap(),
            ),
            Some("aether-instanced-aether.component.trampoline:cam".to_owned())
        );

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
        assert_eq!(
            pid_label_for(all_events, s["pid"].as_u64().unwrap()),
            Some("actor:aether.input".to_owned())
        );
        assert!(
            pid_label_for(all_events, f["pid"].as_u64().unwrap())
                .map(|l| l.starts_with("mbx-"))
                .unwrap_or(false)
        );
        // Issue 734: flow endpoints carry the same per-thread tid
        // their pid lane was assigned — s on the parent's thread, f
        // on the child's thread. Lining the s endpoint up with the
        // parent's row is what keeps the flow arrow attached to the
        // visible parent slice in Perfetto.
        assert_eq!(s["tid"], root_complete["tid"]);
        assert_eq!(f["tid"], child_complete["tid"]);
    }

    /// Helper: resolve an integer pid back to its `process_name`
    /// metadata label for human-readable assertions.
    fn pid_label_for(events: &[Value], pid: u64) -> Option<String> {
        events
            .iter()
            .filter(|e| e["ph"] == "M" && e["name"] == "process_name")
            .find(|e| e["pid"].as_u64() == Some(pid))
            .and_then(|e| e["args"]["name"].as_str().map(str::to_owned))
    }

    /// Helper: resolve a (pid, tid) pair back to its `thread_name`
    /// metadata label.
    fn thread_label_for(events: &[Value], pid: u64, tid: u64) -> Option<String> {
        events
            .iter()
            .filter(|e| e["ph"] == "M" && e["name"] == "thread_name")
            .find(|e| e["pid"].as_u64() == Some(pid) && e["tid"].as_u64() == Some(tid))
            .and_then(|e| e["args"]["name"].as_str().map(str::to_owned))
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
                thread_name: None,
            }],
        };
        let json = render(&result, &HashMap::new(), &HashMap::new()).expect("render");
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert!(
            parsed["traceEvents"].as_array().unwrap().is_empty(),
            "in-flight mail must not produce events"
        );
    }

    /// `Err::not_found` produces a trace doc with the diagnostic
    /// metadata event noting the missing root, so chrome://tracing
    /// displays the diagnostic instead of a blank file. The
    /// `process_name` auto-prepend (issue 731 follow-up to silence
    /// Perfetto's hashed-pid display) adds one more metadata event
    /// for the same pid; both are valid metadata entries.
    #[test]
    fn render_not_found_emits_metadata_doc() {
        let missing = mid(0xFF, 99);
        let result = DescribeTreeResult::Err { not_found: missing };
        let json = render(&result, &HashMap::new(), &HashMap::new()).expect("render");
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let events = parsed["traceEvents"].as_array().unwrap();
        // Both events are metadata: the auto-prepended `process_name`
        // for the missing-root pid, plus the `not_found` diagnostic
        // M event with the human-readable explanation in args.name.
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e["ph"] == "M"));
        let diagnostic = events
            .iter()
            .find(|e| {
                e["args"]["name"]
                    .as_str()
                    .map(|s| s.contains("not_found"))
                    .unwrap_or(false)
            })
            .expect("diagnostic event present");
        let arg_name = diagnostic["args"]["name"].as_str().unwrap();
        assert!(
            arg_name.contains("not_found"),
            "metadata should mention not_found: {arg_name}"
        );
    }

    /// Issue 731 follow-up: Perfetto auto-hashes string pids and
    /// shows the lane label as `<pid> <hash>` (e.g.
    /// `actor:aether.input 7230`) when no `process_name` metadata
    /// event registers the friendly name. Auto-prepend one
    /// `process_name` per unique pid so Perfetto uses just the
    /// configured name.
    #[test]
    fn render_prepends_process_name_metadata_per_unique_pid() {
        let id = mid(0xAA, 1);
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
        let result = DescribeTreeResult::Ok {
            root: id,
            in_flight: 0,
            mails: vec![MailNodeWire {
                mail_id: id,
                parent: None,
                sender: MailboxId(with_tag(Tag::Mailbox, 0xAA)),
                recipient: MailboxId(with_tag(Tag::Mailbox, 0xCC)),
                kind: KindId(with_tag(Tag::Kind, 0x99)),
                t_sent: Nanos(1_000),
                t_received: Some(Nanos(2_000)),
                t_finished: Some(Nanos(3_000)),
                thread_name: Some("aether-root-aether.input".to_owned()),
            }],
        };
        let json = render(&result, &HashMap::new(), &mailbox_names).expect("render");
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let events = parsed["traceEvents"].as_array().unwrap();
        // Single X event has one unique pid (`actor:aether.input`)
        // and one unique thread (`aether-root-aether.input`), so we
        // expect one process_name M + one thread_name M + the X event.
        let metadata: Vec<&Value> = events.iter().filter(|e| e["ph"] == "M").collect();
        assert_eq!(metadata.len(), 2);
        let process_name = metadata
            .iter()
            .find(|m| m["name"] == "process_name")
            .expect("process_name metadata");
        // Issue 734: pid is now an integer; the M event's args.name
        // carries the bare label that Perfetto uses for the lane.
        assert!(
            process_name["pid"].is_u64(),
            "pid should be integer, got: {:?}",
            process_name["pid"]
        );
        assert_eq!(process_name["args"]["name"], "actor:aether.input");
        let thread_name = metadata
            .iter()
            .find(|m| m["name"] == "thread_name")
            .expect("thread_name metadata");
        assert_eq!(thread_name["pid"], process_name["pid"]);
        assert!(thread_name["tid"].is_u64());
        assert_eq!(thread_name["args"]["name"], "aether-root-aether.input");
        // Metadata events come BEFORE slice events in the output —
        // chrome importers expect process metadata up-front.
        let first_slice_idx = events
            .iter()
            .position(|e| e["ph"] == "X")
            .expect("X event present");
        assert!(
            metadata
                .iter()
                .all(|m| events.iter().position(|e| std::ptr::eq(e, *m)).unwrap() < first_slice_idx),
            "process_name / thread_name metadata must precede slice events"
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
                thread_name: None,
            }],
        };
        let json = render(&result, &HashMap::new(), &HashMap::new()).expect("render");
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let evt = parsed["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["ph"] == "X")
            .expect("X event present");
        // dur survives as 0.5us — non-zero, so Perfetto draws the
        // slice.
        assert_eq!(evt["dur"], 0.5);
        assert_eq!(evt["ts"], 2.0);
    }

    /// Issue 734 follow-up: Perfetto inherits the Linux convention
    /// "tid == pid means the process's main thread" from chrome
    /// trace files and stamps such rows with a `main-thread` tag. Our
    /// renderer offsets the tid namespace past the pid range so the
    /// heuristic never fires; this test pins the invariant — every
    /// emitted tid must be strictly greater than every emitted pid.
    #[test]
    fn render_tid_namespace_does_not_overlap_pid_namespace() {
        let root_id = mid(0xAA, 1);
        let child_id = mid(0xBB, 2);
        // Three distinct pids (root.recipient = 0xCC, child.recipient
        // = 0xEE, plus an unrelated mail to 0xFF) and two distinct
        // thread names so we stress both maps.
        let result = DescribeTreeResult::Ok {
            root: root_id,
            in_flight: 0,
            mails: vec![
                MailNodeWire {
                    mail_id: root_id,
                    parent: None,
                    sender: MailboxId(with_tag(Tag::Mailbox, 0xAA)),
                    recipient: MailboxId(with_tag(Tag::Mailbox, 0xCC)),
                    kind: KindId(with_tag(Tag::Kind, 0x99)),
                    t_sent: Nanos(1_000),
                    t_received: Some(Nanos(2_000)),
                    t_finished: Some(Nanos(3_000)),
                    thread_name: Some("aether-worker-1".to_owned()),
                },
                MailNodeWire {
                    mail_id: child_id,
                    parent: Some(root_id),
                    sender: MailboxId(with_tag(Tag::Mailbox, 0xCC)),
                    recipient: MailboxId(with_tag(Tag::Mailbox, 0xEE)),
                    kind: KindId(with_tag(Tag::Kind, 0x88)),
                    t_sent: Nanos(2_500),
                    t_received: Some(Nanos(4_000)),
                    t_finished: Some(Nanos(5_000)),
                    thread_name: Some("aether-worker-2".to_owned()),
                },
            ],
        };
        let json = render(&result, &HashMap::new(), &HashMap::new()).expect("render");
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let events = parsed["traceEvents"].as_array().unwrap();
        let max_pid = events
            .iter()
            .filter_map(|e| e["pid"].as_u64())
            .max()
            .expect("pid present");
        let min_tid = events
            .iter()
            .filter_map(|e| {
                if e["ph"] == "M" && e["name"] == "process_name" {
                    None
                } else {
                    e["tid"].as_u64()
                }
            })
            .min()
            .expect("tid present");
        assert!(
            min_tid > max_pid,
            "tid {min_tid} must be > max pid {max_pid} so Perfetto's \
             tid==pid main-thread heuristic never fires; got events: {events:#?}"
        );
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
                thread_name: None,
            }],
        };
        let json = render(&result, &HashMap::new(), &mailbox_names).expect("render");
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let all_events = parsed["traceEvents"].as_array().unwrap();
        let evt = all_events
            .iter()
            .find(|e| e["ph"] == "X")
            .expect("X event present");
        // Issue 734: pid is an integer; the bare-label assertion goes
        // through the `process_name` M event lookup.
        assert_eq!(
            pid_label_for(all_events, evt["pid"].as_u64().unwrap()),
            Some("user_thing".to_owned())
        );
        assert_eq!(evt["args"]["sender"], "user_thing");
        // Mail id retains the bare-name + correlation_id form,
        // separated by `#`.
        assert_eq!(evt["args"]["mail_id"], "user_thing#5");
    }
}
