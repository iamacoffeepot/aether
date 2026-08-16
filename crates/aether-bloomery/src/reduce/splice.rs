//! Splice-based construct basing (ADR-0196 slice three).
//!
//! A dependent member's construct base is the bloom base with each resolved
//! dependency candidate spliced in, in the journaled graph's topological
//! order — the same composition the weave performs at integration, applied
//! per member at dispatch. Two replays of the same claims therefore name
//! the same checkout digest.
//!
//! A chain (or any unique-maximum ancestor set) needs no git merge: the
//! closest ancestor was itself built on everything behind it, so its
//! capture commit *is* the spliced tree. Multiple independent tips are
//! the residual fold-conflict class and dispatch Reconcile (ADR-0189)
//! rather than guessing a checkout.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use super::BloomRecord;
use crate::digest::Digest;
use crate::ids::WorkpieceId;
use crate::values::MemberDependency;

/// The construct checkout assembled from `member`'s resolved ancestors.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum SplicedBase {
    /// One tree the lane can check out: the bloom base (a root) or the
    /// unique-maximum ancestor's capture.
    Ready(Digest),
    /// Two or more independent ancestor tips — a residual collision the
    /// fold cannot name a single checkout for.
    Conflict {
        /// The ancestor tips that cannot be ordered, in sealed member order.
        tips: Vec<WorkpieceId>,
    },
}

/// The capture commit of `id`, or the claimed tree when no matching vehicle
/// was recorded.
///
/// Prefers the replayed [`BloomRecord::vehicles`] row (ADR-0152 / #5079). A
/// live cursor is the same-bloom upgrade source — a journal written before
/// the vehicle effect still carries the capture on `StageProgress`. The
/// claimed tree is the legacy fallback: a claim-only integrate never had a
/// capture commit, and `commitish_for` wraps that tree.
pub(super) fn checkout_from(record: &BloomRecord, id: &WorkpieceId) -> Option<Digest> {
    let claimed = record.claims.get(id).map(|claim| claim.candidate);
    record
        .vehicles
        .get(id)
        .filter(|vehicle| claimed.is_none_or(|tree| tree == vehicle.tree))
        .map(|vehicle| vehicle.checkout)
        .or_else(|| {
            record
                .progress
                .get(id)
                .and_then(|progress| progress.candidate)
                .filter(|candidate| claimed.is_none_or(|tree| tree == candidate.tree))
                .map(|candidate| candidate.checkout)
        })
        .or(claimed)
}

/// The sealed member order, the tie-break every splice walk uses.
pub(super) fn member_ids(record: &BloomRecord) -> Vec<WorkpieceId> {
    record.spec.members().iter().map(|member| member.workpiece.clone()).collect()
}

/// The checkout a member's construct (and its Verify diff range) stands on.
///
/// A base-assembly Reconcile that has not yet produced the member's own
/// candidate leaves the assembled head on `fold_checkpoint`; that head
/// wins so the following Construct checks out the tree Reconcile wrote
/// rather than falling back to the bloom base.
pub(super) fn member_construct_base(record: &BloomRecord, member: &WorkpieceId) -> Digest {
    if let Some(progress) = record.progress.get(member)
        && progress.candidate.is_none()
        && let Some(head) = progress.fold_checkpoint
    {
        return head;
    }
    let ids = member_ids(record);
    match spliced_base(record.spec.base(), &ids, &record.dependencies, member, &|id| checkout_from(record, id)) {
        SplicedBase::Ready(digest) => digest,
        SplicedBase::Conflict { .. } => record.spec.base(),
    }
}

/// The ancestor capture commits of `member` in journaled topological order.
///
/// Empty for a root. Replay of the same claims produces the same vec, which
/// is the digest-pinned identity of the splice.
pub(super) fn splice_lineage<F: Fn(&WorkpieceId) -> Option<Digest>>(
    members: &[WorkpieceId],
    edges: &[MemberDependency],
    member: &WorkpieceId,
    checkout_of: &F,
) -> Vec<Digest> {
    topo_ancestors(members, edges, member).into_iter().filter_map(|id| checkout_of(&id)).collect()
}

/// Assemble `member`'s construct checkout from the journaled graph and the
/// checkouts `checkout_of` names.
pub(super) fn spliced_base<F: Fn(&WorkpieceId) -> Option<Digest>>(
    bloom_base: Digest,
    members: &[WorkpieceId],
    edges: &[MemberDependency],
    member: &WorkpieceId,
    checkout_of: &F,
) -> SplicedBase {
    let ancestors = topo_ancestors(members, edges, member);
    if ancestors.is_empty() {
        return SplicedBase::Ready(bloom_base);
    }
    let tips = maxima(&ancestors, edges);
    match tips.as_slice() {
        [_] => {
            let lineage = splice_lineage(members, edges, member, checkout_of);
            SplicedBase::Ready(lineage.last().copied().unwrap_or(bloom_base))
        }
        [] => SplicedBase::Ready(bloom_base),
        _ => SplicedBase::Conflict { tips },
    }
}

fn ancestor_set(edges: &[MemberDependency], member: &WorkpieceId) -> BTreeSet<WorkpieceId> {
    let mut stack: Vec<&WorkpieceId> =
        edges.iter().filter(|edge| edge.member == *member).map(|edge| &edge.depends_on).collect();
    let mut seen = BTreeSet::new();
    while let Some(dep) = stack.pop() {
        if !seen.insert(dep.clone()) {
            continue;
        }
        stack.extend(edges.iter().filter(|edge| edge.member == *dep).map(|edge| &edge.depends_on));
    }
    seen
}

fn topo_ancestors(members: &[WorkpieceId], edges: &[MemberDependency], member: &WorkpieceId) -> Vec<WorkpieceId> {
    let ancestors = ancestor_set(edges, member);
    if ancestors.is_empty() {
        return Vec::new();
    }

    let mut incoming: BTreeMap<&WorkpieceId, usize> = ancestors.iter().map(|id| (id, 0usize)).collect();
    let mut kids: BTreeMap<&WorkpieceId, Vec<&WorkpieceId>> = BTreeMap::new();
    for edge in edges {
        if ancestors.contains(&edge.member) && ancestors.contains(&edge.depends_on) {
            *incoming.get_mut(&edge.member).expect("ancestor is in the incoming map") += 1;
            kids.entry(&edge.depends_on).or_default().push(&edge.member);
        }
    }

    let mut ready: Vec<&WorkpieceId> = incoming.iter().filter(|(_, count)| **count == 0).map(|(id, _)| *id).collect();
    ready.sort_by_key(|id| member_index(members, id));

    let mut order = Vec::with_capacity(ancestors.len());
    while !ready.is_empty() {
        let next = ready.remove(0);
        order.push(next.clone());
        let Some(dependents) = kids.get(next) else {
            continue;
        };
        for dep in dependents {
            let Some(count) = incoming.get_mut(dep) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                ready.push(*dep);
                ready.sort_by_key(|id| member_index(members, id));
            }
        }
    }
    order
}

fn maxima(ancestors: &[WorkpieceId], edges: &[MemberDependency]) -> Vec<WorkpieceId> {
    ancestors
        .iter()
        .filter(|id| !edges.iter().any(|edge| ancestors.contains(&edge.member) && edge.depends_on == **id))
        .cloned()
        .collect()
}

fn member_index(members: &[WorkpieceId], id: &WorkpieceId) -> usize {
    members.iter().position(|member| member == id).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use aether_data::wire::{from_bytes, to_vec};

    use super::{SplicedBase, member_construct_base, splice_lineage, spliced_base};
    use crate::digest::Digest;
    use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
    use crate::reduce::{
        BloomRecord, Decision, Decisions, Event, Fact, Outcome, Snapshot, decode_recorded_decisions, reduce,
    };
    use crate::values::{
        BloomDraft, BloomSpec, CandidateRef, ConfigRegistry, Evidence, EvidenceKind, Forecast, MemberDependency,
        Membership, ResolutionClaim, ResolvedConfigs, SpendWindow, StageCatalog,
    };

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn wp(name: &str) -> WorkpieceId {
        WorkpieceId(name.into())
    }

    fn membership(name: &str, revision: u8) -> Membership {
        let mut member = Membership {
            workpiece: wp(name),
            scope_revision: digest(revision),
            configs: ConfigRegistry::default(),
            approval: Evidence { subject: digest(0), kind: EvidenceKind::Approval, detail: digest(200) },
        };
        member.approval.subject = member.subject();
        member
    }

    fn edge(member: &str, depends_on: &str) -> MemberDependency {
        MemberDependency { member: wp(member), depends_on: wp(depends_on) }
    }

    fn spec(members: &[(&str, u8)]) -> BloomSpec {
        spec_at(members, 0)
    }

    fn spec_at(members: &[(&str, u8)], forecast_tokens: u64) -> BloomSpec {
        BloomDraft {
            proposals: members.iter().map(|(name, revision)| membership(name, *revision)).collect(),
            base: digest(0),
            forecast: Forecast { predicted_tokens: forecast_tokens, predicted_worker_secs: 0, predicted_retries: 0 },
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

    fn claim(name: &str, revision: u8, candidate: u8) -> ResolutionClaim {
        ResolutionClaim {
            workpiece: wp(name),
            scope_revision: digest(revision),
            candidate: digest(candidate),
            evidence: Evidence { subject: digest(candidate), kind: EvidenceKind::ResolutionClaim, detail: digest(201) },
        }
    }

    fn verified_claim(name: &str, revision: u8, candidate: u8, verdict: u8) -> ResolutionClaim {
        ResolutionClaim {
            workpiece: wp(name),
            scope_revision: digest(revision),
            candidate: digest(candidate),
            evidence: Evidence {
                subject: digest(candidate),
                kind: EvidenceKind::VerificationResult,
                detail: digest(verdict),
            },
        }
    }

    fn record<'a>(snapshot: &'a Snapshot, spec: &BloomSpec) -> &'a BloomRecord {
        snapshot.blooms.get(&spec.id()).expect("sealed bloom")
    }

    fn ids(names: &[&str]) -> Vec<WorkpieceId> {
        names.iter().map(|name| wp(name)).collect()
    }

    fn construct_checkout(decisions: &Decisions, name: &str) -> Option<Digest> {
        decisions.effects.iter().find_map(|effect| match effect {
            Decision::DispatchAttempt { workpiece, stage, transformation, .. }
                if workpiece.0 == name && *stage == StageCatalog::entry_stage() =>
            {
                Some(transformation.checkout)
            }
            _ => None,
        })
    }

    // The plausible bug: B's construct still names the bloom base, so the
    // lane materializes a tree that does not contain A's candidate.
    #[test]
    fn a_dependent_constructs_on_its_dependencys_candidate() {
        let spec = spec(&[("wp-a", 1), ("wp-b", 2)]);
        let seal =
            event("seal", Fact::GraphSeal { predecessor: None, spec: spec.clone(), edges: vec![edge("wp-b", "wp-a")] });
        let (after_seal, _) = step(&Snapshot::new(digest(0)), &seal);
        let integrate = event("a-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-a", 1, 10) });
        let (after, decided) = step(&after_seal, &integrate);

        assert_eq!(
            construct_checkout(&decided, "wp-b"),
            Some(digest(10)),
            "B checks out A's claimed candidate, not the bloom base",
        );
        assert_eq!(
            member_construct_base(record(&after, &spec), &wp("wp-b")),
            digest(10),
            "the record's construct base is the same digest the dispatch named",
        );
        assert_eq!(
            member_construct_base(record(&after, &spec), &wp("wp-a")),
            digest(0),
            "a root still constructs on the sealed bloom base",
        );
    }

    // The plausible bug: splice order is derived from live map iteration, so
    // two replays of the same journaled graph assemble different lineage
    // vectors and name different checkouts.
    #[test]
    fn replay_reassembles_the_identical_spliced_tree() {
        let spec = spec(&[("wp-a", 1), ("wp-b", 2), ("wp-c", 3)]);
        let edges = vec![edge("wp-b", "wp-a"), edge("wp-c", "wp-b")];
        let members = ids(&["wp-a", "wp-b", "wp-c"]);
        let checkout_of = |id: &WorkpieceId| match id.0.as_str() {
            "wp-a" => Some(digest(10)),
            "wp-b" => Some(digest(20)),
            _ => None,
        };

        let first = splice_lineage(&members, &edges, &wp("wp-c"), &checkout_of);
        let second = splice_lineage(&members, &edges, &wp("wp-c"), &checkout_of);
        assert_eq!(first, second, "two walks of the same journaled graph produce the same lineage");
        assert_eq!(
            first,
            vec![digest(10), digest(20)],
            "topo order is A then B — the journaled chain, not sealed listing of C's parents",
        );
        assert_eq!(
            spliced_base(digest(0), &members, &edges, &wp("wp-c"), &checkout_of),
            SplicedBase::Ready(digest(20)),
            "the unique maximum is B, whose capture already contains A",
        );

        let seal = event("seal", Fact::GraphSeal { predecessor: None, spec: spec.clone(), edges });
        let a_done = event("a-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-a", 1, 10) });
        let b_done = event("b-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-b", 2, 20) });

        let base = Snapshot::new(digest(0));
        let sealed = reduce(&base, &seal, &ResolvedConfigs::default(), &SpendWindow::default());
        let after_seal = base.apply(&seal, &sealed, &ResolvedConfigs::default());
        let decided_a = reduce(&after_seal, &a_done, &ResolvedConfigs::default(), &SpendWindow::default());
        let after_a = after_seal.apply(&a_done, &decided_a, &ResolvedConfigs::default());
        let decided_b = reduce(&after_a, &b_done, &ResolvedConfigs::default(), &SpendWindow::default());
        let live = after_a.apply(&b_done, &decided_b, &ResolvedConfigs::default());

        let replayed = base
            .apply(
                &from_bytes(&to_vec(&seal).expect("event encodes")).expect("event decodes"),
                &decode_recorded_decisions(&to_vec(&sealed).expect("seal encodes"), None).expect("seal decodes"),
                &ResolvedConfigs::default(),
            )
            .apply(
                &from_bytes(&to_vec(&a_done).expect("event encodes")).expect("event decodes"),
                &decode_recorded_decisions(&to_vec(&decided_a).expect("integrate a encodes"), None)
                    .expect("integrate a decodes"),
                &ResolvedConfigs::default(),
            )
            .apply(
                &from_bytes(&to_vec(&b_done).expect("event encodes")).expect("event decodes"),
                &decode_recorded_decisions(&to_vec(&decided_b).expect("integrate b encodes"), None)
                    .expect("integrate b decodes"),
                &ResolvedConfigs::default(),
            );

        assert_eq!(live, replayed, "apply-only replay rebuilds the live snapshot");
        assert_eq!(
            member_construct_base(record(&replayed, &spec), &wp("wp-c")),
            digest(20),
            "replay names the same spliced checkout the live reduction did",
        );
        assert_eq!(construct_checkout(&decided_b, "wp-c"), Some(digest(20)));
    }

    // The plausible bug: a weave whose fold is byte-identical to the last
    // dependent's already-proven candidate still dispatches AggregateVerify,
    // re-paying a mechanical run the identity memo already holds (#4891).
    #[test]
    fn a_weave_equal_to_a_proven_dependent_passes_by_identity() {
        let spec = spec(&[("wp-a", 1), ("wp-b", 2)]);
        let seal =
            event("seal", Fact::GraphSeal { predecessor: None, spec: spec.clone(), edges: vec![edge("wp-b", "wp-a")] });
        let (snapshot, _) = step(&Snapshot::new(digest(0)), &seal);
        let (snapshot, _) = step(
            &snapshot,
            &event("a-done", Fact::Integrate { bloom: spec.id(), claim: verified_claim("wp-a", 1, 10, 60) }),
        );
        let (snapshot, _) = step(
            &snapshot,
            &event("b-done", Fact::Integrate { bloom: spec.id(), claim: verified_claim("wp-b", 2, 20, 61) }),
        );

        let (after, resolved) = step(
            &snapshot,
            &event("resolve", Fact::Resolve { bloom: spec.id(), tree: digest(20), head: digest(21), lineage: vec![] }),
        );

        assert_eq!(
            resolved.outcome,
            Outcome::AggregateVerifyReused { bloom: spec.id(), rolls: 1, proof: digest(61) },
            "the fold is B's tree, which B already proved",
        );
        assert!(
            !resolved.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })),
            "a proven spliced tree is not re-verified",
        );
        let reuse =
            after.blooms.get(&spec.id()).expect("sealed bloom").verify_reuses.first().expect("identity receipt");
        assert_eq!(reuse.stage, StageId::AggregateVerify);
        assert_eq!(reuse.proof.evidence.detail, digest(61));
    }

    // The plausible bug: two independent parents are silently ordered by
    // sealed listing and B constructs on only one of them, dropping the other.
    #[test]
    fn independent_parent_tips_are_a_splice_conflict() {
        let members = ids(&["wp-a", "wp-c", "wp-b"]);
        let edges = vec![edge("wp-b", "wp-a"), edge("wp-b", "wp-c")];
        let checkout_of = |id: &WorkpieceId| match id.0.as_str() {
            "wp-a" => Some(digest(10)),
            "wp-c" => Some(digest(30)),
            _ => None,
        };

        assert_eq!(
            spliced_base(digest(0), &members, &edges, &wp("wp-b"), &checkout_of),
            SplicedBase::Conflict { tips: vec![wp("wp-a"), wp("wp-c")] },
            "A and C are both maxima; neither capture contains the other",
        );
        assert_eq!(
            splice_lineage(&members, &edges, &wp("wp-b"), &checkout_of),
            vec![digest(10), digest(30)],
            "the lineage still names both parents, in sealed order",
        );
    }

    // The plausible bug: both parents resolving starts B on one tip's
    // checkout, silently dropping the other parent's work.
    #[test]
    fn independent_tips_do_not_start_construct() {
        let spec = spec(&[("wp-a", 1), ("wp-c", 3), ("wp-b", 2)]);
        let edges = vec![edge("wp-b", "wp-a"), edge("wp-b", "wp-c")];
        let seal = event("seal", Fact::GraphSeal { predecessor: None, spec: spec.clone(), edges });
        let (snapshot, _) = step(&Snapshot::new(digest(0)), &seal);
        let (snapshot, _) =
            step(&snapshot, &event("a-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-a", 1, 10) }));
        let (after, decided) =
            step(&snapshot, &event("c-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-c", 3, 30) }));

        assert!(
            construct_checkout(&decided, "wp-b").is_none(),
            "B stays out of Construct: A and C are independent tips",
        );
        let reconcile = decided.effects.iter().find_map(|effect| match effect {
            Decision::DispatchAttempt { workpiece, stage, transformation, .. } if workpiece.0 == "wp-b" => {
                Some((*stage, transformation.checkout))
            }
            _ => None,
        });
        assert_eq!(
            reconcile,
            Some((StageId::Reconcile, digest(10))),
            "B enters at Reconcile on A's tip rather than deadlocking with no cursor",
        );
        let progress = record(&after, &spec).progress.get(&wp("wp-b")).expect("B has a cursor");
        assert_eq!(progress.stage, StageId::Reconcile);
        assert_eq!(progress.fold_checkpoint, Some(digest(10)));
    }

    fn construct_pass(bloom: BloomId, name: &str, tree: u8, checkout: u8, key: &str) -> Event {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: wp(name),
                stage: StageId::Construct,
                passed: true,
                evidence: Evidence {
                    subject: digest(tree),
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(80),
                },
                candidate: Some(CandidateRef { tree: digest(tree), checkout: digest(checkout) }),
            },
        )
    }

    fn pass_construct(snapshot: &Snapshot, bloom: BloomId, name: &str, tree: u8, checkout: u8, key: &str) -> Snapshot {
        step(snapshot, &construct_pass(bloom, name, tree, checkout, key)).0
    }

    fn vehicle_of(decisions: &Decisions, name: &str) -> Option<CandidateRef> {
        decisions.effects.iter().find_map(|effect| match effect {
            Decision::RecordCandidateVehicle { workpiece, vehicle, .. } if workpiece.0 == name => Some(*vehicle),
            _ => None,
        })
    }

    // The plausible bug: B's construct still names A's claimed tree, so the
    // local executor wraps it as a parentless epoch and a clean dependent
    // patch looks like a fold conflict (#5079).
    #[test]
    fn a_dependent_constructs_on_the_recorded_capture_commit() {
        let spec = spec(&[("wp-a", 1), ("wp-b", 2)]);
        let seal =
            event("seal", Fact::GraphSeal { predecessor: None, spec: spec.clone(), edges: vec![edge("wp-b", "wp-a")] });
        let (after_seal, _) = step(&Snapshot::new(digest(0)), &seal);
        let after_build = pass_construct(&after_seal, spec.id(), "wp-a", 10, 110, "a-build");
        let integrate = event("a-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-a", 1, 10) });
        let (after, decided) = step(&after_build, &integrate);

        assert_eq!(
            vehicle_of(&decided, "wp-a"),
            Some(CandidateRef { tree: digest(10), checkout: digest(110) }),
            "integration records the cursor's capture, not the claimed tree alone",
        );
        assert_eq!(
            construct_checkout(&decided, "wp-b"),
            Some(digest(110)),
            "B checks out A's capture commit, not the claimed tree",
        );
        assert_eq!(
            member_construct_base(record(&after, &spec), &wp("wp-b")),
            digest(110),
            "the record's construct base is the same capture the dispatch named",
        );
        assert_eq!(
            record(&after, &spec).vehicles.get(&wp("wp-a")).copied(),
            Some(CandidateRef { tree: digest(10), checkout: digest(110) }),
        );
    }

    // The plausible bug: a cursor whose checkout equals the claim is treated
    // as a match, so a checkout digest substitutes for the claim's tree.
    #[test]
    fn a_checkout_digest_does_not_record_as_the_claim_vehicle() {
        let spec = spec(&[("wp-a", 1), ("wp-b", 2)]);
        let seal =
            event("seal", Fact::GraphSeal { predecessor: None, spec: spec.clone(), edges: vec![edge("wp-b", "wp-a")] });
        let (after_seal, _) = step(&Snapshot::new(digest(0)), &seal);
        let after_build = pass_construct(&after_seal, spec.id(), "wp-a", 11, 10, "a-build");
        let integrate = event("a-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-a", 1, 10) });
        let (after, decided) = step(&after_build, &integrate);

        assert!(
            vehicle_of(&decided, "wp-a").is_none(),
            "a cursor whose tree is not the claim must not become the vehicle: {:?}",
            decided.effects,
        );
        assert_eq!(
            construct_checkout(&decided, "wp-b"),
            Some(digest(10)),
            "B falls back to the claimed tree when no vehicle matched",
        );
        assert!(!record(&after, &spec).vehicles.contains_key(&wp("wp-a")));
    }

    fn replay_row(snapshot: &Snapshot, event: &Event, decided: &Decisions) -> Snapshot {
        snapshot.apply(
            &from_bytes(&to_vec(event).expect("event encodes")).expect("event decodes"),
            &decode_recorded_decisions(&to_vec(decided).expect("decisions encode"), None).expect("decisions decode"),
            &ResolvedConfigs::default(),
        )
    }

    // The plausible bug: apply-only replay loses the vehicle, so a restart
    // names the claimed tree and the dependent construct is parented to a
    // parentless wrapper again.
    #[test]
    fn replay_resolves_the_unique_maximum_to_the_capture_commit() {
        let spec = spec(&[("wp-a", 1), ("wp-b", 2), ("wp-c", 3)]);
        let edges = vec![edge("wp-b", "wp-a"), edge("wp-c", "wp-b")];
        let seal = event("seal", Fact::GraphSeal { predecessor: None, spec: spec.clone(), edges });
        let a_build = construct_pass(spec.id(), "wp-a", 10, 110, "a-build");
        let a_done = event("a-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-a", 1, 10) });
        let b_build = construct_pass(spec.id(), "wp-b", 20, 120, "b-build");
        let b_done = event("b-done", Fact::Integrate { bloom: spec.id(), claim: claim("wp-b", 2, 20) });

        let base = Snapshot::new(digest(0));
        let (after_seal, sealed) = step(&base, &seal);
        let (after_built_a, built_a) = step(&after_seal, &a_build);
        let (after_a, decided_a) = step(&after_built_a, &a_done);
        let (after_built_b, built_b) = step(&after_a, &b_build);
        let (live, decided_b) = step(&after_built_b, &b_done);

        let replayed = replay_row(
            &replay_row(
                &replay_row(&replay_row(&replay_row(&base, &seal, &sealed), &a_build, &built_a), &a_done, &decided_a),
                &b_build,
                &built_b,
            ),
            &b_done,
            &decided_b,
        );

        assert_eq!(live, replayed, "apply-only replay rebuilds the live snapshot, vehicle included");
        assert_eq!(
            member_construct_base(record(&replayed, &spec), &wp("wp-c")),
            digest(120),
            "replay names B's capture commit — the unique maximum — not B's claimed tree",
        );
        assert_eq!(construct_checkout(&decided_b, "wp-c"), Some(digest(120)));
    }

    // The plausible bug: the successor inherits A's claim but not its
    // capture, so C's unique-maximum checkout is the claimed tree.
    #[test]
    fn an_inherited_claim_resolves_the_unique_maximum_to_the_capture_commit() {
        let predecessor = spec(&[("wp-a", 1), ("wp-b", 2), ("wp-c", 3)]);
        let edges = vec![edge("wp-b", "wp-a"), edge("wp-c", "wp-b")];
        let (snapshot, _) = step(
            &Snapshot::new(digest(0)),
            &event("seal", Fact::GraphSeal { predecessor: None, spec: predecessor.clone(), edges }),
        );
        let snapshot = pass_construct(&snapshot, predecessor.id(), "wp-a", 10, 110, "a-build");
        let (snapshot, _) =
            step(&snapshot, &event("a-done", Fact::Integrate { bloom: predecessor.id(), claim: claim("wp-a", 1, 10) }));
        let snapshot = pass_construct(&snapshot, predecessor.id(), "wp-b", 20, 120, "b-build");
        let (snapshot, _) =
            step(&snapshot, &event("b-done", Fact::Integrate { bloom: predecessor.id(), claim: claim("wp-b", 2, 20) }));

        let successor = spec_at(&[("wp-a", 1), ("wp-b", 2), ("wp-c", 3)], 1);
        let (after, decided) = step(
            &snapshot,
            &event("sup", Fact::Supersede { predecessor: predecessor.id(), successor: successor.clone() }),
        );

        assert_eq!(
            construct_checkout(&decided, "wp-c"),
            Some(digest(120)),
            "C constructs on B's inherited capture, not B's claimed tree",
        );
        assert_eq!(
            member_construct_base(record(&after, &successor), &wp("wp-c")),
            digest(120),
            "the successor record's unique maximum is the inherited capture commit",
        );
        assert_eq!(
            after.blooms.get(&successor.id()).expect("successor").vehicles.get(&wp("wp-b")).copied(),
            Some(CandidateRef { tree: digest(20), checkout: digest(120) }),
        );
    }
}
