//! The pure planning half of the claim-ref interposition (ADR-0150 §The claim
//! registry): what `aether.source.*` mail a locally-accepted seal, supersession,
//! land, or boot reconcile implies, and how a `Held` refusal maps back into the
//! reducer's own refusal vocabulary.
//!
//! The native `ControlCore` cap is only the sender; every
//! decision here is a pure function of the snapshot and the event's decisions.
//! That split is what makes the interposition's enforcement and reconcile
//! decisions testable without a wasm host: `aether-chassis-bloomery`'s
//! `claim_reconcile` integration test drives these exact plans — as the wire
//! bytes the actor would send — through the real source capability over the
//! adapter's in-process fake, so the two crates' independently-declared
//! `aether.source.*` kind definitions are held byte-compatible by test.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use aether_data::wire::{Error as WireError, to_vec};

use super::{ClaimSeal, CompleteRelease, CompleteTransfer, ReleaseSeal, TransferSeal};
use crate::ids::{BloomId, WorkpieceId};
use crate::port::{ClaimHolder, ClaimRefKind, ClaimRefState};
use crate::reduce::{
    BloomRecord, BloomStatus, Decision, Decisions, SealConflict, SealError, Snapshot, SupersedeError,
    is_active_unlanded,
};
use crate::values::BloomSpec;

/// The claim op a replayed bloom's status implies at boot (ADR-0150 §The claim
/// registry, as amended 2026-07-17): converge the shared refs to the holding
/// the journal proves.
#[derive(Debug, Clone)]
pub enum ReconcileOp {
    /// Re-assert the member + admission refs of an active-and-unlanded bloom
    /// (`Sealed | Resolved`) with `claim_seal` — a fully-lost ref set is
    /// re-acquired; an intact holding refuses on the bloom's own ref and rolls
    /// back, leaving the holding exactly as it was.
    Assert(ClaimSeal),
    /// Re-release a `Landed` bloom's member + admission refs with
    /// `release_seal`, healing a land whose fire-and-forget release never
    /// reached the source. Idempotent: an absent ref is skipped, and the CAS
    /// read-guard spares a ref a successor bloom has since re-claimed.
    Release(ReleaseSeal),
}

/// The boot reconcile decision for one replayed bloom record — the walk
/// `ControlCore` drives after journal replay maps each
/// record through this and sends what comes back. Statuses through the
/// [`is_active_unlanded`] predicate re-assert (the same predicate the V1 seal
/// guard reads, so the two never drift); `Landed` re-releases; a `Superseded`
/// bloom's refs were transferred or released by its supersession, so it implies
/// nothing. Deeper heals — reclaiming an own orphan the journal never learned
/// of, sweeping a tombstoned ref, completing a half-transfer — need the
/// enumeration/ownership surface deferred to the deep-heal slice (#3555) and
/// deliberately have no op here.
pub fn reconcile_op(record: &BloomRecord) -> Option<Result<ReconcileOp, WireError>> {
    let bloom = record.spec.id();
    if is_active_unlanded(record.status) {
        Some(seal_claim_mail(&bloom, &record.spec).map(ReconcileOp::Assert))
    } else if matches!(record.status, BloomStatus::Landed) {
        Some(release_reclaim_mail(&bloom, &record.spec).map(ReconcileOp::Release))
    } else {
        None
    }
}

/// One boot-reconcile deep heal (ADR-0150 §The claim registry, amended PR #3556)
/// — the fire-and-forget mail a folded [`ClaimRefState`] implies. Both ops are
/// idempotent, so a crash between the enumeration and the heal re-plans and
/// re-drives cleanly on the next boot.
#[derive(Debug, Clone)]
pub enum HealOp {
    /// Complete an interrupted supersede transfer of one carried/admission ref
    /// from its stranded predecessor to the recorded successor (case-3 (a)), or
    /// restore a ref an uncommitted successor still holds back to the journal
    /// owner (issue 5135).
    Transfer(CompleteTransfer),
    /// Sweep a tombstoned ref (an interrupted release's stranded cleanup, case 2)
    /// or release a dropped ref stranded on a superseded predecessor (case 3's
    /// dropped subset). The empty-`bloom` sweep and the holder-named release are
    /// the two [`CompleteRelease`] shapes.
    Release(CompleteRelease),
}

/// Fold the enumerated claim refs into the boot-reconcile deep heals (ADR-0150
/// §The claim registry, amended PR #3556): a tombstone sweep (case 2), an
/// own-bloom half-transfer completion (case 3), and a restore of refs an
/// interrupted transfer left on a successor the journal never sealed (issue
/// 5135). Pure — a function of the replay-rebuilt `snapshot` and the
/// enumeration — so the same `claim_reconcile` integration test drives it
/// through the real source capability, exactly as it drives [`reconcile_op`].
///
/// Own-orphan reclaim of a fresh acquire that never reached the journal is
/// **not** planned here: it needs the write-ahead intent journal deferred to
/// its own follow-on. A foreign hold cannot be told from that orphan without
/// an instance id the claim commit does not carry, so a ref held by a bloom
/// this journal does not know is left untouched *unless* an active-and-unlanded
/// bloom in this journal still owns that ref — that ownership is the proof the
/// holder is this instance's uncommitted successor, not a peer.
#[must_use]
pub fn plan_heals(snapshot: &Snapshot, states: &[ClaimRefState]) -> Vec<Result<HealOp, WireError>> {
    let mut ops = Vec::new();
    for state in states {
        match &state.holder {
            // A tombstoned ref is an interrupted release whose CAS-to-tombstone
            // linearized but whose name-only cleanup delete never ran — sweep it.
            // Holder-agnostic and safe for any instance: a tombstone *is* released.
            ClaimHolder::Tombstoned => ops.push(sweep_mail(&state.ref_kind).map(HealOp::Release)),
            // A ref still held by a superseded predecessor is a crashed transfer's
            // stranded ref. A carried/admission ref completes to the successor; a
            // dropped ref the successor never re-admitted is released. A ref held
            // by a bloom this journal never sealed, while an active bloom here
            // still owns that ref, is the uncommitted-successor split: restore
            // it to the journal owner (the parent). An active bloom's own hold
            // (V1 re-assert owns it) or a foreign bloom on a ref we do not own
            // (report-only) is left untouched.
            ClaimHolder::Held(holder) => {
                if let Some(successor) = superseded_successor(snapshot, holder) {
                    if transferred_to_successor(snapshot, &state.ref_kind, &successor) {
                        ops.push(transfer_heal_mail(holder, &successor, &state.ref_kind).map(HealOp::Transfer));
                    } else {
                        ops.push(release_heal_mail(holder, &state.ref_kind).map(HealOp::Release));
                    }
                } else if !snapshot.blooms.contains_key(holder)
                    && let Some(owner) = journal_owner(snapshot, &state.ref_kind)
                {
                    ops.push(transfer_heal_mail(holder, &owner, &state.ref_kind).map(HealOp::Transfer));
                }
            }
        }
    }
    ops
}

/// The bloom this journal still records as owning `ref_kind`, if it is
/// active-and-unlanded. An interrupted transfer can leave the ref pointing at
/// a successor id the journal never sealed; the owner here is the parent to
/// restore it to.
fn journal_owner(snapshot: &Snapshot, ref_kind: &ClaimRefKind) -> Option<BloomId> {
    match ref_kind {
        ClaimRefKind::MainlineAdmission => {
            snapshot.blooms.iter().find(|(_, record)| is_active_unlanded(record.status)).map(|(id, _)| *id)
        }
        ClaimRefKind::Workpiece(workpiece) => {
            let owner = snapshot.active.get(workpiece)?;
            snapshot.blooms.get(owner).is_some_and(|record| is_active_unlanded(record.status)).then_some(*owner)
        }
    }
}

/// The successor a replayed bloom was superseded by, or `None` if the bloom is
/// not a superseded predecessor this journal recorded — the ownership proof that
/// gates case-3 healing (a ref held by a non-superseded bloom is left untouched).
fn superseded_successor(snapshot: &Snapshot, bloom: &BloomId) -> Option<BloomId> {
    let record = snapshot.blooms.get(bloom)?;
    matches!(record.status, BloomStatus::Superseded).then_some(record.superseded_by).flatten()
}

/// Whether a predecessor-held ref should transfer to the successor (a carried
/// member or the admission ref) rather than be released (a member the successor
/// dropped). Mirrors `partition_members`: the admission ref is always carried, a
/// workpiece is carried exactly when the successor re-admits it. A successor
/// absent from the snapshot (never observed in practice — a superseded
/// predecessor's successor is a live bloom) conservatively treats a member as
/// dropped, so its stranded ref is released rather than moved to a phantom holder.
fn transferred_to_successor(snapshot: &Snapshot, ref_kind: &ClaimRefKind, successor: &BloomId) -> bool {
    match ref_kind {
        ClaimRefKind::MainlineAdmission => true,
        ClaimRefKind::Workpiece(workpiece) => snapshot
            .blooms
            .get(successor)
            .is_some_and(|record| record.spec.members().iter().any(|member| member.workpiece == *workpiece)),
    }
}

/// The tombstone-sweep [`CompleteRelease`] mail — an **empty** `bloom` (the
/// `None` holder) so the sweep deletes the tombstone without authorizing any live
/// holder.
fn sweep_mail(ref_kind: &ClaimRefKind) -> Result<CompleteRelease, WireError> {
    Ok(CompleteRelease { bloom: Vec::new(), ref_kind: to_vec(ref_kind)? })
}

/// The stranded-drop [`CompleteRelease`] mail — the predecessor named as the
/// authorized holder, so the release CAS-tombstones and deletes exactly its ref.
fn release_heal_mail(holder: &BloomId, ref_kind: &ClaimRefKind) -> Result<CompleteRelease, WireError> {
    Ok(CompleteRelease { bloom: to_vec(holder)?, ref_kind: to_vec(ref_kind)? })
}

/// The half-transfer-completion [`CompleteTransfer`] mail — fast-forward one
/// carried/admission ref from the stranded predecessor to the recorded successor.
fn transfer_heal_mail(
    predecessor: &BloomId,
    successor: &BloomId,
    ref_kind: &ClaimRefKind,
) -> Result<CompleteTransfer, WireError> {
    Ok(CompleteTransfer {
        predecessor: to_vec(predecessor)?,
        successor: to_vec(successor)?,
        ref_kind: to_vec(ref_kind)?,
    })
}

/// Encode each workpiece to its canonical [`aether_data::wire`] bytes — the
/// per-member axis the `aether.source.*` claim mail carries (the port value
/// types are serde-encoded but not `Schema`).
fn encode_workpieces<'a>(workpieces: impl Iterator<Item = &'a WorkpieceId>) -> Result<Vec<Vec<u8>>, WireError> {
    workpieces.map(to_vec).collect()
}

/// The `claim_seal` mail for an accepted seal (also the boot-reconcile
/// re-assertion): acquire the member refs plus the mainline-admission ref (the
/// host adds the admission ref from the bloom, ADR-0150).
pub fn seal_claim_mail(bloom: &BloomId, spec: &BloomSpec) -> Result<ClaimSeal, WireError> {
    Ok(ClaimSeal {
        bloom: to_vec(bloom)?,
        workpieces: encode_workpieces(spec.members().iter().map(|member| &member.workpiece))?,
    })
}

/// The `transfer_seal` mail for an accepted supersession: partition the
/// successor's members against the predecessor's (mirroring `reduce_supersede`)
/// — carried = shared, net-new = successor-only, dropped = predecessor-only —
/// so the predecessor's carried + admission refs fast-forward to the successor
/// (the admission ref never momentarily free), net-new refs are fresh-acquired,
/// and dropped refs released. The predecessor is in the snapshot (the reducer
/// accepted the supersession), so a missing record yields empty carried/dropped
/// sets rather than a panic.
pub fn transfer_seal_mail(
    snapshot: &Snapshot,
    predecessor: &BloomId,
    successor: &BloomSpec,
) -> Result<TransferSeal, WireError> {
    let predecessor_members: BTreeSet<&WorkpieceId> = snapshot
        .blooms
        .get(predecessor)
        .map(|record| record.spec.members().iter().map(|member| &member.workpiece).collect())
        .unwrap_or_default();
    let successor_members: BTreeSet<&WorkpieceId> =
        successor.members().iter().map(|member| &member.workpiece).collect();
    let (carried, net_new, dropped) = partition_members(&predecessor_members, &successor_members);
    Ok(TransferSeal {
        predecessor: to_vec(predecessor)?,
        successor: to_vec(&successor.id())?,
        carried: encode_workpieces(carried.into_iter())?,
        net_new: encode_workpieces(net_new.into_iter())?,
        dropped: encode_workpieces(dropped.into_iter())?,
    })
}

/// Partition a supersession's members into the transfer axes, mirroring
/// `reduce_supersede`'s membership handling: `carried` = shared (fast-forwarded
/// predecessor→successor), `net_new` = successor-only (fresh-acquired),
/// `dropped` = predecessor-only (released). Sorted by the `BTreeSet` iteration,
/// so the mail is deterministic.
fn partition_members<'a>(
    predecessor: &BTreeSet<&'a WorkpieceId>,
    successor: &BTreeSet<&'a WorkpieceId>,
) -> (Vec<&'a WorkpieceId>, Vec<&'a WorkpieceId>, Vec<&'a WorkpieceId>) {
    let carried = successor.iter().copied().filter(|w| predecessor.contains(w)).collect();
    let net_new = successor.iter().copied().filter(|w| !predecessor.contains(w)).collect();
    let dropped = predecessor.iter().copied().filter(|w| !successor.contains(w)).collect();
    (carried, net_new, dropped)
}

/// The `release_seal` mail for a durably-landed bloom: release the member refs
/// the land freed (the [`Decision::ReleaseMembership`] effects) plus the
/// admission ref (the host adds it). A land carries a release per member, all
/// naming the landed bloom; `None` if a land somehow carries none.
#[must_use]
pub fn release_seal_mail(decisions: &Decisions) -> Option<Result<ReleaseSeal, WireError>> {
    let mut bloom = None;
    let mut workpieces = Vec::new();
    for effect in &decisions.effects {
        if let Decision::ReleaseMembership { workpiece, bloom: released_from } = effect {
            bloom = Some(*released_from);
            workpieces.push(workpiece);
        }
    }
    let bloom = bloom?;
    Some((|| Ok(ReleaseSeal { bloom: to_vec(&bloom)?, workpieces: encode_workpieces(workpieces.into_iter())? }))())
}

/// The `release_seal` mail for a boot-reconcile re-release of a `Landed` bloom:
/// release every member ref plus the admission ref (the host adds it), mirroring
/// the land-time release ([`release_seal_mail`]) but built from the retained
/// spec rather than the land's decisions. Idempotent via the source's CAS
/// read-guard — a release of already-freed refs is a no-op and a ref a successor
/// has re-claimed is spared — so re-issuing at boot heals a land whose
/// fire-and-forget release was lost to a crash without stomping a live holding.
pub fn release_reclaim_mail(bloom: &BloomId, spec: &BloomSpec) -> Result<ReleaseSeal, WireError> {
    Ok(ReleaseSeal {
        bloom: to_vec(bloom)?,
        workpieces: encode_workpieces(spec.members().iter().map(|member| &member.workpiece))?,
    })
}

/// Map a seal's `ClaimResult::Held` to the matching [`SealError`] — a
/// per-workpiece ref is a membership conflict, the mainline-admission ref is
/// the one-active-bloom-per-mainline conflict — so a cross-instance refusal
/// reads exactly as the local reducer's own (ADR-0150).
#[must_use]
pub fn held_to_seal_error(ref_kind: &ClaimRefKind, held_by: BloomId) -> SealError {
    match ref_kind {
        ClaimRefKind::Workpiece(workpiece) => {
            SealError::MembershipConflict(SealConflict { workpiece: workpiece.clone(), held_by })
        }
        ClaimRefKind::MainlineAdmission => SealError::ActiveBloomExists(held_by),
    }
}

/// Map a supersession's `ClaimResult::Held` to a [`SupersedeError`]. A
/// foreign hold on a net-new member is a membership conflict (mirroring
/// `reduce_supersede`'s `membership_conflict`); a lost admission-ref transfer
/// CAS has no clean `SupersedeError` (it is a concurrent mutation, not a
/// logical refusal), so it returns `None` and the caller surfaces a retryable
/// error instead.
#[must_use]
pub fn held_to_supersede_error(ref_kind: &ClaimRefKind, held_by: BloomId) -> Option<SupersedeError> {
    match ref_kind {
        ClaimRefKind::Workpiece(workpiece) => {
            Some(SupersedeError::MembershipConflict(SealConflict { workpiece: workpiece.clone(), held_by }))
        }
        ClaimRefKind::MainlineAdmission => None,
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeSet;

    use crate::digest::Digest;
    use crate::ids::{BloomId, WorkpieceId};
    use crate::port::ClaimRefKind;
    use crate::reduce::{SealConflict, SealError, SupersedeError};

    use super::{held_to_seal_error, held_to_supersede_error, partition_members};

    fn workpiece(name: &str) -> WorkpieceId {
        WorkpieceId(name.into())
    }

    #[test]
    fn partition_splits_supersede_members_into_carried_net_new_and_dropped() {
        // A supersession keeping w2, adding w3, dropping w1: the transfer must
        // carry the shared ref, fresh-acquire the successor-only one, and release
        // the predecessor-only one — the axes `transfer_seal` fast-forwards,
        // creates, and releases respectively (ADR-0150). A swap here would free a
        // still-claimed workpiece or leave a dropped one claimed.
        let (w1, w2, w3) = (workpiece("w1"), workpiece("w2"), workpiece("w3"));
        let predecessor: BTreeSet<&WorkpieceId> = [&w1, &w2].into_iter().collect();
        let successor: BTreeSet<&WorkpieceId> = [&w2, &w3].into_iter().collect();
        let (carried, net_new, dropped) = partition_members(&predecessor, &successor);
        assert_eq!(carried, vec![&w2]);
        assert_eq!(net_new, vec![&w3]);
        assert_eq!(dropped, vec![&w1]);
    }

    #[test]
    fn held_maps_to_the_matching_seal_refusal_vocabulary() {
        // The cross-instance refusal must read exactly as the local reducer's: a
        // per-workpiece hold is a membership conflict, the mainline-admission hold
        // is the one-active-bloom refusal (ADR-0150). Mapping admission to a
        // membership conflict (or vice versa) would misreport the reason.
        let held_by = BloomId(Digest::of_wire_bytes(b"holder"));
        let wp = workpiece("w1");
        assert_eq!(
            held_to_seal_error(&ClaimRefKind::Workpiece(wp.clone()), held_by),
            SealError::MembershipConflict(SealConflict { workpiece: wp, held_by }),
        );
        assert_eq!(
            held_to_seal_error(&ClaimRefKind::MainlineAdmission, held_by),
            SealError::ActiveBloomExists(held_by)
        );
    }

    #[test]
    fn held_maps_to_the_matching_supersede_refusal_or_a_retryable_none() {
        // A foreign net-new hold is a supersede membership conflict; a lost
        // admission-ref transfer CAS has no clean SupersedeError, so it is `None`
        // (the caller surfaces a retryable error rather than forcing a wrong
        // variant).
        let held_by = BloomId(Digest::of_wire_bytes(b"holder"));
        let wp = workpiece("w1");
        assert_eq!(
            held_to_supersede_error(&ClaimRefKind::Workpiece(wp.clone()), held_by),
            Some(SupersedeError::MembershipConflict(SealConflict { workpiece: wp, held_by })),
        );
        assert_eq!(held_to_supersede_error(&ClaimRefKind::MainlineAdmission, held_by), None);
    }
}
