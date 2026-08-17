//! Proof-fact addressing and recording for the verification ledger
//! (ADR-0200 §The fact).
//!
//! A proof fact is addressed by `(closure_key, test, result, host_class)`.
//! [`closure_key`] hashes a package's git-addressed dependency closure;
//! [`HostClass`] is the opaque host the coordinator supplies; [`discriminate`]
//! is the only constructor of facts the ledger will store.

mod closure;
mod facts;

pub use closure::{ClosureKey, ClosureKeyError, closure_key};
#[cfg(feature = "runtime")]
pub use facts::record_proof_facts;
pub use facts::{DiscriminatedFact, DiscriminatedFacts, ProofResult, ProofSource, RunnerReport, discriminate};

/// The host class a proof fact is keyed on (ADR-0200 integrity rule 2).
///
/// Opaque on purpose: the coordinator supplies the string (fleet host vs GPU
/// host). This type does not detect or classify hosts.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct HostClass(String);

impl HostClass {
    /// Wrap a coordinator-supplied host class string.
    #[must_use]
    pub fn new(class: impl Into<String>) -> Self {
        Self(class.into())
    }

    /// The coordinator-supplied string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests;
