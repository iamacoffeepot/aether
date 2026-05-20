//! ADR-0047 computation-DAG wire vocabulary: the descriptor
//! (`DagDescriptor` + `Node`/`Edge`), the `Bundle` meta-type, the
//! three request kinds (`aether.dag.submit` / `cancel` / `status`),
//! their reply kinds, and the structured `DagError` set.
//!
//! These kinds are postcard-shaped — `Vec<Node>` / `Vec<Edge>` make
//! the descriptor non-cast — and register in the substrate descriptor
//! inventory through `#[derive(Kind)]` exactly like the other reply
//! enums (`LoadResult`, `ReadResult`).
//!
//! **Pair-element shape.** ADR-0047 §1/§2 describe `Bundle`'s elements
//! and the submit/status output handles as ordered `(X, Y)` pairs. The
//! schema vocabulary (`aether_data::SchemaType`) has no tuple arm, so
//! the wire encodes each pair as a named two-field struct
//! ([`BundleElement`], [`NodeHandle`]) — the standard idiom for
//! structured collections in a reply enum (cf. `ListenerInfo`,
//! `EngineDescriptor`). A `Vec` of two-field records is the ordered
//! pair list the ADR specifies.

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::{DagId, HandleId, KindId, MailboxId, TransformId};
use serde::{Deserialize, Serialize};

/// Descriptor-local node identifier (ADR-0047 §2). A `u32` index
/// assigned by the submitter, unique within one `DagDescriptor` —
/// **not** a globally-unique handle id. Two DAGs submitted in
/// parallel can both carry a `NodeId(0)` without collision because
/// the namespaces don't cross.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    aether_data::Schema,
)]
pub struct NodeId(pub u32);

/// One node in a computation DAG (ADR-0047 §2). Variant ordering is
/// wire-stable and additive-only: `Source` (root, effectful),
/// `Transform` (mid-graph, pure — Phase 3 dispatch), `Call`
/// (mid-graph, effectful — its output is a self-describing
/// [`Bundle`]), `Observer` (terminal, effectful).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Schema)]
pub enum Node {
    /// Root node: dispatches `payload` to `mailbox` as the kind
    /// `kind_id` and feeds the reply downstream. `payload` is opaque
    /// bytes the substrate forwards unparsed (ADR-0047 §2), keeping
    /// the descriptor capability-decoupled. Sources have no incoming
    /// edges.
    Source {
        id: NodeId,
        mailbox: MailboxId,
        kind_id: KindId,
        payload: Vec<u8>,
    },
    /// Mid-graph pure transform (ADR-0048 native-transform shape).
    /// Identity is the global `transform_id`; `output_kind_id`
    /// declares what the node produces. Dispatch lights up in Phase 3
    /// (iamacoffeepot/aether#976) — Phase 2 reserves the wire shape.
    Transform {
        id: NodeId,
        transform_id: TransformId,
        output_kind_id: KindId,
    },
    /// Mid-graph effectful cap dispatch (ADR-0047 rev 2026-05-20). Its
    /// request is assembled from incoming edges (the observer-side
    /// slot-fill path), dispatched to `recipient`, and its correlated
    /// replies accumulate into a [`Bundle`] that closes on settlement.
    /// Replies are heterogeneous and self-describing, so a `Call`
    /// declares **no** `output_kind_id` — its output handle is typed
    /// as a `Bundle`.
    Call {
        id: NodeId,
        recipient: MailboxId,
        kind_id: KindId,
    },
    /// Terminal node: assembles `kind_id` from its incoming edges and
    /// dispatches it to `recipient` (the slot-fill consumer). Observers
    /// have no outgoing edges.
    Observer {
        id: NodeId,
        recipient: MailboxId,
        kind_id: KindId,
    },
}

impl Node {
    /// The descriptor-local id of this node, regardless of variant.
    /// The validator (iamacoffeepot/aether#975) leans on this for the
    /// uniqueness + edge-endpoint checks.
    #[must_use]
    pub const fn id(&self) -> NodeId {
        match self {
            Self::Source { id, .. }
            | Self::Transform { id, .. }
            | Self::Call { id, .. }
            | Self::Observer { id, .. } => *id,
        }
    }
}

/// One directed edge in a computation DAG (ADR-0047 §2). `from` is the
/// producing node, `to` the consuming node, and `slot` the
/// consumer-side input index — for an `Observer` / `Call` it selects
/// the `Ref<K>` field of the consumer's assembled-request schema the
/// upstream output fills.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, aether_data::Schema,
)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub slot: u32,
}

/// A computation DAG (ADR-0047 §2). `version` is the first field — a
/// decode-boundary guard rail (ADR-0047 §10): a substrate handed a
/// version it doesn't implement rejects the submit with a clear
/// [`DagError`] rather than mis-decoding an unknown node shape. The
/// version check itself lives in the validator
/// (iamacoffeepot/aether#975); this kind only carries the field.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.dag.descriptor")]
pub struct DagDescriptor {
    pub version: u16,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// One self-describing element of a [`Bundle`] — a single correlated
/// reply, tagged with its own `kind_id`. The `payload` is opaque
/// postcard bytes the element's own kind decodes downstream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Schema)]
pub struct BundleElement {
    pub kind_id: KindId,
    pub payload: Vec<u8>,
}

/// First-class meta-type whose value is an ordered list of
/// self-describing reply elements (ADR-0047 §2, rev 2026-05-20). A
/// [`Node::Call`]'s output handle resolves to a `Bundle`: a
/// single-reply cap yields a 1-element bundle, zero replies an empty
/// bundle, N replies an N-element bundle. The heterogeneity lives in
/// the tagged `elements`, not in `Bundle`'s own fixed schema, so a
/// downstream `Transform` / `Observer` simply declares a `Bundle`
/// input and dispatches on each element's `kind_id` in its body.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.dag.bundle")]
pub struct Bundle {
    pub elements: Vec<BundleElement>,
}

/// One terminal node's assigned output handle, returned in a submit /
/// status reply (ADR-0047 §1). Handle ids are allocated at submit time
/// so downstream `Ref::Handle` slots can be substituted before the
/// values resolve.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, aether_data::Schema,
)]
pub struct NodeHandle {
    pub node_id: NodeId,
    pub handle_id: HandleId,
}

/// `aether.dag.submit` — submit a computation DAG for validation and
/// execution. The substrate replies synchronously with
/// [`SubmitResult`] as soon as validation completes, before any source
/// dispatches.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.dag.submit")]
pub struct Submit {
    pub descriptor: DagDescriptor,
}

/// `aether.dag.cancel` — cancel an in-flight DAG by its substrate-minted
/// [`DagId`]. Reply: [`CancelResult`].
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.dag.cancel")]
pub struct Cancel {
    pub dag_id: DagId,
}

/// `aether.dag.status` — query an in-flight or completed DAG's
/// progress. Reply: [`StatusResult`].
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.dag.status")]
pub struct Status {
    pub dag_id: DagId,
}

/// Reply to [`Submit`] (ADR-0047 §1). `Ok` carries the minted
/// [`DagId`] plus the full per-node output-handle list (handle ids
/// assigned to terminal nodes) so a caller can hand them to downstream
/// consumers immediately, even though the values aren't resolved yet.
/// `Err` carries a structured [`DagError`] from the validator.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.dag.submit_result")]
pub enum SubmitResult {
    Ok {
        dag_id: DagId,
        output_handles: Vec<NodeHandle>,
    },
    Err {
        error: DagError,
    },
}

/// Reply to [`Cancel`] (ADR-0047 §1). `Ok.cancelled` is `false` when
/// the DAG had already completed (nothing to cancel); `Err` carries a
/// human-readable reason (e.g. unknown `dag_id`).
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.dag.cancel_result")]
pub enum CancelResult {
    Ok { cancelled: bool },
    Err { error: String },
}

/// Per-node execution state in a [`StatusResult::Running`] progress
/// list (ADR-0047 §6).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Schema)]
pub enum NodeState {
    Pending,
    Resolved,
    Failed,
}

/// One node's status in a [`StatusResult::Running`] progress list
/// (ADR-0047 §1/§6).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Schema)]
pub struct NodeStatus {
    pub node_id: NodeId,
    pub state: NodeState,
}

/// Reply to [`Status`] (ADR-0047 §1/§6). `Pending` — submitted, no
/// source has resolved yet. `Running` — at least one node resolved,
/// some haven't. `Complete` — all terminal handles resolved, with the
/// same `(node, handle)` pairs the submit reply returned. `Failed` —
/// a node failed, naming the node and a human-readable reason.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.dag.status_result")]
pub enum StatusResult {
    Pending,
    Running { progress: Vec<NodeStatus> },
    Complete { outputs: Vec<NodeHandle> },
    Failed { node_id: NodeId, error: String },
}

/// Structured validation / execution failure for a submitted DAG
/// (ADR-0047 §3). The §3 code block is authoritative for the variant
/// set; the validator (iamacoffeepot/aether#975) maps each rule
/// violation to one variant and short-circuits on the first failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Schema)]
pub enum DagError {
    /// Two nodes share the same [`NodeId`].
    DuplicateNodeId(NodeId),
    /// An edge endpoint references a node id not present in `nodes`.
    UnknownNodeId(NodeId),
    /// The graph is not acyclic — the residual after Kahn's algorithm.
    Cycle(Vec<NodeId>),
    /// A `Source` node carries an incoming edge.
    SourceWithIncomingEdge(NodeId),
    /// An `Observer` node carries an outgoing edge.
    ObserverWithOutgoingEdge(NodeId),
    /// A `Source.mailbox` doesn't resolve to a live mailbox.
    UnknownSink(String),
    /// A `Call.recipient` / `Observer.recipient` doesn't resolve to a
    /// live mailbox.
    UnknownRecipient(String),
    /// A dispatch target's accept set doesn't include the named kind.
    KindNotAccepted {
        node: NodeId,
        kind_id: KindId,
        mailbox_or_recipient: String,
    },
    /// A `Transform.transform_id` doesn't resolve to a registered
    /// native transform.
    UnknownTransform {
        node: NodeId,
        transform_id: TransformId,
    },
    /// A `Transform`'s declared `output_kind_id` disagrees with the
    /// registered transform's manifest output kind.
    TransformOutputMismatch {
        node: NodeId,
        declared: KindId,
        manifest: KindId,
    },
    /// An edge wires an upstream output kind to a downstream input
    /// slot that expects a different kind.
    EdgeTypeMismatch {
        edge_index: u32,
        expected_kind: KindId,
        got_kind: KindId,
    },
    /// The descriptor exceeds a structural cap (node / edge count or
    /// serialized byte size), or names an unsupported descriptor
    /// version (ADR-0047 §3/§8 wire-churn-avoiding reuse).
    TooLarge { reason: String },
}
