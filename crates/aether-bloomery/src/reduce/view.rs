//! The outward projection: a self-contained [`ViewDocument`] an adapter can
//! render without querying back into the store (ADR-0149 §The boundary).

use super::Snapshot;
use super::readiness::blocking_ancestor;
use crate::digest::Digest;
use crate::ids::{StageId, WorkpieceId};
use crate::port::{BloomView, ExecutorFaultView, LandingBlock, MemberView, PendingDecisionView, ViewDocument};
use crate::values::Question;

/// Assemble a self-contained [`ViewDocument`] from a snapshot — the pure
/// `Snapshot -> ViewDocument` projection the reconcile port pushes outward
/// (ADR-0149 §The boundary, as amended by [#3471]). Every field an adapter
/// renders rides on the returned document, so the adapter never queries back
/// into the store. Pure: reads the snapshot, allocates a document, mutates
/// nothing.
///
/// Each [`BloomRecord`](crate::BloomRecord) becomes a [`BloomView`] (its sealed-spec id, status,
/// and successor), and each sealed [`crate::Membership`] a [`MemberView`]
/// carrying the member's scope revision, approval evidence, — matched by
/// workpiece from the record's accumulated claims — its resolution claim once
/// integrated (`None` until then), — matched by workpiece from the
/// [`Question`] each open hold resolves to — its pending-decision hold (`None`
/// when the member is not held), its wedge if it has stopped dispatching
/// for good (`None` while it is still working), and — when the sealed graph
/// is holding it out of the line — the ancestor it is waiting on.
///
/// `resolve_question` resolves an open hold's question digest to its
/// [`Question`] bytes, the same injected read-only resolver
/// [`grade`](crate::study_report::grade) uses for study records: the reducer's
/// snapshot holds question *digests*, not the rendered prompt/options or the
/// member the hold binds to, so a snapshot-only signature could carry neither.
/// A hold whose bytes the resolver cannot read (a caller with no artifact
/// access, e.g. the live-query path) surfaces no `pending_decision` on its
/// member, exactly as an unresolvable study record contributes no cost to a
/// grade.
///
/// [#3471]: https://github.com/iamacoffeepot/aether/issues/3471
#[must_use]
pub fn view_of(snapshot: &Snapshot, resolve_question: impl Fn(&Digest) -> Option<Question>) -> ViewDocument {
    let blooms = snapshot
        .blooms
        .values()
        .map(|record| {
            // Resolve each open hold once, then bind it to the member it names —
            // a parked question raises one hold per member, so the map is small.
            let held: Vec<(WorkpieceId, PendingDecisionView)> = record
                .holds
                .iter()
                .filter_map(|digest| {
                    let question = resolve_question(digest)?;
                    Some((
                        question.workpiece.clone(),
                        PendingDecisionView {
                            question: *digest,
                            stage: question.stage,
                            prompt: question.prompt,
                            options: question.options,
                            blocked: question.blocked,
                        },
                    ))
                })
                .collect();
            let members = record
                .spec
                .members()
                .iter()
                .map(|member| MemberView {
                    workpiece: member.workpiece.clone(),
                    scope_revision: member.scope_revision,
                    approval: member.approval.clone(),
                    resolution: record.claims.get(&member.workpiece).cloned(),
                    pending_decision: held
                        .iter()
                        .find(|(workpiece, _)| *workpiece == member.workpiece)
                        .map(|(_, view)| view.clone()),
                    wedge: record.wedged.get(&member.workpiece).copied(),
                    blocked_by: blocking_ancestor(record, &member.workpiece),
                })
                .collect();
            // Rendered only once a landing has actually been refused, so an
            // ordinary bloom's view is unchanged.
            let landing_blocked = (record.landing_rolls > 0).then(|| LandingBlock {
                rolls: record.landing_rolls,
                budget: record.stage_catalog.retry_budget_of(StageId::Land).unwrap_or(1),
            });

            // Rendered only once a review has actually failed to run, so an
            // ordinary bloom's view is unchanged here too.
            let review_budget = record.stage_catalog.retry_budget_of(StageId::AggregateReview).unwrap_or(1);
            let executor_fault = record.aggregate_fault.map(|fault| ExecutorFaultView {
                subject: fault.subject,
                rolls: fault.rolls,
                budget: review_budget,
                evidence: fault.evidence,
                terminal: fault.rolls >= review_budget,
            });

            BloomView {
                id: record.spec.id(),
                status: record.status,
                superseded_by: record.superseded_by,
                members,
                landing_blocked,
                executor_fault,
            }
        })
        .collect();
    ViewDocument {
        mainline: snapshot.mainline,
        observed: snapshot.observed,
        spend_quiesce: snapshot.spend_quiesce.clone(),
        blooms,
    }
}

#[cfg(test)]
mod tests {
    use super::view_of;
    use crate::digest::Digest;
    use crate::ids::{IdempotencyKey, WorkpieceId};
    use crate::reduce::{Event, Fact, Snapshot, reduce};
    use crate::values::{
        BloomDraft, ConfigRegistry, Evidence, EvidenceKind, MemberDependency, Membership, ResolvedConfigs, SpendWindow,
    };

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn membership(name: &str, revision: u8) -> Membership {
        let mut member = Membership {
            workpiece: WorkpieceId(name.into()),
            scope_revision: digest(revision),
            configs: ConfigRegistry::default(),
            approval: Evidence { subject: digest(0), kind: EvidenceKind::Approval, detail: digest(200) },
        };
        member.approval.subject = member.subject();
        member
    }

    // The plausible bug: a dependent waiting on a still-running ancestor
    // renders identically to a working member, so `/view` looks idle for a
    // reason the operator cannot name.
    #[test]
    fn a_dependent_surfaces_its_blocking_ancestor() {
        let spec = BloomDraft {
            proposals: vec![membership("wp-a", 1), membership("wp-b", 2)],
            base: digest(0),
            ..BloomDraft::default()
        }
        .seal();
        let event = Event {
            idempotency_key: IdempotencyKey("seal".into()),
            fact: Fact::GraphSeal {
                predecessor: None,
                spec,
                edges: vec![MemberDependency {
                    member: WorkpieceId("wp-b".into()),
                    depends_on: WorkpieceId("wp-a".into()),
                }],
            },
        };
        let snapshot = Snapshot::new(digest(0));
        let snapshot = snapshot.apply(
            &event,
            &reduce(&snapshot, &event, &ResolvedConfigs::default(), &SpendWindow::default()),
            &ResolvedConfigs::default(),
        );

        let view = view_of(&snapshot, |_| None);
        let members = &view.blooms[0].members;
        let root = members.iter().find(|member| member.workpiece.0 == "wp-a").expect("root member");
        let dependent = members.iter().find(|member| member.workpiece.0 == "wp-b").expect("dependent member");
        assert_eq!(root.blocked_by, None, "a dispatched root is not waiting");
        assert_eq!(
            dependent.blocked_by,
            Some(WorkpieceId("wp-a".into())),
            "the held dependent names the ancestor the operator has to wait on",
        );
    }
}
