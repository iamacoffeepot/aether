//! The day's coverage map, derived from the live view and the journal.
//!
//! A landed workpiece is covered only by an `Integrate` claim whose evidence
//! is a `VerificationResult`. Inherited or fixture evidence, or no integrate
//! fact at all, leaves the workpiece on the hold list. Superseded blooms never
//! reached main and are not required, and neither is a member the day withdrew:
//! it left the line before integration, so no receipt for it can ever exist.

use std::collections::BTreeSet;

use aether_bloomery::{BloomStatus, EvidenceKind};
use aether_bloomery_git::DayCoverage;
use serde_json::Value;

use crate::bloom::dto::{IntegrateClaimView, JournalView, ViewDocument};

/// Standing landed workpieces minus those with a `VerificationResult` integrate claim.
pub fn day_coverage(view: &ViewDocument, journal: &JournalView) -> DayCoverage {
    let required: BTreeSet<String> = view
        .blooms
        .iter()
        .filter(|bloom| bloom.status == BloomStatus::Landed)
        .flat_map(|bloom| {
            bloom.members.iter().filter(|member| member.withdrawn.is_none()).map(|member| member.workpiece.clone())
        })
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
        let members = required.difference(&covered).cloned().collect::<Vec<_>>().join("\n");
        DayCoverage::hold(format!("{members}\n{}", evaluated_trailer(journal)))
    }
}

fn evaluated_trailer(journal: &JournalView) -> String {
    let shown = journal.shown;
    journal.journal_span().map_or_else(
        || format!("evaluated journal: {shown} records"),
        |(first, last)| format!("evaluated journal: {shown} records, sequences {first}..={last}"),
    )
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
                            awaiting_surface: None,
                            withdrawn: None,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    fn journal(facts: impl IntoIterator<Item = Value>) -> JournalView {
        let records: Vec<_> =
            facts.into_iter().map(|fact| JournalEntry { sequence: None, event: JournalEvent { fact } }).collect();
        let shown = u64::try_from(records.len()).unwrap_or(u64::MAX);
        JournalView { records, total_matched: shown, shown, truncated: false, next_from_sequence: None }
    }

    fn held(members: &str, journal: &JournalView) -> DayCoverage {
        DayCoverage::hold(format!("{members}\n{}", super::evaluated_trailer(journal)))
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
        let journal = journal([integrate("issue-4945", EvidenceKind::Approval)]);
        assert_eq!(day_coverage(&landed(&["issue-4945"]), &journal), held("issue-4945", &journal));
    }

    #[test]
    fn a_landed_member_with_no_integrate_fact_is_held() {
        let journal = journal([]);
        assert_eq!(day_coverage(&landed(&["issue-4945"]), &journal), held("issue-4945", &journal));
    }

    #[test]
    fn two_uncovered_members_are_both_named() {
        let journal = journal([]);
        assert_eq!(day_coverage(&landed(&["issue-b", "issue-a"]), &journal), held("issue-a\nissue-b", &journal));
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

    #[test]
    fn a_withdrawn_member_owes_no_receipt() {
        // Tripwire: bloom 79137ad910a6 landed with the one member that stayed,
        // after fifteen were withdrawn. Requiring a receipt from every
        // `members[]` entry held the roll on fifteen members that never
        // integrated and so can never hold an integrate claim. The view is
        // decoded the way the edge renders it, so a mirror that drops the
        // withdrawal fails here too.
        let view: ViewDocument = serde_json::from_value(json!({
            "mainline": DigestHex::from_bytes([1; 32]),
            "observed": DigestHex::from_bytes([2; 32]),
            "blooms": [{
                "id": DigestHex::from_bytes([3; 32]),
                "status": "Landed",
                "superseded_by": null,
                "members": [
                    { "workpiece": "issue-resolved", "scope_revision": DigestHex::from_bytes([7; 32]) },
                    {
                        "workpiece": "issue-withdrawn",
                        "scope_revision": DigestHex::from_bytes([7; 32]),
                        "withdrawn": {
                            "cause": "operator",
                            "depends_on": null,
                            "reason": "the day no longer needs it",
                            "operator": "operator-eve"
                        }
                    }
                ]
            }]
        }))
        .expect("the view decodes");

        assert_eq!(
            day_coverage(&view, &journal([integrate("issue-resolved", EvidenceKind::VerificationResult)])),
            DayCoverage::green()
        );
    }

    #[test]
    fn a_landed_member_whose_proof_sits_past_the_first_page_is_green_on_the_full_walk() {
        // A map that only saw the first journal page would miss this proof and
        // hold; the full walk includes it.
        let mut facts: Vec<Value> = (0..100).map(|n| json!({ "other": n })).collect();
        facts.push(integrate("issue-4945", EvidenceKind::VerificationResult));
        let first_page = journal(facts.iter().take(100).cloned());
        let full = journal(facts);
        assert_eq!(day_coverage(&landed(&["issue-4945"]), &first_page), held("issue-4945", &first_page));
        assert_eq!(day_coverage(&landed(&["issue-4945"]), &full), DayCoverage::green());
    }
}
