//! The closed set an executing program may return.

use crate::program::fault::{Detail, FaultReason};
use crate::program::invoke::DigestMismatch;

/// Why a program produced no result. The driver maps this onto [`FaultReason`].
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub enum Refusal {
    /// The program declined to attempt. Bounded reason.
    Refused { reason: Detail },
    /// A cited digest was not present in the injected closure.
    InputMissing,
    /// A closure blob had the wrong kind or did not decode, or its bytes did
    /// not hash to the digest they were read under.
    InputDecode,
}

impl From<Refusal> for FaultReason {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Refused { reason } => Self::Refused { reason },
            Refusal::InputMissing => Self::InputMissing,
            Refusal::InputDecode => Self::InputDecode,
        }
    }
}

/// Bytes that do not hash to the digest they were read under are an input
/// that did not decode.
impl From<DigestMismatch> for Refusal {
    fn from(_: DigestMismatch) -> Self {
        Self::InputDecode
    }
}
