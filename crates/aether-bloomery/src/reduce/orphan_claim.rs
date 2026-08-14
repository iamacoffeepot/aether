//! Arms of [`super::reduce`]'s fact dispatch (`Fact::RequestOrphanClaimRelease`,
//! `Fact::CompleteOrphanClaimRelease`); wiring lives in `mod.rs`.
//!
//! The authorized orphan-claim release door (ADR-0179): admitting an operator's
//! signed request, and folding the terminal result the reactor brings back.

use super::{Decision, Decisions, OrphanClaimReleaseError, Outcome, Snapshot};
use crate::digest::Digest;
use crate::values::{OrphanClaimRelease, OrphanClaimReleaseCompletion, Statement};

/// Admit an operator's authorized release of one orphaned claim ref.
///
/// Three gates, in the order that keeps a refusal cheap and a mutation
/// unreachable without every one of them:
///
/// 1. **Provenance** — only an author signature becomes instruction, so a
///    statement carrying an observation or a stage receipt authorizes nothing.
/// 2. **Binding** — the words are exactly
///    [`ORPHAN_CLAIM_RELEASE_WORDS`](crate::ORPHAN_CLAIM_RELEASE_WORDS) and the
///    parents name this request's own digest. The parent half is what stops one
///    signature from authorizing a second, different ref.
/// 3. **Orphanhood** — no `BloomRecord` for the expected holder exists locally.
///    A holder this journal knows is the ordinary lifecycle's business, whatever
///    its status: an active or resolved bloom is still working, a landed or
///    superseded one already has a release path, and letting an operator reach
///    any of them here would be a second, unaudited route around reconcile.
///
/// The cryptographic verification is *not* here. The host route runs it against
/// the custodied signer allowlist before admission, exactly as the adopted-answer
/// door does; the reducer holds no key material and re-checks only what it can
/// see. Local absence is still not proof the holder is dead — the signature is
/// the operator accepting that uncertainty, which is why it is required and why
/// nothing automatic can produce one.
///
/// A request digest already on record short-circuits: the recorded state is
/// returned with **no effects**, so a resubmission — an impatient operator, a
/// retried HTTP call, a redriven client — cannot enqueue a second release.
pub(super) fn reduce_request_orphan_claim_release(
    snapshot: &Snapshot,
    request: &OrphanClaimRelease,
    authorization: &Statement,
) -> Decisions {
    if !authorization.is_instruction_capable() {
        return Decisions::rejected(Outcome::OrphanClaimReleaseRejected(
            OrphanClaimReleaseError::NotInstructionCapable,
        ));
    }
    if !request.authorized_by(authorization) {
        return Decisions::rejected(Outcome::OrphanClaimReleaseRejected(
            OrphanClaimReleaseError::AuthorizationNotBound,
        ));
    }
    if snapshot.blooms.contains_key(&request.expected_holder) {
        return Decisions::rejected(Outcome::OrphanClaimReleaseRejected(OrphanClaimReleaseError::HolderKnown(
            request.expected_holder,
        )));
    }

    let digest = request.request();
    // Already on record: return the handle and enqueue nothing. The status route
    // is what reports pending-versus-terminal, so there is nothing to add here.
    if snapshot.orphan_releases.contains_key(&digest) {
        return Decisions::rejected(Outcome::OrphanClaimReleaseRequested { request: digest });
    }

    Decisions {
        outcome: Outcome::OrphanClaimReleaseRequested { request: digest },
        effects: alloc::vec![
            Decision::RecordOrphanClaimRelease { request: digest, target: request.clone(), completion: None },
            Decision::DispatchOrphanClaimRelease { request: digest, target: request.clone() },
        ],
    }
}

/// Fold the terminal result of an authorized release onto its pending record.
///
/// The request must be one this journal admitted and must still be pending. A
/// completion for an unknown digest is refused rather than opening a record —
/// the reactor only ever completes what the outbox handed it, so an unknown
/// digest is a fabricated or badly-routed fact. A completion for an already
/// terminal request is refused for the same reason a first one is kept: the
/// first result is what the source actually did, and a redrive arriving after it
/// would otherwise overwrite `Released` with the `AlreadyAbsent` its own second
/// look reports.
pub(super) fn reduce_complete_orphan_claim_release(
    snapshot: &Snapshot,
    request: &Digest,
    completion: OrphanClaimReleaseCompletion,
) -> Decisions {
    let Some(record) = snapshot.orphan_releases.get(request) else {
        return Decisions::rejected(Outcome::OrphanClaimReleaseRejected(OrphanClaimReleaseError::UnknownRequest(
            *request,
        )));
    };
    if record.completion.is_some() {
        return Decisions::rejected(Outcome::OrphanClaimReleaseRejected(OrphanClaimReleaseError::AlreadyCompleted(
            *request,
        )));
    }

    Decisions {
        outcome: Outcome::OrphanClaimReleaseCompleted { request: *request, completion },
        effects: alloc::vec![Decision::RecordOrphanClaimRelease {
            request: *request,
            target: record.target.clone(),
            completion: Some(completion),
        }],
    }
}
