//! The evidence-naming seam: turn a pulled reference into the attempt result a
//! worker uploaded, decoding it from the artifact name alone.

use aether_bloomery::{Digest, EvidenceRef, Nonce, StageVerdict, VerifyFailureSet};

use super::admit::UploadedEvidence;

/// The seam that turns a pulled [`EvidenceRef`] into the attempt result a worker
/// uploaded. The executor port surfaces only references (name / id / size);
/// decoding the referenced bytes into an [`UploadedEvidence`] is this
/// evidence-return path's job (the `EvidenceRef` doc defers "decoding the
/// referenced bytes into a reducer attempt-result" here). The production
/// GitHub-artifact fetch + decode lands with the dispatch wiring (#3505); host
/// tests implement this directly to drive claims.
pub trait EvidenceClaims {
    /// The attempt result the referenced upload carries, or `None` when the
    /// reference is not a decodable attempt result.
    fn claim_for(&self, evidence: &EvidenceRef) -> Option<UploadedEvidence>;
}

/// The artifact-name prefix an attempt-result upload carries. The full name is
/// `attempt.<verdict>.<failure_mask>.<subject_hex>.<detail_hex>.<nonce>` — the thin wrapper
/// (#3501) names its evidence artifact this way, and the executor port's
/// nonce-scoped [`ExecutorShell::stream_evidence`](crate::bloomery::executor::ExecutorShell::stream_evidence)
/// returns it because the trailing `nonce` segment is delimiter-bounded (ADR-0149 §The line).
const ATTEMPT_ARTIFACT_PREFIX: &str = "attempt";

/// The artifact name the wrapper uploads an attempt result under, encoding the
/// verdict and the subject/detail digests so the pull-side [`NameEvidenceClaims`]
/// can decode it from the name alone — no artifact-byte fetch (the executor port
/// surfaces only references, and GitHub artifacts are opaque zips). The nonce is
/// the trailing delimiter-bounded segment `stream_evidence` filters on.
#[must_use]
pub fn attempt_artifact_name(nonce: &Nonce, subject: &Digest, verdict: StageVerdict, detail: &Digest) -> String {
    NameEvidenceClaims::attempt_artifact_name(nonce, subject, verdict, VerifyFailureSet::EMPTY, detail)
}

/// The production [`EvidenceClaims`]: decode an attempt result from the pulled
/// [`EvidenceRef`]'s **name** (the data channel a nonce-scoped GitHub artifact
/// exposes without a byte fetch), pairing the name-encoded verdict + failure
/// mask + subject + detail with the reference's own nonce. A reference whose name is not a
/// well-formed attempt artifact is skipped (`None`) — the same seam a study
/// artifact or a stray upload rides past. CI-verifiable against `FakeGithub`,
/// whose `seed_run_artifacts` sets the artifact name this decodes.
#[derive(Clone, Copy, Debug, Default)]
pub struct NameEvidenceClaims;

impl NameEvidenceClaims {
    /// Compose a canonical attempt name carrying ADR-0178's verifier mask.
    #[must_use]
    pub fn attempt_artifact_name(
        nonce: &Nonce,
        subject: &Digest,
        verdict: StageVerdict,
        failed_verifiers: VerifyFailureSet,
        detail: &Digest,
    ) -> String {
        format!(
            "{ATTEMPT_ARTIFACT_PREFIX}.{}.{}.{}.{}.{}",
            verdict_token(verdict),
            failed_verifiers.to_mask(),
            hex_of(subject),
            hex_of(detail),
            nonce.0,
        )
    }
}

impl EvidenceClaims for NameEvidenceClaims {
    fn claim_for(&self, reference: &EvidenceRef) -> Option<UploadedEvidence> {
        let rest = reference.name.strip_prefix(ATTEMPT_ARTIFACT_PREFIX)?.strip_prefix('.')?;
        // verdict . failure_mask . subject_hex . detail_hex . <nonce…>; the
        // nonce may itself contain '.', so bound the split to the four leading
        // fields.
        let mut fields = rest.splitn(5, '.');
        let verdict = verdict_from_token(fields.next()?)?;
        let mask_or_subject = fields.next()?;
        let (failed_verifiers, subject, detail, _named_nonce) = if mask_or_subject.len() == 2 {
            // A two-character token sits in the mask position, so it must be a
            // canonical ADR-0178 mask; anything else is a malformed attempt
            // name and buys no upload.
            (
                VerifyFailureSet::from_mask(mask_or_subject)?,
                digest_from_hex(fields.next()?)?,
                digest_from_hex(fields.next()?)?,
                fields.next()?,
            )
        } else {
            // The credential-bearing model wrapper is outside the mechanical
            // ADR-0178 lane and still emits the pre-mask name shape. Preserve
            // that non-Verify transport as an empty failure set; a malformed
            // short mask cannot take this path because a subject is 64 hex.
            (
                VerifyFailureSet::EMPTY,
                digest_from_hex(mask_or_subject)?,
                digest_from_hex(fields.next()?)?,
                fields.next()?,
            )
        };
        // The nonce, candidate, findings, and cost are authoritative from the
        // reference (what the port matched the run by / what the backend read
        // out of the run's own evidence), not the name. The failure set is the
        // name's, and the reference's own copy is no second opinion to check it
        // against: the Actions backend derives that copy from this very name
        // token, and the local backend composes the name from the copy it
        // reports, so the pair is one value on both transports. What does guard
        // the set is elsewhere — the malformed-mask refusal above, the local
        // backend's fail-closed body decode, the nonce binding a body to its
        // order, the artifact digest binding the bytes, and intake's
        // `verifier_failure_refusal`, which refuses a set that disagrees with
        // the order's stage and the claimed verdict.
        Some(UploadedEvidence {
            nonce: reference.nonce.clone(),
            subject,
            verdict,
            detail,
            failed_verifiers,
            candidate: reference.candidate,
            findings: reference.findings.clone(),
            cost: reference.cost,
        })
    }
}

/// The stable one-token spelling of a verdict in an attempt artifact name.
fn verdict_token(verdict: StageVerdict) -> &'static str {
    match verdict {
        StageVerdict::Approved => "approved",
        StageVerdict::VerificationPassed => "pass",
        StageVerdict::VerificationFailed => "fail",
        StageVerdict::ReviewFinding => "finding",
        StageVerdict::Parked => "parked",
        StageVerdict::ExecutorFault => "fault",
    }
}

/// Decode a verdict token; `None` for an unrecognized token (a non-attempt name).
fn verdict_from_token(token: &str) -> Option<StageVerdict> {
    Some(match token {
        "approved" => StageVerdict::Approved,
        "pass" => StageVerdict::VerificationPassed,
        "fail" => StageVerdict::VerificationFailed,
        "finding" => StageVerdict::ReviewFinding,
        "parked" => StageVerdict::Parked,
        "fault" => StageVerdict::ExecutorFault,
        _ => return None,
    })
}

/// Lowercase-hex a digest's 32 bytes.
fn hex_of(digest: &Digest) -> String {
    let mut hex = String::with_capacity(64);
    for byte in digest.as_bytes() {
        // Two lowercase hex nibbles per byte.
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    hex
}

/// Decode a 64-char lowercase-hex string into a [`Digest`]; `None` on any
/// non-hex character or a wrong length (a malformed / non-attempt name).
fn digest_from_hex(hex: &str) -> Option<Digest> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let high = hex.as_bytes()[index * 2] as char;
        let low = hex.as_bytes()[index * 2 + 1] as char;
        *byte = (u8::try_from(high.to_digit(16)?).ok()? << 4) | u8::try_from(low.to_digit(16)?).ok()?;
    }
    Some(Digest::from_bytes(bytes))
}
