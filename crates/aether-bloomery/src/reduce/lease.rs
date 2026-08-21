//! Per-file write observations over a bloom's construct lanes (ADR-0204).
//!
//! Two co-sealed members whose declared surfaces overlap dispatch together and
//! only *then* discover whether they collide. A shared file is not a collision.
//! The 39.6% ADR-0204 cites counts pairs that touched a common *path*, and the
//! pair that produced #5401 edited disjoint hunks of one file — a three-way
//! merge applied cleanly, and the lane stopped for it lost five minutes and a
//! machinery roll to a conflict that never existed.
//!
//! So a lease is an observation, not an exclusion. The executor reads each
//! construct lane's working tree and admits
//! [`Fact::LaneWritesObserved`](crate::Fact::LaneWritesObserved); this module
//! folds it into a table naming which member wrote each path first, which is
//! what the operator projection renders (ADR-0198). No lane is stopped, in
//! either canonical direction.
//!
//! Contention settles where the trees actually meet. Every construct lane runs
//! to completion, and the integration fold merges each member's candidate onto
//! the accumulated tree in canonical order. A clean merge costs nothing at all.
//! Only a merge that reports a textual conflict costs anything, and it costs
//! what it always did: the later member takes an ADR-0189 `Reconcile` lap on
//! the advanced base, on the session its lane never left — nothing cancelled
//! it, and the executor's reuse pool keys sessions by member.
//!
//! [`resume_entries`] stays for the upgrade, not for this binary's decisions. A
//! journal written before #5401 carries `Outcome::LeasesObserved` rows whose
//! `evicted` list is non-empty, replay folds *recorded* decisions, and a member
//! caught mid-eviction at upgrade still has to re-dispatch when the member that
//! took its path integrates — or the upgrade strands it.
//!
//! Empty effects: the snapshot folds the table straight off the fact, the way a
//! surface request is folded from its own, so no new [`Decision`] enters the
//! wire-frozen decisions graph.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use super::attempt::{DispatchTargets, SealedLine, move_effects};
use super::snapshot::LeaseEviction;
use super::{BloomRecord, BloomStatus, Decision, Decisions, LeaseObservationError, Outcome, Snapshot, StageProgress};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::StageCatalog;

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

    Decisions {
        outcome: Outcome::LeasesObserved {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            acquired: acquire(snapshot, bloom, paths),
            // Retired by #5401, kept empty rather than removed: the field sits
            // at a fixed position in the journaled decisions shape, and rows
            // written before the fix still carry holders in it.
            evicted: Vec::new(),
        },
        // An observation is an observation. It stops no lane, so it decides
        // nothing outside the table.
        effects: Vec::new(),
    }
}

/// Walk the observed paths in path order, taking a lease on each one no member
/// holds yet.
///
/// A path a sibling already holds stays with that sibling, in both canonical
/// directions: the table answers "who wrote this first", and the two trees meet
/// at the fold. A member re-observing a path it already holds reads its own
/// lease and changes nothing, which is what makes a repeated observation of a
/// still-live lane inert.
fn acquire(snapshot: &Snapshot, bloom: &BloomId, paths: &[String]) -> Vec<String> {
    paths.iter().filter(|path| snapshot.file_lease(bloom, path).is_none()).cloned().collect()
}

/// Re-dispatch every member `just_resolved` evicted, on the base its
/// integration just advanced to (ADR-0204 §Contention resolves by canonical
/// id).
///
/// Nothing this binary decides produces an eviction (#5401), so on a journal it
/// wrote itself this walks an empty map. It stays because replay folds
/// *recorded* decisions: a journal written before the fix can hand this binary
/// a bloom whose later member was evicted and not yet resumed, and the resume
/// is decided on the evicting member's integration row. Dropping it would
/// strand that member for the life of its bloom.
///
/// Emitted from the same [`claim_effects`](super::integrate::claim_effects)
/// that dispatches newly-ready dependents, so a resume is journaled on the
/// integrating row and replay recovers the schedule (ADR-0190).
///
/// The member re-enters at the entry stage against `just_checkout`: the tree it
/// was building is gone — the whole point of the eviction was that it was
/// building it on top of a file someone else owns — so resuming at a later
/// stage would name a candidate that no longer exists. What actually carries
/// the work across is the session, which the executor's reuse pool keys by
/// member and which an eviction never retired. Its attempt and repair-roll
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
            base_proven: record.base_proven,
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
    use crate::reduce::{Decisions, Event, Fact, LeaseObservationError, Outcome, Snapshot, reduce};
    use crate::testing::{draft, membership};
    use crate::values::{EvictedHolder, ResolvedConfigs, SpendWindow};

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
        let snapshot = Snapshot::new(digest(0)).with_green_base(digest(0));
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
        // Half of the symmetry: the later-canonical member goes on working and
        // takes the rebase at integration rather than being cancelled.
        let (snapshot, bloom, wp_a, wp_b) = sealed();
        let (snapshot, _) = observe(&snapshot, "obs-a", bloom, &wp_a, &["crates/shared/src/lib.rs"]);

        let (after, decided) = observe(&snapshot, "obs-b", bloom, &wp_b, &["crates/shared/src/lib.rs"]);

        assert!(decided.effects.is_empty(), "a later member's write cancels nothing");
        assert_eq!(after.file_lease(&bloom, "crates/shared/src/lib.rs").unwrap().holder, wp_a);
        assert!(after.lease_eviction(&bloom, &wp_b).is_none());
    }

    // The plausible bug (#5401): the earlier-canonical member's observation
    // used to evict the later one — cancelling a live lane on the first shared
    // *path*, before anything knew whether the two edits even touched the same
    // hunks. Disjoint hunks of one file three-way merge cleanly, so the cancel
    // threw away good work and charged the member a machinery roll for a
    // conflict the fold never saw.
    #[test]
    fn an_earlier_member_writing_a_later_members_path_stops_no_lane() {
        let (snapshot, bloom, wp_a, wp_b) = sealed();
        let (snapshot, _) = observe(&snapshot, "obs-b", bloom, &wp_b, &["crates/shared/src/lib.rs"]);

        let (after, decided) = observe(&snapshot, "obs-a", bloom, &wp_a, &["crates/shared/src/lib.rs"]);

        assert!(decided.effects.is_empty(), "a shared path cancels nobody: {:?}", decided.effects);
        assert!(
            matches!(&decided.outcome, Outcome::LeasesObserved { acquired, evicted, .. }
                if acquired.is_empty() && evicted.is_empty()),
            "the observation is an observation: {:?}",
            decided.outcome,
        );
        assert_eq!(
            after.file_lease(&bloom, "crates/shared/src/lib.rs").unwrap().holder,
            wp_b,
            "the table keeps naming whoever wrote the path first",
        );
        assert!(after.lease_eviction(&bloom, &wp_b).is_none());
        assert_eq!(after.leases_held(&bloom, &wp_b), vec!["crates/shared/src/lib.rs".to_string()]);
    }

    #[test]
    fn a_shared_path_does_not_hold_back_the_members_other_writes() {
        // A member sharing one file with a sibling keeps taking leases on
        // everything else it writes: the shared path is a merge, not a stop,
        // and the rest of its write set is uncontended either way.
        let (snapshot, bloom, wp_a, wp_b) = sealed();
        let (snapshot, _) = observe(&snapshot, "obs-a", bloom, &wp_a, &["crates/shared/x.rs"]);

        let (after, decided) =
            observe(&snapshot, "obs-b", bloom, &wp_b, &["crates/b/src/own.rs", "crates/shared/x.rs"]);

        assert!(
            matches!(&decided.outcome, Outcome::LeasesObserved { acquired, .. }
                if acquired == &vec!["crates/b/src/own.rs".to_string()]),
            "only the uncontended path is taken: {:?}",
            decided.outcome,
        );
        assert_eq!(after.leases_held(&bloom, &wp_b), vec!["crates/b/src/own.rs".to_string()]);
        assert_eq!(after.leases_held(&bloom, &wp_a), vec!["crates/shared/x.rs".to_string()]);
    }

    // The plausible bug: `Outcome::LeasesObserved.evicted` is journaled at a
    // fixed position, and replay folds *recorded* decisions — so a coordinator
    // upgrading onto a journal written before #5401 must still fold an
    // eviction that binary decided. Dropping the fold would leave the evicted
    // member holding leases it will never write and no eviction record for
    // `resume_entries` to redeem at integration.
    #[test]
    fn an_eviction_recorded_before_the_fix_still_folds_on_replay() {
        let (snapshot, bloom, wp_a, wp_b) = sealed();
        let (snapshot, _) = observe(&snapshot, "obs-b", bloom, &wp_b, &["crates/b/src/own.rs", "crates/shared/x.rs"]);
        let stage = snapshot.blooms[&bloom].progress[&wp_a].stage;

        let recorded = Decisions {
            outcome: Outcome::LeasesObserved {
                bloom,
                workpiece: wp_a.clone(),
                acquired: vec!["crates/shared/x.rs".to_string()],
                evicted: vec![EvictedHolder { workpiece: wp_b.clone(), path: "crates/shared/x.rs".to_string() }],
            },
            effects: Vec::new(),
        };
        let event = Event {
            idempotency_key: IdempotencyKey("obs-a-v1".into()),
            fact: observation(bloom, &wp_a, stage, &["crates/shared/x.rs"]),
        };

        let after = snapshot.apply(&event, &recorded, &ResolvedConfigs::default());

        assert_eq!(after.lease_eviction(&bloom, &wp_b).unwrap().by, wp_a);
        assert_eq!(after.lease_eviction(&bloom, &wp_b).unwrap().path, "crates/shared/x.rs");
        assert!(after.leases_held(&bloom, &wp_b).is_empty(), "the evicted member's leases are released on replay");
        assert_eq!(after.file_lease(&bloom, "crates/shared/x.rs").unwrap().holder, wp_a);
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

        assert!(matches!(decided.outcome, Outcome::LeaseObservationRejected(LeaseObservationError::NotAMember(_))));
    }
}
