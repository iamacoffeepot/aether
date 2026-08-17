//! Proof-fact addressing for the verification ledger (ADR-0200 §The fact).
//!
//! A proof fact is addressed by `(closure_key, test, result, host_class)`. This
//! slice owns the two halves that do not need a journal row: the
//! [`closure_key`] computed over a package's git-addressed dependency closure,
//! and the opaque [`HostClass`] the coordinator supplies. The key is a value.

mod closure;

pub use closure::{ClosureKey, ClosureKeyError, closure_key};

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
#[allow(clippy::unwrap_used, reason = "a fixture that cannot be built is a broken test, not a recoverable path")]
mod tests;
