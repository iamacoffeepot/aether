//! The outward projection: a self-contained [`ViewDocument`] an adapter can
//! render without querying back into the store (ADR-0149 §The boundary).

use super::Snapshot;
use super::readiness::blocking_ancestor;
use crate::digest::Digest;
use crate::ids::{StageId, WorkpieceId};
use crate::port::{
    BloomView, ExecutorFaultView, HostFaultView, LandingBlock, MemberView, PendingDecisionView, ReviewParkView,
    ViewDocument,
};
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
/// grade. A bloom-scoped [`BloomRecord::review_park`](crate::BloomRecord::review_park)
/// is different: the digest is always projected, and resolved details ride
/// only when the same resolver can read them. The park is never copied onto
/// a member hold.
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
                    host_fault: record
                        .host_faults
                        .get(&member.workpiece)
                        .map(|hold| HostFaultView { findings: hold.findings.clone() }),
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

            // The digest is the recovery key even when the live-query path
            // cannot read the question bytes — unlike a member hold, which
            // degrades to `None`.
            let review_park = record.review_park.map(|question| match resolve_question(&question) {
                Some(resolved) => ReviewParkView {
                    question,
                    stage: Some(resolved.stage),
                    prompt: Some(resolved.prompt),
                    options: resolved.options,
                    blocked: Some(resolved.blocked),
                },
                None => ReviewParkView { question, stage: None, prompt: None, options: Vec::new(), blocked: None },
            });

            BloomView {
                id: record.spec.id(),
                status: record.status,
                superseded_by: record.superseded_by,
                members,
                landing_blocked,
                executor_fault,
                review_park,
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
    use crate::ids::{IdempotencyKey, StageId, WorkpieceId};
    use crate::reduce::{Event, Fact, Snapshot, reduce};
    use crate::values::{
        BloomDraft, ConfigRegistry, Evidence, EvidenceKind, MemberDependency, Membership, Question, ResolvedConfigs,
        SpendWindow,
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

    // The plausible bug: a member waiting on a missing host tool renders
    // identically to one that is still working, so `/view` cannot name the
    // host condition (#5020).
    #[test]
    fn a_host_fault_surfaces_the_preflight_findings() {
        let spec = BloomDraft { proposals: vec![membership("wp", 1)], base: digest(0), ..BloomDraft::default() }.seal();
        let bloom = spec.id();
        let configs = ResolvedConfigs::default();
        let spend = SpendWindow::default();
        let mut snapshot = Snapshot::new(digest(0));
        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        snapshot = snapshot.apply(&seal, &reduce(&snapshot, &seal, &configs, &spend), &configs);

        let construct = Event {
            idempotency_key: IdempotencyKey("c-pass".into()),
            fact: Fact::AttemptCompleted {
                bloom,
                workpiece: WorkpieceId("wp".into()),
                stage: StageId::Construct,
                passed: true,
                evidence: Evidence { subject: digest(1), kind: EvidenceKind::VerificationResult, detail: digest(70) },
                candidate: None,
            },
        };
        snapshot = snapshot.apply(&construct, &reduce(&snapshot, &construct, &configs, &spend), &configs);

        let findings = "Verification did not run.\n\n- `jscpd` — npm install -g jscpd";
        let fault = Event {
            idempotency_key: IdempotencyKey("preflight".into()),
            fact: Fact::VerifyHostFault {
                bloom,
                workpiece: WorkpieceId("wp".into()),
                evidence: Evidence { subject: digest(1), kind: EvidenceKind::VerificationResult, detail: digest(71) },
                findings: findings.to_owned(),
            },
        };
        snapshot = snapshot.apply(&fault, &reduce(&snapshot, &fault, &configs, &spend), &configs);

        let view = view_of(&snapshot, |_| None);
        let hold = view.blooms[0].members[0].host_fault.as_ref().expect("the member is held on the host");
        assert_eq!(hold.findings, findings, "the missing tools are what the operator reads");
        assert!(view.blooms[0].members[0].wedge.is_none(), "a host fault is not a wedge");
    }

    fn sealed(name: &str) -> Snapshot {
        let spec = BloomDraft { proposals: vec![membership(name, 1)], base: digest(0), ..BloomDraft::default() }.seal();
        let snapshot = Snapshot::new(digest(0));
        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        snapshot.apply(
            &seal,
            &reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default()),
            &ResolvedConfigs::default(),
        )
    }

    fn contested_question() -> Question {
        Question {
            stage: StageId::AggregateReview,
            subject: digest(40),
            workpiece: WorkpieceId("wp".into()),
            prompt: "delta-confirm still fails; accept the weave or file a follow-up?".into(),
            options: vec!["accept — land as-is".into(), "defer — file the finding forward".into()],
            blocked: "the bloom cannot land until the owner settles the review".into(),
        }
    }

    // The plausible bug: a live-query path (`resolve_question` is `|_| None`)
    // drops the park the way it drops a member hold, so GET /view of an
    // otherwise idle sealed bloom still looks like nothing is waiting.
    #[test]
    fn a_review_park_surfaces_its_digest_when_the_question_cannot_resolve() {
        let question = digest(51);
        let mut snapshot = sealed("wp");
        let record = snapshot.blooms.values_mut().next().expect("the sealed bloom");
        record.review_park = Some(question);
        record.holds.insert(question);

        let view = view_of(&snapshot, |_| None);
        let park = view.blooms[0].review_park.as_ref().expect("the park is named even without artifact bytes");
        assert_eq!(park.question, question, "the digest is what adjudicate --finding quotes");
        assert_eq!(park.stage, None, "unresolved details stay off the reduced rendering");
        assert_eq!(park.prompt, None);
        assert!(park.options.is_empty());
        assert_eq!(park.blocked, None);
        assert!(
            view.blooms[0].members.iter().all(|member| member.pending_decision.is_none()),
            "an unresolvable ceiling park is not rewritten as a member hold",
        );
    }

    // The plausible bug: resolved park details are copied onto every member
    // as `pending_decision`, so a bloom-scoped question reads as N member
    // holds and the operator reaches for the member-answer route.
    #[test]
    fn a_resolved_review_park_carries_question_details_without_holding_members() {
        let question = contested_question();
        let digest = question.id();
        let mut snapshot = sealed("wp");
        snapshot.blooms.values_mut().next().expect("the sealed bloom").review_park = Some(digest);

        let view = view_of(&snapshot, |asked| (*asked == digest).then(|| question.clone()));
        let park = view.blooms[0].review_park.as_ref().expect("the park is projected");
        assert_eq!(park.question, digest);
        assert_eq!(park.stage, Some(StageId::AggregateReview));
        assert_eq!(park.prompt.as_deref(), Some(question.prompt.as_str()));
        assert_eq!(park.options, question.options);
        assert_eq!(park.blocked.as_deref(), Some(question.blocked.as_str()));
        assert!(
            view.blooms[0].members.iter().all(|member| member.pending_decision.is_none()),
            "the bloom-scoped park is not mapped onto a member pending-decision",
        );
    }

    // The plausible bug: a member-scope park is copied onto the bloom as a
    // review park, so status names an aggregate-review hold that is not
    // there and prints an adjudicate line for a question that is not
    // adjudicable through the bloom-scope door.
    #[test]
    fn a_member_question_does_not_project_as_a_review_park() {
        let spec = BloomDraft {
            proposals: vec![membership("wp-held", 1), membership("wp-free", 2)],
            base: digest(0),
            ..BloomDraft::default()
        }
        .seal();
        let bloom = spec.id();
        let mut snapshot = Snapshot::new(digest(0));
        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        snapshot = snapshot.apply(
            &seal,
            &reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default()),
            &ResolvedConfigs::default(),
        );

        let question = Question {
            stage: StageId::Construct,
            subject: digest(50),
            workpiece: WorkpieceId("wp-held".into()),
            prompt: "which approach?".into(),
            options: vec!["A".into(), "B".into()],
            blocked: "construct is held".into(),
        };
        let question_digest = question.id();
        let evidence = Evidence { subject: digest(50), kind: EvidenceKind::Question, detail: question_digest };
        let admit =
            Event { idempotency_key: IdempotencyKey("park-1".into()), fact: Fact::AdmitEvidence { bloom, evidence } };
        snapshot = snapshot.apply(
            &admit,
            &reduce(&snapshot, &admit, &ResolvedConfigs::default(), &SpendWindow::default()),
            &ResolvedConfigs::default(),
        );

        let view = view_of(&snapshot, |asked| (*asked == question_digest).then(|| question.clone()));
        assert!(view.blooms[0].review_park.is_none(), "a member question is not a bloom-scoped park");
        let held = view.blooms[0].members.iter().find(|member| member.workpiece.0 == "wp-held").expect("held member");
        assert!(held.pending_decision.is_some(), "the member hold still projects as itself");
    }
}
