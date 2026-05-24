//! ADR-0086 Phase 3b decentralized trace-tree reconstruction. The
//! central observer's `build_describe_tree` walks a single in-memory
//! `mails_by_root` index; this module reconstructs the same tree by a
//! guided fan-out across the per-actor trace rings (ADR-0086 Phase 3a),
//! stitched client-side.
//!
//! The walk self-directs. It seeds at the root mail's `sender` — the
//! chassis-host pseudo-mailbox ([`MailboxId::CHASSIS_MAILBOX_ID`])
//! for an injected root, an actor otherwise — to pick up the root's own
//! `Sent`, then follows every `Sent` event's `recipient`. Each
//! recipient's ring holds that mail's `Received` / `Finished` plus any
//! onward `Sent`s, so the frontier expands purely from observed
//! recipients: the walk visits exactly the actors participating in the
//! tree and never enumerates the full actor set. (That bound is what
//! lets a query during a barrier touch O(tree) actors rather than
//! O(live actors) — ADR-0086 Phase 3b cost note.)
//!
//! Transport is the caller's: the MCP issues `aether.trace.tail` over
//! the wire (addressing each mailbox by id, routing
//! `CHASSIS_MAILBOX_ID` to the chassis-host ring), the in-process
//! harness calls the per-actor ring tail / `chassis_host_tail`
//! directly. [`TreeWalk`] owns the seed, frontier, dedup, and stitch;
//! the caller owns only the fetch.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use aether_data::{KindId, MailId, MailboxId, ThreadId};
use aether_kinds::trace::{DescribeTreeResult, MailNodeWire, Nanos, TraceEvent, TraceRingEntry};

/// Resolve a `Received` event's optional [`ThreadId`] (ADR-0088 §7) to a
/// display name. The trace event carries a `Copy` [`ThreadId`] (no
/// per-hop alloc); this cold fold path reverses it to the
/// `aether-worker-N` / `aether-instanced-…` string for
/// [`MailNodeWire::thread_name`].
///
/// `trace_walk` stays transport-agnostic and compiles in every feature
/// config (including the wasm-component build), so the registry lookup
/// is `native`-gated: the in-process substrate reverses through its
/// process-global registry; the wasm build and the out-of-process MCP
/// (which can't reach a substrate's registry until the ADR-0088 §6
/// inventory actor lands) get `None` and the renderer falls back exactly
/// as before — nothing regresses.
fn resolve_thread_name(thread_id: Option<ThreadId>) -> Option<String> {
    let thread_id = thread_id?;
    #[cfg(feature = "native")]
    {
        use aether_substrate::runtime::thread_name;
        thread_name::resolve(thread_id.0)
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = thread_id;
        None
    }
}

/// A guided breadth-first walk of one root's mail tree across per-actor
/// trace rings. Construct with [`TreeWalk::new`], then drive the loop:
/// call [`TreeWalk::next_mailbox`] for the next ring to query, fetch
/// that mailbox's `root`-filtered tail, feed the entries to
/// [`TreeWalk::absorb`]. When `next_mailbox` returns `None` the frontier
/// is exhausted; [`TreeWalk::finish`] stitches the collected events into
/// a [`DescribeTreeResult`].
pub struct TreeWalk {
    root: MailId,
    visited: BTreeSet<MailboxId>,
    frontier: VecDeque<MailboxId>,
    collected: Vec<TraceRingEntry>,
}

impl TreeWalk {
    /// Begin a walk for `root`, seeding the frontier with the root
    /// mail's `sender` (where the root's own `Sent` lives).
    #[must_use]
    pub fn new(root: MailId) -> Self {
        let mut frontier = VecDeque::new();
        frontier.push_back(root.sender);
        Self {
            root,
            visited: BTreeSet::new(),
            frontier,
            collected: Vec::new(),
        }
    }

    /// The next mailbox whose trace ring should be queried, or `None`
    /// when the frontier is exhausted. Skips mailboxes already visited
    /// (a diamond in the mail graph enqueues the same recipient twice).
    pub fn next_mailbox(&mut self) -> Option<MailboxId> {
        while let Some(mbx) = self.frontier.pop_front() {
            if self.visited.insert(mbx) {
                return Some(mbx);
            }
        }
        None
    }

    /// Feed the entries returned for the mailbox handed out by the most
    /// recent [`Self::next_mailbox`]. Entries for other roots are
    /// ignored (a `root`-filtered tail keeps the fetch cheap, but the
    /// guard is belt-and-braces). Each in-tree `Sent` enqueues its
    /// recipient onto the frontier.
    pub fn absorb(&mut self, entries: impl IntoIterator<Item = TraceRingEntry>) {
        for entry in entries {
            if entry.root != self.root {
                continue;
            }
            if let TraceEvent::Sent { recipient, .. } = entry.event
                && !self.visited.contains(&recipient)
            {
                self.frontier.push_back(recipient);
            }
            self.collected.push(entry);
        }
    }

    /// Stitch the collected events into a [`DescribeTreeResult`].
    #[must_use]
    pub fn finish(self) -> DescribeTreeResult {
        stitch(self.root, self.collected)
    }
}

/// Fold a flat set of [`TraceRingEntry`]s — gathered from however many
/// per-actor rings a walk visited — into one [`MailNodeWire`] per
/// `mail_id`. `Sent` seeds the node's topology fields and `t_sent`;
/// `Received` adds `t_received` + the dispatching thread's display name
/// (resolved from the event's `Copy` [`ThreadId`] via the installed
/// reverse-lookup resolver, ADR-0088 §7); `Finished` adds
/// `t_finished`. Holds (`HoldOpen` / `Release`) carry no `mail_id` and
/// are skipped — they aren't tree nodes (ADR-0086 Phase 3 §C). The fold
/// is order-independent, so a node first seen via `Received` (its `Sent`
/// in a ring absorbed later) resolves once the `Sent` lands.
///
/// Returns `Err { not_found }` when the root produced no `Sent` — the
/// tree never existed or its seed ring evicted it — matching the
/// central observer's contract. `in_flight` counts nodes with a `Sent`
/// but no `Finished`; the only caller today walks post-settlement, so
/// it sees `0`.
#[must_use]
pub fn stitch(
    root: MailId,
    entries: impl IntoIterator<Item = TraceRingEntry>,
) -> DescribeTreeResult {
    let mails = fold_nodes(entries);
    if !mails.iter().any(|n| n.mail_id == root) {
        return DescribeTreeResult::Err { not_found: root };
    }
    let in_flight =
        u32::try_from(mails.iter().filter(|n| n.t_finished.is_none()).count()).unwrap_or(u32::MAX);
    DescribeTreeResult::Ok {
        root,
        in_flight,
        mails,
    }
}

/// The order-independent fold under [`stitch`], without the root /
/// `in_flight` framing: collapse a flat event stream into one
/// [`MailNodeWire`] per `mail_id`. A node with no `Sent` (its sender's
/// ring never visited, or evicted) is dropped — `MailNodeWire` requires
/// the topology fields a `Sent` carries. Exposed for callers that
/// aggregate across many roots' rings at once (the latency harness folds
/// every relay's ring this way) rather than reconstructing one tree.
#[must_use]
pub fn fold_nodes(entries: impl IntoIterator<Item = TraceRingEntry>) -> Vec<MailNodeWire> {
    let mut nodes: BTreeMap<MailId, PartialNode> = BTreeMap::new();
    for entry in entries {
        match entry.event {
            TraceEvent::Sent {
                mail_id,
                parent_mail,
                sender,
                recipient,
                kind,
                t,
                ..
            } => {
                nodes.entry(mail_id).or_default().sent = Some(SentFields {
                    parent: parent_mail,
                    sender,
                    recipient,
                    kind,
                    t_sent: t,
                });
            }
            TraceEvent::Received {
                mail_id,
                t,
                thread_id,
            } => {
                let node = nodes.entry(mail_id).or_default();
                node.t_received = Some(t);
                // ADR-0088 §7: the event carries a `Copy` `ThreadId`;
                // resolve it to a display name on this cold fold path.
                node.thread_name = resolve_thread_name(thread_id);
            }
            TraceEvent::Finished { mail_id, t } => {
                nodes.entry(mail_id).or_default().t_finished = Some(t);
            }
            TraceEvent::HoldOpen { .. } | TraceEvent::Release { .. } => {}
        }
    }

    nodes
        .into_iter()
        .filter_map(|(mail_id, node)| {
            let sent = node.sent?;
            Some(MailNodeWire {
                mail_id,
                parent: sent.parent,
                sender: sent.sender,
                recipient: sent.recipient,
                kind: sent.kind,
                t_sent: sent.t_sent,
                t_received: node.t_received,
                t_finished: node.t_finished,
                thread_name: node.thread_name,
            })
        })
        .collect()
}

#[derive(Default)]
struct PartialNode {
    sent: Option<SentFields>,
    t_received: Option<Nanos>,
    t_finished: Option<Nanos>,
    thread_name: Option<String>,
}

struct SentFields {
    parent: Option<MailId>,
    sender: MailboxId,
    recipient: MailboxId,
    kind: KindId,
    t_sent: Nanos,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(sender: u64, cid: u64) -> MailId {
        MailId {
            sender: MailboxId(sender),
            correlation_id: cid,
        }
    }

    fn sent(mail_id: MailId, root: MailId, recipient: u64) -> TraceRingEntry {
        sent_parent(mail_id, root, None, recipient)
    }

    fn sent_parent(
        mail_id: MailId,
        root: MailId,
        parent: Option<MailId>,
        recipient: u64,
    ) -> TraceRingEntry {
        TraceRingEntry {
            sequence: 0,
            root,
            event: TraceEvent::Sent {
                mail_id,
                root,
                parent_mail: parent,
                sender: mail_id.sender,
                recipient: MailboxId(recipient),
                kind: KindId(0xAB),
                t: Nanos(mail_id.correlation_id),
            },
        }
    }

    /// The thread name every `received` fixture event hashes into a
    /// `ThreadId`. [`register_fixture_thread_name`] seeds the substrate's
    /// reverse-lookup registry so the `native`-gated fold path reverses
    /// it back.
    const FIXTURE_THREAD_NAME: &str = "aether-worker-0";

    fn received(mail_id: MailId, root: MailId) -> TraceRingEntry {
        TraceRingEntry {
            sequence: 0,
            root,
            event: TraceEvent::Received {
                mail_id,
                t: Nanos(mail_id.correlation_id + 1),
                thread_id: Some(ThreadId::from_name(FIXTURE_THREAD_NAME)),
            },
        }
    }

    /// Register the fixture thread name in the substrate's process-global
    /// reverse-lookup registry so [`resolve_thread_name`] reverses the
    /// fixture `ThreadId` back to its display name (ADR-0088 §7).
    /// Idempotent — `register` no-ops on a duplicate.
    fn register_fixture_thread_name() {
        use aether_substrate::runtime::thread_name;
        let id = ThreadId::from_name(FIXTURE_THREAD_NAME);
        thread_name::register(id.0, FIXTURE_THREAD_NAME);
    }

    fn finished(mail_id: MailId, root: MailId) -> TraceRingEntry {
        TraceRingEntry {
            sequence: 0,
            root,
            event: TraceEvent::Finished {
                mail_id,
                t: Nanos(mail_id.correlation_id + 2),
            },
        }
    }

    fn ok(result: DescribeTreeResult) -> (MailId, u32, Vec<MailNodeWire>) {
        match result {
            DescribeTreeResult::Ok {
                root,
                in_flight,
                mails,
            } => (root, in_flight, mails),
            DescribeTreeResult::Err { not_found } => panic!("expected Ok, got Err {not_found:?}"),
        }
    }

    /// Stitch is order-independent: feeding `Finished` before its
    /// `Sent` still produces one complete node.
    #[test]
    fn stitch_folds_events_per_mail_id_regardless_of_order() {
        register_fixture_thread_name();
        let root = mid(1, 1);
        let entries = vec![
            finished(root, root),
            received(root, root),
            sent(root, root, 2),
        ];
        let (got_root, in_flight, mails) = ok(stitch(root, entries));
        assert_eq!(got_root, root);
        assert_eq!(in_flight, 0, "node has a Finished");
        assert_eq!(mails.len(), 1);
        let node = &mails[0];
        assert_eq!(node.mail_id, root);
        assert_eq!(node.recipient, MailboxId(2));
        assert_eq!(node.t_sent, Nanos(1));
        assert_eq!(node.t_received, Some(Nanos(2)));
        assert_eq!(node.t_finished, Some(Nanos(3)));
        // The cold fold resolved the event's `ThreadId` back to the
        // fixture display name via the test resolver (ADR-0088 §7).
        assert_eq!(node.thread_name.as_deref(), Some("aether-worker-0"));
    }

    /// A root that produced no `Sent` (never seen / seed ring evicted)
    /// reports `Err { not_found }`, matching the observer.
    #[test]
    fn stitch_missing_root_sent_is_not_found() {
        let root = mid(1, 1);
        // Only Received/Finished for the root, no Sent.
        let entries = vec![received(root, root), finished(root, root)];
        match stitch(root, entries) {
            DescribeTreeResult::Err { not_found } => assert_eq!(not_found, root),
            ok @ DescribeTreeResult::Ok { .. } => panic!("expected Err, got {ok:?}"),
        }
    }

    /// A node still in flight (Sent, no Finished) is counted.
    #[test]
    fn stitch_counts_unfinished_nodes_as_in_flight() {
        let root = mid(1, 1);
        let child = mid(2, 1);
        let entries = vec![
            sent(root, root, 2),
            received(root, root),
            finished(root, root),
            sent_parent(child, root, Some(root), 3), // child sent, never finished
        ];
        let (_, in_flight, mails) = ok(stitch(root, entries));
        assert_eq!(mails.len(), 2);
        assert_eq!(in_flight, 1, "the child has no Finished");
    }

    /// End-to-end guided walk over a fake multi-ring substrate. The
    /// topology mirrors a `send_mail_traced` tree: an injected root
    /// (chassis -> observer) whose handler re-sends to two recipients,
    /// one of which forwards once more. Each `Sent` lands in the
    /// sender's ring, each `Received`/`Finished` in the recipient's.
    #[test]
    fn guided_walk_reconstructs_tree_across_rings() {
        // Mailbox ids: 0 = chassis-host, 10 = observer, 20/21 = leaves,
        // 30 = a grandchild reached only by following 20's onward send.
        let chassis = 0u64;
        let observer = 10u64;
        let leaf_a = 20u64;
        let leaf_b = 21u64;
        let grandchild = 30u64;

        let root = MailId {
            sender: MailboxId(chassis),
            correlation_id: 1,
        };
        let child_a = mid(observer, 2);
        let child_b = mid(observer, 3);
        let gc = mid(leaf_a, 4);

        // Per-mailbox rings. Sent in the sender's ring; Received +
        // Finished in the recipient's ring.
        let mut rings: BTreeMap<MailboxId, Vec<TraceRingEntry>> = BTreeMap::new();
        // chassis-host: the root's Sent (chassis -> observer).
        rings
            .entry(MailboxId(chassis))
            .or_default()
            .push(sent(root, root, observer));
        // observer: root's Received/Finished + the two children's Sents.
        rings.entry(MailboxId(observer)).or_default().extend([
            received(root, root),
            finished(root, root),
            sent_parent(child_a, root, Some(root), leaf_a),
            sent_parent(child_b, root, Some(root), leaf_b),
        ]);
        // leaf_a: child_a's Received/Finished + an onward Sent to gc.
        rings.entry(MailboxId(leaf_a)).or_default().extend([
            received(child_a, root),
            finished(child_a, root),
            sent_parent(gc, root, Some(child_a), grandchild),
        ]);
        // leaf_b: child_b's Received/Finished, no onward send.
        rings
            .entry(MailboxId(leaf_b))
            .or_default()
            .extend([received(child_b, root), finished(child_b, root)]);
        // grandchild: gc's Received/Finished.
        rings
            .entry(MailboxId(grandchild))
            .or_default()
            .extend([received(gc, root), finished(gc, root)]);

        // Drive the walk against the fake substrate.
        let mut walk = TreeWalk::new(root);
        let mut visited_order = Vec::new();
        while let Some(mbx) = walk.next_mailbox() {
            visited_order.push(mbx);
            let entries = rings.get(&mbx).cloned().unwrap_or_default();
            walk.absorb(entries);
        }
        let (got_root, in_flight, mails) = ok(walk.finish());

        assert_eq!(got_root, root);
        assert_eq!(in_flight, 0, "fully settled tree");
        // Four mails: root + two children + one grandchild.
        assert_eq!(mails.len(), 4, "root, child_a, child_b, grandchild");

        let by_id: BTreeMap<MailId, &MailNodeWire> = mails.iter().map(|n| (n.mail_id, n)).collect();
        assert_eq!(by_id[&root].parent, None);
        assert_eq!(by_id[&child_a].parent, Some(root));
        assert_eq!(by_id[&child_b].parent, Some(root));
        assert_eq!(by_id[&gc].parent, Some(child_a));
        // Every node carries Received + Finished — the walk visited
        // every recipient ring.
        assert!(
            mails
                .iter()
                .all(|n| n.t_received.is_some() && n.t_finished.is_some())
        );

        // The walk visited only the five participating mailboxes, never
        // an actor outside the tree.
        assert_eq!(visited_order.len(), 5);
        let visited: BTreeSet<MailboxId> = visited_order.into_iter().collect();
        assert_eq!(
            visited,
            [chassis, observer, leaf_a, leaf_b, grandchild]
                .into_iter()
                .map(MailboxId)
                .collect()
        );
    }

    /// A diamond (two parents send to the same recipient) visits the
    /// shared recipient's ring exactly once.
    #[test]
    fn guided_walk_dedups_diamond_recipient() {
        let root = mid(0, 1);
        let child_a = mid(10, 2);
        let child_b = mid(10, 3);
        let shared = 40u64;

        let mut rings: BTreeMap<MailboxId, Vec<TraceRingEntry>> = BTreeMap::new();
        rings
            .entry(MailboxId(0))
            .or_default()
            .push(sent(root, root, 10));
        rings.entry(MailboxId(10)).or_default().extend([
            received(root, root),
            finished(root, root),
            sent_parent(child_a, root, Some(root), shared),
            sent_parent(child_b, root, Some(root), shared),
        ]);
        // The shared recipient receives both children.
        rings.entry(MailboxId(shared)).or_default().extend([
            received(child_a, root),
            finished(child_a, root),
            received(child_b, root),
            finished(child_b, root),
        ]);

        let mut walk = TreeWalk::new(root);
        let mut visits = 0;
        while let Some(mbx) = walk.next_mailbox() {
            visits += 1;
            walk.absorb(rings.get(&mbx).cloned().unwrap_or_default());
        }
        let (_, _, mails) = ok(walk.finish());
        assert_eq!(visits, 3, "chassis, observer, shared — shared once");
        assert_eq!(mails.len(), 3, "root + two children");
    }
}
