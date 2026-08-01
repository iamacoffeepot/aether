//! The compare-and-swap land: mainline moves onto a resolved bloom, or the
//! bloom is refused and a successor seals on the new head (ADR-0149 §The bloom).

use super::{BaseMismatch, BloomStatus, Decision, Decisions, LandError, Outcome, Snapshot};
use crate::digest::Digest;
use crate::ids::BloomId;
use crate::values::LandingReceipt;

pub(super) fn reduce_land(snapshot: &Snapshot, bloom: &BloomId, new_head: &Digest) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::LandRejected(LandError::UnknownBloom(*bloom)));
    };
    if record.status != BloomStatus::Resolved {
        return Decisions::rejected(Outcome::LandRejected(LandError::NotResolved(*bloom)));
    }
    // Compare-and-swap against the bloom's own sealed base — the only head a V1
    // bloom may land on. A moved mainline forces supersession, never a land onto
    // the new head (ADR-0149 §The bloom).
    let base = record.spec.base();
    if snapshot.mainline != base {
        return Decisions::rejected(Outcome::LandRejected(LandError::BaseMismatch(BaseMismatch {
            expected: base,
            actual: snapshot.mainline,
        })));
    }
    let receipt = LandingReceipt { bloom: *bloom, previous_base: base, new_head: *new_head };
    // Release the landed bloom's memberships from `active`, then advance
    // mainline and emit the receipt — one atomic decision set (m5: a land frees
    // its workpieces so the next bloom may seal them).
    let mut effects: Vec<Decision> = record
        .spec
        .members()
        .iter()
        .map(|member| Decision::ReleaseMembership { workpiece: member.workpiece.clone(), bloom: *bloom })
        .collect();
    effects.push(Decision::AdvanceMainline { from: snapshot.mainline, to: *new_head });
    effects.push(Decision::EmitReceipt(receipt.clone()));
    Decisions { outcome: Outcome::Landed(receipt), effects }
}
