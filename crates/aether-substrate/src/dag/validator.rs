//! ADR-0047 §3 DAG descriptor validator (iamacoffeepot/aether#975).
//!
//! [`validate`] runs the §3 phases in the order the ADR fixes —
//! version gate → structural integrity → dispatchability → type
//! compatibility — short-circuiting on the first failure so the
//! returned [`DagError`] localises the real problem rather than
//! aggregating every downstream consequence of one upstream mistake.
//! Success yields a [`ValidatedDag`]: the descriptor plus a single
//! topological order the executor (iamacoffeepot/aether#976) dispatches
//! from without re-running Kahn's algorithm.
//!
//! **Re-scoped 2026-05-20 ("handlers promise nothing about replies").**
//! Phase 2 dispatchability reads accept-sets from the queryable
//! [`CapabilityRegistry`]
//! (iamacoffeepot/aether#1037), *not* the routing registry — the
//! routing table carries no accept-sets. Phase 3 type-compat checks
//! only statically-declared output kinds: a `Call`'s output is the
//! `Bundle` meta-type, so an edge out of a `Call` requires the consumer
//! to declare a `Bundle` input at the matching slot. Edges out of a
//! `Source` are *not* type-checked — a source's output kind is whatever
//! the cap replies, which a handler never declares. There is no
//! reply-kind resolution anywhere in this validator.

use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;

use aether_data::canonical::canonical_kind_bytes;
use aether_data::wire;
use aether_data::{Kind, Schema, SchemaType};
use aether_kinds::{Bundle, DagDescriptor, DagError, Edge, Node, NodeId};

use crate::dag::kind_id_for_schema;
use crate::dag::transform_registry::TransformRegistry;
use crate::mail::{CapabilityRegistry, KindId, MailboxEntry, MailboxId, Registry};

/// Env override for the node-count cap (ADR-0047 §3). Default
/// [`DEFAULT_MAX_NODES`].
pub const ENV_MAX_NODES: &str = "AETHER_DAG_MAX_NODES";
/// Env override for the edge-count cap (ADR-0047 §3). Default
/// [`DEFAULT_MAX_EDGES`].
pub const ENV_MAX_EDGES: &str = "AETHER_DAG_MAX_EDGES";
/// Env override for the serialized-descriptor byte cap (ADR-0047 §3).
/// Default [`DEFAULT_MAX_DESCRIPTOR_BYTES`].
pub const ENV_MAX_DESCRIPTOR_BYTES: &str = "AETHER_DAG_MAX_DESCRIPTOR_BYTES";

/// Default ceiling on `nodes.len()` (ADR-0047 §3).
pub const DEFAULT_MAX_NODES: u64 = 256;
/// Default ceiling on `edges.len()` (ADR-0047 §3).
pub const DEFAULT_MAX_EDGES: u64 = 1024;
/// Default ceiling on the wire-serialized descriptor size, in
/// bytes (ADR-0047 §3).
pub const DEFAULT_MAX_DESCRIPTOR_BYTES: u64 = 1024 * 1024;

/// The single descriptor version this substrate implements. A submit
/// naming any other version is rejected at the Phase 0 gate
/// (ADR-0047 §10) before node semantics are touched.
pub const SUPPORTED_VERSION: u16 = 1;

/// A descriptor that has passed every validation phase, carrying the
/// one topological order the executor dispatches from. Kahn's algorithm
/// runs once here (acyclicity check), so the executor never re-detects
/// cycles or re-sorts (ADR-0047 §3 "Doors opened").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedDag {
    /// The validated descriptor, owned so the executor holds it past
    /// the submit-path borrow.
    pub descriptor: DagDescriptor,
    /// Node ids in a topological order (every edge points from an
    /// earlier entry to a later one). Length equals
    /// `descriptor.nodes.len()`.
    pub topo_order: Vec<NodeId>,
}

/// Validate a submitted DAG descriptor (ADR-0047 §3).
///
/// Runs the phases in fixed order, short-circuiting on the first
/// failure: version gate → structural integrity → dispatchability →
/// type compatibility. `mailboxes` is the routing-side table (existence
/// checks via [`Registry::entry`] / reverse-name lookup); `caps` is the
/// queryable capability registry (iamacoffeepot/aether#1037) the
/// dispatchability phase reads accept-sets from.
///
/// On success the returned [`ValidatedDag`] carries the topological
/// order the executor dispatches from directly.
///
/// # Errors
/// Returns the first [`DagError`] a phase produces. See the per-phase
/// helpers for the variant each rule maps to.
pub fn validate(
    descriptor: &DagDescriptor,
    mailboxes: &Registry,
    caps: &CapabilityRegistry,
    transforms: Option<&TransformRegistry>,
) -> Result<ValidatedDag, DagError> {
    check_version(descriptor)?;
    let structure = check_structure(descriptor)?;
    check_dispatchability(descriptor, mailboxes, caps, transforms)?;
    check_edge_types(descriptor, mailboxes, transforms)?;
    Ok(ValidatedDag {
        descriptor: descriptor.clone(),
        topo_order: structure.topo_order,
    })
}

/// Phase 0 — descriptor version (ADR-0047 §10). The cheapest check,
/// run before anything touches `nodes`: an unimplemented version is
/// rejected as a decode-boundary guard rail. There is no dedicated
/// version `DagError` variant in the §3 set, so this reuses
/// `TooLarge { reason }` (the same wire-churn-avoiding reuse §8
/// applies to hub-unsupported submits).
fn check_version(descriptor: &DagDescriptor) -> Result<(), DagError> {
    if descriptor.version != SUPPORTED_VERSION {
        return Err(DagError::TooLarge {
            reason: format!(
                "unsupported descriptor version {} (this substrate implements version {SUPPORTED_VERSION})",
                descriptor.version,
            ),
        });
    }
    Ok(())
}

/// The structural-integrity products Phase 1 hands forward: the node
/// id set (used by later phases to resolve edge endpoints) and the
/// topological order (handed to the executor in [`ValidatedDag`]).
struct Structure {
    topo_order: Vec<NodeId>,
}

/// Phase 1 — structural integrity (ADR-0047 §3). Node-id uniqueness,
/// edge-endpoint existence, source/observer degree constraints,
/// acyclicity (Kahn's algorithm), and the structural caps. Runs after
/// the version gate and before any registry access, so a malformed
/// shape never reaches the (more expensive) dispatchability lookups.
fn check_structure(descriptor: &DagDescriptor) -> Result<Structure, DagError> {
    let caps = Caps::from_env();

    if descriptor.nodes.len() as u64 > caps.nodes {
        return Err(DagError::TooLarge {
            reason: format!(
                "node count {} exceeds cap {}",
                descriptor.nodes.len(),
                caps.nodes,
            ),
        });
    }
    if descriptor.edges.len() as u64 > caps.edges {
        return Err(DagError::TooLarge {
            reason: format!(
                "edge count {} exceeds cap {}",
                descriptor.edges.len(),
                caps.edges,
            ),
        });
    }
    let serialized = wire::to_vec(descriptor)
        .expect("descriptor wire serialization is infallible into a growable Vec");
    if serialized.len() as u64 > caps.descriptor_bytes {
        return Err(DagError::TooLarge {
            reason: format!(
                "descriptor size {} bytes exceeds cap {}",
                serialized.len(),
                caps.descriptor_bytes,
            ),
        });
    }

    // NodeId uniqueness — a BTreeSet collects in id order; a failed
    // insert is the first collision.
    let mut node_ids: BTreeSet<NodeId> = BTreeSet::new();
    for node in &descriptor.nodes {
        if !node_ids.insert(node.id()) {
            return Err(DagError::DuplicateNodeId(node.id()));
        }
    }

    // Edge endpoints must reference declared node ids.
    for edge in &descriptor.edges {
        if !node_ids.contains(&edge.from) {
            return Err(DagError::UnknownNodeId(edge.from));
        }
        if !node_ids.contains(&edge.to) {
            return Err(DagError::UnknownNodeId(edge.to));
        }
    }

    // Per-node incoming / outgoing degree, for the source/observer
    // root-and-terminal constraints. `Call` and `Transform` are
    // mid-graph and carry no degree restriction.
    let mut incoming: HashMap<NodeId, u32> = HashMap::new();
    let mut outgoing: HashMap<NodeId, u32> = HashMap::new();
    for edge in &descriptor.edges {
        *outgoing.entry(edge.from).or_insert(0) += 1;
        *incoming.entry(edge.to).or_insert(0) += 1;
    }
    for node in &descriptor.nodes {
        match node {
            Node::Source { id, .. } => {
                if incoming.get(id).copied().unwrap_or(0) > 0 {
                    return Err(DagError::SourceWithIncomingEdge(*id));
                }
            }
            Node::Observer { id, .. } => {
                if outgoing.get(id).copied().unwrap_or(0) > 0 {
                    return Err(DagError::ObserverWithOutgoingEdge(*id));
                }
            }
            Node::Call { .. } | Node::Transform { .. } => {}
        }
    }

    // Acyclicity via Kahn's algorithm. The residual after the queue
    // drains is the cycle (ADR-0047 §3).
    let topo_order = toposort(&node_ids, &descriptor.edges)?;

    Ok(Structure { topo_order })
}

/// Kahn's algorithm. Returns the full topological order on success; on a
/// cycle, returns `Cycle(residual)` where the residual is every node id
/// that never reached in-degree zero.
fn toposort(node_ids: &BTreeSet<NodeId>, edges: &[Edge]) -> Result<Vec<NodeId>, DagError> {
    let mut indegree: HashMap<NodeId, u32> = node_ids.iter().map(|id| (*id, 0)).collect();
    for edge in edges {
        *indegree.entry(edge.to).or_insert(0) += 1;
    }

    // Seed the queue with every zero-in-degree node, in id order for a
    // deterministic topological order.
    let mut queue: Vec<NodeId> = node_ids
        .iter()
        .copied()
        .filter(|id| indegree.get(id).copied().unwrap_or(0) == 0)
        .collect();
    let mut order: Vec<NodeId> = Vec::with_capacity(node_ids.len());

    while let Some(id) = queue.pop() {
        order.push(id);
        // Drop the in-degree contribution of `id`'s outgoing edges; any
        // successor that hits zero joins the queue.
        let mut freshly_ready: Vec<NodeId> = Vec::new();
        for edge in edges.iter().filter(|e| e.from == id) {
            if let Some(d) = indegree.get_mut(&edge.to) {
                *d = d.saturating_sub(1);
                if *d == 0 {
                    freshly_ready.push(edge.to);
                }
            }
        }
        // Sort the newly-ready ids so the order is reproducible.
        freshly_ready.sort_unstable();
        queue.extend(freshly_ready);
    }

    if order.len() < node_ids.len() {
        // The residual — every node still carrying a positive in-degree
        // — is the cycle. Emit it in id order (the test asserts set
        // membership, not vec order).
        let residual: Vec<NodeId> = node_ids
            .iter()
            .copied()
            .filter(|id| !order.contains(id))
            .collect();
        return Err(DagError::Cycle(residual));
    }

    Ok(order)
}

/// Phase 2 — dispatchability (ADR-0047 §3, ADR-0048 §2). Each effectful
/// node's target mailbox must exist (routing `Registry`) and accept the
/// node's kind (`CapabilityRegistry`). A `Transform` node's
/// `transform_id` must resolve in the native-transform registry
/// (`UnknownTransform` otherwise), and its declared `output_kind_id`
/// must equal the registered transform's manifest output kind
/// (`TransformOutputMismatch` otherwise). A `None` transform registry
/// (a chassis that doesn't host the executor, or a validator test)
/// treats every `Transform` as `UnknownTransform`.
fn check_dispatchability(
    descriptor: &DagDescriptor,
    mailboxes: &Registry,
    caps: &CapabilityRegistry,
    transforms: Option<&TransformRegistry>,
) -> Result<(), DagError> {
    for node in &descriptor.nodes {
        match node {
            Node::Source {
                id,
                mailbox,
                kind_id,
                ..
            } => {
                if !mailbox_exists(mailboxes, *mailbox) {
                    return Err(DagError::UnknownSink(mailbox_label(mailboxes, *mailbox)));
                }
                if !caps.accepts(*mailbox, *kind_id) {
                    return Err(DagError::KindNotAccepted {
                        node: *id,
                        kind_id: *kind_id,
                        mailbox_or_recipient: mailbox_label(mailboxes, *mailbox),
                    });
                }
            }
            Node::Call {
                id,
                recipient,
                kind_id,
            } => {
                // A `Call` is an ordinary cap dispatch that happens
                // mid-graph: the same existence + accept check a source
                // runs (ADR-0047 §3).
                if !mailbox_exists(mailboxes, *recipient) {
                    return Err(DagError::UnknownRecipient(mailbox_label(
                        mailboxes, *recipient,
                    )));
                }
                if !caps.accepts(*recipient, *kind_id) {
                    return Err(DagError::KindNotAccepted {
                        node: *id,
                        kind_id: *kind_id,
                        mailbox_or_recipient: mailbox_label(mailboxes, *recipient),
                    });
                }
            }
            Node::Observer {
                id,
                recipient,
                kind_id,
            } => {
                if !mailbox_exists(mailboxes, *recipient) {
                    return Err(DagError::UnknownRecipient(mailbox_label(
                        mailboxes, *recipient,
                    )));
                }
                // An observer's kind is accepted if the recipient
                // handles it OR carries a `#[fallback]` catch-all.
                if !caps.accepts(*recipient, *kind_id) && !caps.has_fallback(*recipient) {
                    return Err(DagError::KindNotAccepted {
                        node: *id,
                        kind_id: *kind_id,
                        mailbox_or_recipient: mailbox_label(mailboxes, *recipient),
                    });
                }
            }
            Node::Transform {
                id,
                transform_id,
                output_kind_id,
                ..
            } => {
                // Resolve the transform against the native-transform
                // registry. Unknown id (or no registry on this chassis)
                // -> UnknownTransform; output-kind disagreement ->
                // TransformOutputMismatch (ADR-0048 §2).
                let Some(entry) = transforms.and_then(|r| r.lookup(*transform_id)) else {
                    return Err(DagError::UnknownTransform {
                        node: *id,
                        transform_id: *transform_id,
                    });
                };
                if entry.output_kind_id != *output_kind_id {
                    return Err(DagError::TransformOutputMismatch {
                        node: *id,
                        declared: *output_kind_id,
                        manifest: entry.output_kind_id,
                    });
                }
            }
        }
    }
    Ok(())
}

/// Phase 3 — type compatibility on edges (ADR-0047 §3, ADR-0048 §2,
/// re-scoped). Only statically-declared output kinds are checkable.
/// Two producers carry one:
///
/// - a `Call`'s output is always the `Bundle` meta-type — an edge out of
///   a `Call` requires the consumer at `to` to declare a `Bundle` input
///   at the matching `slot`;
/// - a `Transform`'s output is its registered manifest `output_kind_id`
///   — an edge out of a `Transform` requires the consumer's slot to
///   declare that exact kind.
///
/// Edges out of a `Source` are skipped — a source's output kind depends
/// on what the cap replies, which a handler never declares. A
/// `Transform` consumer has no registered input schema (its inputs are
/// raw byte slices keyed by slot), so nothing checks an edge *into* a
/// `Transform` here; the transform's declared input arity is the
/// registry's business at dispatch, not the edge type-check's.
fn check_edge_types(
    descriptor: &DagDescriptor,
    mailboxes: &Registry,
    transforms: Option<&TransformRegistry>,
) -> Result<(), DagError> {
    // Node lookup by id for resolving each edge's producer / consumer.
    let by_id: HashMap<NodeId, &Node> = descriptor.nodes.iter().map(|n| (n.id(), n)).collect();

    for (edge_index, edge) in descriptor.edges.iter().enumerate() {
        let Some(producer) = by_id.get(&edge.from) else {
            // Endpoint existence was already enforced in Phase 1; an
            // unknown producer here would be a logic bug, so skip
            // defensively rather than panic.
            continue;
        };

        // The statically-knowable output of this producer as
        // `(expected_kind_id, expected_schema)`, or `None` for an
        // un-checkable producer (a `Source`, whose output is whatever
        // the cap replies). A `Call` produces a `Bundle`; a `Transform`
        // produces its registered manifest output kind.
        let expected = match producer {
            Node::Call { .. } => Some((Bundle::ID, Bundle::SCHEMA)),
            Node::Transform {
                transform_id,
                output_kind_id,
                ..
            } => {
                let kind = transforms
                    .and_then(|r| r.lookup(*transform_id))
                    .map_or(*output_kind_id, |entry| entry.output_kind_id);
                // The transform's output schema, for the canonical
                // comparison. `None` when the output kind isn't
                // registered — fall back to an id-only check below.
                mailboxes.kind_descriptor(kind).map(|d| (kind, d.schema))
            }
            Node::Source { .. } | Node::Observer { .. } => None,
        };
        let Some((expected_kind, expected_schema)) = expected else {
            continue;
        };

        let Some(consumer) = by_id.get(&edge.to) else {
            continue;
        };
        let consumer_kind = match consumer {
            Node::Observer { kind_id, .. } | Node::Call { kind_id, .. } => *kind_id,
            // A producer cannot feed a `Source` (sources have no incoming
            // edges, enforced in Phase 1) and `Transform` consumers have
            // no registered input schema, so nothing else is checkable
            // here.
            Node::Source { .. } | Node::Transform { .. } => continue,
        };

        // The consumer's slot maps to a `Ref<K>` field of its
        // assembled-request schema. Resolve `K`'s schema; the edge is
        // type-valid iff that `K` canonically matches the producer's
        // output schema.
        let slot_schema = consumer_slot_kind(mailboxes, consumer_kind, edge.slot);
        let matches = slot_schema.as_ref().is_some_and(|s| {
            canonical_kind_bytes("", s) == canonical_kind_bytes("", &expected_schema)
        });
        if !matches {
            let got_kind = slot_schema
                .as_ref()
                .map_or(KindId(0), |s| declared_kind_id(mailboxes, s));
            return Err(DagError::EdgeTypeMismatch {
                edge_index: u32::try_from(edge_index).unwrap_or(u32::MAX),
                expected_kind,
                got_kind,
            });
        }
    }
    Ok(())
}

/// Resolve the inner kind schema a consumer declares at `slot`: walk the
/// consumer's `kind_id` schema for its `Ref<K>` struct fields (in
/// declaration order) and return the `K` schema at index `slot`. Returns
/// `None` if the kind isn't registered, the schema isn't a struct, or
/// the slot index is out of range / the field at that index isn't a
/// `Ref`.
fn consumer_slot_kind(
    mailboxes: &Registry,
    consumer_kind: KindId,
    slot: u32,
) -> Option<SchemaType> {
    let descriptor = mailboxes.kind_descriptor(consumer_kind)?;
    let SchemaType::Struct { fields, .. } = &descriptor.schema else {
        return None;
    };
    // Slots index the consumer's `Ref<K>` fields in declaration order.
    fields
        .iter()
        .filter_map(|f| match &f.ty {
            SchemaType::Ref(cell) => Some((**cell).clone()),
            _ => None,
        })
        .nth(slot as usize)
}

/// Best-effort kind-id for a declared input schema, for the `got_kind`
/// field of an `EdgeTypeMismatch` diagnostic. Searches the registered
/// kind vocabulary for a descriptor whose schema canonically matches
/// `schema` and returns its id; falls back to `KindId(0)` when no
/// registered kind matches (the schema names a kind this substrate
/// doesn't know — still a mismatch, just without a precise id to name).
fn declared_kind_id(mailboxes: &Registry, schema: &SchemaType) -> KindId {
    kind_id_for_schema(mailboxes, schema)
}

/// Does `id` resolve to a live (non-dropped) mailbox in the routing
/// registry? An `entry` of `None` is an unregistered id; the
/// `Registry::entry` accessor already filters dropped slots out of
/// `lookup`, but returns the `Dropped` entry verbatim — so a dropped
/// mailbox is treated as absent here.
fn mailbox_exists(mailboxes: &Registry, id: MailboxId) -> bool {
    matches!(
        mailboxes.entry(id),
        Some(MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_))
    )
}

/// Human-readable label for a mailbox id, for the `UnknownSink` /
/// `UnknownRecipient` / `KindNotAccepted` diagnostics. Uses the
/// registry's reverse name map when the id is known; falls back to the
/// raw tagged id otherwise (an unknown id by definition has no name).
fn mailbox_label(mailboxes: &Registry, id: MailboxId) -> String {
    mailboxes
        .mailbox_name(id)
        .unwrap_or_else(|| format!("{id:?}"))
}

/// Resolved structural caps (ADR-0047 §3), read once per validation from
/// the environment with the documented defaults.
///
/// Resolution runs through confique (ADR-0090): the private `CapsLayer`
/// declares each cap's `AETHER_DAG_MAX_*` env key + default in one place.
/// Behaviour is byte-identical to the prior hand-rolled `parse_env_u64`
/// reader — an unparseable value still falls back to its default. The
/// hard-error stance (ADR-0090 §4) lands with the chassis-env validation
/// pass.
struct Caps {
    nodes: u64,
    edges: u64,
    descriptor_bytes: u64,
}

impl Caps {
    fn from_env() -> Self {
        use confique::Config as _;

        // Every field has a literal default and a total parser, so the
        // layer always resolves; a failure would be a malformed default
        // literal (caught by `caps_layer_defaults_match`).
        let layer = CapsLayer::builder()
            .env()
            .load()
            .expect("CapsLayer defaults are well-formed");
        Self {
            nodes: layer.nodes,
            edges: layer.edges,
            descriptor_bytes: layer.descriptor_bytes,
        }
    }
}

/// Env-shaped confique layer behind the structural `Caps` (ADR-0090).
/// Kept private — the consumed shape stays `Caps`.
#[derive(confique::Config)]
struct CapsLayer {
    /// Max node count. Literal default mirrors [`DEFAULT_MAX_NODES`]
    /// (256); `caps_layer_defaults_match` guards the match.
    #[config(env = "AETHER_DAG_MAX_NODES", parse_env = parse_max_nodes, default = 256u64)]
    nodes: u64,
    /// Max edge count. Literal default mirrors [`DEFAULT_MAX_EDGES`] (1024).
    #[config(env = "AETHER_DAG_MAX_EDGES", parse_env = parse_max_edges, default = 1024u64)]
    edges: u64,
    /// Max descriptor bytes. Literal default mirrors
    /// [`DEFAULT_MAX_DESCRIPTOR_BYTES`] (1 MiB).
    #[config(
        env = "AETHER_DAG_MAX_DESCRIPTOR_BYTES",
        parse_env = parse_max_descriptor_bytes,
        default = 1_048_576u64
    )]
    descriptor_bytes: u64,
}

// confique's `parse_env` contract is `fn(&str) -> Result<T, impl Error>`,
// so these total helpers carry a `Result` they never fill with `Err` — an
// unparseable value folds back to the same default as the prior
// `parse_env_u64` (the warn-on-malformed log is dropped, the disposition is
// byte-identical). The strict (erroring) variant lands with the ADR-0090 §4
// validation pass; hence the per-fn `unnecessary_wraps` allow.

/// Parse the node cap; unparseable falls back to [`DEFAULT_MAX_NODES`].
#[allow(clippy::unnecessary_wraps)]
fn parse_max_nodes(s: &str) -> Result<u64, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_MAX_NODES))
}

/// Parse the edge cap; unparseable falls back to [`DEFAULT_MAX_EDGES`].
#[allow(clippy::unnecessary_wraps)]
fn parse_max_edges(s: &str) -> Result<u64, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_MAX_EDGES))
}

/// Parse the descriptor-bytes cap; unparseable falls back to
/// [`DEFAULT_MAX_DESCRIPTOR_BYTES`].
#[allow(clippy::unnecessary_wraps)]
fn parse_max_descriptor_bytes(s: &str) -> Result<u64, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_MAX_DESCRIPTOR_BYTES))
}

#[cfg(test)]
mod tests {
    use super::{
        CapsLayer, DEFAULT_MAX_DESCRIPTOR_BYTES, DEFAULT_MAX_EDGES, DEFAULT_MAX_NODES,
        parse_max_descriptor_bytes, parse_max_edges, parse_max_nodes,
    };

    // ADR-0090: the confique migration is byte-identical to the prior
    // hand-rolled `parse_env_u64` reader. These exercise resolution
    // without touching process env (issue 464) — the parsers are pure,
    // and the defaults check loads the layer with no `.env()` source.

    #[test]
    fn parse_caps_soft_fall_back_to_defaults() {
        assert_eq!(parse_max_nodes("8").unwrap(), 8);
        assert_eq!(parse_max_nodes("nope").unwrap(), DEFAULT_MAX_NODES);
        assert_eq!(parse_max_edges("16").unwrap(), 16);
        assert_eq!(parse_max_edges("nope").unwrap(), DEFAULT_MAX_EDGES);
        assert_eq!(parse_max_descriptor_bytes("2048").unwrap(), 2048);
        assert_eq!(
            parse_max_descriptor_bytes("nope").unwrap(),
            DEFAULT_MAX_DESCRIPTOR_BYTES
        );
    }

    #[test]
    fn caps_layer_defaults_match() {
        use confique::Config as _;
        // No `.env()` source: literal defaults only, env-free. Guards the
        // layer defaults against the named consts.
        let layer = CapsLayer::builder().load().expect("defaults load");
        assert_eq!(layer.nodes, DEFAULT_MAX_NODES);
        assert_eq!(layer.edges, DEFAULT_MAX_EDGES);
        assert_eq!(layer.descriptor_bytes, DEFAULT_MAX_DESCRIPTOR_BYTES);
    }
}
