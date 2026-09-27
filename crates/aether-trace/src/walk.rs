//! ADR-0086 Phase 3b decentralized trace-tree reconstruction. The
//! central observer's `build_describe_tree` walks a single in-memory
//! `mails_by_root` index; this module reconstructs the same tree by a
//! guided fan-out across the per-actor trace rings (ADR-0086 Phase 3a),
//! stitched client-side.
//!
//! The walk self-directs, and it follows actor paths: every trace export
//! names actors by canonical path (ADR-0230 §1). It seeds at the root
//! mail's `sender` path — `aether.chassis`, the chassis-host ring, for an
//! injected root, an actor otherwise — to pick up the root's own `Sent`,
//! then follows every `Sent` event's `recipient` path. Each recipient's
//! ring holds that mail's `Received` / `Finished` plus any onward
//! `Sent`s, so the frontier expands purely from observed recipients: the
//! walk visits exactly the actors participating in the tree and never
//! enumerates the full actor set. (That bound is what lets a query during
//! a barrier touch O(tree) actors rather than O(live actors) — ADR-0086
//! Phase 3b cost note.) A retired actor's records still name its path, so
//! the walk reaches its ring for as long as the ring answers. An endpoint
//! the export could not name (`None`, a position with no route record) is
//! not followed.
//!
//! Transport is the caller's: the MCP issues `aether.trace.tail` over
//! the wire addressed by each path, and the in-process harness tails the
//! chassis-host ring directly and every other ring through the proof it
//! holds for that path. [`TreeWalk`] owns the seed, frontier, dedup, and
//! stitch; the caller owns only the fetch.
//!
//! Thread-name resolution is the caller's too (ADR-0102). A
//! [`MailNodeWire::thread_name`] is recovered from the event's `Copy`
//! [`ThreadId`] only if the caller supplies a resolver: the in-process
//! substrate passes `aether_substrate::runtime::thread_name::resolve`,
//! which reads its process-global reverse-lookup registry; the
//! out-of-process MCP (which can't reach a substrate's registry) and the
//! wasm build use the [`stitch`] / [`fold_nodes`] / [`TreeWalk::finish`]
//! variants that resolve to `None` and let the renderer fall back to the
//! ADR-0064 tagged-id string. This crate carries no native dependency,
//! so it never reaches a registry itself.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use aether_data::{ErasedActorPath, KindId, ThreadId};
use aether_kinds::trace::{DescribeTreeResult, MailNodeWire, Nanos, TraceEvent, TraceMailId, TraceRingEntry};

/// A guided breadth-first walk of one root's mail tree across per-actor
/// trace rings. Construct with [`TreeWalk::new`], then drive the loop:
/// call [`TreeWalk::next_actor`] for the path of the next ring to query,
/// fetch that actor's `root`-filtered tail, feed the entries to
/// [`TreeWalk::absorb`]. When `next_actor` returns `None` the frontier
/// is exhausted; [`TreeWalk::finish`] stitches the collected events into
/// a [`DescribeTreeResult`].
pub struct TreeWalk {
    root: TraceMailId,
    visited: BTreeSet<ErasedActorPath>,
    frontier: VecDeque<ErasedActorPath>,
    collected: Vec<TraceRingEntry>,
}

impl TreeWalk {
    /// Begin a walk for `root`, seeding the frontier with the root
    /// mail's `sender` path (where the root's own `Sent` lives). A root
    /// with no sender path seeds nothing, so [`Self::finish`] answers
    /// `Err { not_found }`.
    #[must_use]
    pub fn new(root: TraceMailId) -> Self {
        let frontier = root.sender.iter().cloned().collect();
        Self { root, visited: BTreeSet::new(), frontier, collected: Vec::new() }
    }

    /// The path of the next actor whose trace ring should be queried, or
    /// `None` when the frontier is exhausted. Skips actors already
    /// visited (a diamond in the mail graph enqueues the same recipient
    /// twice).
    pub fn next_actor(&mut self) -> Option<ErasedActorPath> {
        while let Some(actor) = self.frontier.pop_front() {
            if self.visited.insert(actor.clone()) {
                return Some(actor);
            }
        }
        None
    }

    /// Feed the entries returned for the actor handed out by the most
    /// recent [`Self::next_actor`]. Entries for other roots are
    /// ignored (a `root`-filtered tail keeps the fetch cheap, but the
    /// guard is belt-and-braces). Each in-tree `Sent` enqueues its
    /// recipient path onto the frontier; a recipient the export could not
    /// name is not followed.
    pub fn absorb(&mut self, entries: impl IntoIterator<Item = TraceRingEntry>) {
        for entry in entries {
            if entry.root != self.root {
                continue;
            }
            if let TraceEvent::Sent { recipient: Some(recipient), .. } = &entry.event
                && !self.visited.contains(recipient)
            {
                self.frontier.push_back(recipient.clone());
            }
            self.collected.push(entry);
        }
    }

    /// Stitch the collected events into a [`DescribeTreeResult`],
    /// resolving every node's `thread_name` to `None`. The path the MCP
    /// and the wasm build take — neither can reach a substrate's
    /// reverse-lookup registry.
    #[must_use]
    pub fn finish(self) -> DescribeTreeResult {
        self.finish_with(|_| None)
    }

    /// Stitch the collected events into a [`DescribeTreeResult`], using
    /// `resolve` to recover each node's `thread_name` from the trace
    /// event's [`ThreadId`]. The in-process substrate passes
    /// `aether_substrate::runtime::thread_name::resolve` (ADR-0088 §7).
    #[must_use]
    pub fn finish_with<F>(self, resolve: F) -> DescribeTreeResult
    where
        F: Fn(ThreadId) -> Option<String>,
    {
        stitch_with(self.root, self.collected, resolve)
    }
}

/// Fold a flat set of [`TraceRingEntry`]s into one [`MailNodeWire`] per
/// `mail_id` and frame the result as a [`DescribeTreeResult`], resolving
/// every node's `thread_name` to `None`. See [`stitch_with`] for the
/// resolver-injecting form and the full contract.
#[must_use]
pub fn stitch(root: TraceMailId, entries: impl IntoIterator<Item = TraceRingEntry>) -> DescribeTreeResult {
    stitch_with(root, entries, |_| None)
}

/// Fold a flat set of [`TraceRingEntry`]s — gathered from however many
/// per-actor rings a walk visited — into one [`MailNodeWire`] per
/// `mail_id`. `Sent` seeds the node's topology fields and `t_sent`;
/// `Received` adds `t_received` + the dispatching thread's display name
/// (recovered from the event's `Copy` [`ThreadId`] via the caller's
/// `resolve`, ADR-0088 §7); `Finished` adds `t_finished`. Holds
/// (`HoldOpen` / `Release`) carry no `mail_id` and are skipped — they
/// aren't tree nodes (ADR-0086 Phase 3 §C). The fold is
/// order-independent, so a node first seen via `Received` (its `Sent`
/// in a ring absorbed later) resolves once the `Sent` lands.
///
/// Returns `Err { not_found }` when the root produced no `Sent` — the
/// tree never existed or its seed ring evicted it — matching the
/// central observer's contract. `in_flight` counts nodes with a `Sent`
/// but no `Finished`; the only caller today walks post-settlement, so
/// it sees `0`.
#[must_use]
pub fn stitch_with<F>(
    root: TraceMailId,
    entries: impl IntoIterator<Item = TraceRingEntry>,
    resolve: F,
) -> DescribeTreeResult
where
    F: Fn(ThreadId) -> Option<String>,
{
    let mails = fold_nodes_with(entries, resolve);
    if !mails.iter().any(|n| n.mail_id == root) {
        return DescribeTreeResult::Err { not_found: root };
    }
    let in_flight = u32::try_from(mails.iter().filter(|n| n.t_finished.is_none()).count()).unwrap_or(u32::MAX);
    DescribeTreeResult::Ok { root, in_flight, mails }
}

/// Collapse a flat event stream into one [`MailNodeWire`] per `mail_id`,
/// resolving every node's `thread_name` to `None`. See
/// [`fold_nodes_with`] for the resolver-injecting form.
#[must_use]
pub fn fold_nodes(entries: impl IntoIterator<Item = TraceRingEntry>) -> Vec<MailNodeWire> {
    fold_nodes_with(entries, |_| None)
}

/// The order-independent fold under [`stitch_with`], without the root /
/// `in_flight` framing: collapse a flat event stream into one
/// [`MailNodeWire`] per `mail_id`, using `resolve` to recover each
/// node's `thread_name` from its [`ThreadId`]. A node with no `Sent`
/// (its sender's ring never visited, or evicted) is dropped —
/// `MailNodeWire` requires the topology fields a `Sent` carries. Exposed
/// for callers that aggregate across many roots' rings at once (the
/// latency harness folds every relay's ring this way) rather than
/// reconstructing one tree.
#[must_use]
pub fn fold_nodes_with<F>(entries: impl IntoIterator<Item = TraceRingEntry>, resolve: F) -> Vec<MailNodeWire>
where
    F: Fn(ThreadId) -> Option<String>,
{
    let mut nodes: BTreeMap<TraceMailId, PartialNode> = BTreeMap::new();
    for entry in entries {
        match entry.event {
            TraceEvent::Sent { mail_id, parent_mail, sender, recipient, kind, t_construct_start, t, .. } => {
                nodes.entry(mail_id).or_default().sent =
                    Some(SentFields { parent: parent_mail, sender, recipient, kind, t_construct_start, t_sent: t });
            }
            TraceEvent::Received { mail_id, t, t_enqueue, enqueue_depth, thread_id } => {
                let node = nodes.entry(mail_id).or_default();
                node.t_received = Some(t);
                // iamacoffeepot/aether#1134: the deposit instant + backlog
                // ride the `Received` event; carry them onto the node so
                // the harness can split the hop into send→enqueue +
                // residence.
                node.t_enqueue = Some(t_enqueue);
                node.enqueue_depth = Some(enqueue_depth);
                // ADR-0088 §7: the event carries a `Copy` `ThreadId`;
                // recover its display name on this cold fold path via the
                // caller's resolver (`None` for the MCP / wasm path).
                node.thread_name = thread_id.and_then(&resolve);
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
                t_construct_start: sent.t_construct_start,
                t_sent: sent.t_sent,
                t_enqueue: node.t_enqueue,
                enqueue_depth: node.enqueue_depth,
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
    t_enqueue: Option<Nanos>,
    enqueue_depth: Option<u32>,
    t_received: Option<Nanos>,
    t_finished: Option<Nanos>,
    thread_name: Option<String>,
}

struct SentFields {
    parent: Option<TraceMailId>,
    sender: Option<ErasedActorPath>,
    recipient: Option<ErasedActorPath>,
    kind: KindId,
    t_construct_start: Nanos,
    t_sent: Nanos,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_kinds::trace::MailNodeWire;

    fn path(text: &str) -> ErasedActorPath {
        ErasedActorPath::new(text).expect("fixture is an actor path")
    }

    fn mid(sender: &str, cid: u64) -> TraceMailId {
        TraceMailId { sender: Some(path(sender)), correlation_id: cid }
    }

    fn sent(mail_id: &TraceMailId, root: &TraceMailId, recipient: &str) -> TraceRingEntry {
        sent_parent(mail_id, root, None, recipient)
    }

    fn sent_parent(
        mail_id: &TraceMailId,
        root: &TraceMailId,
        parent: Option<&TraceMailId>,
        recipient: &str,
    ) -> TraceRingEntry {
        TraceRingEntry {
            sequence: 0,
            root: root.clone(),
            event: TraceEvent::Sent {
                mail_id: mail_id.clone(),
                root: root.clone(),
                parent_mail: parent.cloned(),
                sender: mail_id.sender.clone(),
                recipient: Some(path(recipient)),
                kind: KindId(0xAB),
                // iamacoffeepot/aether#1158: fixture construct-start ==
                // flush-begin (eager-path equivalent, construct ≈ 0).
                t_construct_start: Nanos(mail_id.correlation_id),
                t: Nanos(mail_id.correlation_id),
            },
        }
    }

    /// The thread name every `received` fixture event hashes into a
    /// `ThreadId`. [`fixture_resolver`] reverses that one id back to its
    /// display name, standing in for the substrate's reverse-lookup
    /// registry without dragging a native dependency into this crate.
    const FIXTURE_THREAD_NAME: &str = "aether-worker-0";

    /// A pure stand-in for `aether_substrate::runtime::thread_name::resolve`
    /// (ADR-0102): reverses the one fixture `ThreadId` the `received`
    /// events carry, `None` for anything else.
    fn fixture_resolver(id: ThreadId) -> Option<String> {
        (id == ThreadId::from_name(FIXTURE_THREAD_NAME)).then(|| FIXTURE_THREAD_NAME.to_string())
    }

    fn received(mail_id: &TraceMailId, root: &TraceMailId) -> TraceRingEntry {
        TraceRingEntry {
            sequence: 0,
            root: root.clone(),
            event: TraceEvent::Received {
                mail_id: mail_id.clone(),
                t: Nanos(mail_id.correlation_id + 1),
                // iamacoffeepot/aether#1134: fixture deposit just before
                // receive (correlation_id) at depth 0 (warm chain).
                t_enqueue: Nanos(mail_id.correlation_id),
                enqueue_depth: 0,
                thread_id: Some(ThreadId::from_name(FIXTURE_THREAD_NAME)),
            },
        }
    }

    fn finished(mail_id: &TraceMailId, root: &TraceMailId) -> TraceRingEntry {
        TraceRingEntry {
            sequence: 0,
            root: root.clone(),
            event: TraceEvent::Finished { mail_id: mail_id.clone(), t: Nanos(mail_id.correlation_id + 2) },
        }
    }

    fn ok(result: DescribeTreeResult) -> (TraceMailId, u32, Vec<MailNodeWire>) {
        match result {
            DescribeTreeResult::Ok { root, in_flight, mails } => (root, in_flight, mails),
            DescribeTreeResult::Err { not_found } => panic!("expected Ok, got Err {not_found:?}"),
        }
    }

    /// Drive `walk` against a fake substrate of per-actor rings, returning
    /// the actors visited in order.
    fn drive(walk: &mut TreeWalk, rings: &BTreeMap<ErasedActorPath, Vec<TraceRingEntry>>) -> Vec<ErasedActorPath> {
        let mut visited = Vec::new();
        while let Some(actor) = walk.next_actor() {
            walk.absorb(rings.get(&actor).cloned().unwrap_or_default());
            visited.push(actor);
        }
        visited
    }

    /// Stitch is order-independent: feeding `Finished` before its
    /// `Sent` still produces one complete node. Drives the
    /// resolver-injecting [`stitch_with`] so the cold fold reverses the
    /// fixture `ThreadId` back to its display name (ADR-0088 §7).
    #[test]
    fn stitch_folds_events_per_mail_id_regardless_of_order() {
        let root = mid("aether.test.a", 1);
        let entries = vec![finished(&root, &root), received(&root, &root), sent(&root, &root, "aether.test.b")];
        let (got_root, in_flight, mails) = ok(stitch_with(root.clone(), entries, fixture_resolver));
        assert_eq!(got_root, root);
        assert_eq!(in_flight, 0, "node has a Finished");
        assert_eq!(mails.len(), 1);
        let node = &mails[0];
        assert_eq!(node.mail_id, root);
        assert_eq!(node.recipient, Some(path("aether.test.b")));
        assert_eq!(node.t_sent, Nanos(1));
        assert_eq!(node.t_received, Some(Nanos(2)));
        assert_eq!(node.t_finished, Some(Nanos(3)));
        // The cold fold resolved the event's `ThreadId` back to the
        // fixture display name via the injected resolver (ADR-0088 §7).
        assert_eq!(node.thread_name.as_deref(), Some("aether-worker-0"));
    }

    /// The `None`-resolving [`stitch`] leaves `thread_name` empty — the
    /// MCP / wasm path, where no reverse-lookup registry is reachable.
    #[test]
    fn stitch_without_resolver_leaves_thread_name_none() {
        let root = mid("aether.test.a", 1);
        let entries = vec![sent(&root, &root, "aether.test.b"), received(&root, &root), finished(&root, &root)];
        let (_, _, mails) = ok(stitch(root, entries));
        assert_eq!(mails.len(), 1);
        assert_eq!(mails[0].thread_name, None);
    }

    /// A root that produced no `Sent` (never seen / seed ring evicted)
    /// reports `Err { not_found }`, matching the observer.
    #[test]
    fn stitch_missing_root_sent_is_not_found() {
        let root = mid("aether.test.a", 1);
        // Only Received/Finished for the root, no Sent.
        let entries = vec![received(&root, &root), finished(&root, &root)];
        match stitch(root.clone(), entries) {
            DescribeTreeResult::Err { not_found } => assert_eq!(not_found, root),
            ok @ DescribeTreeResult::Ok { .. } => panic!("expected Err, got {ok:?}"),
        }
    }

    /// A node still in flight (Sent, no Finished) is counted.
    #[test]
    fn stitch_counts_unfinished_nodes_as_in_flight() {
        let root = mid("aether.test.a", 1);
        let child = mid("aether.test.b", 1);
        let entries = vec![
            sent(&root, &root, "aether.test.b"),
            received(&root, &root),
            finished(&root, &root),
            sent_parent(&child, &root, Some(&root), "aether.test.c"), // child sent, never finished
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
        let chassis = "aether.chassis";
        let observer = "aether.test.observer";
        let leaf_a = "aether.test.leaf/aether.test.leaf:a";
        let leaf_b = "aether.test.leaf/aether.test.leaf:b";
        let grandchild = "aether.test.grandchild";

        let root = mid(chassis, 1);
        let child_a = mid(observer, 2);
        let child_b = mid(observer, 3);
        let gc = mid(leaf_a, 4);

        // Per-actor rings. Sent in the sender's ring; Received +
        // Finished in the recipient's ring.
        let rings = BTreeMap::from([
            // chassis-host: the root's Sent (chassis -> observer).
            (path(chassis), vec![sent(&root, &root, observer)]),
            // observer: root's Received/Finished + the two children's Sents.
            (
                path(observer),
                vec![
                    received(&root, &root),
                    finished(&root, &root),
                    sent_parent(&child_a, &root, Some(&root), leaf_a),
                    sent_parent(&child_b, &root, Some(&root), leaf_b),
                ],
            ),
            // leaf_a: child_a's Received/Finished + an onward Sent to gc.
            (
                path(leaf_a),
                vec![
                    received(&child_a, &root),
                    finished(&child_a, &root),
                    sent_parent(&gc, &root, Some(&child_a), grandchild),
                ],
            ),
            // leaf_b: child_b's Received/Finished, no onward send.
            (path(leaf_b), vec![received(&child_b, &root), finished(&child_b, &root)]),
            // grandchild: gc's Received/Finished.
            (path(grandchild), vec![received(&gc, &root), finished(&gc, &root)]),
        ]);

        let mut walk = TreeWalk::new(root.clone());
        let visited_order = drive(&mut walk, &rings);
        let (got_root, in_flight, mails) = ok(walk.finish());

        assert_eq!(got_root, root);
        assert_eq!(in_flight, 0, "fully settled tree");
        // Four mails: root + two children + one grandchild.
        assert_eq!(mails.len(), 4, "root, child_a, child_b, grandchild");

        let by_id: BTreeMap<&TraceMailId, &MailNodeWire> = mails.iter().map(|n| (&n.mail_id, n)).collect();
        assert_eq!(by_id[&root].parent, None);
        assert_eq!(by_id[&child_a].parent.as_ref(), Some(&root));
        assert_eq!(by_id[&child_b].parent.as_ref(), Some(&root));
        assert_eq!(by_id[&gc].parent.as_ref(), Some(&child_a));
        // Every node carries Received + Finished — the walk visited
        // every recipient ring.
        assert!(mails.iter().all(|n| n.t_received.is_some() && n.t_finished.is_some()));

        // The walk visited only the five participating actors, never an
        // actor outside the tree.
        assert_eq!(visited_order.len(), 5);
        let visited: BTreeSet<ErasedActorPath> = visited_order.into_iter().collect();
        assert_eq!(visited, rings.into_keys().collect());
    }

    /// A diamond (two parents send to the same recipient) visits the
    /// shared recipient's ring exactly once.
    #[test]
    fn guided_walk_dedups_diamond_recipient() {
        let root = mid("aether.test.seed", 1);
        let child_a = mid("aether.test.middle", 2);
        let child_b = mid("aether.test.middle", 3);
        let shared = "aether.test.shared";

        let rings = BTreeMap::from([
            (path("aether.test.seed"), vec![sent(&root, &root, "aether.test.middle")]),
            (
                path("aether.test.middle"),
                vec![
                    received(&root, &root),
                    finished(&root, &root),
                    sent_parent(&child_a, &root, Some(&root), shared),
                    sent_parent(&child_b, &root, Some(&root), shared),
                ],
            ),
            // The shared recipient receives both children.
            (
                path(shared),
                vec![
                    received(&child_a, &root),
                    finished(&child_a, &root),
                    received(&child_b, &root),
                    finished(&child_b, &root),
                ],
            ),
        ]);

        let mut walk = TreeWalk::new(root);
        let visits = drive(&mut walk, &rings).len();
        let (_, _, mails) = ok(walk.finish());
        assert_eq!(visits, 3, "seed, middle, shared — shared once");
        assert_eq!(mails.len(), 3, "root + two children");
    }

    /// A root whose sender the export could not name seeds nothing, so the
    /// walk queries no ring and reports the root not found rather than
    /// tailing a guessed actor.
    #[test]
    fn a_root_with_no_sender_path_is_not_found() {
        let root = TraceMailId { sender: None, correlation_id: 1 };
        let mut walk = TreeWalk::new(root.clone());
        assert_eq!(walk.next_actor(), None);
        assert!(matches!(walk.finish(), DescribeTreeResult::Err { not_found } if not_found == root));
    }
}
