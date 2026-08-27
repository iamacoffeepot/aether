//! The day's coverage map, derived from the live view and the journal.
//!
//! A landed workpiece is covered only by an `Integrate` claim whose evidence
//! is a `VerificationResult`. Inherited or fixture evidence, or no integrate
//! fact at all, leaves the workpiece on the hold list. Superseded blooms never
//! reached main and are not required, and neither is a member the day withdrew:
//! it left the line before integration, so no receipt for it can ever exist.

use std::collections::BTreeSet;

use aether_bloomery::{BloomStatus, EvidenceKind, Fact, JournalView, ViewDocument};

/// Standing landed workpieces minus those with a `VerificationResult` integrate claim.
pub fn day_coverage(view: &ViewDocument, journal: &JournalView) -> aether_bloomery_git::DayCoverage {
    let required: BTreeSet<String> = view
        .blooms
        .iter()
        .filter(|bloom| bloom.status == BloomStatus::Landed)
        .flat_map(|bloom| {
            bloom.members.iter().filter(|member| member.withdrawn.is_none()).map(|member| member.workpiece.0.clone())
        })
        .collect();
    let covered: BTreeSet<String> =
        journal.records.iter().filter_map(|record| integrate_workpiece(&record.event.fact)).collect();

    if required.is_subset(&covered) {
        aether_bloomery_git::DayCoverage::green()
    } else {
        let members = required.difference(&covered).cloned().collect::<Vec<_>>().join("\n");
        aether_bloomery_git::DayCoverage::hold(format!("{members}\n{}", evaluated_trailer(journal)))
    }
}

fn evaluated_trailer(journal: &JournalView) -> String {
    let shown = journal.shown;
    journal.journal_span().map_or_else(
        || format!("evaluated journal: {shown} records"),
        |(first, last)| format!("evaluated journal: {shown} records, sequences {first}..={last}"),
    )
}

fn integrate_workpiece(fact: &Fact) -> Option<String> {
    match fact {
        Fact::Integrate { claim, .. } if claim.evidence.kind == EvidenceKind::VerificationResult => {
            Some(claim.workpiece.0.clone())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::day_coverage;
    use crate::bloom::dto::{test_bloom, test_member, test_view};
    use aether_bloomery::{
        BloomStatus, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, JournalEntry, JournalView, Outcome,
        ResolutionClaim, WithdrawnView, WorkpieceId,
    };
    use aether_bloomery_git::DayCoverage;

    fn digest(byte: u8) -> Digest {
        Digest::from_bytes([byte; 32])
    }

    fn landed(workpieces: &[&str]) -> aether_bloomery::ViewDocument {
        blooms([(BloomStatus::Landed, workpieces)])
    }

    fn blooms<const N: usize>(entries: [(BloomStatus, &[&str]); N]) -> aether_bloomery::ViewDocument {
        test_view(
            digest(1),
            digest(2),
            entries
                .into_iter()
                .enumerate()
                .map(|(index, (status, workpieces))| {
                    test_bloom(
                        digest(u8::try_from(index).expect("few blooms")),
                        status,
                        workpieces.iter().map(|workpiece| test_member(workpiece, digest(7))).collect(),
                    )
                })
                .collect(),
        )
    }

    fn journal(facts: impl IntoIterator<Item = Fact>) -> JournalView {
        let records: Vec<_> = facts
            .into_iter()
            .enumerate()
            .map(|(index, fact)| JournalEntry {
                sequence: u64::try_from(index).expect("few"),
                idempotency_key: "k".to_owned(),
                event: Event { idempotency_key: IdempotencyKey("k".to_owned()), fact },
                outcome: Outcome::Duplicate,
                decider: "test".to_owned(),
            })
            .collect();
        let shown = u64::try_from(records.len()).unwrap_or(u64::MAX);
        JournalView { records, total_matched: shown, shown, truncated: false, next_from_sequence: None, notice: None }
    }

    fn held(members: &str, journal: &JournalView) -> DayCoverage {
        DayCoverage::hold(format!("{members}\n{}", super::evaluated_trailer(journal)))
    }

    fn integrate(workpiece: &str, kind: EvidenceKind) -> Fact {
        Fact::Integrate {
            bloom: aether_bloomery::BloomId(digest(1)),
            claim: ResolutionClaim {
                workpiece: WorkpieceId(workpiece.to_owned()),
                scope_revision: digest(7),
                candidate: digest(8),
                evidence: Evidence { subject: digest(8), kind, detail: digest(9) },
            },
        }
    }

    #[test]
    fn a_landed_member_with_a_verification_result_is_green() {
        assert_eq!(
            day_coverage(
                &landed(&["issue-4945"]),
                &journal([integrate("issue-4945", EvidenceKind::VerificationResult)])
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
        // integrated and so can never hold an integrate claim.
        let mut withdrawn = test_member("issue-withdrawn", digest(7));
        withdrawn.withdrawn = Some(WithdrawnView {
            cause: "operator".to_owned(),
            depends_on: None,
            reason: "the day no longer needs it".to_owned(),
            operator: "operator-eve".to_owned(),
        });
        let view = test_view(
            digest(1),
            digest(2),
            vec![test_bloom(digest(3), BloomStatus::Landed, vec![test_member("issue-resolved", digest(7)), withdrawn])],
        );
        assert_eq!(
            day_coverage(&view, &journal([integrate("issue-resolved", EvidenceKind::VerificationResult)])),
            DayCoverage::green()
        );
    }
}
