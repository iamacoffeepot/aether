//! Typed field records of a workpiece (ADR-0208).
//!
//! A workpiece is a set of [`WorkpieceFact`] records, not one struct. The wire
//! encoding freezes a struct at its digest: fields are positional with no names
//! and no count, and decode is strict in both directions, so appending a field
//! makes every previously persisted byte string undecodable. An enum variant is
//! a `u32` selector; appending one leaves every earlier discriminant where it
//! was.
//!
//! [`WorkpieceFact`] is three fields permanently. [`FieldKind`] is the growth
//! axis. [`WorkpieceFields`] is the emission-ordered projection.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest};
use crate::ids::WorkpieceId;

/// One typed field record of a workpiece (ADR-0208).
///
/// Three fields permanently. The struct is frozen at its digest; every axis of
/// variation lives on [`FieldKind`]. Appending a field here would make every
/// previously persisted byte string undecodable — the defect
/// [`ScopeRevision`](super::ScopeRevision) already paid for.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct WorkpieceFact {
    /// The workpiece this record belongs to.
    pub workpiece: WorkpieceId,
    /// Which field this record is.
    pub kind: FieldKind,
    /// The artifact that holds this field's content.
    pub detail: Digest,
}

impl ContentAddressed for WorkpieceFact {
    const DOMAIN: &'static str = "aether.bloomery.workpiece_fact";
}

/// The class of a [`WorkpieceFact`] record (ADR-0208).
///
/// This is the growth axis of the workpiece vocabulary: a new field is a new
/// variant, appended so every earlier discriminant stays where it was. Removing
/// a field is ceasing to produce it; persisted facts of the retired kind still
/// decode.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum FieldKind {
    /// The problem statement. Its `detail` names the prose artifact that states
    /// what is wrong.
    Problem,
    /// Evidence grounding the problem statement — not attestation over a digest
    /// (that is [`Evidence`](super::Evidence)). Its `detail` names the grounding
    /// artifact.
    Evidence,
    /// What success looks like. Its `detail` names the success-criteria artifact.
    Success,
    /// The chosen approach. Its `detail` names the design-notes artifact for the
    /// selected path.
    Approach,
    /// An option considered and rejected. Its `detail` names the rejected option's
    /// artifact.
    RejectedOption,
    /// One step of the implementation plan. Its `detail` names the step artifact.
    PlanStep,
    /// An acceptance criterion. Its `detail` names the criterion artifact.
    Acceptance,
    /// One declared-surface glob. Its `detail` names the glob artifact.
    DeclaredSurface,
    /// Inverse-dependency search results for a symbol a plan step named. Its
    /// `detail` names the search-result artifact. The builder fills this field;
    /// no lane authors it.
    InverseSearch,
    /// A declared edge to another workpiece. Its `detail` names the dependency
    /// artifact.
    Edge,
    /// A routing property of the work — remaining judgement, risk class — mapped
    /// to a seat at dispatch. Not a home for an authored size, and not a model
    /// name: seats change, and a frozen artifact must not pin one.
    RoutingHint,
    /// An ADR this workpiece implements. Its `detail` is that ADR's digest.
    /// Appended past [`Self::RoutingHint`] so the prior kinds' wire discriminants
    /// are unchanged.
    Implements,
}

/// The emission-ordered field records of one workpiece (ADR-0208).
///
/// A linear-scan projection: [`carries`](Self::carries) reports presence,
/// [`records`](Self::records) streams the matching facts. `false` from
/// [`carries`](Self::carries) is **absent**; `true` includes present-and-empty
/// (a kind emitted with a zero-valued detail). The distinction does not resolve
/// any detail artifact.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct WorkpieceFields {
    /// The workpiece these records belong to.
    pub workpiece: WorkpieceId,
    /// Field records in emission order.
    pub facts: Vec<WorkpieceFact>,
}

impl WorkpieceFields {
    /// Whether this workpiece carries at least one record of `kind`.
    ///
    /// `false` is **absent** — no record of this kind was emitted, which is
    /// distinct from present-and-empty. `true` includes a kind emitted with a
    /// zero-valued detail. Decidable without resolving any detail artifact.
    #[must_use]
    pub fn carries(&self, kind: FieldKind) -> bool {
        self.facts.iter().any(|fact| fact.kind == kind)
    }

    /// The records of `kind` in emission order.
    ///
    /// Empty when [`carries`](Self::carries) returns `false`. A kind emitted
    /// three times yields three items, in the order they were recorded.
    pub fn records(&self, kind: FieldKind) -> impl Iterator<Item = &WorkpieceFact> {
        self.facts.iter().filter(move |fact| fact.kind == kind)
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use aether_data::wire::to_vec;

    use super::{FieldKind, WorkpieceFact, WorkpieceFields};
    use crate::digest::{Digest, digest_of};
    use crate::ids::WorkpieceId;

    fn workpiece() -> WorkpieceId {
        WorkpieceId(String::from("issue-5298"))
    }

    fn fact(kind: FieldKind, detail: [u8; 32]) -> WorkpieceFact {
        WorkpieceFact { workpiece: workpiece(), kind, detail: Digest::from_bytes(detail) }
    }

    // Tripwire: content address of the WorkpieceFact fixture under
    // `aether.bloomery.workpiece_fact`. This golden is not repinnable. A drift
    // means the frozen struct grew a field or the domain tag moved, and either
    // is a new type.
    const GOLDEN_WORKPIECE_FACT_DIGEST: [u8; 32] = [
        251, 122, 93, 138, 102, 100, 86, 173, 194, 151, 244, 63, 20, 195, 17, 116, 4, 80, 154, 189, 215, 225, 135, 6,
        94, 22, 29, 129, 7, 95, 158, 167,
    ];

    fn field_kind_wire_bytes(kind: FieldKind) -> [u8; 4] {
        match kind {
            FieldKind::Problem => [0, 0, 0, 0],
            FieldKind::Evidence => [1, 0, 0, 0],
            FieldKind::Success => [2, 0, 0, 0],
            FieldKind::Approach => [3, 0, 0, 0],
            FieldKind::RejectedOption => [4, 0, 0, 0],
            FieldKind::PlanStep => [5, 0, 0, 0],
            FieldKind::Acceptance => [6, 0, 0, 0],
            FieldKind::DeclaredSurface => [7, 0, 0, 0],
            FieldKind::InverseSearch => [8, 0, 0, 0],
            FieldKind::Edge => [9, 0, 0, 0],
            FieldKind::RoutingHint => [10, 0, 0, 0],
            FieldKind::Implements => [11, 0, 0, 0],
        }
    }

    #[test]
    fn field_kind_wire_bytes_are_append_only() {
        // Tripwire: FieldKind discriminants are positional u32. Appending a
        // variant adds a row and disturbs none; reordering or inserting shifts
        // every later row and every persisted fact of those kinds.
        for kind in [
            FieldKind::Problem,
            FieldKind::Evidence,
            FieldKind::Success,
            FieldKind::Approach,
            FieldKind::RejectedOption,
            FieldKind::PlanStep,
            FieldKind::Acceptance,
            FieldKind::DeclaredSurface,
            FieldKind::InverseSearch,
            FieldKind::Edge,
            FieldKind::RoutingHint,
            FieldKind::Implements,
        ] {
            assert_eq!(
                to_vec(&kind).expect("FieldKind encodes").as_slice(),
                field_kind_wire_bytes(kind).as_slice(),
                "FieldKind wire drifted for {kind:?}"
            );
        }
    }

    #[test]
    fn workpiece_fact_digest_is_not_repinnable() {
        let digest = digest_of(&fact(FieldKind::Problem, [7; 32]));
        assert_eq!(
            *digest.as_bytes(),
            GOLDEN_WORKPIECE_FACT_DIGEST,
            "WorkpieceFact content addressing drifted; digest={digest:?}"
        );
    }

    #[test]
    fn carries_distinguishes_absent_from_present_and_empty() {
        let fields = WorkpieceFields {
            workpiece: workpiece(),
            facts: vec![
                fact(FieldKind::Problem, [0; 32]),
                fact(FieldKind::PlanStep, [1; 32]),
                fact(FieldKind::PlanStep, [2; 32]),
                fact(FieldKind::PlanStep, [3; 32]),
            ],
        };

        assert!(!fields.carries(FieldKind::Implements), "a never-emitted kind is absent");
        assert_eq!(fields.records(FieldKind::Implements).count(), 0, "absent streams nothing");

        assert!(fields.carries(FieldKind::Problem), "a zero-valued detail is present, not absent");
        let present: Vec<&WorkpieceFact> = fields.records(FieldKind::Problem).collect();
        assert_eq!(present.len(), 1);
        assert_eq!(present[0].detail, Digest::from_bytes([0; 32]));

        let steps: Vec<Digest> = fields.records(FieldKind::PlanStep).map(|record| record.detail).collect();
        assert_eq!(
            steps,
            vec![Digest::from_bytes([1; 32]), Digest::from_bytes([2; 32]), Digest::from_bytes([3; 32])],
            "repeated kinds stream in emission order"
        );
    }
}
