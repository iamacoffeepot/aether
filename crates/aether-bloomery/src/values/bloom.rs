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
use core::iter::once;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::{BloomId, WorkpieceId};
use crate::values::{ConfigRegistry, ConfigScopes, Evidence, Forecast};

/// One workpiece's admission into a bloom: its identity, the exact scope
/// revision the bloom pins, and the approval evidence bound to that
/// revision.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Membership {
    /// The admitted workpiece.
    pub workpiece: WorkpieceId,
    /// The exact scope-revision digest sealed into the bloom.
    pub scope_revision: Digest,
    /// This member's configuration, resolved ahead of the bloom's (ADR-0174).
    /// Empty is the ordinary case — a member configures only what it wants to
    /// differ from the bloom-wide choice.
    pub configs: ConfigRegistry,
    /// The approval evidence, bound to this member's [`subject`](Self::subject).
    pub approval: Evidence,
}

/// The exact content a member's approval binds to (ADR-0174): its workpiece, the
/// scope revision it was approved at, and the configuration it will run under.
///
/// The binding covers configuration because a receipt that reads "approved" over
/// a model nobody approved is the same divergence a sealed-but-ignored digest
/// was. The override used to sit inside the scope revision, so a signature over
/// the revision covered it by accident; sealing it in the member's registry
/// instead makes that coverage something the subject has to state.
///
/// `approval` itself is excluded, since the subject is what the approval is
/// *of* — including it would be circular.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MemberSubject {
    /// The workpiece under approval.
    pub workpiece: WorkpieceId,
    /// The scope revision the approval was granted at.
    pub scope_revision: Digest,
    /// The configuration the member will run under.
    pub configs: ConfigRegistry,
}

impl ContentAddressed for MemberSubject {
    const DOMAIN: &'static str = "aether.bloomery.member_subject";
}

impl Membership {
    /// The digest this member's `approval` must bind — and, for an above-auto
    /// member, the exact bytes an author signs.
    #[must_use]
    pub fn subject(&self) -> Digest {
        digest_of(&MemberSubject {
            workpiece: self.workpiece.clone(),
            scope_revision: self.scope_revision,
            configs: self.configs.clone(),
        })
    }
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
    /// The bloom-wide configuration (ADR-0174), resolved when no member seals
    /// its own entry for a kind. A sealed
    /// [`StageCatalog`](crate::StageCatalog) entry here is what chooses agents
    /// per stage; sealing none runs the compiled line.
    pub configs: ConfigRegistry,
    /// The sealed forecast study grades against.
    pub forecast: Forecast,
}

impl BloomDraft {
    /// Freeze the draft into an immutable spec with a canonical member order.
    ///
    /// Members are sorted over their full content and de-duplicated, so a draft
    /// and the same draft with its proposals in any other order, or with an
    /// exact proposal repeated, seal to byte-identical specs and therefore the
    /// same [`BloomId`]. Ordering over the full member content is what makes the
    /// key a total order: sorting on any single field leaves two members sharing
    /// that field order-undetermined, so their input position leaks into the id.
    /// The id is then a stable function of the member *set*, not its order —
    /// even for a degenerate set the reducer will later reject at admission.
    ///
    /// The sort leads on `workpiece` because that is the member's identity; the
    /// remaining keys break ties among proposals that name the same workpiece.
    #[must_use]
    pub fn seal(&self) -> BloomSpec {
        let mut members = self.proposals.clone();
        members.sort_by(|a, b| {
            a.workpiece
                .cmp(&b.workpiece)
                .then_with(|| a.scope_revision.cmp(&b.scope_revision))
                .then_with(|| a.configs.cmp(&b.configs))
                .then_with(|| a.approval.subject.cmp(&b.approval.subject))
                .then_with(|| a.approval.kind.cmp(&b.approval.kind))
                .then_with(|| a.approval.detail.cmp(&b.approval.detail))
        });
        members.dedup();
        BloomSpec { members, base: self.base, configs: self.configs.clone(), forecast: self.forecast }
    }
}

/// An immutable, sealed bloom (ADR-0149 §The bloom).
///
/// Fields are private and there is no mutation API: a sealed spec never
/// amends. Changed membership, scope, base, or configuration creates a *successor*
/// bloom (see [`crate::reduce()`]), never an edit. The only constructors are
/// [`BloomDraft::seal`] and deserialization (journal replay).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BloomSpec {
    members: Vec<Membership>,
    base: Digest,
    configs: ConfigRegistry,
    forecast: Forecast,
}

impl ContentAddressed for BloomSpec {
    const DOMAIN: &'static str = "aether.bloomery.bloom_spec";
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

    /// The bloom-wide configuration registry (ADR-0174) — the outer scope every
    /// member's lookup falls through to.
    #[must_use]
    pub const fn configs(&self) -> &ConfigRegistry {
        &self.configs
    }

    /// Every configuration registry this spec seals: the bloom-wide one, then
    /// one per member in canonical order.
    ///
    /// The whole configuration surface a sealed bloom names, which is what a
    /// caller walks to know what content it must be able to produce before the
    /// spec can be admitted or run.
    pub fn config_registries(&self) -> impl Iterator<Item = &ConfigRegistry> {
        once(&self.configs).chain(self.members.iter().map(|member| &member.configs))
    }

    /// The scope chain a lookup on `member`'s behalf walks: that member's
    /// registry, then this bloom's.
    #[must_use]
    pub const fn scopes<'a>(&'a self, member: &'a Membership) -> ConfigScopes<'a> {
        ConfigScopes::member_of(&member.configs, &self.configs)
    }

    /// The sealed forecast.
    #[must_use]
    pub const fn forecast(&self) -> Forecast {
        self.forecast
    }
}

/// One member's contribution to an integration fold: which workpiece, and the
/// candidate tree it claimed.
///
/// The fold needs both. The tree is what a same-base single-member fold states
/// directly; the workpiece is what addresses that member's candidate *ref*,
/// which is what a fold has to merge when it combines work built against
/// different points — several members onto one branch, or a bloom caught up to
/// a moved base. A tree carries no ancestry, so it cannot be merged; only the
/// commit its ref points at can.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MemberCandidate {
    /// The member whose candidate this is — the ref address's workpiece segment.
    pub workpiece: WorkpieceId,
    /// The claimed candidate tree.
    pub candidate: Digest,
}

/// A per-member claim that a candidate resolves its workpiece on the final
/// tree, with evidence bound to the candidate digest (ADR-0149 §The bloom).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ResolutionClaim {
    /// The workpiece this claim resolves.
    pub workpiece: WorkpieceId,
    /// The scope revision this candidate was integrated against. A successor
    /// inherits a predecessor's claim only when it re-admits the same workpiece
    /// at this same revision — a scope-changed member drops its stale claim
    /// (ADR-0149 §The bloom).
    pub scope_revision: Digest,
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
    /// The landable head commit's digest (distinct from `tree`), carried from
    /// the integrate outcome so `land` swaps mainline onto a commit rather than
    /// the bare artifact tree.
    pub head: Digest,
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
