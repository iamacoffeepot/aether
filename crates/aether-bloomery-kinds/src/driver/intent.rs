//! Reactor intents: the two kinds a rule's `ReactorIntent.kind` may name.

use aether_data::Kind;

use crate::{Digest, Head, OpaqueBytes, ProgramName, RecordedHead, RecordedHeadMove, Ref};

/// Ask the driver to run program `name` from the bundle `program` resolves to, over `input`.
///
/// A reactor intent: the driver records `Requested` with a `Reaction`
/// source, caused by the trigger seq, and resolves `program` from heads
/// folded through that seq (ADR-0226 decision 7).
#[aether_data::kind(name = "aether.bloomery.driver.call_program", eq, no_serde)]
pub struct CallProgram {
    pub program: Head<OpaqueBytes>,
    pub name: ProgramName,
    pub input: Digest,
}

/// Move one head, compare-and-swap on `from`.
///
/// A reactor intent: the driver appends [`Self::to_move`] only if the
/// head's binding at append is `from`; otherwise it records `ReactionFailed`.
#[aether_data::kind(name = "aether.bloomery.driver.set_head", eq, no_serde)]
pub struct SetHead {
    head: RecordedHead,
    from: Option<Digest>,
    to: Digest,
}

impl SetHead {
    /// Move `head` to `to`, compare-and-swap on `from`.
    ///
    /// The head and both digests must share one kind:
    ///
    /// ```compile_fail
    /// use aether_bloomery_kinds::{Digest, Head, Program, Ref, SetHead, Tree};
    ///
    /// let head = Head::<Program>::new("main");
    /// let to = Ref::<Tree>::from_digest(Digest::from_bytes([0; 32]));
    /// let _ = SetHead::new(&head, None, to);
    /// ```
    ///
    /// The same-kind call compiles:
    ///
    /// ```
    /// use aether_bloomery_kinds::{Digest, Head, Ref, SetHead, Tree};
    ///
    /// let head = Head::<Tree>::new("main");
    /// let to = Ref::<Tree>::from_digest(Digest::from_bytes([0; 32]));
    /// let _ = SetHead::new(&head, None, to);
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
}
