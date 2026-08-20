//! The day's coverage map, derived from the live view and the journal.
//!
//! A landed workpiece is covered only by an `Integrate` claim whose evidence
//! is a `VerificationResult`. Inherited or fixture evidence, or no integrate
//! fact at all, leaves the workpiece on the hold list. Superseded blooms never
//! reached main and are not required.

use std::collections::BTreeSet;

use aether_bloomery::{BloomStatus, EvidenceKind};
use aether_bloomery_git::DayCoverage;
use serde_json::Value;

use crate::bloom::dto::{IntegrateClaimView, JournalView, ViewDocument};

/// Landed workpieces minus those with a `VerificationResult` integrate claim.
pub fn day_coverage(view: &ViewDocument, journal: &JournalView) -> DayCoverage {
    let required: BTreeSet<String> = view
        .blooms
        .iter()
        .filter(|bloom| bloom.status == BloomStatus::Landed)
        .flat_map(|bloom| bloom.members.iter().map(|member| member.workpiece.clone()))
        .collect();
    let covered: BTreeSet<String> = journal
        .records
        .iter()
        .filter_map(|record| integrate_claim(&record.event.fact))
        .filter(|claim| claim.evidence.kind == EvidenceKind::VerificationResult)
        .map(|claim| claim.workpiece)
        .collect();

    if required.is_subset(&covered) {
        DayCoverage::green()
    } else {
        DayCoverage::hold(required.difference(&covered).cloned().collect::<Vec<_>>().join("\n"))
    }
}

fn integrate_claim(fact: &Value) -> Option<IntegrateClaimView> {
    serde_json::from_value(fact.get("Integrate")?.get("claim")?.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::day_coverage;
    use crate::bloom::dto::{BloomView, DigestHex, JournalEntry, JournalEvent, JournalView, MemberView, ViewDocument};
    use aether_bloomery::{BloomStatus, EvidenceKind};
    use aether_bloomery_git::DayCoverage;
    use serde_json::{Value, json};

    fn landed(workpieces: &[&str]) -> ViewDocument {
        blooms([(BloomStatus::Landed, workpieces)])
    }

    fn blooms<const N: usize>(entries: [(BloomStatus, &[&str]); N]) -> ViewDocument {
        ViewDocument {
            mainline: DigestHex::from_bytes([1; 32]),
            observed: DigestHex::from_bytes([2; 32]),
            blooms: entries
                .into_iter()
                .enumerate()
                .map(|(index, (status, workpieces))| BloomView {
                    id: DigestHex::from_bytes([u8::try_from(index).expect("few blooms"); 32]),
                    status,
                    superseded_by: None,
                    members: workpieces
                        .iter()
                        .map(|workpiece| MemberView {
                            workpiece: (*workpiece).to_owned(),
                            scope_revision: DigestHex::from_bytes([7; 32]),
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    fn journal(facts: impl IntoIterator<Item = Value>) -> JournalView {
        JournalView { records: facts.into_iter().map(|fact| JournalEntry { event: JournalEvent { fact } }).collect() }
    }

    fn integrate(workpiece: &str, kind: EvidenceKind) -> Value {
        json!({ "Integrate": { "claim": { "workpiece": workpiece, "evidence": { "kind": kind } } } })
    }

    #[test]
    fn a_landed_member_with_a_verification_result_is_green() {
        assert_eq!(
            day_coverage(
                &landed(&["issue-4945"]),
                &journal([integrate("issue-4945", EvidenceKind::VerificationResult)]),
            ),
            DayCoverage::green()
        );
    }

    #[test]
    fn a_landed_member_with_any_other_evidence_kind_is_held() {
        assert_eq!(
            day_coverage(&landed(&["issue-4945"]), &journal([integrate("issue-4945", EvidenceKind::Approval)]),),
            DayCoverage::hold("issue-4945")
        );
    }

    #[test]
    fn a_landed_member_with_no_integrate_fact_is_held() {
        assert_eq!(day_coverage(&landed(&["issue-4945"]), &journal([])), DayCoverage::hold("issue-4945"));
    }

    #[test]
    fn two_uncovered_members_are_both_named() {
        assert_eq!(day_coverage(&landed(&["issue-b", "issue-a"]), &journal([])), DayCoverage::hold("issue-a\nissue-b"));
    }

    #[test]
    fn a_superseded_bloom_is_not_required() {
        assert_eq!(
            day_coverage(
                &blooms([
                    (BloomStatus::Landed, &["issue-landed"][..]),
                    (BloomStatus::Superseded, &["issue-superseded"][..]),
                ]),
                &journal([integrate("issue-landed", EvidenceKind::VerificationResult)]),
            ),
            DayCoverage::green()
        );
    }
}
