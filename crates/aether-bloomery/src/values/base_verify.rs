//! The snapshot-scoped receipt that a sealed base either holds or is waiting
//! on (ADR-0200).
//!
//! A member's first work order is withheld until this receipt is green under
//! [`crate::VerifyGateSet::base`]. The receipt is keyed by [`crate::VerifiedTree`] so two
//! commits that peel to the same tree share one answer.

use alloc::string::String;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest};
use crate::values::{Evidence, VerifiedTree, VerifyFailureSet};

/// Whether a base-verify run is still in flight, or what it concluded.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum BaseVerdict {
    /// A `verify.base` dispatch is outstanding; member entry is withheld.
    Pending,
    /// The whole-workspace fan-out passed on this tree.
    Green {
        /// The green verdict, bound to the tree it judged.
        evidence: Evidence,
    },
    /// The whole-workspace fan-out failed; member entry stays withheld.
    Red {
        /// The red verdict, bound to the tree it judged.
        evidence: Evidence,
        /// The nonempty verifier identities that failed together.
        failed: VerifyFailureSet,
    },
}

/// One recorded base-verify result — pending, green, or red — keyed by the
/// tree the run peeled to.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BaseReceipt {
    /// The commit the verify ran at.
    pub base: Digest,
    /// The tree it peeled to — the content key.
    pub tree: Digest,
    /// The [`crate::VerifyGateSet::digest`] the verdict was collected under.
    pub gate_set: Digest,
    /// Pending, green, or red.
    pub verdict: BaseVerdict,
}

impl BaseReceipt {
    /// The key this receipt is filed under: its peeled tree, paired with the
    /// gate set that judged it. Derived the way
    /// [`crate::VerifyProof::verified`] is, so a receipt can never be filed
    /// under a tree its verdict did not name.
    #[must_use]
    pub fn verified(&self) -> VerifiedTree {
        VerifiedTree { tree: self.tree, gate_set: self.gate_set }
    }

    /// Whether this receipt is a green whole-workspace verdict.
    #[must_use]
    pub fn is_green(&self) -> bool {
        matches!(self.verdict, BaseVerdict::Green { .. })
    }
}

/// An operator decided this base's red verdict does not describe the tree and
/// asked for the gates to run again.
///
/// It stamps nothing green and authorizes nothing — the gates still judge.
/// [`reason`](Self::reason) and [`operator`](Self::operator) are fields rather
/// than optional context for the reason they are on [`crate::OperatorHold`]: a
/// re-verify is an act no verdict produced, so the record of who asked and why
/// is its whole product. Both doors refuse a blank one rather than defaulting
/// it.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BaseReverify {
    /// The commit whose red receipt is being asked again.
    pub base: Digest,
    /// Why this red does not describe the tree, in the operator's own words.
    pub reason: String,
    /// Who asked. An unsigned identity, exactly as
    /// [`crate::OperatorHold::operator`] is: it records the decider, and a
    /// re-verify authorizes nothing.
    pub operator: String,
}

/// Content-addressed so the REST door can key a re-verify's idempotency on what
/// the act says. Two identical asks are one ask; a second one that differs in
/// any field is a distinct act and admits on its own.
impl ContentAddressed for BaseReverify {
    const DOMAIN: &'static str = "aether.bloomery.base_reverify";
}
