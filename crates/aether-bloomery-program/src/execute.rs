//! The typed executor contract.

use crate::{Execution, Program, ReadArtifacts};

/// A runtime type that can perform executions of `P`. Never stored.
pub trait Execute<P: Program> {
    /// Perform one execution of `P` on `input`.
    ///
    /// # Errors
    ///
    /// [`Refusal`] is the only error an executor may return.
    fn execute(&self, input: P::Input, store: &dyn ReadArtifacts) -> Result<Execution<P>, Refusal>;
}

/// The only errors an executor may return. Maps 1:1 onto the three
/// executor-assignable [`aether_bloomery_kinds::FaultReason`] variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The executor declined to attempt.
    Refused(String),
    /// The input digest names nothing in the store.
    InputMissing,
    /// The input blob had the declared kind but did not decode.
    InputDecode,
}
