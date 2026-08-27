//! Arms of [`super::reduce`]'s fact dispatch (`Fact::BaseVerifyCompleted`
//! and `Fact::BaseReverify`); wiring lives in `mod.rs`.
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

use super::attempt::stage_binding;
use super::operator_hold::{owed_aggregates, owed_base_dispatch};
use super::{BaseReverifyError, BloomRecord, BloomStatus, Decision, Decisions, Outcome, Snapshot};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId};
use crate::values::{
    BaseReceipt, BaseReverify, BaseVerdict, Evidence, StageCatalog, Transformation, VerifyFailureSet, VerifyGateSet,
};

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
    for (bloom, record) in sealed_on_tree(snapshot, base, tree) {
        // A base release must not lift an operator brake.
        if record.operator_hold.is_some() {
            continue;
        }
        if dispatch_owed(snapshot, record, bloom, &mut effects) {
            released.push(bloom);
        }
    }
    Decisions { outcome: Outcome::BaseProven { base, tree, released }, effects }
}

/// Reduce an operator re-verify ([`crate::Fact::BaseReverify`]): overwrite the
/// red receipt as pending and queue the same `verify.base` dispatch the seal
/// door would have, under the catalog the waiting bloom sealed.
pub(super) fn reduce_base_reverify(snapshot: &Snapshot, reverify: &BaseReverify) -> Decisions {
    if reverify.reason.trim().is_empty() {
        return Decisions::rejected(Outcome::BaseReverifyRejected(BaseReverifyError::BlankReason));
    }
    if reverify.operator.trim().is_empty() {
        return Decisions::rejected(Outcome::BaseReverifyRejected(BaseReverifyError::BlankOperator));
    }
    let Some(receipt) = snapshot.base_receipt_for(reverify.base) else {
        return Decisions::rejected(Outcome::BaseReverifyRejected(BaseReverifyError::NoReceipt));
    };
    if !matches!(receipt.verdict, BaseVerdict::Red { .. }) {
        return Decisions::rejected(Outcome::BaseReverifyRejected(BaseReverifyError::NotRed));
    }

    let catalog = catalog_for_tree(snapshot, receipt.base, receipt.tree);
    let binding = stage_binding(&catalog, StageId::BaseVerify);
    let mut pending = receipt.clone();
    pending.verdict = BaseVerdict::Pending;
    let base = pending.base;
    Decisions {
        outcome: Outcome::BaseVerifyQueued { base },
        effects: alloc::vec![
            Decision::RecordBaseReceipt { receipt: pending },
            Decision::DispatchBaseVerify {
                base,
                transformation: Transformation::for_base_verify(&binding, base, base),
                profile: binding.profile,
            },
        ],
    }
}

/// Sealed blooms whose base resolves to `tree` — the same walk a green receipt
/// uses to find who is waiting, so a re-verify and a completion name the same
/// set.
fn sealed_on_tree(snapshot: &Snapshot, base: Digest, tree: Digest) -> impl Iterator<Item = (BloomId, &BloomRecord)> {
    snapshot.blooms.iter().filter_map(move |(bloom, record)| {
        if record.status != BloomStatus::Sealed {
            return None;
        }
        if record.spec.base() != base && snapshot.base_trees.get(&record.spec.base()).copied() != Some(tree) {
            return None;
        }
        Some((*bloom, record))
    })
}

fn catalog_for_tree(snapshot: &Snapshot, base: Digest, tree: Digest) -> StageCatalog {
    sealed_on_tree(snapshot, base, tree)
        .next()
        .map_or_else(StageCatalog::line, |(_, record)| record.stage_catalog.clone())
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
    use super::{reduce_base_reverify, reduce_base_verify_completed};
    use crate::digest::Digest;
    use crate::ids::{IdempotencyKey, WorkpieceId};
    use crate::reduce::{BaseReverifyError, Decision, Event, Fact, Outcome, Snapshot, reduce};
    use crate::values::{
        BaseReceipt, BaseReverify, BaseVerdict, BloomDraft, ConfigRegistry, Evidence, EvidenceKind, Membership,
        ResolvedConfigs, SpendWindow, VerifyFailure, VerifyFailureSet, VerifyGateSet,
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

    fn red_receipt(base: Digest, tree: Digest) -> BaseReceipt {
        BaseReceipt {
            base,
            tree,
            gate_set: VerifyGateSet::base().digest(),
            verdict: BaseVerdict::Red { evidence: evidence(base), failed: VerifyFailureSet::one(VerifyFailure::Docs) },
        }
    }

    fn snapshot_with(receipt: BaseReceipt) -> Snapshot {
        let mut snapshot = Snapshot::new(digest(0));
        snapshot.base_trees.insert(receipt.base, receipt.tree);
        snapshot.base_receipts.insert(receipt.verified(), receipt);
        snapshot
    }

    fn reverify(base: Digest) -> BaseReverify {
        BaseReverify { base, reason: "the host killed the fan-out".into(), operator: "eve".into() }
    }

    #[test]
    fn a_red_receipt_reverify_queues_a_pending_overwrite_and_a_dispatch() {
        let base = digest(1);
        let decided = reduce_base_reverify(&snapshot_with(red_receipt(base, base)), &reverify(base));
        assert!(matches!(decided.outcome, Outcome::BaseVerifyQueued { base: queued } if queued == base));
        assert_eq!(decided.effects.len(), 2, "exactly the overwrite and the dispatch: {:?}", decided.effects);
        assert!(
            matches!(
                &decided.effects[0],
                Decision::RecordBaseReceipt { receipt }
                    if receipt.base == base && matches!(receipt.verdict, BaseVerdict::Pending)
            ),
            "the red is overwritten as pending: {:?}",
            decided.effects[0],
        );
        assert!(
            matches!(&decided.effects[1], Decision::DispatchBaseVerify { base: dispatched, .. } if *dispatched == base),
            "the re-run is the same base: {:?}",
            decided.effects[1],
        );
    }

    #[test]
    fn a_green_receipt_is_refused_as_not_red() {
        let decided = reduce_base_reverify(&Snapshot::new(digest(0)).with_green_base(digest(1)), &reverify(digest(1)));
        assert!(matches!(decided.outcome, Outcome::BaseReverifyRejected(BaseReverifyError::NotRed)));
        assert!(decided.effects.is_empty(), "a settled green must not re-spend a whole-workspace build");
    }

    #[test]
    fn a_base_with_no_receipt_is_refused() {
        let decided = reduce_base_reverify(&Snapshot::new(digest(0)), &reverify(digest(1)));
        assert!(matches!(decided.outcome, Outcome::BaseReverifyRejected(BaseReverifyError::NoReceipt)));
        assert!(decided.effects.is_empty());
    }

    #[test]
    fn the_pending_overwrite_keeps_the_red_receipts_tree_and_gate_set() {
        let base = digest(1);
        let tree = digest(2);
        let snapshot = snapshot_with(red_receipt(base, tree));
        let reverify = reverify(base);
        let decided = reduce_base_reverify(&snapshot, &reverify);
        let Decision::RecordBaseReceipt { receipt } = &decided.effects[0] else {
            panic!("first effect is the overwrite: {:?}", decided.effects[0]);
        };
        assert_eq!(receipt.tree, tree, "the overwrite must not file under the commit as a tree");
        assert_eq!(receipt.gate_set, VerifyGateSet::base().digest());

        let event = Event { idempotency_key: IdempotencyKey("reverify".into()), fact: Fact::BaseReverify(reverify) };
        let next = snapshot.apply(&event, &decided, &ResolvedConfigs::default());
        let Some(filed) = next.base_receipt_for(base) else {
            panic!("the overwrite is still resolvable from the commit");
        };
        assert_eq!(filed.tree, tree);
        assert!(matches!(filed.verdict, BaseVerdict::Pending));
    }
}
