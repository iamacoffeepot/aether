//! Proof-fact addressing, recording, attribution, the batch gate, daily
//! sweeps, the roll barrier (ADR-0200 §The fact, §"Attribution through
//! the ledger", §"The batch gate", §"The gate ladder"), and member-Verify
//! declared-surface containment.
//!
//! A proof fact is addressed by `(closure_key, test, result, host_class)`.
//! [`closure_key`] hashes a package's git-addressed dependency closure;
//! [`HostClass`] is the opaque host the coordinator supplies; [`discriminate`]
//! is the only constructor of facts the ledger will store;
//! [`attribute_gate_failure`] is the failure-attribution path member verify
//! and the aggregate gate share; [`run_batch_gate`] composes disjoint-surface
//! members into one prove; [`run_sweep`] converts unknown facts on idle
//! prover time and taints a closure on red; [`decide_roll`] holds the day
//! on main until the coverage map is fully green.
//! [`apply_containment`] fails a member Verify whose candidate edited a path
//! no declared-surface glob covers.

#[cfg(feature = "runtime")]
mod attribution;
#[cfg(feature = "runtime")]
mod batch;
mod closure;
mod containment;
mod facts;
#[cfg(feature = "runtime")]
mod roll;
#[cfg(feature = "runtime")]
mod sweep;

#[cfg(feature = "runtime")]
pub use attribution::{
    Attribution, AttributionError, AttributionRequest, BaseProbe, BaseRepairWorkpiece, RepairBoard, TaintSet,
    attribute_gate_failure, consult_proof_fact,
};
#[cfg(feature = "runtime")]
pub use batch::{
    Accumulation, BatchBisect, BatchComposer, BatchContext, BatchFailure, BatchFailureHooks, BatchGate, BatchMember,
    BatchReport, BatchRestart, GateOutcome, MemberFate, RunningGate, SurfaceOverlap, decide_accumulation,
    run_batch_gate,
};
pub use closure::{ClosureKey, ClosureKeyError, closure_key};
pub use containment::{apply_containment, changed_paths, out_of_surface, path_in_surface};
#[cfg(feature = "runtime")]
pub use facts::record_proof_facts;
pub use facts::{DiscriminatedFact, DiscriminatedFacts, ProofResult, ProofSource, RunnerReport, discriminate};
#[cfg(feature = "runtime")]
pub use roll::{
    CoverageEntry, CoverageMap, CoverageStatus, MissingCoverage, RollDecision, RollHold, TestClosure, coverage_map,
    decide_roll,
};
#[cfg(feature = "runtime")]
pub use sweep::{
    BloomDisposition, Land, LandProbe, SweepContext, SweepDecision, SweepOutcome, UnknownFact, bisect_land_order,
    bloom_disposition, decide_sweep, repair_landed, run_sweep, unknowns,
};

/// The host class a proof fact is keyed on (ADR-0200 integrity rule 2).
///
/// Opaque on purpose: the coordinator supplies the string (fleet host vs GPU
/// host). This type does not detect or classify hosts.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct HostClass(String);

impl HostClass {
    /// Wrap a coordinator-supplied host class string.
    #[must_use]
    pub fn new(class: impl Into<String>) -> Self {
        Self(class.into())
    }

    /// The coordinator-supplied string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests;
