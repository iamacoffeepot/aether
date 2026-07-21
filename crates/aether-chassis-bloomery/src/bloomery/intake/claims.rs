//! The evidence-naming seam: turn a pulled reference into the attempt result a
//! worker uploaded, decoding it from the artifact name alone.

use aether_bloomery::{Digest, EvidenceRef, Nonce};
use aether_bloomery_github::StageVerdict;

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
/// `attempt.<verdict>.<subject_hex>.<detail_hex>.<nonce>` — the thin wrapper
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
    format!("{ATTEMPT_ARTIFACT_PREFIX}.{}.{}.{}.{}", verdict_token(verdict), hex_of(subject), hex_of(detail), nonce.0)
}

/// The production [`EvidenceClaims`]: decode an attempt result from the pulled
/// [`EvidenceRef`]'s **name** (the data channel a nonce-scoped GitHub artifact
/// exposes without a byte fetch), pairing the name-encoded verdict + subject +
/// detail with the reference's own nonce. A reference whose name is not a
/// well-formed attempt artifact is skipped (`None`) — the same seam a study
/// artifact or a stray upload rides past. CI-verifiable against `FakeGithub`,
/// whose `seed_run_artifacts` sets the artifact name this decodes.
#[derive(Clone, Copy, Debug, Default)]
pub struct NameEvidenceClaims;

impl EvidenceClaims for NameEvidenceClaims {
    fn claim_for(&self, reference: &EvidenceRef) -> Option<UploadedEvidence> {
        let rest = reference.name.strip_prefix(ATTEMPT_ARTIFACT_PREFIX)?.strip_prefix('.')?;
        // verdict . subject_hex . detail_hex . <nonce…>; the nonce may itself
        // contain '.', so bound the split to the three leading fields.
        let mut fields = rest.splitn(4, '.');
        let verdict = verdict_from_token(fields.next()?)?;
        let subject = digest_from_hex(fields.next()?)?;
        let detail = digest_from_hex(fields.next()?)?;
        // The nonce and candidate are authoritative from the reference (what the
        // port matched the run by / what the backend captured), not the name.
        Some(UploadedEvidence {
            nonce: reference.nonce.clone(),
            subject,
            verdict,
            detail,
            candidate: reference.candidate,
            findings: reference.findings.clone(),
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
