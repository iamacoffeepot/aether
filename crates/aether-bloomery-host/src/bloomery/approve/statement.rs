//! The statement-approval entry points: the synchronous pre-checks an above-auto
//! signed [`Statement`] must pass, and the evidence-formation that populates a
//! membership `approval` from a verified statement.

use aether_bloomery::{Digest, Evidence, EvidenceKind, KeyProvider, Statement, digest_of};

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
pub fn precheck_statement(scope_revision: Digest, statement: &Statement) -> Result<(), StatementRejected> {
    if statement.words.as_slice() != scope_revision.as_bytes() {
        return Err(StatementRejected::WrongSubject);
    }
    if !statement.is_instruction_capable() {
        return Err(StatementRejected::NotAnAuthorSignature);
    }
    Ok(())
}

/// Form the above-auto membership `approval` [`Evidence`] for a statement whose
/// signature **has already verified** — bound to the `scope_revision` and
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
pub fn verified_statement_approval(scope_revision: Digest, statement: &Statement) -> Evidence {
    Evidence { subject: scope_revision, kind: EvidenceKind::Approval, detail: digest_of(statement) }
}

/// Populate an above-auto membership `approval` from an owner-authorized signed
/// [`Statement`] (ADR-0151, #3560). The statement must sign exactly the
/// `scope_revision` bytes it approves, be an author signature, and verify against
/// the host's [`KeyProvider`] (the `aether.signing` capability's allowlist) —
/// every other case is a fail-closed rejection. On success the formed `approval`
/// [`Evidence`] binds the `scope_revision` and details the signed statement, so
/// the seal-time `validate_member_admission` accepts it exactly as it does an
/// auto approval.
///
/// This is a **distinct** reader from the tier policy: tier policy decides *what*
/// tier a surface earns; this key-policy verification decides *who* may sign in
/// the owner's stead. The two are never folded (ADR-0151 owner rider 1).
///
/// The synchronous composition of [`precheck_statement`] → authority verification
/// → [`verified_statement_approval`]; the deferred-verify seal path splits the
/// same three steps across the async `aether.signing` boundary.
///
/// # Errors
/// [`StatementRejected`] if the statement's subject, provenance, or signature
/// does not hold.
pub fn approval_from_statement(
    scope_revision: Digest,
    statement: &Statement,
    keys: &dyn KeyProvider,
) -> Result<Evidence, StatementRejected> {
    precheck_statement(scope_revision, statement)?;
    if !statement.verify_authority(keys) {
        return Err(StatementRejected::Unverified);
    }
    Ok(verified_statement_approval(scope_revision, statement))
}
