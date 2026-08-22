//! Arms of [`super::reduce`]'s fact dispatch (`Fact::BaseVerifyCompleted`);
//! wiring lives in `mod.rs`.
//!
//! A green whole-workspace receipt releases every withheld member dispatch
//! whose bloom sealed onto that base — walking [`BloomRecord::deferred_dispatches`]
//! rather than cursors, the same correctness argument
//! [`super::operator_hold::reduce_operator_release`] makes: a workpiece whose
//! worker is still running holds the same cursor as one whose dispatch was
//! swallowed, so dispatching from every cursor would put a second worker on a
//! lap that outlived the wait.
//!
//! A bloom whose [`BloomRecord::operator_hold`] is still set is skipped
//! explicitly: a base release must not lift an operator brake. The receipt
//! still folds onto the record, so a later operator release sees `base_proven`
//! already true.

use alloc::vec::Vec;

use super::operator_hold::{owed_aggregates, owed_base_dispatch};
use super::{BloomRecord, BloomStatus, Decision, Decisions, Outcome, Snapshot};
use crate::digest::Digest;
use crate::ids::BloomId;
use crate::values::{BaseReceipt, BaseVerdict, Evidence, VerifyFailureSet, VerifyGateSet};

/// Fold a completed whole-workspace base verify into the snapshot ledger and,
/// on green, re-derive the withheld member dispatches.
pub(super) fn reduce_base_verify_completed(
    snapshot: &Snapshot,
    base: Digest,
    tree: Digest,
    passed: bool,
    evidence: &Evidence,
    failed: VerifyFailureSet,
) -> Decisions {
    let receipt = BaseReceipt {
        base,
        tree,
        gate_set: VerifyGateSet::base().digest(),
        verdict: if passed {
            BaseVerdict::Green { evidence: evidence.clone() }
        } else {
            BaseVerdict::Red { evidence: evidence.clone(), failed }
        },
    };
    let mut effects = alloc::vec![Decision::RecordBaseReceipt { receipt }];
    if !passed {
        return Decisions { outcome: Outcome::BaseRefused { base, tree, failed }, effects };
    }

    let mut released = Vec::new();
    for (bloom, record) in &snapshot.blooms {
        if record.status != BloomStatus::Sealed {
            continue;
        }
        if record.spec.base() != base && snapshot.base_trees.get(&record.spec.base()).copied() != Some(tree) {
            continue;
        }
        // A base release must not lift an operator brake.
        if record.operator_hold.is_some() {
            continue;
        }
        if dispatch_owed(snapshot, record, *bloom, &mut effects) {
            released.push(*bloom);
        }
    }
    Decisions { outcome: Outcome::BaseProven { base, tree, released }, effects }
}

fn dispatch_owed(snapshot: &Snapshot, record: &BloomRecord, bloom: BloomId, effects: &mut Vec<Decision>) -> bool {
    let mut any = false;
    for workpiece in &record.deferred_dispatches {
        if let Some(owed) = owed_base_dispatch(record, bloom, workpiece, snapshot.member_checkpoint(&bloom, workpiece))
        {
            effects.extend(owed);
            any = true;
        }
    }
    let aggregates = owed_aggregates(record, bloom);
    any |= !aggregates.is_empty();
    effects.extend(aggregates);
    any
}

#[cfg(test)]
mod tests {
    use super::reduce_base_verify_completed;
    use crate::digest::Digest;
    use crate::ids::{IdempotencyKey, WorkpieceId};
    use crate::reduce::{Decision, Event, Fact, Outcome, Snapshot, reduce};
    use crate::values::{
        BloomDraft, ConfigRegistry, Evidence, EvidenceKind, Membership, ResolvedConfigs, SpendWindow, VerifyFailure,
        VerifyFailureSet,
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

    fn evidence(subject: Digest) -> Evidence {
        Evidence { subject, kind: EvidenceKind::VerificationResult, detail: digest(9) }
    }

    #[test]
    fn a_green_receipt_releases_withheld_construct() {
        let spec =
            BloomDraft { proposals: vec![membership("wp-a", 1)], base: digest(0), ..BloomDraft::default() }.seal();
        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        let sealed = reduce(&Snapshot::new(digest(0)), &seal, &ResolvedConfigs::default(), &SpendWindow::default());
        let snapshot = Snapshot::new(digest(0)).apply(&seal, &sealed, &ResolvedConfigs::default());
        assert!(sealed.effects.iter().any(|effect| matches!(effect, Decision::DeferDispatch { .. })));

        let decided = reduce_base_verify_completed(
            &snapshot,
            digest(0),
            digest(0),
            true,
            &evidence(digest(0)),
            VerifyFailureSet::EMPTY,
        );
        assert!(matches!(decided.outcome, Outcome::BaseProven { .. }));
        assert!(
            decided.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })),
            "a green receipt releases the withheld construct",
        );
    }

    #[test]
    fn a_red_receipt_releases_nothing() {
        let decided = reduce_base_verify_completed(
            &Snapshot::new(digest(0)),
            digest(0),
            digest(0),
            false,
            &evidence(digest(0)),
            VerifyFailureSet::one(VerifyFailure::Docs),
        );
        assert!(matches!(decided.outcome, Outcome::BaseRefused { .. }));
        assert!(
            !decided.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })),
            "a red receipt must not dispatch construct",
        );
    }
}
