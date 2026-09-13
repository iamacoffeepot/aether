//! Bounded, replayable attribution of failures in an immutable composition.
//!
//! A diagnostic supplies a failing check, never a guilty member. The host
//! executes each requested dependency-closed subset in its retained prover
//! slot and persists the receipt before asking for the next step. No mutable
//! composer, arrival timer, or standalone proof is hidden in this policy.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::iter::once;
use std::slice::from_ref;

use aether_bloomery::digest::{ContentAddressed, digest_of};
use aether_bloomery::{Digest, WorkpieceId};
use serde::{Deserialize, Serialize};

/// A current member version's dependency edges within this physical plan.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BatchMember {
    pub workpiece: WorkpieceId,
    pub dependencies: Vec<WorkpieceId>,
    /// Members inseparable from this one's recorded composition input. A
    /// repaired multi-member commit is never flattened back to leaf refs.
    pub atomic_peers: Vec<WorkpieceId>,
    /// A late member must also be compared with the head it inherited.
    pub inherited: bool,
}

/// Closed identity of the observation a probe must actually reach.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum BatchCheck {
    Gate { id: String },
    Test { gate: String, id: String },
}

impl BatchCheck {
    #[must_use]
    pub fn gate(&self) -> &str {
        match self {
            Self::Gate { id } => id,
            Self::Test { gate, .. } => gate,
        }
    }

    #[must_use]
    pub fn observation_key(&self) -> String {
        match self {
            Self::Gate { id } => format!("gate:{id}"),
            Self::Test { id, .. } => format!("test:{id}"),
        }
    }
}

/// One actual invocation requested by the attribution policy.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BatchProbeRequest {
    /// Immutable physical plan identity; prevents cross-plan receipt reuse.
    pub plan: Digest,
    /// Ordered, dependency-closed subset. Empty means the recorded base.
    pub members: Vec<WorkpieceId>,
    /// With an empty subset, select this member's recorded starting head
    /// rather than the composition's common base.
    pub baseline: Option<WorkpieceId>,
    /// Exact verifier or test whose result is sought.
    pub check: BatchCheck,
    /// Independent repetition, zero or one. Replaying a receipt is not a run.
    pub repetition: u8,
}

impl ContentAddressed for BatchProbeRequest {
    const DOMAIN: &'static str = "aether.bloomery.batch_probe_request.v1";
}

impl BatchProbeRequest {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// An observation about the requested check, not the exit code of a build
/// which never reached that check.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ProbeVerdict {
    Passed,
    Failed,
    Unknown,
    Infrastructure,
}

/// Retained before a slot advances to another subset. Host admission binds
/// request, actual invocation and tested input; worker text cannot choose them.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BatchProbeReceipt {
    pub request: BatchProbeRequest,
    pub invocation: Digest,
    pub verdict: ProbeVerdict,
    pub evidence: Digest,
}

/// The narrow claim supported by independent probe observations.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum BatchFailure {
    /// A singleton fails against a base which passed. Multiple records may
    /// identify independent causes of the same failing check.
    Attributed { member: WorkpieceId, check: BatchCheck, evidence: Vec<Digest> },
    /// This dependency-closed group fails while its available splits pass,
    /// or its dependency edges prevent a narrower valid experiment.
    Interaction { members: Vec<WorkpieceId>, check: BatchCheck, evidence: Vec<Digest> },
    /// The baseline repeats the failure. No member is charged.
    Inherited { member: Option<WorkpieceId>, check: BatchCheck, evidence: Vec<Digest> },
    /// Missing, disagreeing, unreached, or budget-exhausted observations.
    Unknown { members: Vec<WorkpieceId>, check: BatchCheck, evidence: Vec<Digest> },
    /// The host could not execute the experiment. No member is charged.
    Infrastructure { check: BatchCheck, evidence: Vec<Digest> },
}

/// Attribution alone never proves survivors. They form a fresh node whose
/// obligations the caller must still verify.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct BatchReport {
    pub failures: Vec<BatchFailure>,
    pub ejected: Vec<WorkpieceId>,
    pub survivors: Vec<WorkpieceId>,
}

/// One deterministic scheduling decision. The journal/physical-run owner
/// retains the request and receipt; this policy owns no execution state.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BatchProgress {
    Probe(BatchProbeRequest),
    Complete(BatchReport),
    Invalid(String),
}

struct ProbeHistory<'a> {
    plan: Digest,
    receipts: &'a [BatchProbeReceipt],
    budget: usize,
    retained_count: usize,
}

enum Discrimination {
    Observed(ProbeVerdict, Vec<Digest>),
    Next(BatchProbeRequest),
}

impl ProbeHistory<'_> {
    fn discriminated(&self, members: &[WorkpieceId], check: &BatchCheck) -> Discrimination {
        self.discriminated_at(members, check, None)
    }

    fn discriminated_at(
        &self,
        members: &[WorkpieceId],
        check: &BatchCheck,
        baseline: Option<&WorkpieceId>,
    ) -> Discrimination {
        let mut accepted = Vec::new();
        for repetition in 0..2 {
            let request = BatchProbeRequest {
                plan: self.plan,
                members: members.to_vec(),
                baseline: baseline.cloned(),
                check: check.to_owned(),
                repetition,
            };
            let Some(receipt) = self.receipts.iter().find(|receipt| receipt.request == request) else {
                if self.retained_count < self.budget {
                    return Discrimination::Next(request);
                }
                return Discrimination::Observed(
                    ProbeVerdict::Unknown,
                    accepted.iter().map(|receipt: &&BatchProbeReceipt| receipt.evidence).collect(),
                );
            };
            if receipt.verdict == ProbeVerdict::Infrastructure {
                return Discrimination::Observed(ProbeVerdict::Infrastructure, vec![receipt.evidence]);
            }
            if receipt.verdict == ProbeVerdict::Unknown {
                return Discrimination::Observed(ProbeVerdict::Unknown, vec![receipt.evidence]);
            }
            accepted.push(receipt);
        }
        let verdict = if accepted[0].invocation != accepted[1].invocation && accepted[0].verdict == accepted[1].verdict
        {
            accepted[0].verdict
        } else {
            ProbeVerdict::Unknown
        };
        Discrimination::Observed(verdict, accepted.iter().map(|receipt| receipt.evidence).collect())
    }
}

fn discriminate_check(
    history: &ProbeHistory<'_>,
    members: &[BatchMember],
    ordered: &[WorkpieceId],
    check: &BatchCheck,
    report: &mut BatchReport,
) -> Option<BatchProbeRequest> {
    let baseline = match history.discriminated(&[], check) {
        Discrimination::Next(request) => return Some(request),
        Discrimination::Observed(verdict, evidence) => (verdict, evidence),
    };
    match baseline {
        (ProbeVerdict::Failed, evidence) => {
            report.failures.push(BatchFailure::Inherited { member: None, check: check.clone(), evidence });
            return None;
        }
        (ProbeVerdict::Passed, _) => {}
        (verdict, evidence) => {
            report.failures.push(unresolved(verdict, ordered, check, evidence));
            return None;
        }
    }

    let mut queue = VecDeque::from([ordered.to_vec()]);
    let mut seen = BTreeSet::new();
    while let Some(subset) = queue.pop_front() {
        if subset.is_empty() || !seen.insert(subset.clone()) {
            continue;
        }
        let evidence = match history.discriminated(&subset, check) {
            Discrimination::Next(request) => return Some(request),
            Discrimination::Observed(ProbeVerdict::Failed, evidence) => evidence,
            Discrimination::Observed(ProbeVerdict::Passed, _) => continue,
            Discrimination::Observed(verdict, evidence) => {
                report.failures.push(unresolved(verdict, &subset, check, evidence));
                continue;
            }
        };
        // Narrow the failing set before inspecting inherited heads. An
        // unresolved baseline on A must not hide an independent failure
        // in C on the other side of the split.
        let mut failed = Vec::new();
        let mut split_unknown = false;
        for half in proper_subsets(members, &subset) {
            match history.discriminated(&half, check) {
                Discrimination::Next(request) => return Some(request),
                Discrimination::Observed(ProbeVerdict::Failed, _) => failed.push(half),
                Discrimination::Observed(ProbeVerdict::Passed, _) => {}
                Discrimination::Observed(verdict, evidence) => {
                    split_unknown = true;
                    report.failures.push(unresolved(verdict, &half, check, evidence));
                }
            }
        }
        if !failed.is_empty() || split_unknown {
            queue.extend(failed);
            continue;
        }

        let mut inherited_unresolved = false;
        for member in members.iter().filter(|member| member.inherited && subset.contains(&member.workpiece)) {
            match history.discriminated_at(&[], check, Some(&member.workpiece)) {
                Discrimination::Next(request) => return Some(request),
                Discrimination::Observed(ProbeVerdict::Passed, _) => {}
                Discrimination::Observed(ProbeVerdict::Failed, evidence) => {
                    report.failures.push(BatchFailure::Inherited {
                        member: Some(member.workpiece.clone()),
                        check: check.clone(),
                        evidence,
                    });
                    inherited_unresolved = true;
                }
                Discrimination::Observed(verdict, evidence) => {
                    report.failures.push(unresolved(verdict, &subset, check, evidence));
                    inherited_unresolved = true;
                }
            }
        }
        if inherited_unresolved {
            continue;
        }
        if subset.len() == 1 {
            report.failures.push(BatchFailure::Attributed {
                member: subset[0].clone(),
                check: check.clone(),
                evidence,
            });
        } else {
            report.failures.push(BatchFailure::Interaction { members: subset, check: check.clone(), evidence });
        }
    }
    None
}

/// Plan the next bounded experiment from retained receipts. New arrivals do
/// not change `members`: they belong to another immutable physical plan.
///
/// Both halves are examined, preserving independent A/C failures and an
/// interaction witness when both halves pass. Dependencies close each subset;
/// an unsplittable dependency group remains an interaction, never scalar blame.
#[must_use]
pub fn next_batch_probe(
    plan: Digest,
    members: &[BatchMember],
    failing_checks: &[BatchCheck],
    receipts: &[BatchProbeReceipt],
    probe_budget: u32,
) -> BatchProgress {
    if let Err(reason) = validate_inputs(plan, members, receipts) {
        return BatchProgress::Invalid(reason);
    }
    let ordered: Vec<WorkpieceId> = members.iter().map(|member| member.workpiece.clone()).collect();
    let history = ProbeHistory {
        plan,
        receipts,
        budget: probe_budget as usize,
        retained_count: receipts.iter().map(|receipt| receipt.request.digest()).collect::<BTreeSet<_>>().len(),
    };
    let mut report = BatchReport::default();
    let mut checks = BTreeSet::new();
    for check in failing_checks {
        if !checks.insert(check) {
            continue;
        }
        if let Some(request) = discriminate_check(&history, members, &ordered, check, &mut report) {
            return BatchProgress::Probe(request);
        }
    }
    let ejected: BTreeSet<WorkpieceId> = report
        .failures
        .iter()
        .filter_map(|failure| match failure {
            BatchFailure::Attributed { member, .. } => Some(member.clone()),
            _ => None,
        })
        .collect();
    report.ejected = ordered.iter().filter(|member| ejected.contains(*member)).cloned().collect();
    let blocked: BTreeSet<WorkpieceId> = report
        .failures
        .iter()
        .flat_map(|failure| match failure {
            BatchFailure::Attributed { member, .. } | BatchFailure::Inherited { member: Some(member), .. } => {
                from_ref(member)
            }
            BatchFailure::Interaction { members, .. } | BatchFailure::Unknown { members, .. } => members,
            BatchFailure::Inherited { member: None, .. } | BatchFailure::Infrastructure { .. } => &ordered,
        })
        .cloned()
        .collect();
    // Inherited or unresolved contributions wait without being ejected or
    // charged a repair. Their dependents and atomic peers cannot silently
    // carry the same blocked code into the fresh survivor node.
    report.survivors = ordered
        .iter()
        .filter(|member| dependency_closure(members, from_ref(member)).iter().all(|pin| !blocked.contains(pin)))
        .cloned()
        .collect();
    BatchProgress::Complete(report)
}

fn unresolved(
    verdict: ProbeVerdict,
    members: &[WorkpieceId],
    check: &BatchCheck,
    evidence: Vec<Digest>,
) -> BatchFailure {
    if verdict == ProbeVerdict::Infrastructure {
        BatchFailure::Infrastructure { check: check.to_owned(), evidence }
    } else {
        BatchFailure::Unknown { members: members.to_vec(), check: check.to_owned(), evidence }
    }
}

/// Prefer balanced cuts that remain two proper dependency-closed subsets.
/// When every cut expands back to the parent, examine the maximal proper
/// member closures instead. Skipping a closure-equal half could otherwise
/// hide an independent failure elsewhere in that half.
fn proper_subsets(members: &[BatchMember], subset: &[WorkpieceId]) -> Vec<Vec<WorkpieceId>> {
    let middle = subset.len() / 2;
    let cuts = once(middle).chain((1..subset.len()).filter(|cut| *cut != middle));
    for cut in cuts {
        let left = dependency_closure(members, &subset[..cut]);
        let right = dependency_closure(members, &subset[cut..]);
        if left != subset && right != subset {
            return vec![left, right];
        }
    }
    let closures: BTreeSet<Vec<WorkpieceId>> = subset
        .iter()
        .map(|member| dependency_closure(members, from_ref(member)))
        .filter(|closure| closure != subset)
        .collect();
    closures
        .iter()
        .filter(|closure| {
            !closures
                .iter()
                .any(|other| other.len() > closure.len() && closure.iter().all(|member| other.contains(member)))
        })
        .cloned()
        .collect()
}

fn dependency_closure(members: &[BatchMember], subset: &[WorkpieceId]) -> Vec<WorkpieceId> {
    let mut selected: BTreeSet<WorkpieceId> = subset.iter().cloned().collect();
    let mut pending = subset.to_vec();
    while let Some(current) = pending.pop() {
        if let Some(member) = members.iter().find(|member| member.workpiece == current) {
            for dependency in member.dependencies.iter().chain(&member.atomic_peers) {
                if selected.insert(dependency.clone()) {
                    pending.push(dependency.clone());
                }
            }
        }
    }
    members
        .iter()
        .filter(|member| selected.contains(&member.workpiece))
        .map(|member| member.workpiece.clone())
        .collect()
}

fn validate_inputs(plan: Digest, members: &[BatchMember], receipts: &[BatchProbeReceipt]) -> Result<(), String> {
    let ids: BTreeSet<&WorkpieceId> = members.iter().map(|member| &member.workpiece).collect();
    if members.is_empty() || ids.len() != members.len() {
        return Err("an attribution plan needs distinct members".to_owned());
    }
    if members.iter().any(|member| member.dependencies.iter().any(|dependency| !ids.contains(dependency))) {
        return Err("an attribution dependency is absent from the plan".to_owned());
    }
    if members.iter().any(|member| {
        member.atomic_peers.iter().any(|peer| {
            !members.iter().any(|other| other.workpiece == *peer && other.atomic_peers.contains(&member.workpiece))
        })
    }) {
        return Err("an atomic composition input is absent or asymmetric".to_owned());
    }
    let mut settled = BTreeSet::new();
    while settled.len() < members.len() {
        let before = settled.len();
        for member in members {
            if member.dependencies.iter().all(|dependency| settled.contains(dependency)) {
                settled.insert(member.workpiece.clone());
            }
        }
        if settled.len() == before {
            return Err("an attribution dependency cycle has no valid probe order".to_owned());
        }
    }
    let mut retained = BTreeMap::new();
    for receipt in receipts {
        if receipt.request.plan != plan || receipt.request.repetition > 1 {
            return Err("a probe receipt names another plan or repetition".to_owned());
        }
        if let Some(baseline) = &receipt.request.baseline
            && (!receipt.request.members.is_empty()
                || !members.iter().any(|member| member.workpiece == *baseline && member.inherited))
        {
            return Err("a probe baseline is not a recorded inherited member head".to_owned());
        }
        if dependency_closure(members, &receipt.request.members) != receipt.request.members {
            return Err("a probe receipt is not an ordered dependency-closed subset".to_owned());
        }
        if let Some(previous) = retained.insert(receipt.request.digest(), receipt)
            && previous != receipt
        {
            return Err("a retained probe receipt was replaced".to_owned());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str) -> BatchMember {
        BatchMember {
            workpiece: WorkpieceId(name.to_owned()),
            dependencies: Vec::new(),
            atomic_peers: Vec::new(),
            inherited: false,
        }
    }

    fn check() -> BatchCheck {
        BatchCheck::Test { gate: "verify.test".to_owned(), id: "test::shared".to_owned() }
    }

    fn drive(members: &[BatchMember], budget: u32, answer: impl Fn(&[WorkpieceId]) -> ProbeVerdict) -> BatchReport {
        drive_requests(members, budget, |request| answer(&request.members))
    }

    fn drive_requests(
        members: &[BatchMember],
        budget: u32,
        answer: impl Fn(&BatchProbeRequest) -> ProbeVerdict,
    ) -> BatchReport {
        let plan = Digest::from_bytes([1; 32]);
        let mut receipts = Vec::new();
        loop {
            match next_batch_probe(plan, members, &[check()], &receipts, budget) {
                BatchProgress::Probe(request) => {
                    let verdict = answer(&request);
                    receipts.push(BatchProbeReceipt {
                        invocation: Digest::from_bytes(
                            [u8::try_from(receipts.len() + 2).expect("probe fixture fits a byte"); 32],
                        ),
                        evidence: request.digest(),
                        request,
                        verdict,
                    });
                }
                BatchProgress::Complete(report) => return report,
                BatchProgress::Invalid(reason) => panic!("{reason}"),
            }
        }
    }

    #[test]
    fn independent_failures_eject_a_and_c_and_keep_b_and_d_together() {
        let members = [member("a"), member("b"), member("c"), member("d")];
        let report = drive(&members, 64, |subset| {
            if subset.contains(&members[0].workpiece) || subset.contains(&members[2].workpiece) {
                ProbeVerdict::Failed
            } else {
                ProbeVerdict::Passed
            }
        });
        assert_eq!(report.ejected, [members[0].workpiece.clone(), members[2].workpiece.clone()]);
        assert_eq!(report.survivors, [members[1].workpiece.clone(), members[3].workpiece.clone()]);
        assert_eq!(report.failures.len(), 2);
    }

    #[test]
    fn two_green_halves_retain_the_failing_interaction() {
        let members = [member("a"), member("b")];
        let report = drive(&members, 64, |subset| {
            if subset.len() == 2 {
                ProbeVerdict::Failed
            } else {
                ProbeVerdict::Passed
            }
        });
        assert!(report.ejected.is_empty());
        assert!(matches!(&report.failures[..], [BatchFailure::Interaction { members, .. }] if members.len() == 2));
    }

    #[test]
    fn unknown_and_infrastructure_never_become_member_blame() {
        let members = [member("a"), member("b")];
        for verdict in [ProbeVerdict::Unknown, ProbeVerdict::Infrastructure] {
            let report = drive(&members, 64, |_| verdict);
            assert!(report.ejected.is_empty());
            assert_eq!(report.failures.len(), 1);
        }
        let exhausted = drive(&members, 1, |_| ProbeVerdict::Passed);
        assert!(exhausted.ejected.is_empty());
        assert!(matches!(&exhausted.failures[..], [BatchFailure::Unknown { .. }]));
    }

    #[test]
    fn a_dependent_of_an_ejected_member_is_not_a_survivor() {
        let mut members = [member("a"), member("b"), member("c"), member("d")];
        members[1].dependencies.push(members[0].workpiece.clone());
        let report = drive(&members, 64, |subset| {
            if subset.contains(&members[0].workpiece) {
                ProbeVerdict::Failed
            } else {
                ProbeVerdict::Passed
            }
        });
        assert!(!report.survivors.contains(&members[0].workpiece));
        assert!(!report.survivors.contains(&members[1].workpiece));
    }

    #[test]
    fn dependency_expansion_does_not_hide_an_independent_failure() {
        for fail_a in [false, true] {
            let mut members = [member("a"), member("b"), member("c")];
            members[1].dependencies.push(members[0].workpiece.clone());
            let report = drive(&members, 64, |subset| {
                if (fail_a && subset.contains(&members[0].workpiece)) || subset.contains(&members[2].workpiece) {
                    ProbeVerdict::Failed
                } else {
                    ProbeVerdict::Passed
                }
            });
            assert!(report.ejected.contains(&members[2].workpiece));
            assert_eq!(report.ejected.contains(&members[0].workpiece), fail_a);
            assert!(!report.survivors.contains(&members[2].workpiece));
        }
    }

    #[test]
    fn a_single_dependent_covering_every_member_still_examines_both_roots() {
        let mut members = [member("a"), member("c"), member("dependent")];
        members[2].dependencies = vec![members[0].workpiece.clone(), members[1].workpiece.clone()];
        let report = drive(&members, 64, |subset| {
            if subset.is_empty() {
                ProbeVerdict::Passed
            } else {
                ProbeVerdict::Failed
            }
        });
        assert_eq!(report.ejected, [members[0].workpiece.clone(), members[1].workpiece.clone()]);
        assert!(report.survivors.is_empty());
    }

    #[test]
    fn duplicate_receipt_delivery_does_not_spend_the_remaining_probe_budget() {
        let plan = Digest::from_bytes([1; 32]);
        let receipt = BatchProbeReceipt {
            request: BatchProbeRequest { plan, members: vec![], baseline: None, check: check(), repetition: 0 },
            invocation: Digest::from_bytes([2; 32]),
            verdict: ProbeVerdict::Passed,
            evidence: Digest::from_bytes([3; 32]),
        };
        let result = next_batch_probe(plan, &[member("a")], &[check()], &[receipt.clone(), receipt], 2);
        assert!(matches!(result, BatchProgress::Probe(BatchProbeRequest { repetition: 1, .. })));
    }

    #[test]
    fn a_repaired_group_is_probed_only_as_its_recorded_atomic_input() {
        let mut members = [member("a"), member("b"), member("c")];
        members[0].atomic_peers = vec![members[1].workpiece.clone()];
        members[1].atomic_peers = vec![members[0].workpiece.clone()];
        let report = drive(&members, 64, |subset| {
            assert_eq!(subset.contains(&members[0].workpiece), subset.contains(&members[1].workpiece));
            if subset.contains(&members[0].workpiece) {
                ProbeVerdict::Failed
            } else {
                ProbeVerdict::Passed
            }
        });
        assert!(report.ejected.is_empty());
        assert!(
            matches!(&report.failures[..], [BatchFailure::Interaction { members: group, .. }] if group == &[members[0].workpiece.clone(), members[1].workpiece.clone()])
        );
    }

    #[test]
    fn replaying_one_invocation_twice_is_not_independent_discrimination() {
        let plan = Digest::from_bytes([1; 32]);
        let members = [member("a")];
        let receipts: Vec<_> = (0..2)
            .map(|repetition| BatchProbeReceipt {
                request: BatchProbeRequest { plan, members: vec![], baseline: None, check: check(), repetition },
                invocation: Digest::from_bytes([2; 32]),
                verdict: ProbeVerdict::Passed,
                evidence: Digest::from_bytes([3; 32]),
            })
            .collect();
        let BatchProgress::Complete(report) = next_batch_probe(plan, &members, &[check()], &receipts, 64) else {
            panic!("duplicate invocation must remain unknown");
        };
        assert!(report.ejected.is_empty());
        assert!(matches!(&report.failures[..], [BatchFailure::Unknown { .. }]));
    }

    #[test]
    fn a_late_member_is_not_blamed_for_its_unverified_inherited_head() {
        let plan = Digest::from_bytes([1; 32]);
        let mut late = member("late");
        late.inherited = true;
        let mut receipts = Vec::new();
        loop {
            match next_batch_probe(plan, from_ref(&late), &[check()], &receipts, 16) {
                BatchProgress::Probe(request) => {
                    let verdict = if request.members.is_empty() && request.baseline.is_none() {
                        ProbeVerdict::Passed
                    } else {
                        ProbeVerdict::Failed
                    };
                    receipts.push(BatchProbeReceipt {
                        invocation: Digest::from_bytes(
                            [u8::try_from(receipts.len() + 2).expect("probe fixture fits a byte"); 32],
                        ),
                        evidence: request.digest(),
                        request,
                        verdict,
                    });
                }
                BatchProgress::Complete(report) => {
                    assert!(report.ejected.is_empty());
                    assert!(report.survivors.is_empty());
                    assert!(
                        matches!(&report.failures[..], [BatchFailure::Inherited { member: Some(member), .. }] if *member == late.workpiece)
                    );
                    break;
                }
                BatchProgress::Invalid(reason) => panic!("{reason}"),
            }
        }
    }

    #[test]
    fn an_unresolved_inherited_head_does_not_hide_an_independent_failure() {
        let mut members = [member("a"), member("b"), member("c")];
        members[0].inherited = true;
        for baseline in [ProbeVerdict::Failed, ProbeVerdict::Unknown, ProbeVerdict::Infrastructure] {
            let report = drive_requests(&members, 64, |request| {
                if request.baseline.as_ref() == Some(&members[0].workpiece) {
                    baseline
                } else if request.members.contains(&members[0].workpiece)
                    || request.members.contains(&members[2].workpiece)
                {
                    ProbeVerdict::Failed
                } else {
                    ProbeVerdict::Passed
                }
            });
            assert_eq!(report.ejected, [members[2].workpiece.clone()]);
            assert!(!report.survivors.contains(&members[0].workpiece));
            if baseline != ProbeVerdict::Infrastructure {
                assert_eq!(report.survivors, [members[1].workpiece.clone()]);
            }
            if baseline == ProbeVerdict::Failed {
                assert!(report.failures.iter().any(|failure| {
                    matches!(failure, BatchFailure::Inherited { member: Some(member), .. } if member == &members[0].workpiece)
                }));
            }
        }
    }

    #[test]
    fn an_atomic_peer_cannot_survive_a_blocked_inherited_contribution() {
        let mut members = [member("a"), member("b"), member("c")];
        members[0].inherited = true;
        members[0].atomic_peers = vec![members[1].workpiece.clone()];
        members[1].atomic_peers = vec![members[0].workpiece.clone()];
        let report = drive_requests(&members, 64, |request| {
            if request.baseline.is_some() || request.members.contains(&members[0].workpiece) {
                ProbeVerdict::Failed
            } else {
                ProbeVerdict::Passed
            }
        });
        assert!(report.ejected.is_empty());
        assert_eq!(report.survivors, [members[2].workpiece.clone()]);
        assert!(
            matches!(&report.failures[..], [BatchFailure::Inherited { member: Some(member), .. }] if member == &members[0].workpiece)
        );
    }
}
