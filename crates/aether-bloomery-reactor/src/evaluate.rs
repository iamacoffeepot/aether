//! Pure evaluation of authored reactor arms against a retained [`Owner`].
//!
//! [`Reactor::evaluate`] is the in-process preparation/evaluation boundary the
//! `bundle_reactors` export generator wraps. It does not execute intents, append
//! journal entries, or take an engine context. [`Output`] is the mail-capable
//! marker: storage-derived kinds do not implement it, so evaluation cannot
//! call their panicking positional codec.

use alloc::vec::Vec;

use aether_data::{Kind, KindId};

use crate::error::PrepareError;
use crate::owner::Owner;
use crate::params::Params;
use crate::trigger::Trigger;

/// Marker for a mail-capable arm output.
///
/// Implement this for kinds that override [`Kind::encode_into_bytes`] with a
/// real mail codec. Do not implement it for [`aether_data::Storage`] types;
/// those panic on positional encoding and stay on the checked storage path.
pub trait Output: Kind + 'static {}

/// One typed output produced by an invoked arm.
///
/// Bytes are the existing mail codec. Evaluation never appends them and never
/// runs an executor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    rule: &'static str,
    kind: KindId,
    bytes: Vec<u8>,
}

impl Intent {
    /// Encode `output` through the mail codec under `rule`.
    #[must_use]
    pub fn from_output<O: Output>(rule: &'static str, output: &O) -> Self {
        Self { rule, kind: O::ID, bytes: output.encode_into_bytes() }
    }

    /// Rule that produced the output.
    #[must_use]
    pub const fn rule(&self) -> &'static str {
        self.rule
    }

    /// Mail kind id of the output.
    #[must_use]
    pub const fn kind(&self) -> KindId {
        self.kind
    }

    /// Mail-codec payload.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Decode as `O` when the kind id matches.
    #[must_use]
    pub fn decode<O: Output>(&self) -> Option<O> {
        if self.kind != O::ID {
            return None;
        }
        O::decode_from_bytes(&self.bytes)
    }

    /// Take the rule, kind, and payload.
    #[must_use]
    pub fn into_parts(self) -> (&'static str, KindId, Vec<u8>) {
        (self.rule, self.kind, self.bytes)
    }
}

/// Walks each authored rule's trigger, inferred parameter list, and output.
///
/// The visitor, not the signature macro, reads associated types such as
/// [`Params::Views`]. `bundle_reactors` uses this to collect shared views without
/// guessing from type names.
pub trait ArmVisitor {
    /// Observe one rule.
    fn visit<T, L, O>(&mut self, name: &'static str)
    where
        T: Trigger,
        L: Params<T>,
        O: Output;
}

/// Authored reactor: named rules, inferred dependencies, pure evaluation.
///
/// `#[reactor]` generates this impl. Authors write `const NAMESPACE` and `#[rule]`
/// methods on a unit struct, so the authored reactor cannot store per-instance
/// state. Rules receive only their prepared trigger, view, and guard values.
/// This construction check does not prove arbitrary Rust bodies side-effect
/// free; direct implementations of this lower-level trait are not checked.
pub trait Reactor: Sized + 'static {
    /// Stable reactor identity inside the bundle, validated as a `ReactorName`.
    const NAMESPACE: &'static str;

    /// Describe each rule's trigger, parameter list, and output type.
    fn visit_arms(visitor: &mut impl ArmVisitor);

    /// Prepare matching rules against `owner`'s current trigger and invoke
    /// each resolved arm once.
    ///
    /// A refutable trigger pattern or a [`crate::Guard`] that returns [`None`]
    /// declines that arm. A stored-kind or typed-specialization mismatch
    /// declines that arm. Other [`PrepareError`] values fail closed.
    ///
    /// # Errors
    ///
    /// [`PrepareError`] when the prefix is empty, a matching trigger cannot be
    /// decoded, or a required view fold fails.
    fn evaluate(&self, owner: &mut Owner) -> Result<Vec<Intent>, PrepareError>;
}

impl Owner {
    /// Evaluate `reactor` against the current retained trigger.
    ///
    /// # Errors
    ///
    /// [`PrepareError`] from [`Reactor::evaluate`].
    pub fn evaluate<R: Reactor>(&mut self, reactor: &R) -> Result<Vec<Intent>, PrepareError> {
        reactor.evaluate(self)
    }
}

#[doc(hidden)]
pub mod __macro_internals {
    pub use aether_bloomery_kinds::{ReactorName, RuleName};
    pub use alloc::collections::BTreeSet;
    pub use alloc::string::ToString;
    pub use alloc::vec::Vec;
}
