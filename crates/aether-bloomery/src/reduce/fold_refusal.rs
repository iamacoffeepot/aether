//! A fold that refuses records why (ADR-0206).

use super::gate::RecordedRefusal;
use super::{BloomStatus, Decisions, Outcome, Snapshot};
use crate::ids::BloomId;

/// Reduce a fold that refused at a named guard.
///
/// The integrate reactor admits the refusal instead of acking it in silence.
/// Empty effects: the snapshot records the refusal from the fact, the way a
/// construct checkpoint is folded from [`Fact::AttemptCompleted`](crate::Fact::AttemptCompleted)
/// without a new [`Decision`](crate::Decision) (the journal's decisions graph
/// is wire-frozen).
pub(super) fn reduce_fold_refused(snapshot: &Snapshot, bloom: &BloomId, _refusal: &RecordedRefusal) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::FoldRefusalRejected);
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::FoldRefusalRejected);
    }
    Decisions { outcome: Outcome::FoldRefused { bloom: *bloom }, effects: Vec::new() }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::reduce_fold_refused;
    use crate::digest::Digest;
    use crate::ids::{BloomId, IdempotencyKey};
    use crate::reduce::{BloomStatus, Event, Fact, Outcome, RecordedRead, RecordedRefusal, Snapshot, reduce};
    use crate::testing::{draft, membership};
    use crate::values::{ResolvedConfigs, SpendWindow};

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn sealed() -> (Snapshot, BloomId) {
        let spec = draft(0, vec![membership("wp-0", 1)]).seal();
        let bloom = spec.id();
        let snapshot = Snapshot::new(digest(0));
        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        (
            snapshot.apply(
                &seal,
                &reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default()),
                &ResolvedConfigs::default(),
            ),
            bloom,
        )
    }

    fn refusal(member: &str) -> RecordedRefusal {
        RecordedRefusal {
            gate: "fold".into(),
            guard: "candidate_ref_present".into(),
            reads: vec![
                RecordedRead { field: "member".into(), value: member.into() },
                RecordedRead { field: "predecessor".into(), value: "aa".into() },
            ],
        }
    }

    #[test]
    fn a_fold_that_refuses_records_the_guard_and_the_member_on_the_snapshot() {
        let (snapshot, bloom) = sealed();
        let recorded = refusal("wp-0");
        let event = Event {
            idempotency_key: IdempotencyKey("fold-refused".into()),
            fact: Fact::FoldRefused { bloom, refusal: recorded.clone() },
        };
        let decided = reduce_fold_refused(&snapshot, &bloom, &recorded);
        assert!(matches!(decided.outcome, Outcome::FoldRefused { bloom: got } if got == bloom));
        assert!(
            decided.effects.is_empty(),
            "the fact carries the refusal; a new Decision would reshape the frozen graph"
        );

        let snapshot = snapshot.apply(&event, &decided, &ResolvedConfigs::default());
        let stored = snapshot.fold_refusal(&bloom).expect("the refusal is on the snapshot");
        assert_eq!(stored.guard, "candidate_ref_present");
        assert_eq!(stored.reads[0].value, "wp-0");
        assert_eq!(snapshot.blooms[&bloom].status, BloomStatus::Sealed);
    }

    #[test]
    fn an_unknown_bloom_is_refused_rather_than_recorded() {
        let snapshot = Snapshot::new(digest(0));
        let recorded = refusal("wp-0");
        let decided = reduce_fold_refused(&snapshot, &BloomId(digest(9)), &recorded);
        assert_eq!(decided.outcome, Outcome::FoldRefusalRejected);
        assert!(decided.effects.is_empty());
    }
}
