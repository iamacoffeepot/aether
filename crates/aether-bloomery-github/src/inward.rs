//! The inward stage-result normalizer (#3459 step 6) — *port shape only*.
//!
//! The one inward channel ADR-0149 permits: a reviewer verdict or check-run
//! conclusion on work **Bloomery itself dispatched** normalizes into an
//! [`Evidence`] artifact bound to the exact digest Bloomery displayed, ready
//! to enter the reducer like any other attempt result. Free-form platform
//! activity is never translated — a comment never becomes a command; this
//! channel carries only a verdict over a digest Bloomery already put on the
//! wire.
//!
//! This slice ships the type + the pure normalizer + its tests. It does
//! **not** wire GitHub webhook / checks ingestion — that lands with the
//! migration step 2 executor/review bridge. The load-bearing invariant proven
//! here is ADR-0149 §The value vocabulary's: **evidence never validates a
//! digest it does not name.** A verdict whose subject is not the displayed
//! digest is rejected, not silently rebound.

use core::fmt;
use std::error::Error;

use aether_bloomery::{Digest, Evidence, EvidenceKind};

/// What an observed platform stage result asserts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StageVerdict {
    /// An owner (or policy) approval of a scope revision.
    Approved,
    /// A verification stage passed.
    VerificationPassed,
    /// A verification stage failed.
    VerificationFailed,
    /// A review finding was recorded.
    ReviewFinding,
}

impl StageVerdict {
    fn evidence_kind(self) -> EvidenceKind {
        match self {
            Self::Approved => EvidenceKind::Approval,
            Self::VerificationPassed | Self::VerificationFailed => EvidenceKind::VerificationResult,
            Self::ReviewFinding => EvidenceKind::ReviewFinding,
        }
    }
}

/// A stage result as the inward channel observed it: the digest the platform
/// object claimed to be about, the verdict, and the supporting artifact's
/// digest. `subject` is what the observation *claims*; the normalizer checks
/// it against what Bloomery actually displayed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StageResult {
    /// The digest the observed result claims to be about.
    pub subject: Digest,
    /// The verdict the result carries.
    pub verdict: StageVerdict,
    /// The supporting artifact (the check output, the review record).
    pub detail: Digest,
}

/// Why an inward stage result was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InwardError {
    /// The result's subject is not the digest Bloomery displayed. Binding it
    /// would produce evidence naming a digest the stage never ran against —
    /// the exact failure the value-vocabulary invariant forbids.
    DigestMismatch {
        /// The digest Bloomery displayed and expects the verdict to be about.
        displayed: Digest,
        /// The digest the observed result actually claimed.
        claimed: Digest,
    },
}

impl fmt::Display for InwardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DigestMismatch { .. } => {
                f.write_str("inward stage result names a digest other than the one displayed")
            }
        }
    }
}

impl Error for InwardError {}

/// Normalize an observed stage `result` into [`Evidence`] bound to the
/// `displayed` digest.
///
/// The result is accepted only when its `subject` is exactly the digest
/// Bloomery displayed; the produced evidence names that digest. A result
/// claiming any other digest is rejected with [`InwardError::DigestMismatch`]
/// — evidence never validates a digest it does not name.
///
/// # Errors
/// Returns [`InwardError::DigestMismatch`] when `result.subject != *displayed`.
pub fn normalize_stage_result(displayed: &Digest, result: &StageResult) -> Result<Evidence, InwardError> {
    if result.subject != *displayed {
        return Err(InwardError::DigestMismatch { displayed: *displayed, claimed: result.subject });
    }
    Ok(Evidence { subject: *displayed, kind: result.verdict.evidence_kind(), detail: result.detail })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_bloomery::{Digest, EvidenceKind};

    use super::{InwardError, StageResult, StageVerdict, normalize_stage_result};

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    #[test]
    fn a_verdict_over_the_displayed_digest_normalizes_to_evidence_naming_it() {
        let displayed = digest(1);
        let result = StageResult { subject: displayed, verdict: StageVerdict::Approved, detail: digest(9) };

        let evidence = normalize_stage_result(&displayed, &result).expect("matching subject normalizes");

        // The evidence names the exact displayed digest, and validates it.
        assert_eq!(evidence.subject, displayed);
        assert!(evidence.validates(&displayed));
        assert_eq!(evidence.kind, EvidenceKind::Approval);
        assert_eq!(evidence.detail, digest(9));
    }

    #[test]
    fn a_verdict_whose_digest_differs_is_rejected() {
        // The value-vocabulary invariant: evidence never validates a digest it
        // does not name. A verdict about digest(2) can never become evidence
        // bound to the displayed digest(1).
        let displayed = digest(1);
        let result = StageResult { subject: digest(2), verdict: StageVerdict::Approved, detail: digest(9) };

        let error = normalize_stage_result(&displayed, &result).unwrap_err();
        assert_eq!(error, InwardError::DigestMismatch { displayed, claimed: digest(2) });
    }

    #[test]
    fn verdict_variants_map_to_their_evidence_kinds() {
        let subject = digest(3);
        let cases = [
            (StageVerdict::Approved, EvidenceKind::Approval),
            (StageVerdict::VerificationPassed, EvidenceKind::VerificationResult),
            (StageVerdict::VerificationFailed, EvidenceKind::VerificationResult),
            (StageVerdict::ReviewFinding, EvidenceKind::ReviewFinding),
        ];
        for (verdict, kind) in cases {
            let result = StageResult { subject, verdict, detail: digest(0) };
            let evidence = normalize_stage_result(&subject, &result).expect("matching subject");
            assert_eq!(evidence.kind, kind);
        }
    }
}
