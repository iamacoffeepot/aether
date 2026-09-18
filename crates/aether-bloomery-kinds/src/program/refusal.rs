//! The closed set an executing program may return.

use crate::program::fault::{Detail, FaultReason};

/// Why a program produced no result. The driver maps this onto [`FaultReason`].
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub enum Refusal {
    /// The program declined to attempt. Bounded reason.
    Refused { reason: Detail },
    /// A cited digest was not present in the injected closure.
    InputMissing,
    /// A closure blob had the wrong kind or did not decode.
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
