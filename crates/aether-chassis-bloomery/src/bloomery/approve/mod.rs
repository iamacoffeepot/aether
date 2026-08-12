//! The pre-seal approve gate: the `StageId::Approve` lane's native process
//! (ADR-0149 §The line, ADR-0151, issue #3571).
//!
//! Approve is **not** a dispatched worker lane. The member-line dispatch loop
//! (`Construct → Verify`, ADR-0153) is post-seal; Approve is pre-seal,
//! every check is deterministic, and what "approve" realizes is a host-side
//! admission decision. So this gate runs on the coordinator host — beside the
//! evidence-intake gate ([`super::intake`]) at the host admission boundary — and
//! its output is the `approval` [`Evidence`] on a draft's membership proposal
//! (the [`Membership.approval`](aether_bloomery::Membership) the host shapes
//! before [`Fact::Seal`]). The existing seal-time `validate_member_admission`
//! (`aether_bloomery::reduce`) is the reducer's re-check: every member's approval
//! must be an [`EvidenceKind::Approval`] bound to its own `scope_revision`. This
//! gate forms exactly such an approval — no reducer widening, no new `Fact`.
//!
//! # The gate order
//!
//! 1. The **ADR hard gate** (maturity-aware, unconditional): a change that writes
//!    a NEW ADR or edits an ESTABLISHED (non-`Proposed`) one routes to the owner
//!    regardless of what the tier policy says. Only a still-`Proposed` ADR touch
//!    defers to the policy. This lives in the gate, not the policy file — a glob
//!    matches paths, not maturity.
//! 2. The **completeness gate**: the scope revision must be complete — the
//!    `## Problem statement` / `## Design notes` / `## Implementation plan`
//!    sections present and non-empty, referenced ADR PRs merged, exactly one
//!    model routing, not blocked, a fresh declared surface, every `## Depends on`
//!    closed, and umbrella integrity. Any failure fails **closed**: no approval.
//! 3. **Tier resolution** over the declared surface — the ported
//!    `scripts/surface-match.py --tier` semantics (most-restrictive-wins,
//!    fail-closed to `human` out of grammar). The policy it resolves against is
//!    the one the draft seals bloom-wide under `aether.bloomery.approval_policy`
//!    (#4616, ADR-0174), falling back to the host's
//!    [`load_policy`] file only when the draft seals none — so the tier a member
//!    was admitted at is a property of the bloom, not of the coordinator's disk.
//! 4. The `pre_approved` owner override: resolves the *tier* to `auto`
//!    (owner-actor-verified upstream), waiving the tier but **not** the gate
//!    checks — and it cannot pass a firing ADR gate.
//!
//! An `auto` tier forms the `approval` [`Evidence`] directly
//! ([`Gate::evaluate`] → [`Decision::AutoApproved`]). Anything above `auto`
//! requires an owner-authorized signed [`Statement`] (ADR-0151, #3560) to
//! populate the approval ([`approval_from_statement`]) — the tier policy (*what*
//! tier) and the signing key policy (*who* may sign) stay distinct readers.
//!
//! [`Evidence`]: aether_bloomery::Evidence
//! [`EvidenceKind::Approval`]: aether_bloomery::EvidenceKind::Approval
//! [`Fact::Seal`]: aether_bloomery::Fact::Seal
//! [`Statement`]: aether_bloomery::Statement

mod gate;
mod policy;
mod statement;

pub use gate::{AdmissionRequest, AdrTouch, Completeness, Decision, Gate, Incompleteness};
pub use policy::{ApprovalPolicy, PolicyError, Tier, load_policy};
pub use statement::{StatementRejected, approval_from_statement, precheck_statement, verified_statement_approval};

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
