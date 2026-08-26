//! The `aether.signing.*` transact-mail kind family (ADR-0149 step 3).
//!
//! The typed request the live answer gate (`api/runtime.rs`) sends to the
//! `aether.signing` mailbox to verify an author-signed statement against the
//! host-custodied allowlist, plus its reply. The
//! [`aether_bloomery::Statement`] value type is carried as its canonical
//! [`aether_data::wire`] bytes rather than typed fields — exactly as the
//! `aether.source.*` family carries its port values — because `Statement` is
//! serde-encoded but not `Schema`, and this capability has no reason to key or
//! filter on any of its fields.

use aether_bloomery::{AuthorityDoor, Digest, Tier};
use aether_data::wire::to_vec;
use serde::{Deserialize, Serialize};

/// Verify `statement`'s author signature against the host-custodied
/// authorized-signer allowlist (ADR-0151 key policy), as authority for exactly
/// the request `authority` names.
///
/// The capability verifies against the authority the *caller* supplies, never
/// against anything read out of the statement's own bytes (ADR-0182). That is
/// the whole point: a statement's `parents` are outside its signature, so a door
/// that let the envelope name its own target would accept a captured envelope
/// re-pointed at any request. Each route derives its binding independently — the
/// seal path from the member's scope revision, the answer route from the
/// question the request named, the release route by recomputing the request
/// digest from the typed target in the body.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.signing.verify")]
pub struct Verify {
    /// The `aether_data::wire`-encoded [`aether_bloomery::Statement`] to verify.
    #[serde(with = "aether_data::bytes")]
    pub statement: Vec<u8>,
    /// The `aether_data::wire`-encoded `(AuthorityDoor, Digest)` the host
    /// derived — which door this verification is for, and the exact request
    /// digest the signature must be bound to. Build it with [`authority_bytes`].
    #[serde(with = "aether_data::bytes")]
    pub authority: Vec<u8>,
    /// The tier the caller's own policy resolved for what this signature would
    /// approve, or `None` at a door that has no tier ladder (#5324).
    ///
    /// Deliberately **outside** [`authority`](Self::authority): that field is
    /// the signed subject, and the tier is not part of what anyone signed.
    /// Re-hashing it into the authorization message would invalidate every
    /// envelope ever minted and would let a caller move a signature's meaning
    /// by restating the tier. It rides beside instead, as a question the
    /// capability answers from the allowlist — is this signer authorized this
    /// high — after the signature itself has held.
    ///
    /// `None` asks only the signature question, which is what the cancel,
    /// reopen, answer, and claim-release doors want: they are authorized by an
    /// allowlisted signature, not by an approval tier.
    pub required_tier: Option<Tier>,
}

/// Encode the `(door, binding)` authority a [`Verify`] carries.
///
/// Infallible by invariant, like [`aether_bloomery::digest_of`]: a closed enum's
/// tag plus 32 digest bytes cannot approach the ADR-0118 `u32` wire-length
/// ceiling, so an encode failure here is a broken invariant rather than a
/// runtime condition a caller could answer.
///
/// # Panics
///
/// Panics if the pair fails to wire-encode, which cannot happen for any
/// `(AuthorityDoor, Digest)`.
#[must_use]
pub fn authority_bytes(door: AuthorityDoor, binding: Digest) -> Vec<u8> {
    to_vec(&(door, binding)).expect("a door tag and a 32-byte digest never approach the ADR-0118 u32 ceiling")
}

/// Reply to [`Verify`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.signing.verify_result")]
pub enum VerifyResult {
    /// The statement and the authority both decoded; `verified` is whether the
    /// author signature checks against an allowlisted signer over the
    /// authorization message for that door, that binding, and the statement's
    /// exact words. A non-author provenance, an unknown / mismatched / malformed
    /// signature, and a genuine signature supplied under a door or binding it was
    /// not signed for all resolve `verified: false` — the gate is fail-closed.
    Ok {
        /// Whether the author signature verified.
        verified: bool,
    },
    /// The signature verified, but the signer is not authorized that high: the
    /// caller's `required_tier` is above this signer's allowlist ceiling
    /// (#5324). Its own variant rather than `Ok { verified: false }` because
    /// the two are different facts with different repairs — one says the
    /// envelope is no good, this one says the envelope is fine and the wrong
    /// person signed it, and only a refusal that names both tiers tells an
    /// operator which.
    BelowTier {
        /// The tier the caller's policy resolved for the approved surface.
        required: Tier,
        /// The highest tier this signer's allowlist entry authorizes.
        ceiling: Tier,
    },
    /// The request could not be evaluated — the `statement` bytes did not decode
    /// to a [`aether_bloomery::Statement`], or the `authority` bytes did not
    /// decode to a `(AuthorityDoor, Digest)`. Distinct from
    /// `Ok { verified: false }` (a well-formed request that simply did not
    /// verify).
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}
