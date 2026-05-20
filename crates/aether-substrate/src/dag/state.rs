//! ADR-0047 §4 per-DAG executor state (iamacoffeepot/aether#976).
//!
//! [`DagState`] is the executor's bookkeeping for one submitted,
//! validated DAG: the topologically-ordered node list, the per-node
//! handle-id assignment minted at submit time, the per-`Call` input
//! gating counters, the resolved-node set that drives `status`, and
//! the lifecycle status itself. One [`DagState`] is created per
//! `aether.dag.submit` and lives in the executor cap's single-threaded
//! actor state until it's reaped (ADR-0047 §7).
//!
//! The executor ([`super::executor`]) drives transitions; this module
//! holds only the data + the small derived queries (`status_result`,
//! `is_terminal`) that both the executor and the `aether.dag.status`
//! handler read.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use aether_data::{DagId, HandleId};
use aether_kinds::{DagDescriptor, Node, NodeHandle, NodeId, NodeState, NodeStatus, StatusResult};

use crate::dag::validator::ValidatedDag;

/// Lifecycle status of one submitted DAG (ADR-0047 §6). `Cancelled`
/// is an internal terminal state distinct from `Failed`; both surface
/// to the wire as `StatusResult::Failed` (cancellation reports
/// `error == "cancelled"` per ADR-0047 §5), but the executor keeps
/// them apart so reaping can apply the right retention window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DagStatus {
    /// Submitted + validated; no source has resolved yet.
    Pending,
    /// At least one node resolved; some haven't.
    Running,
    /// Every node resolved.
    Complete,
    /// A node failed (a source/`Call` timeout, a malformed reply, or a
    /// dispatch error). Carries the failing node + a human-readable
    /// reason.
    Failed { node_id: NodeId, error: String },
    /// The DAG was cancelled (ADR-0047 §5). Surfaces to the wire as
    /// `Failed { error: "cancelled" }`.
    Cancelled,
}

impl DagStatus {
    /// `true` once the DAG can no longer make progress — `Complete`,
    /// `Failed`, or `Cancelled`. The reaping tick (ADR-0047 §7)
    /// sweeps terminal DAGs whose `completed_at` is past retention.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Complete | Self::Failed { .. } | Self::Cancelled)
    }
}

/// One in-flight `Call` node's accumulation buffer (ADR-0047 §4 step
/// 3). Replies correlated to the call's dispatch append here in
/// arrival order; the buffer drains into the call's `Bundle` handle on
/// `Settled { call_root }`.
#[derive(Debug, Default)]
pub struct CallBuffer {
    /// The node this buffer belongs to.
    pub node_id: NodeId,
    /// Ordered `(KindId, payload_bytes)` elements — one per correlated
    /// reply, in the cap's emission order (FIFO per sender).
    pub elements: Vec<(aether_data::KindId, Vec<u8>)>,
}

/// Per-DAG executor state (ADR-0047 §4).
pub struct DagState {
    /// The substrate-minted id this DAG is addressed by.
    pub dag_id: DagId,
    /// The validated descriptor (owned past the submit-path borrow).
    pub descriptor: DagDescriptor,
    /// Topological order the executor dispatches from (Kahn's order
    /// from the validator — never re-sorted here).
    pub topo_order: Vec<NodeId>,
    /// Handle id assigned to every node at submit time. Sources / calls
    /// resolve to a stored value / `Bundle`; observers' entries are
    /// allocated for uniformity but never resolved (observers consume).
    pub handles: HashMap<NodeId, HandleId>,
    /// Remaining unresolved input edges per `Call` node. Decremented as
    /// upstream handles resolve; the call dispatches at zero (ADR-0047
    /// §4). Observers gate through the parking table instead, so they
    /// don't appear here.
    pub pending_inputs: HashMap<NodeId, u32>,
    /// Nodes whose output handle has resolved (sources / calls) or
    /// which have been dispatched (observers). Drives the `status`
    /// progress list + the `Complete` transition.
    pub resolved: HashSet<NodeId>,
    /// Per-`call_root` accumulation buffers, keyed by the correlation
    /// id of the call's dispatch (the `call_root.correlation_id`). A
    /// reply correlated to one of these appends; `Settled` drains it.
    pub call_buffers: HashMap<u64, CallBuffer>,
    /// Lifecycle status.
    pub status: DagStatus,
    /// When the DAG was submitted (for diagnostics + reaping age).
    pub submitted_at: Instant,
    /// When the DAG reached a terminal status (for reaping retention).
    pub completed_at: Option<Instant>,
}

impl DagState {
    /// Build the per-DAG state from a validated descriptor, minting the
    /// per-node handle assignment + per-`Call` input counters. The
    /// caller supplies the `dag_id` and the handle-id allocator output
    /// (one per node, in `topo_order`) since handle allocation goes
    /// through the shared [`crate::handle_store::HandleStore`], which
    /// the executor owns the handle to.
    #[must_use]
    pub fn new(dag_id: DagId, validated: ValidatedDag, handles: HashMap<NodeId, HandleId>) -> Self {
        let ValidatedDag {
            descriptor,
            topo_order,
        } = validated;

        // Per-`Call` input-edge counts gate dispatch (ADR-0047 §4). Seed
        // every `Call` at 0 so a no-input call dispatches immediately,
        // then bump one per incoming edge.
        let mut pending_inputs: HashMap<NodeId, u32> = HashMap::new();
        let call_ids: HashSet<NodeId> = descriptor
            .nodes
            .iter()
            .filter(|n| matches!(n, Node::Call { .. }))
            .map(Node::id)
            .collect();
        for id in &call_ids {
            pending_inputs.insert(*id, 0);
        }
        for edge in &descriptor.edges {
            if call_ids.contains(&edge.to) {
                *pending_inputs.entry(edge.to).or_insert(0) += 1;
            }
        }

        Self {
            dag_id,
            descriptor,
            topo_order,
            handles,
            pending_inputs,
            resolved: HashSet::new(),
            call_buffers: HashMap::new(),
            status: DagStatus::Pending,
            submitted_at: Instant::now(),
            completed_at: None,
        }
    }

    /// The submit-reply output-handle list: every node's `(node_id,
    /// handle_id)` pair, in descriptor node order (ADR-0047 §1).
    #[must_use]
    pub fn output_handles(&self) -> Vec<NodeHandle> {
        self.descriptor
            .nodes
            .iter()
            .filter_map(|n| {
                self.handles.get(&n.id()).map(|h| NodeHandle {
                    node_id: n.id(),
                    handle_id: *h,
                })
            })
            .collect()
    }

    /// Mark `node` resolved and advance the lifecycle status. Transitions
    /// `Pending`/`Running` toward `Complete` once every node is in the
    /// resolved set; a terminal status (already `Complete` / `Failed` /
    /// `Cancelled`) is sticky and never regresses.
    pub fn mark_resolved(&mut self, node: NodeId) {
        if self.status.is_terminal() {
            return;
        }
        self.resolved.insert(node);
        if self.resolved.len() >= self.descriptor.nodes.len() {
            self.status = DagStatus::Complete;
            self.completed_at = Some(Instant::now());
        } else {
            self.status = DagStatus::Running;
        }
    }

    /// Move the DAG to `Failed`, stamping `completed_at`. Sticky against
    /// a prior terminal status.
    pub fn mark_failed(&mut self, node_id: NodeId, error: String) {
        if self.status.is_terminal() {
            return;
        }
        self.status = DagStatus::Failed { node_id, error };
        self.completed_at = Some(Instant::now());
    }

    /// Move the DAG to `Cancelled`, stamping `completed_at`. Sticky
    /// against a prior terminal status.
    pub fn mark_cancelled(&mut self) {
        if self.status.is_terminal() {
            return;
        }
        self.status = DagStatus::Cancelled;
        self.completed_at = Some(Instant::now());
    }

    /// Project the internal status onto the wire `StatusResult`
    /// (ADR-0047 §1/§6). `Cancelled` surfaces as `Failed { error:
    /// "cancelled" }`; `Running` carries the per-node progress list.
    #[must_use]
    pub fn status_result(&self) -> StatusResult {
        match &self.status {
            DagStatus::Pending => StatusResult::Pending,
            DagStatus::Running => StatusResult::Running {
                progress: self.progress(),
            },
            DagStatus::Complete => StatusResult::Complete {
                outputs: self.output_handles(),
            },
            DagStatus::Failed { node_id, error } => StatusResult::Failed {
                node_id: *node_id,
                error: error.clone(),
            },
            DagStatus::Cancelled => StatusResult::Failed {
                node_id: NodeId(0),
                error: "cancelled".to_owned(),
            },
        }
    }

    /// The per-node progress list for a `Running` status — one
    /// [`NodeStatus`] per descriptor node, in descriptor order.
    fn progress(&self) -> Vec<NodeStatus> {
        self.descriptor
            .nodes
            .iter()
            .map(|n| {
                let state = if self.resolved.contains(&n.id()) {
                    NodeState::Resolved
                } else {
                    NodeState::Pending
                };
                NodeStatus {
                    node_id: n.id(),
                    state,
                }
            })
            .collect()
    }
}
