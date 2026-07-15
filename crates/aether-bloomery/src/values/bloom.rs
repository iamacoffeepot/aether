//! The one-way bloom lifecycle (ADR-0149 §The bloom).
//!
//! ```text
//! BloomDraft   --seal-->   BloomSpec   --resolve-->   ResolvedBloom   --land-->   LandingReceipt
//! (mutable)                (immutable)                (one artifact)             (mainline moved)
//! ```
//!
//! The mutation happens only in the draft. [`BloomDraft::seal`] freezes a
//! draft into a canonically-ordered [`BloomSpec`] whose id is the digest of
//! its own bytes; there is no API to mutate a sealed spec.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{Digest, digest_of};
use crate::ids::{BloomId, WorkpieceId};
use crate::values::{Budget, Evidence, Forecast};

/// One workpiece's admission into a bloom: its identity, the exact scope
/// revision the bloom pins, and the approval evidence bound to that
/// revision.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Membership {
    /// The admitted workpiece.
    pub workpiece: WorkpieceId,
    /// The exact scope-revision digest sealed into the bloom.
    pub scope_revision: Digest,
    /// The approval evidence, bound to `scope_revision`.
    pub approval: Evidence,
}

/// A mutable bloom in shaping: membership proposals, base, and forecast.
/// Drafts overlap harmlessly and claim nothing (ADR-0149 §The bloom) — only
/// sealing takes claims.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct BloomDraft {
    /// The proposed memberships, in any order — [`seal`](Self::seal)
    /// canonicalizes them.
    pub proposals: Vec<Membership>,
    /// The one base source digest the bloom seals against.
    pub base: Digest,
    /// The frozen stage-catalog digest.
    pub stage_catalog: Digest,
    /// The frozen toolchain digest.
    pub toolchain: Digest,
    /// The frozen policy digest.
    pub policy: Digest,
    /// The predetermined resource ceiling.
    pub budget: Budget,
    /// The sealed forecast study grades against.
    pub forecast: Forecast,
}

impl BloomDraft {
    /// Freeze the draft into an immutable spec with a canonical member order.
    ///
    /// Members are sorted by scope-revision digest (ADR-0149 §The bloom:
    /// "sorted workpiece scope-revision digests"), so a draft and the same
    /// draft with its proposals in any other order seal to byte-identical
    /// specs — and therefore the same [`BloomId`]. This canonicalization is
    /// what makes the id a stable function of the *set*, not the order.
    #[must_use]
    pub fn seal(&self) -> BloomSpec {
        let mut members = self.proposals.clone();
        members.sort_by_key(|member| member.scope_revision);
        BloomSpec {
            members,
            base: self.base,
            stage_catalog: self.stage_catalog,
            toolchain: self.toolchain,
            policy: self.policy,
            budget: self.budget,
            forecast: self.forecast,
        }
    }
}

/// An immutable, sealed bloom (ADR-0149 §The bloom).
///
/// Fields are private and there is no mutation API: a sealed spec never
/// amends. Changed membership, scope, base, or policy creates a *successor*
/// bloom (see [`crate::reduce()`]), never an edit. The only constructors are
/// [`BloomDraft::seal`] and deserialization (journal replay).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BloomSpec {
    members: Vec<Membership>,
    base: Digest,
    stage_catalog: Digest,
    toolchain: Digest,
    policy: Digest,
    budget: Budget,
    forecast: Forecast,
}

impl BloomSpec {
    /// The bloom's identity: the digest of its canonical spec bytes.
    #[must_use]
    pub fn id(&self) -> BloomId {
        BloomId(digest_of(self))
    }

    /// The sealed memberships, in canonical order.
    #[must_use]
    pub fn members(&self) -> &[Membership] {
        &self.members
    }

    /// The one base source digest this bloom sealed against — landing is a
    /// compare-and-swap on this base.
    #[must_use]
    pub const fn base(&self) -> Digest {
        self.base
    }

    /// The frozen stage-catalog digest.
    #[must_use]
    pub const fn stage_catalog(&self) -> Digest {
        self.stage_catalog
    }

    /// The frozen toolchain digest.
    #[must_use]
    pub const fn toolchain(&self) -> Digest {
        self.toolchain
    }

    /// The frozen policy digest.
    #[must_use]
    pub const fn policy(&self) -> Digest {
        self.policy
    }

    /// The predetermined resource ceiling.
    #[must_use]
    pub const fn budget(&self) -> Budget {
        self.budget
    }

    /// The sealed forecast.
    #[must_use]
    pub const fn forecast(&self) -> Forecast {
        self.forecast
    }
}

/// A per-member claim that a candidate resolves its workpiece on the final
/// tree, with evidence bound to the candidate digest (ADR-0149 §The bloom).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ResolutionClaim {
    /// The workpiece this claim resolves.
    pub workpiece: WorkpieceId,
    /// The exact candidate digest that resolves it.
    pub candidate: Digest,
    /// Evidence bound to `candidate`.
    pub evidence: Evidence,
}

/// The one artifact a bloom's execution produces: the final tree, its
/// integration lineage, and a resolution claim for every member workpiece
/// (ADR-0149 §The bloom).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ResolvedBloom {
    /// The bloom that resolved.
    pub bloom: BloomId,
    /// The final integrated tree digest.
    pub tree: Digest,
    /// The integration lineage (the checkpoints that built the tree).
    pub lineage: Vec<Digest>,
    /// One resolution claim per frozen member.
    pub resolution_claims: Vec<ResolutionClaim>,
}

/// The receipt of a compare-and-swap land: mainline moved from the sealed
/// base to the new head, and the next bloom seals on this receipt (ADR-0149
/// §The bloom).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LandingReceipt {
    /// The bloom that landed.
    pub bloom: BloomId,
    /// The mainline head the bloom sealed against.
    pub previous_base: Digest,
    /// The new mainline head after the swap.
    pub new_head: Digest,
}
