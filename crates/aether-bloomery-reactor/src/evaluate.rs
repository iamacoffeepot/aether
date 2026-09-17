//! Pure evaluation of authored reactor arms against a retained [`Owner`].
//!
//! [`Reactor::evaluate`] is the in-process preparation/evaluation boundary a
//! later actor-bundle generator can wrap. It does not execute intents, append
//! journal entries, or take an engine context. [`Output`] is the mail-capable
//! marker: storage-derived kinds do not implement it, so evaluation cannot
//! call their panicking positional codec.

use alloc::vec::Vec;

use aether_data::Kind;

use crate::error::PrepareError;
use crate::owner::Owner;
use crate::params::Params;
use crate::trigger::Trigger;
use crate::views::PublishSet;

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
    kind_name: &'static str,
    bytes: Vec<u8>,
}

impl Intent {
    /// Encode `output` through the mail codec.
    #[must_use]
    pub fn from_output<O: Output>(output: &O) -> Self {
        Self { kind_name: O::NAME, bytes: output.encode_into_bytes() }
    }

    /// Stored kind name of the output.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        self.kind_name
    }

    /// Mail-codec payload.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Decode as `O` when the kind name matches.
    #[must_use]
    pub fn decode<O: Output>(&self) -> Option<O> {
        if self.kind_name != O::NAME {
            return None;
        }
        O::decode_from_bytes(&self.bytes)
    }
}

/// Walks each authored rule's trigger, inferred parameter list, and output.
///
/// The visitor, not the signature macro, reads associated types such as
/// [`Params::Views`]. A later bundle generator uses this to collect shared
/// views without guessing from type names.
pub trait ArmVisitor {
    /// Observe one rule.
    fn visit<T, L, O>(&mut self, name: &'static str)
    where
        T: Trigger,
        L: Params<T>,
        L::Views: PublishSet,
        O: Output;
}

/// Authored reactor: named rules, inferred dependencies, pure evaluation.
///
/// `#[reactor]` generates this impl. Authors write `const NAME` and `#[rule]`
/// methods; they do not implement these methods by hand.
pub trait Reactor: Sized + 'static {
    /// Stable authoring name for this reactor.
    const NAME: &'static str;

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
    pub use alloc::collections::BTreeSet;
    pub use alloc::vec::Vec;
}
