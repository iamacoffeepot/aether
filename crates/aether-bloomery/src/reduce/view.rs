//! The outward projection: a self-contained [`ViewDocument`] an adapter can
//! render without querying back into the store (ADR-0149 §The boundary).

use super::readiness::blocking_ancestor;
use super::{AwaitingSurface, BloomRecord, BloomStatus, LeaseEviction, Snapshot};
use crate::digest::Digest;
use crate::ids::{StageId, WorkpieceId};
use crate::port::{
    AwaitingSurfaceView, BaseAlertView, BloomView, CompositionCursorView, CompositionView, ExecutorFaultView,
    HostFaultView, LandingBlock, LeaseEvictionView, LeaseView, MemberView, PendingDecisionView, ReviewParkView,
    ViewDocument, WedgeCause, WithdrawnView,
};
use crate::values::BaseVerdict;
use crate::values::{Question, Withdrawal, WithdrawalCause};

/// Assemble a self-contained [`ViewDocument`] from a snapshot — the pure
/// `Snapshot -> ViewDocument` projection the reconcile port pushes outward
/// (ADR-0149 §The boundary, as amended by [#3471]). Every field an adapter
/// renders rides on the returned document, so the adapter never queries back
/// into the store. Pure: reads the snapshot, allocates a document, mutates
/// nothing.
///
/// Each [`BloomRecord`] becomes a [`BloomView`] (its sealed-spec id, status,
/// and successor), and each sealed [`crate::Membership`] a [`MemberView`]
/// carrying the member's scope revision, approval evidence, — matched by
/// workpiece from the record's accumulated claims — its resolution claim once
/// integrated (`None` until then), — matched by workpiece from the
/// [`Question`] each open hold resolves to — its pending-decision hold (`None`
/// when the member is not held), a construct-declined park (#5292) when the
/// snapshot holds one (so a live-query path that cannot resolve a Question
/// still names the refusal), its wedge if it has stopped dispatching
/// for good (`None` while it is still working), — when the sealed graph
/// is holding it out of the line — the ancestor it is waiting on, and — from
/// [`BloomRecord::progress`] — its stage cursor (`None` until it has been
/// dispatched). The composition workpiece is not a sealed member: its cursor,
/// wedge, and open findings ride on [`BloomView::composition`] so a
/// refine-budget stop is visible without attaching it to every member. A
/// bloom-level [`BloomRecord::operator_hold`] rides on the bloom so a braked
/// one is not indistinguishable from an idle one.
///
/// `resolve_question` resolves an open hold's question digest to its
/// [`Question`] bytes, the same injected read-only resolver
/// [`grade`](crate::study_report::grade) uses for study records: the reducer's
/// snapshot holds question *digests*, not the rendered prompt/options or the
/// member the hold binds to, so a snapshot-only signature could carry neither.
/// A hold whose bytes the resolver cannot read (a caller with no artifact
/// access, e.g. the live-query path) surfaces no `pending_decision` on its
/// member, exactly as an unresolvable study record contributes no cost to a
/// grade. A bloom-scoped [`BloomRecord::review_park`]
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
            let held = held_decisions(record, &resolve_question);
            BloomView {
                id: record.spec.id(),
                status: record.status,
                superseded_by: record.superseded_by,
                members: member_views(record, snapshot, &held),
                landing_blocked: landing_block(record),
                executor_fault: executor_fault_view(record),
                review_park: review_park_view(record, &resolve_question),
                composition: composition_view(record),
                operator_hold: record.operator_hold.clone(),
                blocker: snapshot.fold_refusal(&record.spec.id()).cloned(),
                leases: lease_views(record, snapshot),
            }
        })
        .collect();
    ViewDocument {
        mainline: snapshot.mainline,
        observed: snapshot.observed,
        spend_quiesce: snapshot.spend_quiesce.clone(),
        blooms,
        base_alert: base_alert_of(snapshot),
    }
}

/// The red receipt whose base is the sealed bloom's base, or — with no sealed
/// bloom — the red receipt for `snapshot.observed`.
fn base_alert_of(snapshot: &Snapshot) -> Option<BaseAlertView> {
    let sealed = snapshot.blooms.values().find(|record| record.status == BloomStatus::Sealed);
    let base = sealed.map_or(snapshot.observed, |record| record.spec.base());
    let receipt = snapshot.base_receipt_for(base)?;
    let BaseVerdict::Red { evidence, failed } = &receipt.verdict else {
        return None;
    };
    Some(BaseAlertView::from_failure_set(receipt.base, receipt.tree, *failed, evidence.detail))
}

/// Resolve each open hold once, then bind it to the member it names — a parked
/// question raises one hold per member, so the map is small.
fn held_decisions(
    record: &BloomRecord,
    resolve_question: impl Fn(&Digest) -> Option<Question>,
) -> Vec<(WorkpieceId, PendingDecisionView)> {
    record
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
        .collect()
}

fn member_views(
    record: &BloomRecord,
    snapshot: &Snapshot,
    held: &[(WorkpieceId, PendingDecisionView)],
) -> Vec<MemberView> {
    record
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
            machinery_rolls: snapshot
                .member_machinery(&record.spec.id(), &member.workpiece)
                .map_or(0, |fault| fault.rolls),
            machinery_budget: record
                .progress
                .get(&member.workpiece)
                .map(|cursor| cursor.stage)
                .or_else(|| record.wedged.get(&member.workpiece).map(|wedge| wedge.stage))
                .map_or(0, |stage| record.stage_catalog.retry_budget_of(stage).unwrap_or(1)),
            wedge_cause: record.wedged.get(&member.workpiece).map(|wedge| {
                let budget = record.stage_catalog.retry_budget_of(wedge.stage).unwrap_or(1);
                match snapshot.member_machinery(&record.spec.id(), &member.workpiece) {
                    Some(fault) if fault.stage == wedge.stage && fault.rolls >= budget => WedgeCause::Machinery,
                    _ => WedgeCause::Work,
                }
            }),
            cursor: stage_cursor(record, &member.workpiece),
            park: snapshot.member_park(&record.spec.id(), &member.workpiece).copied(),
            awaiting_surface: snapshot
                .awaiting_surface(&record.spec.id(), &member.workpiece)
                .map(awaiting_surface_view),
            withdrawn: record.withdrawn.get(&member.workpiece).map(withdrawn_view),
            leases: snapshot.leases_held(&record.spec.id(), &member.workpiece),
            evicted_by: snapshot.lease_eviction(&record.spec.id(), &member.workpiece).map(lease_eviction_view),
        })
        .collect()
}

/// Render a lease eviction so an operator reads which sibling took which file
/// without opening the journal (ADR-0204).
/// Every lease the bloom's lanes hold, in path order (ADR-0204 / ADR-0198).
///
/// Path order rather than member order because the operator arrives at this
/// table holding a path: a contended file is what an eviction names, and the
/// table exists so that path can be looked up rather than searched for.
fn lease_views(record: &BloomRecord, snapshot: &Snapshot) -> Vec<LeaseView> {
    snapshot
        .file_leases
        .get(&record.spec.id())
        .into_iter()
        .flatten()
        .map(|(path, lease)| LeaseView {
            path: path.clone(),
            holder: lease.holder.clone(),
            stage: record.progress.get(&lease.holder).map(|cursor| cursor.stage),
            acquired_at: lease.acquired_at,
        })
        .collect()
}

fn lease_eviction_view(eviction: &LeaseEviction) -> LeaseEvictionView {
    LeaseEvictionView { by: eviction.by.clone(), path: eviction.path.clone(), evicted_at: eviction.evicted_at }
}

/// Render a member's surface request so an operator reads which paths are
/// needed without opening the evidence file (ADR-0207).
fn awaiting_surface_view(awaiting: &AwaitingSurface) -> AwaitingSurfaceView {
    AwaitingSurfaceView {
        stage: awaiting.stage,
        scope_revision: awaiting.request.scope_revision,
        evidence: awaiting.evidence,
        paths: awaiting.request.paths.clone(),
        summary: awaiting.request.summary.clone(),
        requests: awaiting.requests,
    }
}

/// Render a withdrawn member so the board can tell it from one still working
/// without opening the journal (#5327).
fn withdrawn_view(withdrawal: &Withdrawal) -> WithdrawnView {
    let (cause, depends_on) = match &withdrawal.cause {
        WithdrawalCause::Operator => ("operator", None),
        WithdrawalCause::Dependency { on } => ("dependency", Some(on.clone())),
    };
    WithdrawnView {
        cause: cause.into(),
        depends_on,
        reason: withdrawal.reason.clone(),
        operator: withdrawal.operator.clone(),
    }
}

/// Rendered only once a landing has actually been refused, so an ordinary
/// bloom's view is unchanged.
fn landing_block(record: &BloomRecord) -> Option<LandingBlock> {
    (record.landing_rolls > 0).then(|| LandingBlock {
        rolls: record.landing_rolls,
        budget: record.stage_catalog.retry_budget_of(StageId::Land).unwrap_or(1),
    })
}

/// Rendered only once a review has actually failed to run, so an ordinary
/// bloom's view is unchanged here too.
fn executor_fault_view(record: &BloomRecord) -> Option<ExecutorFaultView> {
    let review_budget = record.stage_catalog.retry_budget_of(StageId::AggregateReview).unwrap_or(1);
    record.aggregate_fault.map(|fault| ExecutorFaultView {
        subject: fault.subject,
        rolls: fault.rolls,
        budget: review_budget,
        evidence: fault.evidence,
        terminal: fault.rolls >= review_budget,
    })
}

/// The digest is the recovery key even when the live-query path cannot read
/// the question bytes — unlike a member hold, which degrades to `None`.
fn review_park_view(
    record: &BloomRecord,
    resolve_question: impl Fn(&Digest) -> Option<Question>,
) -> Option<ReviewParkView> {
    record.review_park.map(|question| match resolve_question(&question) {
        Some(resolved) => ReviewParkView {
            question,
            stage: Some(resolved.stage),
            prompt: Some(resolved.prompt),
            options: resolved.options,
            blocked: Some(resolved.blocked),
        },
        None => ReviewParkView { question, stage: None, prompt: None, options: Vec::new(), blocked: None },
    })
}

/// Rendered only once the composition has a cursor, a wedge, or an open
/// finding, so an ordinary bloom's view is unchanged here too.
fn composition_view(record: &BloomRecord) -> Option<CompositionView> {
    let cursor = stage_cursor(record, &WorkpieceId::composition());
    let wedge = record.wedged.get(&WorkpieceId::composition()).copied();
    let findings = record.open_composition_findings().cloned().collect::<Vec<_>>();
    (cursor.is_some() || wedge.is_some() || !findings.is_empty()).then_some(CompositionView { cursor, wedge, findings })
}

/// The operator-facing cursor: stage, attempts, candidate. `None` until the
/// workpiece has been dispatched — dependents waiting on an ancestor stay off
/// [`BloomRecord::progress`] until they enter the line.
fn stage_cursor(record: &BloomRecord, workpiece: &WorkpieceId) -> Option<CompositionCursorView> {
    record.progress.get(workpiece).map(|progress| CompositionCursorView {
        stage: progress.stage,
        attempts: progress.attempts,
        candidate: progress.candidate,
    })
}

#[cfg(test)]
mod tests {
    use aether_data::wire::{from_bytes, to_vec};

    use super::view_of;
    use crate::digest::Digest;
    use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
    use crate::port::{BloomView, ViewDocument, WedgeCause};
    use crate::reduce::{BloomStatus, Event, Fact, Outcome, RecordedRead, RecordedRefusal, Snapshot, reduce};
    use crate::values::{
        BloomDraft, CandidateRef, ConfigRegistry, Evidence, EvidenceKind, MemberDependency, Membership, OperatorHold,
        Question, ResolutionClaim, ResolvedConfigs, SpendWindow, VerifyFailureSet, Wedge,
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
        let snapshot = Snapshot::new(digest(0)).with_green_base(digest(0));
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
        let mut snapshot = Snapshot::new(digest(0)).with_green_base(digest(0));
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

    // The plausible bug: a parked construct and a wedged construct render the
    // same on `/view`, so the operator reaches for `grant` instead of the
    // declared surface (#5292).
    #[test]
    fn a_parked_construct_is_distinguishable_from_a_wedged_one() {
        let spec = BloomDraft { proposals: vec![membership("wp", 1)], base: digest(0), ..BloomDraft::default() }.seal();
        let bloom = spec.id();
        let reason = digest(91);
        let mut parked = Snapshot::new(digest(0)).with_green_base(digest(0));
        parked = step(&parked, &event("seal-p", Fact::Seal(spec.clone()))).0;
        parked = step(
            &parked,
            &event(
                "decline",
                Fact::AttemptCompleted {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    stage: StageId::Construct,
                    passed: false,
                    evidence: Evidence { subject: digest(1), kind: EvidenceKind::ConstructDeclined, detail: reason },
                    candidate: None,
                },
            ),
        )
        .0;
        let parked_view = view_of(&parked, |_| None).blooms[0].members[0].clone();
        let park = parked_view.park.as_ref().expect("the park is on the served view");
        assert_eq!(park.evidence, reason, "the park names the lane's evidence");
        assert_eq!(park.stage, StageId::Construct);
        assert!(parked_view.pending_decision.is_none(), "a park is not an ADR-0151 question");
        assert!(parked_view.wedge.is_none(), "a parked member is not wedged");

        let mut wedged = Snapshot::new(digest(0)).with_green_base(digest(0));
        wedged = step(&wedged, &event("seal-w", Fact::Seal(spec))).0;
        for (key, detail) in [("c-die-1", 70_u8), ("c-die-2", 71)] {
            wedged = step(
                &wedged,
                &event(
                    key,
                    Fact::AttemptCompleted {
                        bloom,
                        workpiece: WorkpieceId("wp".into()),
                        stage: StageId::Construct,
                        passed: false,
                        evidence: Evidence {
                            subject: digest(1),
                            kind: EvidenceKind::VerificationResult,
                            detail: digest(detail),
                        },
                        candidate: None,
                    },
                ),
            )
            .0;
        }
        let wedged_view = view_of(&wedged, |_| None).blooms[0].members[0].clone();
        assert!(wedged_view.wedge.is_some(), "a budget-spent construct still wedges");
        assert!(wedged_view.pending_decision.is_none(), "a wedge is not a park");
    }

    // The plausible bug: a member that exhausted its machinery budget renders
    // identically to one that exhausted its work budget, so `/view` cannot tell
    // a sick host from rejected work (ADR-0195).
    #[test]
    fn a_machinery_wedge_surfaces_its_cause_and_roll() {
        let spec = BloomDraft { proposals: vec![membership("wp", 1)], base: digest(0), ..BloomDraft::default() }.seal();
        let bloom = spec.id();
        let configs = ResolvedConfigs::default();
        let spend = SpendWindow::default();
        let mut snapshot = Snapshot::new(digest(0)).with_green_base(digest(0));
        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        snapshot = snapshot.apply(&seal, &reduce(&snapshot, &seal, &configs, &spend), &configs);

        let captured = CandidateRef { tree: digest(21), checkout: digest(22) };
        let construct = Event {
            idempotency_key: IdempotencyKey("c-pass".into()),
            fact: Fact::AttemptCompleted {
                bloom,
                workpiece: WorkpieceId("wp".into()),
                stage: StageId::Construct,
                passed: true,
                evidence: Evidence { subject: digest(21), kind: EvidenceKind::VerificationResult, detail: digest(70) },
                candidate: Some(captured),
            },
        };
        snapshot = snapshot.apply(&construct, &reduce(&snapshot, &construct, &configs, &spend), &configs);

        for (key, detail) in [("f1", 60), ("f2", 61), ("f3", 62)] {
            let fault = Event {
                idempotency_key: IdempotencyKey(key.into()),
                fact: Fact::MemberExecutorFault {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    stage: StageId::Verify,
                    evidence: Evidence {
                        subject: digest(21),
                        kind: EvidenceKind::ExecutorFault,
                        detail: digest(detail),
                    },
                },
            };
            snapshot = snapshot.apply(&fault, &reduce(&snapshot, &fault, &configs, &spend), &configs);
        }

        let view = view_of(&snapshot, |_| None);
        let member = &view.blooms[0].members[0];
        assert_eq!(member.machinery_rolls, 3, "the operator can see how many times the host failed");
        assert_eq!(member.machinery_budget, 3, "and the sealed bound those faults spent");
        assert_eq!(member.wedge_cause, Some(WedgeCause::Machinery), "the wedge names the host, not the work");
        assert!(member.wedge.is_some(), "the member is wedged");
    }

    fn sealed(name: &str) -> Snapshot {
        let spec = BloomDraft { proposals: vec![membership(name, 1)], base: digest(0), ..BloomDraft::default() }.seal();
        let snapshot = Snapshot::new(digest(0)).with_green_base(digest(0));
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

    fn following_bloom() -> BloomView {
        BloomView {
            id: BloomId(digest(16)),
            status: BloomStatus::Sealed,
            superseded_by: None,
            members: Vec::new(),
            landing_blocked: None,
            executor_fault: None,
            review_park: None,
            composition: None,
            operator_hold: None,
            blocker: None,
            leases: Vec::new(),
        }
    }

    // The plausible bug: a fold that refuses leaves the bloom Sealed with every
    // member resolved and every blocker field null, so `/view` cannot say why
    // the bloom is not landing (ADR-0206).
    #[test]
    fn a_fold_that_refuses_surfaces_the_guard_and_the_member() {
        let spec =
            BloomDraft { proposals: vec![membership("wp-0", 1)], base: digest(0), ..BloomDraft::default() }.seal();
        let bloom = spec.id();
        let configs = ResolvedConfigs::default();
        let spend = SpendWindow::default();
        let mut snapshot = Snapshot::new(digest(0)).with_green_base(digest(0));
        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        snapshot = snapshot.apply(&seal, &reduce(&snapshot, &seal, &configs, &spend), &configs);

        let refusal = RecordedRefusal {
            gate: "fold".into(),
            guard: "candidate_ref_present".into(),
            reads: vec![
                RecordedRead { field: "member".into(), value: "wp-0".into() },
                RecordedRead { field: "predecessor".into(), value: "aa".into() },
            ],
        };
        let refused = Event {
            idempotency_key: IdempotencyKey("fold-refused".into()),
            fact: Fact::FoldRefused { bloom, refusal },
        };
        snapshot = snapshot.apply(&refused, &reduce(&snapshot, &refused, &configs, &spend), &configs);

        let view = view_of(&snapshot, |_| None);
        let blocker = view.blooms[0].blocker.as_ref().expect("the served view carries the refusal");
        assert_eq!(blocker.guard, "candidate_ref_present");
        assert_eq!(blocker.reads[0].field, "member");
        assert_eq!(blocker.reads[0].value, "wp-0");
        assert_eq!(view.blooms[0].status, BloomStatus::Sealed);
        assert!(view.blooms[0].landing_blocked.is_none());
        assert!(view.blooms[0].members[0].blocked_by.is_none());
    }

    fn with_following_bloom(mut view: ViewDocument) -> ViewDocument {
        view.blooms.push(following_bloom());
        view
    }

    fn wire_round_trip(view: &ViewDocument) -> ViewDocument {
        from_bytes(&to_vec(view).expect("the projection wire-encodes")).expect("and decodes back")
    }

    fn park_json(view: &ViewDocument) -> serde_json::Value {
        serde_json::to_value(view).expect("the projection has a JSON form")["blooms"][0]["review_park"].clone()
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
        let mut snapshot = Snapshot::new(digest(0)).with_green_base(digest(0));
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

    // The plausible bug: QueryResult nests ViewDocument as raw wire bytes,
    // so a projection type without Schema evades the Kind-level
    // skip_serializing_if guard and GET /view 500s on omitted slots.
    const _: fn() = || {
        let _ = <ViewDocument as aether_data::Schema>::SCHEMA;
    };

    // The plausible bug: skip_serializing_if drops None/empty ReviewParkView
    // fields on the positional wire, so the next bloom's first byte (16) is
    // read as an Option presence marker and GET /view 500s with
    // `invalid bool/presence byte 16`.
    #[test]
    fn an_unresolved_review_park_round_trips_ahead_of_the_next_bloom() {
        let question = digest(51);
        let mut snapshot = sealed("wp");
        snapshot.blooms.values_mut().next().expect("the sealed bloom").review_park = Some(question);

        let view = with_following_bloom(view_of(&snapshot, |_| None));
        let decoded = wire_round_trip(&view);
        assert_eq!(decoded, view, "omitted park slots must not steal the next bloom's bytes");

        let park = park_json(&decoded);
        assert_eq!(park["question"], serde_json::to_value(question).expect("a digest has a JSON form"));
        assert!(park["stage"].is_null(), "unresolved details stay empty in the JSON rendering");
        assert!(park["prompt"].is_null());
        assert_eq!(park["options"], serde_json::json!([]));
        assert!(park["blocked"].is_null());
    }

    // The plausible bug: making the park's wire fields total drops the
    // resolved prompt/options/blocked from the JSON GET /view still renders.
    #[test]
    fn a_resolved_review_park_round_trips_with_its_json_details() {
        let question = contested_question();
        let digest = question.id();
        let mut snapshot = sealed("wp");
        snapshot.blooms.values_mut().next().expect("the sealed bloom").review_park = Some(digest);

        let view = with_following_bloom(view_of(&snapshot, |asked| (*asked == digest).then(|| question.clone())));
        let decoded = wire_round_trip(&view);
        assert_eq!(decoded, view);

        let park = park_json(&decoded);
        assert_eq!(park["question"], serde_json::to_value(digest).expect("a digest has a JSON form"));
        assert_eq!(park["stage"], "AggregateReview");
        assert_eq!(park["prompt"], question.prompt);
        assert_eq!(park["options"], serde_json::to_value(&question.options).expect("options have a JSON form"));
        assert_eq!(park["blocked"], question.blocked);
    }

    fn step(snapshot: &Snapshot, event: &Event) -> (Snapshot, Outcome) {
        let configs = ResolvedConfigs::default();
        let spend = SpendWindow::default();
        let decisions = reduce(snapshot, event, &configs, &spend);
        (snapshot.apply(event, &decisions, &configs), decisions.outcome)
    }

    fn claim(name: &str, revision: u8, candidate: u8) -> ResolutionClaim {
        ResolutionClaim {
            workpiece: WorkpieceId(name.into()),
            scope_revision: digest(revision),
            candidate: digest(candidate),
            evidence: Evidence { subject: digest(candidate), kind: EvidenceKind::ResolutionClaim, detail: digest(200) },
        }
    }

    fn event(key: &str, fact: Fact) -> Event {
        Event { idempotency_key: IdempotencyKey(key.into()), fact }
    }

    // The plausible bug (issue 5138): a composition that exhausted its Refine
    // budget records CompositionWedged + a RecordWedge + an open finding, but
    // /view still renders status Sealed with every member integrated and no
    // park, so the operator cannot see the stop or the digest adjudicate needs.
    #[test]
    fn a_composition_wedge_surfaces_its_wedge_and_open_finding() {
        let spec = BloomDraft { proposals: vec![membership("wp", 1)], base: digest(0), ..BloomDraft::default() }.seal();
        let bloom = spec.id();
        let mut snapshot = Snapshot::new(digest(0)).with_green_base(digest(0));
        snapshot = step(&snapshot, &event("seal", Fact::Seal(spec))).0;
        snapshot = step(&snapshot, &event("integrate", Fact::Integrate { bloom, claim: claim("wp", 1, 10) })).0;
        snapshot = step(
            &snapshot,
            &event("resolve", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: Vec::new() }),
        )
        .0;

        let failed_verify = event(
            "verify-fail",
            Fact::AggregateVerifyCompleted {
                bloom,
                passed: false,
                evidence: Evidence { subject: digest(40), kind: EvidenceKind::VerificationResult, detail: digest(52) },
            },
        );
        snapshot = step(&snapshot, &failed_verify).0;

        let fail_refine = |key: &str, detail: u8| {
            event(
                key,
                Fact::AttemptCompleted {
                    bloom,
                    workpiece: WorkpieceId::composition(),
                    stage: StageId::Refine,
                    passed: false,
                    evidence: Evidence {
                        subject: digest(40),
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(detail),
                    },
                    candidate: None,
                },
            )
        };
        let mut outcome = None;
        let mut last_detail = 70;
        for (index, detail) in [(1_u8, 70_u8), (2, 71), (3, 72), (4, 73)] {
            last_detail = detail;
            let (next, next_outcome) = step(&snapshot, &fail_refine(&format!("refine-{index}"), detail));
            snapshot = next;
            if matches!(next_outcome, Outcome::CompositionWedged { .. }) {
                outcome = Some(next_outcome);
                break;
            }
        }
        assert!(
            matches!(outcome, Some(Outcome::CompositionWedged { refused_at: StageId::Refine, .. })),
            "failed refine completions spend the budget: {outcome:?}"
        );

        let view = with_following_bloom(view_of(&snapshot, |_| None));
        let decoded = wire_round_trip(&view);
        assert_eq!(decoded, view, "composition slots must not steal the next bloom's bytes");

        let composition = view.blooms[0].composition.as_ref().expect("the composition line is on the bloom");
        assert_eq!(
            composition.wedge,
            Some(Wedge {
                stage: StageId::Refine,
                evidence: digest(last_detail),
                repeated_verifiers: VerifyFailureSet::EMPTY,
            }),
            "the wedge is the same shape a member wedge renders",
        );
        assert!(
            composition.findings.iter().any(|finding| finding.detail == digest(52)),
            "the open finding digest is what adjudicate --finding quotes",
        );
        let cursor = composition.cursor.as_ref().expect("the composition still names the stage it stopped at");
        assert_eq!(cursor.stage, StageId::Refine);
        assert!(view.blooms[0].review_park.is_none(), "a refine-budget wedge is not a review park");
        assert!(
            view.blooms[0].members.iter().all(|member| member.wedge.is_none()),
            "the stop is the composition's, not a member's",
        );
    }

    // The plausible bug: MemberView has no cursor, so an operator reading
    // /view cannot tell which stage a dispatched member sits at without
    // scanning /journal outcomes newest-first.
    #[test]
    fn a_dispatched_member_surfaces_its_cursor_and_a_waiting_one_does_not() {
        let spec = BloomDraft {
            proposals: vec![membership("wp-a", 1), membership("wp-b", 2)],
            base: digest(0),
            ..BloomDraft::default()
        }
        .seal();
        let bloom = spec.id();
        let mut snapshot = Snapshot::new(digest(0)).with_green_base(digest(0));
        snapshot = step(
            &snapshot,
            &event(
                "seal",
                Fact::GraphSeal {
                    predecessor: None,
                    spec,
                    edges: vec![MemberDependency {
                        member: WorkpieceId("wp-b".into()),
                        depends_on: WorkpieceId("wp-a".into()),
                    }],
                },
            ),
        )
        .0;

        let captured = CandidateRef { tree: digest(21), checkout: digest(22) };
        snapshot = step(
            &snapshot,
            &event(
                "c-pass",
                Fact::AttemptCompleted {
                    bloom,
                    workpiece: WorkpieceId("wp-a".into()),
                    stage: StageId::Construct,
                    passed: true,
                    evidence: Evidence {
                        subject: digest(21),
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(70),
                    },
                    candidate: Some(captured),
                },
            ),
        )
        .0;

        let view = view_of(&snapshot, |_| None);
        let members = &view.blooms[0].members;
        let root = members.iter().find(|member| member.workpiece.0 == "wp-a").expect("root member");
        let dependent = members.iter().find(|member| member.workpiece.0 == "wp-b").expect("dependent member");
        let cursor = root.cursor.as_ref().expect("a member with dispatch history names its stage");
        assert_eq!(cursor.stage, StageId::Verify, "the cursor is the record's, not a hardcoded entry stage");
        assert_eq!(cursor.attempts, 1);
        assert_eq!(cursor.candidate, Some(captured));
        assert_eq!(dependent.cursor, None, "a member that has never entered the line has no cursor");
    }

    // The plausible bug: a held bloom and an idle one render identically, so
    // /view cannot name the brake an operator already pulled.
    #[test]
    fn a_held_bloom_surfaces_its_hold_and_a_release_clears_it() {
        let mut snapshot = sealed("wp");
        let bloom = snapshot.blooms.keys().copied().next().expect("the sealed bloom");
        let hold =
            OperatorHold { reason: "the fixture stall is not going to clear".into(), operator: "iamacoffeepot".into() };

        snapshot = step(&snapshot, &event("hold", Fact::OperatorHold { bloom, hold: hold.clone() })).0;
        let view = with_following_bloom(view_of(&snapshot, |_| None));
        let decoded = wire_round_trip(&view);
        assert_eq!(decoded, view, "the hold slot must not steal the next bloom's bytes");
        assert_eq!(view.blooms[0].operator_hold.as_ref(), Some(&hold), "a braked bloom names who pulled it and why");

        snapshot = step(&snapshot, &event("release", Fact::OperatorRelease { bloom, release: hold })).0;
        let released = view_of(&snapshot, |_| None);
        assert_eq!(released.blooms[0].operator_hold, None, "releasing clears the projection");
    }

    // The plausible bug: the new slots are required on the JSON document, so a
    // reader that predates them (or a fixture written before they existed)
    // fails to decode.
    #[test]
    fn a_document_without_the_new_fields_still_decodes() {
        let view = view_of(&sealed("wp"), |_| None);
        let mut json = serde_json::to_value(&view).expect("the projection has a JSON form");
        let bloom = json["blooms"][0].as_object_mut().expect("a bloom object");
        assert!(bloom.remove("operator_hold").is_some(), "the live document carries the new hold slot");
        let member = bloom["members"][0].as_object_mut().expect("a member object");
        assert!(member.remove("cursor").is_some(), "the live document carries the new cursor slot");

        let decoded: ViewDocument = serde_json::from_value(json).expect("an older document still decodes");
        assert_eq!(decoded.blooms[0].operator_hold, None);
        assert_eq!(decoded.blooms[0].members[0].cursor, None);
    }

    // The plausible bug (#5259 / ADR-0198): a lease is held, the contended
    // path exists, and `/view` shows nothing — so contention reads as a
    // member sitting idle for no reason, which is the unexplained stall the
    // lease surface exists to abolish. The table is the bloom-level answer to
    // "who holds this path", asked path-first.
    #[test]
    fn a_walking_bloom_carries_its_lease_table_and_an_idle_one_carries_none() {
        let snapshot = sealed("wp");
        let bloom = snapshot.blooms.keys().copied().next().expect("the sealed bloom");
        assert!(view_of(&snapshot, |_| None).blooms[0].leases.is_empty(), "nothing observed, nothing held");

        let observed = step(
            &snapshot,
            &event(
                "writes",
                Fact::LaneWritesObserved {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    stage: StageId::Construct,
                    paths: vec!["crates/aether-bloomery/src/lib.rs".into()],
                    observed_at: 1_700_000_000_000,
                },
            ),
        )
        .0;

        let view = with_following_bloom(view_of(&observed, |_| None));
        assert_eq!(wire_round_trip(&view), view, "the lease slot must not steal the next bloom's bytes");

        let leases = &view.blooms[0].leases;
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].path, "crates/aether-bloomery/src/lib.rs");
        assert_eq!(leases[0].holder.0, "wp");
        assert_eq!(leases[0].acquired_at, 1_700_000_000_000);
        // The stage is the holder's cursor, not the stage the observation
        // carried: the operator question is where the holder stands now.
        assert_eq!(leases[0].stage, observed.blooms[&bloom].progress.get(&WorkpieceId("wp".into())).map(|c| c.stage));
    }
}
