//! The statement-approval entry points: the synchronous pre-checks an above-auto
//! signed [`Statement`] must pass, and the evidence-formation that populates a
//! membership `approval` from a verified statement.

use aether_bloomery::{AuthorityDoor, Digest, Evidence, EvidenceKind, KeyProvider, Statement, Tier, digest_of};

/// Why an above-auto approval's signed statement was rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatementRejected {
    /// The statement's signed words are not the scope revision it must approve —
    /// a statement signed for another revision never approves this one
    /// (ADR-0149: old evidence never validates a replacement).
    WrongSubject,
    /// The statement carries no author signature, so it can never be instruction.
    NotAnAuthorSignature,
    /// The author signature did not verify against the host key policy (#3560).
    Unverified,
    /// The signature verified, but the signer is not authorized that high: the
    /// declared surface resolved a tier above the signer's key-policy ceiling
    /// (#5324). A genuine signature by an allowlisted key, refused on authority
    /// rather than on cryptography.
    BelowTier {
        /// The tier the member's declared surface resolved at.
        required: Tier,
        /// The highest tier this signer's allowlist entry authorizes.
        ceiling: Tier,
    },
}

/// Whether the signer of an already-verified `statement` may approve at
/// `required`, per the key policy's per-signer tier ceiling (#5324).
///
/// The **second** key-policy question, and the one a signature check cannot
/// answer. `verify_authority` establishes that these words were asserted by an
/// allowlisted key at this door for this binding; it says nothing about whether
/// that key stands in for the reader the tier policy asked for. Without this
/// check one allowlist entry authorizes every tier, so an operator key signs a
/// `human`-tier surface and the gate admits it exactly as it would an `auto`
/// one — human tier enforced by the human declining to sign rather than by the
/// machine.
///
/// Runs **after** verification, never before: a ceiling lookup on a merely
/// *claimed* signer would answer for a key the caller has not proven it holds,
/// which turns the refusal message into an oracle over the allowlist.
///
/// # Errors
/// [`StatementRejected::BelowTier`] naming both tiers when the signer's ceiling
/// is below `required`, and [`StatementRejected::Unverified`] when the
/// statement carries no author signature or its signer has no ceiling at all —
/// both unreachable behind a successful verification, and both fail closed
/// rather than assuming an authority the policy did not state.
pub fn check_signer_tier(
    statement: &Statement,
    keys: &dyn KeyProvider,
    required: Tier,
) -> Result<(), StatementRejected> {
    let Some(ceiling) = statement.author_signer().and_then(|signer| keys.tier_ceiling(signer)) else {
        return Err(StatementRejected::Unverified);
    };
    if ceiling < required {
        return Err(StatementRejected::BelowTier { required, ceiling });
    }
    Ok(())
}

/// The two **synchronous** pre-checks an above-auto signed [`Statement`] must
/// pass before its signature is verified: the words must be exactly the
/// `scope_revision` bytes it approves (a statement signed for another revision
/// never approves this one — ADR-0149: old evidence never validates a
/// replacement), and it must be an author signature (only an author signature
/// can become instruction).
///
/// Neither check needs the key policy, so they run without the async
/// `aether.signing` round trip. The synchronous seal path composes them inside
/// [`approval_from_statement`]; the deferred-verify seal path (#3599) runs them
/// itself before dispatching the async `Verify`, so a mis-subjected or
/// non-author above-auto member fails closed *before* any signing dispatch.
///
/// # Errors
/// [`StatementRejected::WrongSubject`] or [`StatementRejected::NotAnAuthorSignature`].
pub fn precheck_statement(subject: Digest, statement: &Statement) -> Result<(), StatementRejected> {
    if statement.words.as_slice() != subject.as_bytes() {
        return Err(StatementRejected::WrongSubject);
    }
    if !statement.is_instruction_capable() {
        return Err(StatementRejected::NotAnAuthorSignature);
    }
    Ok(())
}

/// Form the above-auto membership `approval` [`Evidence`] for a statement whose
/// signature **has already verified** — bound to the member `subject` and
/// detailing the signed statement, so the seal-time `validate_member_admission`
/// accepts it exactly as it does an auto approval.
///
/// This is the evidence-formation half of [`approval_from_statement`], split out
/// so the deferred-verify seal path (#3599) reuses the *exact* evidence format
/// after the `aether.signing` capability's `Verify` round trip has verified the
/// signature — the api cap holds no key material, so verification is that async
/// port call, never a local [`KeyProvider`]. The caller is responsible for
/// having verified authority (via [`Statement::verify_authority`] or the signing
/// port) and run [`precheck_statement`] first.
#[must_use]
pub fn verified_statement_approval(subject: Digest, statement: &Statement) -> Evidence {
    Evidence { subject, kind: EvidenceKind::Approval, detail: digest_of(statement) }
}

/// Populate an above-auto membership `approval` from an owner-authorized signed
/// [`Statement`] (ADR-0151, #3560). The statement must sign exactly the
/// `scope_revision` bytes it approves, be an author signature, verify against
/// the host's [`KeyProvider`] (the `aether.signing` capability's allowlist), and
/// come from a signer that key policy authorizes at `required` or above
/// (#5324) — every other case is a fail-closed rejection. On success the formed `approval`
/// [`Evidence`] binds the `scope_revision` and details the signed statement, so
/// the seal-time `validate_member_admission` accepts it exactly as it does an
/// auto approval.
///
/// This is a **distinct** reader from the tier policy: tier policy decides *what*
/// tier a surface earns; this key-policy verification decides *who* may sign in
/// the owner's stead, and how high (#5324). The two are never folded (ADR-0151
/// owner rider 1) — the gate holds both answers and compares them, which is a
/// different thing from one reader computing the other. `required` is the tier
/// policy's answer, arriving here as a parameter precisely so this function
/// cannot resolve it itself.
///
/// The synchronous composition of [`precheck_statement`] → authority verification
/// → [`verified_statement_approval`]; the deferred-verify seal path splits the
/// same three steps across the async `aether.signing` boundary.
///
/// The `scope_revision` binds this approval twice, and both bindings are kept.
/// It is the signed `binding` under [`AuthorityDoor::Approve`] (ADR-0182), and
/// it remains the words [`precheck_statement`] compares byte-for-byte. This door
/// is the shape ADR-0182 generalized from — its binding was already inside the
/// signed bytes as the words themselves — so the door check is what it gains: an
/// approve envelope cannot be presented at the answer or release door even
/// though its words are a digest either could name.
///
/// # Errors
/// [`StatementRejected`] if the statement's subject, provenance, signature, or
/// signer authority does not hold.
pub fn approval_from_statement(
    scope_revision: Digest,
    statement: &Statement,
    keys: &dyn KeyProvider,
    required: Tier,
) -> Result<Evidence, StatementRejected> {
    precheck_statement(scope_revision, statement)?;
    if !statement.verify_authority(keys, AuthorityDoor::Approve, scope_revision) {
        return Err(StatementRejected::Unverified);
    }
    check_signer_tier(statement, keys, required)?;
    Ok(verified_statement_approval(scope_revision, statement))
}
