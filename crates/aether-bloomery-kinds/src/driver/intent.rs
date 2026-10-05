//! Reactor intents: the two kinds a rule's `ReactorIntent.kind` may name.

use alloc::vec::Vec;

use aether_data::{Cites, Digest, Kind, OpaqueBytes, Ref, Storage, StorageError};

use crate::{EncodedArtifact, Head, ProgramName, RecordedHead, RecordedHeadMove};

/// Input submitted by a reactor call intent.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub enum CallInput {
    /// Invoke over an artifact that is already stored.
    Stored(Digest),
    /// Persist this freshly encoded input with the resulting `Requested` record.
    Value(EncodedArtifact),
}

/// Ask the driver to run program `name` from the bundle `program` resolves to, over `input`.
///
/// A reactor intent: the driver records `Requested` with a `Reaction`
/// source, caused by the trigger seq, and resolves `program` from heads
/// folded through that seq (ADR-0226 decision 7).
#[aether_data::kind(name = "aether.bloomery.driver.call_program.v2", eq, no_serde)]
pub struct CallProgram {
    pub program: Head<OpaqueBytes>,
    pub name: ProgramName,
    pub input: CallInput,
}

impl CallProgram {
    /// Build a call carrying a freshly encoded input value.
    ///
    /// # Errors
    ///
    /// Returns a storage error if `input` cannot be encoded.
    pub fn with_input<K: Storage + Clone + Cites>(
        program: Head<OpaqueBytes>,
        name: ProgramName,
        input: &K,
    ) -> Result<Self, StorageError> {
        Ok(Self { program, name, input: CallInput::Value(EncodedArtifact::new(input)?) })
    }
}

/// One typed head change in an atomic [`SetHeads`] group.
///
/// The head and both references share one kind at construction. On the wire,
/// the recorded head retains that kind alongside the untyped digests.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub struct HeadChange {
    head: RecordedHead,
    from: Option<Digest>,
    to: Digest,
}

impl HeadChange {
    /// Move `head` to `to`, compare-and-swap on `from`.
    ///
    /// The head and both digests must share one kind:
    ///
    /// ```compile_fail
    /// use aether_bloomery_kinds::{Head, HeadChange, Program, Tree};
    /// use aether_data::{Digest, Ref};
    ///
    /// let head = Head::<Program>::new("main");
    /// let to = Ref::<Tree>::from_digest(Digest::from_bytes([0; 32]));
    /// let _ = HeadChange::new(&head, None, to);
    /// ```
    ///
    /// The same-kind call compiles:
    ///
    /// ```
    /// use aether_bloomery_kinds::{Head, HeadChange, Tree};
    /// use aether_data::{Digest, Ref};
    ///
    /// let head = Head::<Tree>::new("main");
    /// let to = Ref::<Tree>::from_digest(Digest::from_bytes([0; 32]));
    /// let _ = HeadChange::new(&head, None, to);
    /// ```
    #[must_use]
    pub fn new<K: Kind>(head: &Head<K>, from: Option<Ref<K>>, to: Ref<K>) -> Self {
        Self { head: RecordedHead::from(head), from: from.map(|from| from.digest()), to: to.digest() }
    }

    /// Head to move.
    #[must_use]
    pub const fn head(&self) -> &RecordedHead {
        &self.head
    }

    /// Compare-and-swap fence: the binding the caller expects at append.
    #[must_use]
    pub const fn from(&self) -> Option<Digest> {
        self.from
    }

    /// Destination digest. The expected prefix is [`RecordedHead::kind`].
    #[must_use]
    pub const fn to(&self) -> Digest {
        self.to
    }

    /// The move the driver appends when the compare-and-swap holds.
    #[must_use]
    pub fn to_move(&self) -> RecordedHeadMove {
        RecordedHeadMove::new(self.head.clone(), self.to)
    }

    /// Take the head, fence, and destination.
    #[must_use]
    pub fn into_parts(self) -> (RecordedHead, Option<Digest>, Digest) {
        (self.head, self.from, self.to)
    }

    pub(super) fn from_recorded(head: RecordedHead, from: Option<Digest>, to: Digest) -> Self {
        Self { head, from, to }
    }
}

/// Atomically compare and move a group of heads.
///
/// A reactor intent: the driver validates every destination and comparison,
/// then appends every [`HeadChange`] in list order or records one
/// `ReactionFailed` without moving any head.
#[aether_data::kind(name = "aether.bloomery.driver.set_heads", eq, no_serde)]
pub struct SetHeads {
    changes: Vec<HeadChange>,
}

impl SetHeads {
    /// Build one atomic group in comparison and append order.
    #[must_use]
    pub const fn new(changes: Vec<HeadChange>) -> Self {
        Self { changes }
    }

    /// Changes in comparison and append order.
    #[must_use]
    pub fn changes(&self) -> &[HeadChange] {
        &self.changes
    }

    /// Take the ordered changes.
    #[must_use]
    pub fn into_changes(self) -> Vec<HeadChange> {
        self.changes
    }
}
