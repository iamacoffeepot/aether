//! The authorized orphan-claim release target and its lifecycle (ADR-0179).
//!
//! A claim ref can outlive every journal that knows its holder. Boot reconcile
//! leaves such a holder untouched on purpose (ADR-0150: absence from one
//! instance's journal is not proof a foreign holder is dead), and supersession
//! needs the predecessor locally — so one orphaned mainline-admission ref refuses
//! every later seal with no in-band route back.
//!
//! What is missing is not a looser rule but a way to *supply* the proof. An
//! operator investigates the holder and signs an [`OrphanClaimRelease`]: one
//! typed ref, one expected holder, and an author signature asserting
//! [`ORPHAN_CLAIM_RELEASE_WORDS`] over that exact request digest. The signature
//! authorizes acting under uncertainty; it never claims local absence proves
//! death. No surface here accepts a raw Git ref path.

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::BloomId;
use crate::port::ClaimRefKind;
use crate::values::Statement;

/// The exact words an authorizing signature asserts (ADR-0179 §Typed
/// authorization). Pinned as a const because the reducer compares against it
/// byte-for-byte: a statement carrying any other words authorizes nothing, so
/// a signature harvested from an unrelated instruction cannot be replayed here.
pub const ORPHAN_CLAIM_RELEASE_WORDS: &str = "release orphan bloomery claim";

/// The complete mutation target of one authorized orphan-claim release: which
/// typed ref, and which holder the compare-and-swap expects to find on it.
///
/// Its content digest is the request id — the value the signature parents, the
/// `202` hands back, and the status route reads by. Naming the target by
/// [`ClaimRefKind`] rather than a ref path is what keeps the release inside the
/// typed namespace: there is no spelling of this request that reaches an
/// unrelated Git ref.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct OrphanClaimRelease {
    /// The typed claim ref to release.
    pub ref_kind: ClaimRefKind,
    /// The holder the release expects on that ref — the compare half of the
    /// source port's compare-and-swap, so a ref that has moved is reported
    /// rather than clobbered.
    pub expected_holder: BloomId,
}

impl ContentAddressed for OrphanClaimRelease {
    const DOMAIN: &'static str = "aether.bloomery.orphan_claim_release";
}

impl OrphanClaimRelease {
    /// The request id — this target's own content digest.
    #[must_use]
    pub fn request(&self) -> Digest {
        digest_of(self)
    }

    /// Whether `authorization` authorizes exactly this request.
    ///
    /// Three conditions, all structural: only an author signature can become
    /// instruction (ADR-0149 §The value vocabulary), the asserted words are
    /// [`ORPHAN_CLAIM_RELEASE_WORDS`] exactly, and the parents name this
    /// request's digest. The cryptographic verification is the host route's —
    /// the reducer holds no key material and re-checks only the binding, the
    /// same trust split the adopted-answer door uses.
    #[must_use]
    pub fn authorized_by(&self, authorization: &Statement) -> bool {
        authorization.is_instruction_capable()
            && authorization.words == ORPHAN_CLAIM_RELEASE_WORDS.as_bytes()
            && authorization.parents.contains(&self.request())
    }
}

/// How an authorized release ended at the source (ADR-0179 §Durable lifecycle).
///
/// All three are terminal and journaled. [`Changed`](Self::Changed) never
/// retries against the holder it observed: the operator authorized releasing one
/// named holder, and a ref that has moved is a different decision.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum OrphanClaimReleaseCompletion {
    /// The expected holder's ref was compare-and-swapped to a tombstone and
    /// deleted.
    Released,
    /// The typed ref was already gone (or already tombstoned — a tombstone *is*
    /// the released state). The idempotent terminal success, and what a redrive
    /// sees when the source mutation succeeded but the process died before the
    /// completion was admitted.
    AlreadyAbsent,
    /// The ref exists under another holder, so nothing was touched — the
    /// expected-holder compare-and-swap protecting a concurrently changed ref.
    Changed {
        /// The holder the ref was actually found at.
        observed_holder: BloomId,
    },
}

/// One release request's journal-derived state — what the status route reads and
/// what makes a repeated request id idempotent.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct OrphanClaimReleaseRecord {
    /// The signed target this request named.
    pub target: OrphanClaimRelease,
    /// The terminal result, or `None` while the release is still pending.
    pub completion: Option<OrphanClaimReleaseCompletion>,
}
