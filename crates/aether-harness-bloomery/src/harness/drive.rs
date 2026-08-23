//! Scripted-verdict helpers the fixture cell shares with every scenario that
//! uploads an answer the coordinator dispatched.

use aether_bloomery::testing::approved;
use aether_bloomery::{
    BloomDraft, BloomSpec, CandidateRef, CompositionParents, ConfigRegistry, Digest, Evidence, EvidenceKind,
    Membership, Nonce, VerifyFailureSet, WorkpieceId,
};
use aether_chassis_bloomery::bloomery::{ScriptedUpload, ScriptedVerdict};
use aether_chassis_bloomery::store::OutstandingOrder;

/// One member, approved. The approval has to bind the member's own subject or
/// the seal door refuses it as an unapproved member (ADR-0149), and the subject
/// is a function of the rest of the member — its workpiece, its scope revision,
/// and the configs ADR-0174 folded in — so it is set after the member is
/// otherwise built.
#[must_use]
pub fn member(workpiece: &str, scope_revision: Digest) -> Membership {
    approved(Membership {
        workpiece: WorkpieceId(workpiece.to_owned()),
        scope_revision,
        configs: ConfigRegistry::default(),
        approval: Evidence { subject: Digest::default(), kind: EvidenceKind::Approval, detail: super::digest(200) },
    })
}

/// Freeze `members` into a spec sealing on `base`.
#[must_use]
pub fn draft(base: Digest, members: &[Membership]) -> BloomSpec {
    BloomDraft { proposals: members.to_vec(), base, ..BloomDraft::default() }.seal()
}

/// The verdict a lane would have uploaded for `order`: no candidate, no
/// findings, no failed verifiers and no measured cost — the plain shape every
/// other constructor here specializes.
///
/// Bound to the order's own displayed digest, because that is the only binding
/// the broker admits: an upload naming anything else is refused before the
/// reducer sees it.
///
/// # Panics
/// The order's displayed-digest column is not 32 bytes, which only a corrupt
/// row can be.
#[must_use]
pub fn verdict(order: &OutstandingOrder, verdict: ScriptedVerdict) -> ScriptedUpload {
    let subject = Digest::from_slice(&order.displayed_digest).expect("a recorded order displays a whole digest");
    ScriptedUpload {
        nonce: Nonce(order.nonce.clone()),
        subject,
        verdict,
        detail: super::digest(0xDE),
        candidate: None,
        findings: None,
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
        narrowing: None,
    }
}

/// A passing verdict, capturing nothing — what a mechanical gate uploads, and
/// what a model lane whose stage produces no new tree uploads.
#[must_use]
pub fn passed(order: &OutstandingOrder) -> ScriptedUpload {
    verdict(order, ScriptedVerdict::VerificationPassed)
}

/// A failing mechanical verdict naming `failed`.
#[must_use]
pub fn failed(order: &OutstandingOrder, failed: VerifyFailureSet) -> ScriptedUpload {
    ScriptedUpload { failed_verifiers: failed, ..verdict(order, ScriptedVerdict::VerificationFailed) }
}

/// A failing verdict the fold narrows to `parents`, bounded by the union of
/// their declared surfaces (ADR-0210).
///
/// What the classifier produces on a real host when a fold refuses over a
/// disagreement between two candidates and the verified member's own delta
/// accounts for none of it. Scripted here because the classifier reads git and
/// a scripted lane has no worktree; what the scenario proves is the half that
/// is the coordinator's — that no member is charged for it.
#[must_use]
pub fn narrowed(order: &OutstandingOrder, parents: &[&str], paths: &[&str], bound: &[&str]) -> ScriptedUpload {
    // Sorted the way `narrow_composition` leaves them, so a scripted narrowing
    // is byte-identical to a classified one and a scenario cannot pass against
    // a shape the classifier never produces.
    let mut parents: Vec<WorkpieceId> = parents.iter().map(|name| WorkpieceId((*name).to_owned())).collect();
    parents.sort();
    let mut paths: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();
    paths.sort();
    let mut bound: Vec<String> = bound.iter().map(|glob| (*glob).to_owned()).collect();
    bound.sort();

    ScriptedUpload {
        narrowing: Some(CompositionParents { parents, paths, bound }),
        ..verdict(order, ScriptedVerdict::VerificationFailed)
    }
}

/// A verdict the lane could not reach — what a gate whose scan refused to run
/// uploads, and what the base fan-out uploaded for real when its suppression
/// member could not resolve `origin/main` (#5384).
#[must_use]
pub fn faulted(order: &OutstandingOrder) -> ScriptedUpload {
    verdict(order, ScriptedVerdict::Faulted)
}

/// A passing verdict that captured `candidate` — what a construct or refine run
/// uploads once it has a tree to stand behind.
#[must_use]
pub fn captured(order: &OutstandingOrder, candidate: CandidateRef) -> ScriptedUpload {
    ScriptedUpload { candidate: Some(candidate), ..passed(order) }
}
