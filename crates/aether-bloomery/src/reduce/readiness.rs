//! Readiness scheduling over a sealed member-dependency graph (ADR-0196 slice two).
//!
//! A member's construct dispatches when every dependency carries a resolution
//! claim — the journaled fact that its candidate verified (ADR-0191's
//! immutability point in today's `Construct → Verify` line) — and not before.
//! Roots have no incoming edges, so they dispatch at seal exactly as an
//! edgeless bloom does today. Dependents stay out of the line until that
//! claim lands; a wedged ancestor therefore never starts them.
//!
//! The decision is made at admission and rides the same
//! [`Decision::DispatchAttempt`] / [`Decision::AdvanceStage`] pair every other
//! entry uses. Replay folds those recorded effects (ADR-0190) and does not
//! recompute who is ready. A ready member whose resolved ancestors have two
//! or more independent tips is a structural join: the host assembles those
//! tips before Construct, and only a residual textual collision enters at
//! Reconcile (ADR-0189 / ADR-0196).

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use super::attempt::{DispatchTargets, SealedLine, move_effects};
use super::splice::{SplicedBase, checkout_from, spliced_base};
use super::{BloomRecord, BloomStatus, Decision, Decisions, Outcome, Snapshot, StageProgress};
use crate::digest::Digest;
use crate::ids::{BloomId, WorkpieceId};
use crate::values::{
    BloomSpec, ConfigRegistry, MemberCandidate, MemberDependency, Membership, StageCatalog, VerifyFailureSet,
};

/// Whether every incoming edge of `member` names a workpiece `resolved` accepts.
///
/// Vacuous when `member` has no incoming edges — that is a root, ready at seal.
pub(super) fn dependencies_resolved<F: Fn(&WorkpieceId) -> bool>(
    resolved: &F,
    edges: &[MemberDependency],
    member: &WorkpieceId,
) -> bool {
    edges.iter().filter(|edge| edge.member == *member).all(|edge| resolved(&edge.depends_on))
}

/// The sealed line a brand-new bloom's entry dispatch runs under. A hold cannot
/// exist yet — the bloom is what the same decision is creating — so `held` is
/// always false here.
pub(super) fn entry_line<'a>(
    member: &Membership,
    bloom_configs: &ConfigRegistry,
    catalog: &'a StageCatalog,
    base: Digest,
) -> SealedLine<'a> {
    SealedLine { configs: member.configs.layered_over(bloom_configs), catalog, base, held: false }
}

/// Seed `member` at the entry stage and dispatch its first construct (or defer
/// it, when `sealed` carries an operator hold).
pub(super) fn construct_entry(bloom: BloomId, member: &Membership, sealed: SealedLine<'_>) -> [Decision; 2] {
    let stage = StageCatalog::entry_stage();
    move_effects(
        bloom,
        &member.workpiece,
        member.scope_revision,
        StageProgress {
            stage,
            attempts: 1,
            candidate: None,
            repair_rolls: 0,
            seen_verify_failures: VerifyFailureSet::EMPTY,
            fold_checkpoint: None,
            fold_conflict_evidence: None,
        },
        DispatchTargets { subject: member.scope_revision, checkout: sealed.base },
        sealed,
    )
}

/// Construct-cursor plus host splice for a member whose resolved ancestors
/// have more than one independent tip. Journaled on the same Integrate /
/// supersede row that would have dispatched Construct, so replay recovers it
/// (ADR-0190). The member sits at Construct without a checkout until the
/// host admits [`Fact::SpliceAssembled`](crate::Fact::SpliceAssembled) (or a
/// residual [`Fact::FoldConflict`](crate::Fact::FoldConflict)).
fn splice_join_entry<F: Fn(&WorkpieceId) -> Option<Digest>>(
    bloom: BloomId,
    member: &Membership,
    sealed: SealedLine<'_>,
    tips: &[WorkpieceId],
    checkout_of: &F,
    adopt_from: Option<BloomId>,
) -> Vec<Decision> {
    let members: Vec<MemberCandidate> = tips
        .iter()
        .filter_map(|tip| checkout_of(tip).map(|candidate| MemberCandidate { workpiece: tip.clone(), candidate }))
        .collect();
    let progress = StageProgress {
        stage: StageCatalog::entry_stage(),
        attempts: 1,
        candidate: None,
        repair_rolls: 0,
        seen_verify_failures: VerifyFailureSet::EMPTY,
        fold_checkpoint: None,
        fold_conflict_evidence: None,
    };
    let advance = Decision::AdvanceStage { bloom, workpiece: member.workpiece.clone(), progress };
    let splice =
        Decision::DispatchSplice { bloom, workpiece: member.workpiece.clone(), base: sealed.base, members, adopt_from };
    if sealed.held {
        alloc::vec![advance, Decision::DeferDispatch { bloom, workpiece: member.workpiece.clone() }, splice]
    } else {
        alloc::vec![advance, splice]
    }
}

/// Reduce a successful host splice assembly ([`Fact::SpliceAssembled`](crate::Fact::SpliceAssembled)).
///
/// The member must already sit at Construct with no candidate — the cursor
/// `splice_join_entry` planted so the join would not start on one tip. The
/// assembled `head` becomes `fold_checkpoint` so Construct and Verify stand
/// on the merged tree, and the ordinary entry dispatch goes out.
pub(super) fn reduce_splice_assembled(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    _tree: Digest,
    head: Digest,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::SpliceRejected(super::SpliceError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::SpliceRejected(super::SpliceError::UnknownOrInactiveBloom));
    }
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
        return Decisions::rejected(Outcome::SpliceRejected(super::SpliceError::NotAMember(workpiece.clone())));
    };
    let Some(progress) = record.progress.get(workpiece).copied() else {
        return Decisions::rejected(Outcome::SpliceRejected(super::SpliceError::NotAwaitingSplice(workpiece.clone())));
    };
    if progress.stage != StageCatalog::entry_stage() || progress.candidate.is_some() {
        return Decisions::rejected(Outcome::SpliceRejected(super::SpliceError::NotAwaitingSplice(workpiece.clone())));
    }

    let progress = StageProgress { fold_checkpoint: Some(head), ..progress };
    let effects = move_effects(
        *bloom,
        workpiece,
        member.scope_revision,
        progress,
        DispatchTargets { subject: member.scope_revision, checkout: head },
        SealedLine {
            configs: member.configs.layered_over(record.spec.configs()),
            catalog: &record.stage_catalog,
            base: head,
            held: record.operator_hold.is_some(),
        },
    )
    .to_vec();
    Decisions { outcome: Outcome::SpliceAssembled { bloom: *bloom, workpiece: workpiece.clone() }, effects }
}

/// Entry dispatches for every member of a newly sealed bloom whose
/// dependencies are already resolved.
///
/// `resolved` is the claim set that exists *in this decision*: empty at a
/// fresh seal. Members that still wait stay out of [`BloomRecord::progress`]
/// so a later resolution can tell them from a member already in the line.
pub(super) fn ready_entries<F: Fn(&WorkpieceId) -> bool>(
    bloom: BloomId,
    members: &[Membership],
    edges: &[MemberDependency],
    resolved: &F,
    bloom_configs: &ConfigRegistry,
    catalog: &StageCatalog,
    base: Digest,
) -> Vec<Decision> {
    let mut effects = Vec::new();
    for member in members {
        if !dependencies_resolved(resolved, edges, &member.workpiece) {
            continue;
        }
        effects.extend(construct_entry(bloom, member, entry_line(member, bloom_configs, catalog, base)));
    }
    effects
}

/// Entry dispatches for successor members that did not inherit a claim and
/// whose remaining edges are already satisfied by inherited claims.
///
/// The bool is whether every successor member arrived already integrated —
/// the fold-now case, because no later integrate will complete the set.
pub(super) fn successor_entries(
    successor_id: BloomId,
    successor: &BloomSpec,
    predecessor_id: BloomId,
    predecessor: &BloomRecord,
    edges: &[MemberDependency],
    catalog: &StageCatalog,
) -> (bool, Vec<Decision>) {
    let inherited = |workpiece: &WorkpieceId| {
        predecessor.claims.get(workpiece).is_some_and(|claim| {
            successor
                .members()
                .iter()
                .any(|member| member.workpiece == *workpiece && member.scope_revision == claim.scope_revision)
        })
    };
    let mut every_inherited = true;
    let mut effects = Vec::new();
    for member in successor.members() {
        if inherited(&member.workpiece) {
            continue;
        }
        every_inherited = false;
        if !dependencies_resolved(&inherited, edges, &member.workpiece) {
            continue;
        }
        let ids: Vec<WorkpieceId> = successor.members().iter().map(|item| item.workpiece.clone()).collect();
        let checkout_of = |id: &WorkpieceId| checkout_from(predecessor, id);
        match spliced_base(successor.base(), &ids, edges, &member.workpiece, &checkout_of) {
            SplicedBase::Ready(digest) => effects.extend(construct_entry(
                successor_id,
                member,
                entry_line(member, successor.configs(), catalog, digest),
            )),
            SplicedBase::Join { tips } => {
                let adopt_from = tips.iter().any(|tip| predecessor.claims.contains_key(tip)).then_some(predecessor_id);
                effects.extend(splice_join_entry(
                    successor_id,
                    member,
                    entry_line(member, successor.configs(), catalog, successor.base()),
                    &tips,
                    &checkout_of,
                    adopt_from,
                ));
            }
        }
    }
    (every_inherited, effects)
}

/// Entry dispatches for members that become ready because `just_resolved` just
/// gained a claim. Already-started, already-claimed, and wedged members are
/// left alone — this is the construct *entry*, not a retry.
///
/// `just_checkout` is that claim's capture (or its tree, when the test
/// door jumps to Integrate). The snapshot has not folded the claim yet, so
/// [`checkout_from`] cannot see it; the splice has to take it as an
/// argument or B would construct on the bloom base.
pub(super) fn newly_ready_entries(
    record: &BloomRecord,
    bloom: BloomId,
    just_resolved: &WorkpieceId,
    just_checkout: Digest,
) -> Vec<Decision> {
    let resolved = |dep: &WorkpieceId| dep == just_resolved || record.claims.contains_key(dep);
    let checkout_of = |id: &WorkpieceId| {
        if id == just_resolved {
            Some(just_checkout)
        } else {
            checkout_from(record, id)
        }
    };
    let ids: Vec<WorkpieceId> = record.spec.members().iter().map(|member| member.workpiece.clone()).collect();
    let mut effects = Vec::new();
    for member in record.spec.members() {
        if member.workpiece == *just_resolved {
            continue;
        }
        if record.claims.contains_key(&member.workpiece)
            || record.progress.contains_key(&member.workpiece)
            || record.wedged.contains_key(&member.workpiece)
        {
            continue;
        }
        if !dependencies_resolved(&resolved, &record.dependencies, &member.workpiece) {
            continue;
        }
        let sealed = |base: Digest| SealedLine {
            configs: member.configs.layered_over(record.spec.configs()),
            catalog: &record.stage_catalog,
            base,
            held: record.operator_hold.is_some(),
        };
        match spliced_base(record.spec.base(), &ids, &record.dependencies, &member.workpiece, &checkout_of) {
            SplicedBase::Ready(digest) => {
                effects.extend(construct_entry(bloom, member, sealed(digest)));
            }
            SplicedBase::Join { tips } => {
                effects.extend(splice_join_entry(bloom, member, sealed(record.spec.base()), &tips, &checkout_of, None));
            }
        }
    }
    effects
}

/// The ancestor whose unresolved or wedged state is why `member` has not
/// entered the line, or `None` when `member` is working, already resolved,
/// or a root.
///
/// Walks incoming edges iteratively, skipping claimed parents so a chain
/// whose root has resolved still names the in-progress parent that is
/// actually holding the member out. A wedged ancestor wins over a merely
/// unfinished one — that is the operator-visible reason the subtree is held
/// — and a tie (two wedged ancestors, or two unfinished reasons) breaks in
/// sealed member order so the view is deterministic.
pub(super) fn blocking_ancestor(record: &BloomRecord, member: &WorkpieceId) -> Option<WorkpieceId> {
    if record.claims.contains_key(member) || record.progress.contains_key(member) {
        return None;
    }

    let mut stack: Vec<&WorkpieceId> = incoming(record, member).collect();
    if stack.is_empty() {
        return None;
    }

    let mut seen = BTreeSet::new();
    let mut wedged: Option<WorkpieceId> = None;
    let mut unfinished: Option<WorkpieceId> = None;
    while let Some(dep) = stack.pop() {
        if !seen.insert(dep) {
            continue;
        }
        if record.wedged.contains_key(dep) {
            keep_earlier(record, &mut wedged, dep);
            continue;
        }
        if record.claims.contains_key(dep) {
            continue;
        }
        // Skip claimed parents: a resolved root must not hide the in-progress
        // parent that is still holding this member out of the line.
        let next: Vec<&WorkpieceId> =
            incoming(record, dep).filter(|parent| !record.claims.contains_key(*parent)).collect();
        if next.is_empty() {
            keep_earlier(record, &mut unfinished, dep);
        } else {
            stack.extend(next);
        }
    }
    wedged.or(unfinished)
}

fn incoming<'a>(record: &'a BloomRecord, member: &'a WorkpieceId) -> impl Iterator<Item = &'a WorkpieceId> {
    record.dependencies.iter().filter(move |edge| edge.member == *member).map(|edge| &edge.depends_on)
}

fn keep_earlier(record: &BloomRecord, current: &mut Option<WorkpieceId>, candidate: &WorkpieceId) {
    match current {
        None => *current = Some(candidate.clone()),
        Some(held) if member_index(record, candidate) < member_index(record, held) => *held = candidate.clone(),
        Some(_) => {}
    }
}

fn member_index(record: &BloomRecord, workpiece: &WorkpieceId) -> usize {
    record.spec.members().iter().position(|member| member.workpiece == *workpiece).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use aether_data::wire::{from_bytes, to_vec};

    use super::{blocking_ancestor, newly_ready_entries};
    use crate::digest::Digest;
    use crate::ids::{IdempotencyKey, StageId, WorkpieceId};
    use crate::reduce::{
        BloomRecord, Decision, Decisions, Event, Fact, Outcome, Snapshot, StageProgress, decode_recorded_decisions,
        reduce,
    };
    use crate::values::{
        BloomDraft, BloomSpec, ConfigRegistry, Evidence, EvidenceKind, MemberDependency, Membership, ResolutionClaim,
        ResolvedConfigs, SpendWindow, StageCatalog,
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

    fn edge(member: &str, depends_on: &str) -> MemberDependency {
        MemberDependency { member: WorkpieceId(member.into()), depends_on: WorkpieceId(depends_on.into()) }
    }

    fn spec(members: &[(&str, u8)]) -> BloomSpec {
        BloomDraft {
            proposals: members.iter().map(|(name, revision)| membership(name, *revision)).collect(),
            base: digest(0),
            ..BloomDraft::default()
        }
        .seal()
    }

    fn event(key: &str, fact: Fact) -> Event {
        Event { idempotency_key: IdempotencyKey(key.into()), fact }
    }

    fn step(snapshot: &Snapshot, event: &Event) -> (Snapshot, Decisions) {
        let decisions = reduce(snapshot, event, &ResolvedConfigs::default(), &SpendWindow::default());
        (snapshot.apply(event, &decisions, &ResolvedConfigs::default()), decisions)
    }

    fn seal_graph(members: &[(&str, u8)], edges: Vec<MemberDependency>) -> (Snapshot, BloomSpec) {
        let spec = spec(members);
        let event = event("seal", Fact::GraphSeal { predecessor: None, spec: spec.clone(), edges });
        (step(&Snapshot::new(digest(0)), &event).0, spec)
    }

    fn construct_dispatches(decisions: &Decisions) -> Vec<WorkpieceId> {
        decisions
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Decision::DispatchAttempt { workpiece, stage, .. } if *stage == StageCatalog::entry_stage() => {
                    Some(workpiece.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn reconcile_checkout(decisions: &Decisions, name: &str) -> Option<Digest> {
        decisions.effects.iter().find_map(|effect| match effect {
            Decision::DispatchAttempt { workpiece, stage, transformation, .. }
                if workpiece.0 == name && *stage == StageId::Reconcile =>
            {
                Some(transformation.checkout)
            }
            _ => None,
        })
    }

    fn claim(name: &str, revision: u8, candidate: u8) -> ResolutionClaim {
        ResolutionClaim {
            workpiece: WorkpieceId(name.into()),
            scope_revision: digest(revision),
            candidate: digest(candidate),
            evidence: Evidence { subject: digest(candidate), kind: EvidenceKind::ResolutionClaim, detail: digest(201) },
        }
    }

    fn record<'a>(snapshot: &'a Snapshot, spec: &BloomSpec) -> &'a BloomRecord {
        snapshot.blooms.get(&spec.id()).expect("sealed bloom")
    }

    fn fail_construct(snapshot: &Snapshot, spec: &BloomSpec, name: &str, key: &str) -> Snapshot {
        let event = event(
            key,
            Fact::AttemptCompleted {
                bloom: spec.id(),
                workpiece: WorkpieceId(name.into()),
                stage: StageId::Construct,
                passed: false,
                evidence: Evidence { subject: digest(1), kind: EvidenceKind::VerificationResult, detail: digest(70) },
                candidate: None,
            },
        );
        step(snapshot, &event).0
    }

    // The plausible bug: a declared edge is journaled but every member still
    // enters Construct at seal, so B spends a lane before A has a candidate.
    #[test]
    fn a_dependent_does_not_dispatch_until_its_dependency_resolves() {
        let spec = spec(&[("wp-a", 1), ("wp-b", 2)]);
        let edges = vec![edge("wp-b", "wp-a")];
        let seal = event("seal", Fact::GraphSeal { predecessor: None, spec: spec.clone(), edges });
        let (after_seal, sealed) = step(&Snapshot::new(digest(0)), &seal);

        assert!(matches!(sealed.outcome, Outcome::Sealed(_)));
        assert_eq!(construct_dispatches(&sealed), vec![WorkpieceId("wp-a".into())]);
        let bloom = record(&after_seal, &spec);
        assert!(bloom.progress.contains_key(&WorkpieceId("wp-a".into())), "the root enters the line at seal");
        assert!(
            !bloom.progress.contains_key(&WorkpieceId("wp-b".into())),
            "the dependent stays out of the line until A resolves",
        );

        let integrate = event("a-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-a", 1, 10) });
        let (after, integrated) = step(&after_seal, &integrate);
        assert_eq!(construct_dispatches(&integrated), vec![WorkpieceId("wp-b".into())]);
        assert!(
            after.blooms.get(&spec.id()).expect("sealed bloom").progress.contains_key(&WorkpieceId("wp-b".into())),
            "B's cursor is seeded by A's resolution, not by a later re-decide",
        );
        assert!(
            !integrated.effects.iter().any(|effect| matches!(effect, Decision::DispatchIntegration { .. })),
            "the weave still waits for every member: B has not resolved",
        );
    }

    // The plausible bug: readiness gating also withholds independent roots, so
    // an edgeless (or multi-root) bloom serializes work the graph did not order.
    #[test]
    fn independent_roots_dispatch_at_seal() {
        let spec = spec(&[("wp-a", 1), ("wp-c", 3)]);
        let seal = event("seal", Fact::GraphSeal { predecessor: None, spec, edges: Vec::new() });
        let (_, sealed) = step(&Snapshot::new(digest(0)), &seal);
        assert_eq!(construct_dispatches(&sealed), vec![WorkpieceId("wp-a".into()), WorkpieceId("wp-c".into())],);
    }

    // The plausible bug: a restarted coordinator re-decides readiness against
    // the live snapshot and either re-dispatches B or forgets A's resolution
    // released it. Replay must fold the journaled pair and rebuild the same
    // ready set (ADR-0190).
    #[test]
    fn replay_rebuilds_the_ready_set_from_journaled_resolution() {
        let spec = spec(&[("wp-a", 1), ("wp-b", 2)]);
        let seal =
            event("seal", Fact::GraphSeal { predecessor: None, spec: spec.clone(), edges: vec![edge("wp-b", "wp-a")] });
        let integrate = event("a-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-a", 1, 10) });

        let base = Snapshot::new(digest(0));
        let sealed = reduce(&base, &seal, &ResolvedConfigs::default(), &SpendWindow::default());
        let after_seal = base.apply(&seal, &sealed, &ResolvedConfigs::default());
        let integrated = reduce(&after_seal, &integrate, &ResolvedConfigs::default(), &SpendWindow::default());
        let live = after_seal.apply(&integrate, &integrated, &ResolvedConfigs::default());

        let replayed_seal: Decisions =
            decode_recorded_decisions(&to_vec(&sealed).expect("seal encodes"), None).expect("seal decodes");
        let replayed_integrate: Decisions =
            decode_recorded_decisions(&to_vec(&integrated).expect("integrate encodes"), None)
                .expect("integrate decodes");
        let replayed = base
            .apply(
                &from_bytes(&to_vec(&seal).expect("event encodes")).expect("event decodes"),
                &replayed_seal,
                &ResolvedConfigs::default(),
            )
            .apply(
                &from_bytes(&to_vec(&integrate).expect("event encodes")).expect("event decodes"),
                &replayed_integrate,
                &ResolvedConfigs::default(),
            );

        let bloom = spec.id();
        assert_eq!(live, replayed, "apply-only replay of the journaled rows rebuilds the live snapshot");
        assert!(
            replayed.blooms.get(&bloom).expect("replayed bloom").progress.contains_key(&WorkpieceId("wp-b".into())),
            "B's construct is present after replay because A's resolution journaled it",
        );
    }

    // The plausible bug: a wedge anywhere freezes the bloom, so a disjoint
    // root stops dispatching even though the graph does not order it behind
    // the wedged member.
    #[test]
    fn a_wedge_blocks_only_its_descendants() {
        let (mut snapshot, spec) = seal_graph(&[("wp-a", 1), ("wp-b", 2), ("wp-c", 3)], vec![edge("wp-b", "wp-a")]);
        let bloom = record(&snapshot, &spec);
        assert!(bloom.progress.contains_key(&WorkpieceId("wp-a".into())));
        assert!(bloom.progress.contains_key(&WorkpieceId("wp-c".into())));
        assert!(!bloom.progress.contains_key(&WorkpieceId("wp-b".into())));

        snapshot = fail_construct(&snapshot, &spec, "wp-a", "a-fail-1");
        snapshot = fail_construct(&snapshot, &spec, "wp-a", "a-fail-2");

        let bloom = record(&snapshot, &spec);
        assert!(bloom.wedged.contains_key(&WorkpieceId("wp-a".into())), "A spent Construct's budget");
        assert!(
            !bloom.progress.contains_key(&WorkpieceId("wp-b".into())),
            "B never entered the line: its ancestor wedged",
        );
        assert!(
            bloom.progress.contains_key(&WorkpieceId("wp-c".into())),
            "C is not a descendant of A, so the wedge does not remove it from the line",
        );
        assert!(!bloom.wedged.contains_key(&WorkpieceId("wp-c".into())), "C still has budget");

        let integrate_c = event("c-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-c", 3, 30) });
        let (after, decided) = step(&snapshot, &integrate_c);
        assert!(
            after.blooms.get(&spec.id()).expect("sealed bloom").claims.contains_key(&WorkpieceId("wp-c".into())),
            "the disjoint root resolves while the wedged subtree is held",
        );
        assert!(
            !decided.effects.iter().any(|effect| matches!(effect, Decision::DispatchIntegration { .. })),
            "the weave still waits for the unfinished members",
        );
        assert!(
            newly_ready_entries(record(&after, &spec), spec.id(), &WorkpieceId("wp-c".into()), digest(30)).is_empty(),
            "resolving C does not start B: B depends on A, not C",
        );
    }

    // The plausible bug: the view names the immediate predecessor (B) as the
    // blocker of C in A→B→C, so an operator chasing idleness walks one hop
    // at a time instead of seeing the held root.
    #[test]
    fn the_view_names_the_blocking_ancestor() {
        let (snapshot, spec) =
            seal_graph(&[("wp-a", 1), ("wp-b", 2), ("wp-c", 3)], vec![edge("wp-b", "wp-a"), edge("wp-c", "wp-b")]);
        let bloom = record(&snapshot, &spec);
        assert_eq!(blocking_ancestor(bloom, &WorkpieceId("wp-a".into())), None);
        assert_eq!(blocking_ancestor(bloom, &WorkpieceId("wp-b".into())), Some(WorkpieceId("wp-a".into())));
        assert_eq!(
            blocking_ancestor(bloom, &WorkpieceId("wp-c".into())),
            Some(WorkpieceId("wp-a".into())),
            "C walks through B to the unfinished root",
        );

        let snapshot = fail_construct(&fail_construct(&snapshot, &spec, "wp-a", "a1"), &spec, "wp-a", "a2");
        let bloom = record(&snapshot, &spec);
        assert!(bloom.wedged.contains_key(&WorkpieceId("wp-a".into())));
        assert_eq!(
            blocking_ancestor(bloom, &WorkpieceId("wp-c".into())),
            Some(WorkpieceId("wp-a".into())),
            "a wedge at A is the reason the whole chain is held",
        );
    }

    // The plausible bug: once A resolves, C's walk goes through in-progress B
    // to a claimed A and reports no blocker, so `/view` paints C as idle for
    // a reason the operator cannot name.
    #[test]
    fn a_dependent_names_its_in_progress_parent() {
        let (snapshot, spec) =
            seal_graph(&[("wp-a", 1), ("wp-b", 2), ("wp-c", 3)], vec![edge("wp-b", "wp-a"), edge("wp-c", "wp-b")]);
        let integrate_a = event("a-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-a", 1, 10) });
        let (after_a, _) = step(&snapshot, &integrate_a);
        let bloom = record(&after_a, &spec);
        assert!(bloom.claims.contains_key(&WorkpieceId("wp-a".into())));
        assert!(bloom.progress.contains_key(&WorkpieceId("wp-b".into())), "B entered when A resolved");
        assert!(!bloom.progress.contains_key(&WorkpieceId("wp-c".into())));
        assert_eq!(
            blocking_ancestor(bloom, &WorkpieceId("wp-c".into())),
            Some(WorkpieceId("wp-b".into())),
            "C is waiting on B, which is the unfinished ancestor now that A has resolved",
        );
        assert_eq!(blocking_ancestor(bloom, &WorkpieceId("wp-b".into())), None, "B is in the line");
    }

    // The plausible bug: a member already in the line reports a blocker, so
    // `/view` paints a working root as idle.
    #[test]
    fn a_started_member_is_not_blocked() {
        let (snapshot, spec) = seal_graph(&[("wp-a", 1), ("wp-b", 2)], vec![edge("wp-b", "wp-a")]);
        let bloom = record(&snapshot, &spec);
        assert!(
            matches!(bloom.progress.get(&WorkpieceId("wp-a".into())), Some(StageProgress { stage, .. }) if *stage == StageId::Construct)
        );
        assert_eq!(blocking_ancestor(bloom, &WorkpieceId("wp-a".into())), None);
    }

    // The plausible bug: resolving A starts every descendant, so C spends a
    // lane against a base that does not yet include B's candidate.
    #[test]
    fn a_chain_dispatches_one_hop_at_a_time() {
        let (snapshot, spec) =
            seal_graph(&[("wp-a", 1), ("wp-b", 2), ("wp-c", 3)], vec![edge("wp-b", "wp-a"), edge("wp-c", "wp-b")]);
        assert!(!record(&snapshot, &spec).progress.contains_key(&WorkpieceId("wp-c".into())));

        let integrate_a = event("a-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-a", 1, 10) });
        let (after_a, decided_a) = step(&snapshot, &integrate_a);
        assert_eq!(construct_dispatches(&decided_a), vec![WorkpieceId("wp-b".into())]);
        assert!(
            !record(&after_a, &spec).progress.contains_key(&WorkpieceId("wp-c".into())),
            "C still waits on B, which has only just entered the line",
        );

        let integrate_b = event("b-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-b", 2, 20) });
        let (after_b, decided_b) = step(&after_a, &integrate_b);
        assert_eq!(construct_dispatches(&decided_b), vec![WorkpieceId("wp-c".into())]);
        assert!(record(&after_b, &spec).progress.contains_key(&WorkpieceId("wp-c".into())));
        assert!(
            !decided_b.effects.iter().any(|effect| matches!(effect, Decision::DispatchIntegration { .. })),
            "the weave still waits for C",
        );
    }

    // The plausible bug: readiness treats one satisfied parent as enough, so
    // B starts the moment A resolves even though C has not.
    #[test]
    fn a_member_waits_for_every_parent() {
        let (snapshot, spec) =
            seal_graph(&[("wp-a", 1), ("wp-b", 2), ("wp-c", 3)], vec![edge("wp-b", "wp-a"), edge("wp-b", "wp-c")]);
        let bloom = record(&snapshot, &spec);
        assert!(bloom.progress.contains_key(&WorkpieceId("wp-a".into())));
        assert!(bloom.progress.contains_key(&WorkpieceId("wp-c".into())));
        assert!(!bloom.progress.contains_key(&WorkpieceId("wp-b".into())));

        let integrate_a = event("a-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-a", 1, 10) });
        let (after_a, decided_a) = step(&snapshot, &integrate_a);
        assert!(construct_dispatches(&decided_a).is_empty(), "A alone does not release B: C is still unresolved");
        assert!(!record(&after_a, &spec).progress.contains_key(&WorkpieceId("wp-b".into())));

        let integrate_c = event("c-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-c", 3, 30) });
        let (after_c, decided_c) = step(&after_a, &integrate_c);
        assert!(
            construct_dispatches(&decided_c).is_empty(),
            "A and C are independent tips: B does not construct on one of them",
        );
        assert!(reconcile_checkout(&decided_c, "wp-b").is_none(), "a structural join is not a residual collision",);
        let splice = decided_c.effects.iter().find_map(|effect| match effect {
            Decision::DispatchSplice { workpiece, base, members, .. } if workpiece.0 == "wp-b" => {
                Some((*base, members.iter().map(|member| member.workpiece.clone()).collect::<Vec<_>>()))
            }
            _ => None,
        });
        assert_eq!(
            splice,
            Some((digest(0), vec![WorkpieceId("wp-a".into()), WorkpieceId("wp-c".into())])),
            "B's join names both tips on the bloom base",
        );
        let progress = record(&after_c, &spec).progress.get(&WorkpieceId("wp-b".into())).expect("B has a cursor");
        assert_eq!(progress.stage, StageId::Construct);
        assert_eq!(progress.fold_checkpoint, None);
    }

    // The plausible bug: a successor treats inherited claims as unresolved, so
    // a net-new dependent of an inherited member never enters the line — no
    // later Integrate arrives for work the predecessor already finished.
    #[test]
    fn an_inherited_claim_unblocks_a_successor_dependent() {
        let predecessor_spec = spec(&[("wp-a", 1)]);
        let seal = event("seal", Fact::GraphSeal { predecessor: None, spec: predecessor_spec.clone(), edges: vec![] });
        let (after_seal, _) = step(&Snapshot::new(digest(0)), &seal);
        let integrate = event("a-done", Fact::Integrate { bloom: predecessor_spec.id(), claim: claim("wp-a", 1, 10) });
        let (after_a, _) = step(&after_seal, &integrate);

        let successor_spec = spec(&[("wp-a", 1), ("wp-b", 2)]);
        let supersede = event(
            "sup",
            Fact::GraphSeal {
                predecessor: Some(predecessor_spec.id()),
                spec: successor_spec.clone(),
                edges: vec![edge("wp-b", "wp-a")],
            },
        );
        let (after, decided) = step(&after_a, &supersede);
        assert!(matches!(decided.outcome, Outcome::Superseded { .. }), "got {:?}", decided.outcome);
        assert_eq!(construct_dispatches(&decided), vec![WorkpieceId("wp-b".into())]);
        let successor = after.blooms.get(&successor_spec.id()).expect("successor bloom");
        assert!(successor.claims.contains_key(&WorkpieceId("wp-a".into())), "A arrives already resolved");
        assert!(successor.progress.contains_key(&WorkpieceId("wp-b".into())), "B enters because A is inherited");
        assert!(!successor.progress.contains_key(&WorkpieceId("wp-a".into())), "an inherited member is not re-run");
    }

    // The plausible bug: successor_entries drops a net-new join the same way
    // newly_ready_entries did — two inherited independent parents, no later
    // Integrate arrives, and B never enters the line.
    #[test]
    fn an_inherited_join_dispatches_splice_for_the_dependent() {
        let predecessor_spec = spec(&[("wp-a", 1), ("wp-c", 3)]);
        let seal = event("seal", Fact::GraphSeal { predecessor: None, spec: predecessor_spec.clone(), edges: vec![] });
        let (snapshot, _) = step(&Snapshot::new(digest(0)), &seal);
        let (snapshot, _) = step(
            &snapshot,
            &event("a-done", Fact::Integrate { bloom: predecessor_spec.id(), claim: claim("wp-a", 1, 10) }),
        );
        let (after_parents, _) = step(
            &snapshot,
            &event("c-done", Fact::Integrate { bloom: predecessor_spec.id(), claim: claim("wp-c", 3, 30) }),
        );

        let successor_spec = spec(&[("wp-a", 1), ("wp-c", 3), ("wp-b", 2)]);
        let supersede = event(
            "sup",
            Fact::GraphSeal {
                predecessor: Some(predecessor_spec.id()),
                spec: successor_spec.clone(),
                edges: vec![edge("wp-b", "wp-a"), edge("wp-b", "wp-c")],
            },
        );
        let (after, decided) = step(&after_parents, &supersede);
        assert!(matches!(decided.outcome, Outcome::Superseded { .. }), "got {:?}", decided.outcome);
        assert!(construct_dispatches(&decided).is_empty(), "B does not construct on one inherited tip");
        let splice = decided.effects.iter().find_map(|effect| match effect {
            Decision::DispatchSplice { workpiece, adopt_from, members, .. } if workpiece.0 == "wp-b" => {
                Some((*adopt_from, members.iter().map(|member| member.workpiece.clone()).collect::<Vec<_>>()))
            }
            _ => None,
        });
        assert_eq!(
            splice,
            Some((Some(predecessor_spec.id()), vec![WorkpieceId("wp-a".into()), WorkpieceId("wp-c".into())])),
            "B's inherited join adopts the predecessor's candidate refs",
        );
        let successor = after.blooms.get(&successor_spec.id()).expect("successor bloom");
        let progress = successor.progress.get(&WorkpieceId("wp-b".into())).expect("B has a cursor");
        assert_eq!(progress.stage, StageId::Construct);
        assert_eq!(progress.fold_checkpoint, None);
    }
}
