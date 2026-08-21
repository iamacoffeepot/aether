//! Per-file write leases over a bloom's construct lanes (ADR-0204).
//!
//! Two co-sealed members whose declared surfaces overlap dispatch together and
//! only *then* discover whether they collide. The measured collision rate is
//! 39.6% of overlapping pairs, so the other 60% used to pay dispatch
//! serialization for a conflict that never existed — and when a pair did
//! collide, both lanes ran to completion before the fold found out.
//!
//! Exclusivity is therefore per file and acquired at first observed write. The
//! executor reads each construct lane's working tree and admits
//! [`Fact::LaneWritesObserved`](crate::Fact::LaneWritesObserved); this module
//! decides what that means for the table.
//!
//! Contention resolves by **canonical workpiece order**, which is total, so no
//! deadlock is possible:
//!
//! - An earlier-canonical member writing a path a later one holds evicts the
//!   later holder. Its lane is cancelled and its session persists, and it
//!   re-dispatches on the advanced base once the earlier member integrates
//!   ([`resume_entries`]).
//! - A later-canonical member writing a path an earlier one holds keeps
//!   working. It takes the rebase at integration, which is the existing
//!   ADR-0189 reconcile lap, not a new mechanism.
//!
//! Empty effects beyond the cancels: the snapshot folds the table straight off
//! the fact, the way a surface request is folded from its own, so no new
//! [`Decision`] enters the wire-frozen decisions graph.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use super::attempt::{DispatchTargets, SealedLine, move_effects};
use super::snapshot::LeaseEviction;
use super::{BloomRecord, BloomStatus, Decision, Decisions, LeaseObservationError, Outcome, Snapshot, StageProgress};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{EvictedHolder, StageCatalog};

/// The stages whose lane authors a working tree, and therefore the only ones
/// whose observation can take a lease. The same construct family
/// [`reduce_surface_requested`](super::surface_request) admits: `LaneGates::of`
/// keys `is_construct` on the command, which `StageCatalog` maps from all
/// three. A mechanical Verify reads and builds; it authors nothing.
const fn is_construct_family(stage: StageId) -> bool {
    matches!(stage, StageId::Construct | StageId::Refine | StageId::Reconcile)
}

/// Reduce one observation of a construct lane's working tree against a
/// snapshot.
///
/// The refusal ladder mirrors
/// [`reduce_surface_requested`](super::surface_request)'s: an unknown or
/// non-`Sealed` bloom, a workpiece that is not a member, a member with no
/// cursor, a cursor whose stage is not `stage`, a stage outside the construct
/// family, and an observation with nothing left in it.
pub(super) fn reduce_lane_writes_observed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    stage: StageId,
    paths: &[String],
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::LeaseObservationRejected(LeaseObservationError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::LeaseObservationRejected(LeaseObservationError::UnknownOrInactiveBloom));
    }
    if !record.spec.members().iter().any(|member| member.workpiece == *workpiece) {
        return Decisions::rejected(Outcome::LeaseObservationRejected(LeaseObservationError::NotAMember(
            workpiece.clone(),
        )));
    }
    let Some(cursor) = record.progress.get(workpiece).copied() else {
        return Decisions::rejected(Outcome::LeaseObservationRejected(LeaseObservationError::NotDispatched(
            workpiece.clone(),
        )));
    };
    if cursor.stage != stage {
        return Decisions::rejected(Outcome::LeaseObservationRejected(LeaseObservationError::StageMismatch {
            expected: cursor.stage,
            got: stage,
        }));
    }
    if !is_construct_family(stage) {
        return Decisions::rejected(Outcome::LeaseObservationRejected(
            LeaseObservationError::NotAConstructFamilyStage(stage),
        ));
    }
    if paths.is_empty() {
        return Decisions::rejected(Outcome::LeaseObservationRejected(LeaseObservationError::NoPathsObserved));
    }

    let (acquired, evicted) = arbitrate(snapshot, bloom, workpiece, paths);

    Decisions {
        outcome: Outcome::LeasesObserved {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            acquired,
            evicted: evicted.clone(),
        },
        // The only effect an observation has outside the table: a stopped
        // lane. `CancelDispatch` consumes the member's outstanding orders
        // without admitting evidence (#5327) — which is exactly right here
        // too, because an evicted attempt produced no verdict and must spend
        // no budget.
        effects: evicted
            .into_iter()
            .map(|holder| Decision::CancelDispatch { bloom: *bloom, workpiece: holder.workpiece })
            .collect(),
    }
}

/// Walk the observed paths in path order, deciding each against the table.
///
/// A member re-observing a path it already holds reads its own lease and
/// changes nothing, which is what makes a repeated observation of a still-live
/// lane inert. The eviction list is deduplicated on the member and keeps the
/// *first* contended path, so a sibling that lost five files is named once,
/// with the file that lost it first.
fn arbitrate(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    paths: &[String],
) -> (Vec<String>, Vec<EvictedHolder>) {
    let mut acquired = Vec::new();
    let mut evicted: Vec<EvictedHolder> = Vec::new();
    for path in paths {
        match snapshot.file_lease(bloom, path) {
            None => acquired.push(path.clone()),
            Some(lease) if lease.holder == *workpiece => {}
            // An earlier-canonical holder keeps the path. This member goes on
            // with its other work and takes the rebase at integration — the
            // ADR-0189 reconcile lap, unchanged.
            Some(lease) if lease.holder < *workpiece => {}
            Some(lease) => {
                if !evicted.iter().any(|holder| holder.workpiece == lease.holder) {
                    evicted.push(EvictedHolder { workpiece: lease.holder.clone(), path: path.clone() });
                }
                acquired.push(path.clone());
            }
        }
    }
    evicted.sort_by(|left, right| left.workpiece.cmp(&right.workpiece));
    (acquired, evicted)
}

/// Re-dispatch every member `just_resolved` evicted, on the base its
/// integration just advanced to (ADR-0204 §Contention resolves by canonical
/// id).
///
/// Emitted from the same [`claim_effects`](super::integrate::claim_effects)
/// that dispatches newly-ready dependents, so a resume is journaled on the
/// integrating row and replay recovers the schedule (ADR-0190).
///
/// The member re-enters at the entry stage against `just_checkout`: the tree it
/// was building is gone — the whole point of the eviction is that it was
/// building it on top of a file someone else owns — so resuming at a later
/// stage would name a candidate that no longer exists. What actually carries
/// the work across is the session, which the executor's reuse pool keys by
/// member and which an eviction never retires. Its attempt and repair-roll
/// counters are preserved: an eviction is not the member's fault and buys it no
/// budget, but it must not refund what earlier laps spent either.
pub(super) fn resume_entries(
    record: &BloomRecord,
    bloom: BloomId,
    just_resolved: &WorkpieceId,
    just_checkout: Digest,
    evictions: Option<&BTreeMap<WorkpieceId, LeaseEviction>>,
) -> Vec<Decision> {
    let mut effects = Vec::new();
    for (workpiece, eviction) in evictions.into_iter().flatten() {
        if eviction.by != *just_resolved {
            continue;
        }
        if record.claims.contains_key(workpiece)
            || record.wedged.contains_key(workpiece)
            || record.withdrawn.contains_key(workpiece)
        {
            continue;
        }
        let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
            continue;
        };
        let Some(prior) = record.progress.get(workpiece).copied() else {
            continue;
        };
        let progress = StageProgress {
            stage: StageCatalog::entry_stage(),
            candidate: None,
            fold_checkpoint: None,
            fold_conflict_evidence: None,
            reconcile_assembles_base: false,
            ..prior
        };
        let sealed = SealedLine {
            configs: member.configs.layered_over(record.spec.configs()),
            catalog: &record.stage_catalog,
            base: just_checkout,
            held: record.operator_hold.is_some(),
        };
        effects.extend(move_effects(
            bloom,
            workpiece,
            member.scope_revision,
            progress,
            DispatchTargets { subject: member.scope_revision, checkout: just_checkout },
            sealed,
        ));
    }
    effects
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use alloc::string::{String, ToString as _};
    use alloc::vec;
    use alloc::vec::Vec;

    use super::reduce_lane_writes_observed;
    use crate::digest::Digest;
    use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
    use crate::reduce::{Decision, Decisions, Event, Fact, LeaseObservationError, Outcome, Snapshot, reduce};
    use crate::testing::{draft, membership};
    use crate::values::{ResolvedConfigs, SpendWindow};

    const OBSERVED_AT: u64 = 1_700_000_000_000;

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    /// A sealed two-member bloom whose members both sit at their entry cursor.
    /// `wp-a` sorts before `wp-b`, so `wp-a` is the earlier canonical member.
    fn sealed() -> (Snapshot, BloomId, WorkpieceId, WorkpieceId) {
        let spec = draft(0, vec![membership("wp-a", 1), membership("wp-b", 2)]).seal();
        let bloom = spec.id();
        let members: Vec<WorkpieceId> = spec.members().iter().map(|member| member.workpiece.clone()).collect();
        let snapshot = Snapshot::new(digest(0));
        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        let snapshot = snapshot.apply(
            &seal,
            &reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default()),
            &ResolvedConfigs::default(),
        );
        (snapshot, bloom, members[0].clone(), members[1].clone())
    }

    fn observation(bloom: BloomId, workpiece: &WorkpieceId, stage: StageId, paths: &[&str]) -> Fact {
        Fact::LaneWritesObserved {
            bloom,
            workpiece: workpiece.clone(),
            stage,
            paths: paths.iter().map(|path| (*path).to_string()).collect(),
            observed_at: OBSERVED_AT,
        }
    }

    fn observe(
        snapshot: &Snapshot,
        key: &str,
        bloom: BloomId,
        workpiece: &WorkpieceId,
        paths: &[&str],
    ) -> (Snapshot, Decisions) {
        let stage = snapshot.blooms[&bloom].progress[workpiece].stage;
        let fact = observation(bloom, workpiece, stage, paths);
        let owned: Vec<String> = paths.iter().map(|path| (*path).to_string()).collect();
        let decided = reduce_lane_writes_observed(snapshot, &bloom, workpiece, stage, &owned);
        let event = Event { idempotency_key: IdempotencyKey(key.into()), fact };
        (snapshot.apply(&event, &decided, &ResolvedConfigs::default()), decided)
    }

    #[test]
    fn a_first_observed_write_takes_the_lease() {
        let (snapshot, bloom, wp_a, _) = sealed();

        let (after, decided) = observe(&snapshot, "obs-1", bloom, &wp_a, &["crates/a/src/lib.rs"]);

        assert!(matches!(decided.outcome, Outcome::LeasesObserved { .. }));
        assert_eq!(after.file_lease(&bloom, "crates/a/src/lib.rs").unwrap().holder, wp_a);
        assert_eq!(after.file_lease(&bloom, "crates/a/src/lib.rs").unwrap().acquired_at, OBSERVED_AT);
        assert!(decided.effects.is_empty(), "an uncontended acquisition stops nobody");
    }

    #[test]
    fn a_later_member_writing_an_earlier_members_path_keeps_working() {
        // The half of the rule that must NOT stop anyone: the earlier member
        // proceeds uninterrupted, and the later one takes the rebase at
        // integration rather than being cancelled.
        let (snapshot, bloom, wp_a, wp_b) = sealed();
        let (snapshot, _) = observe(&snapshot, "obs-a", bloom, &wp_a, &["crates/shared/src/lib.rs"]);

        let (after, decided) = observe(&snapshot, "obs-b", bloom, &wp_b, &["crates/shared/src/lib.rs"]);

        assert!(decided.effects.is_empty(), "a later member's write cancels nothing");
        assert_eq!(after.file_lease(&bloom, "crates/shared/src/lib.rs").unwrap().holder, wp_a);
        assert!(after.lease_eviction(&bloom, &wp_b).is_none());
    }

    #[test]
    fn an_earlier_member_writing_a_later_members_path_evicts_it() {
        // The acceptance case: the later canonical member is evicted, its lane
        // cancelled, and the earlier member takes the path.
        let (snapshot, bloom, wp_a, wp_b) = sealed();
        let (snapshot, _) = observe(&snapshot, "obs-b", bloom, &wp_b, &["crates/shared/src/lib.rs"]);

        let (after, decided) = observe(&snapshot, "obs-a", bloom, &wp_a, &["crates/shared/src/lib.rs"]);

        assert!(
            matches!(&decided.effects[..], [Decision::CancelDispatch { workpiece, .. }] if *workpiece == wp_b),
            "the evicted member's lane is cancelled, and only its lane",
        );
        assert_eq!(after.file_lease(&bloom, "crates/shared/src/lib.rs").unwrap().holder, wp_a);
        assert_eq!(after.lease_eviction(&bloom, &wp_b).unwrap().by, wp_a);
        assert_eq!(after.lease_eviction(&bloom, &wp_b).unwrap().path, "crates/shared/src/lib.rs");
    }

    #[test]
    fn an_evicted_member_releases_every_lease_it_held() {
        // Holding leases for work that has been rebased away is a lie: the
        // evicted member restarts from the entry stage and re-acquires from
        // its own observations.
        let (snapshot, bloom, wp_a, wp_b) = sealed();
        let (snapshot, _) = observe(&snapshot, "obs-b", bloom, &wp_b, &["crates/b/src/own.rs", "crates/shared/x.rs"]);

        let (after, _) = observe(&snapshot, "obs-a", bloom, &wp_a, &["crates/shared/x.rs"]);

        assert!(after.leases_held(&bloom, &wp_b).is_empty());
        assert_eq!(after.leases_held(&bloom, &wp_a), vec!["crates/shared/x.rs".to_string()]);
    }

    #[test]
    fn re_observing_a_held_path_changes_nothing() {
        // The observation cadence re-reads the same working tree every tick, so
        // a member restating its own write set must not churn the table or
        // cancel anything.
        let (snapshot, bloom, wp_a, _) = sealed();
        let (snapshot, _) = observe(&snapshot, "obs-1", bloom, &wp_a, &["crates/a/src/lib.rs"]);

        let (after, decided) = observe(&snapshot, "obs-2", bloom, &wp_a, &["crates/a/src/lib.rs"]);

        assert!(matches!(&decided.outcome, Outcome::LeasesObserved { acquired, evicted, .. }
            if acquired.is_empty() && evicted.is_empty()));
        assert_eq!(after.file_lease(&bloom, "crates/a/src/lib.rs").unwrap().acquired_at, OBSERVED_AT);
        assert!(decided.effects.is_empty());
    }

    #[test]
    fn an_observation_of_a_stage_the_member_has_left_is_refused() {
        // A stale observation must not take a lease for a lane that is gone.
        let (snapshot, bloom, wp_a, _) = sealed();

        let decided = reduce_lane_writes_observed(
            &snapshot,
            &bloom,
            &wp_a,
            StageId::Verify,
            &["crates/a/src/lib.rs".to_string()],
        );

        assert!(matches!(
            decided.outcome,
            Outcome::LeaseObservationRejected(LeaseObservationError::StageMismatch { .. })
        ));
        assert!(decided.effects.is_empty());
    }

    #[test]
    fn an_observation_naming_no_member_is_refused() {
        let (snapshot, bloom, wp_a, _) = sealed();
        let stage = snapshot.blooms[&bloom].progress[&wp_a].stage;

        let decided = reduce_lane_writes_observed(
            &snapshot,
            &bloom,
            &WorkpieceId("wp-ghost".into()),
            stage,
            &["crates/a/src/lib.rs".to_string()],
        );

        assert!(matches!(
            decided.outcome,
            Outcome::LeaseObservationRejected(LeaseObservationError::NotAMember(_))
        ));
    }
}
