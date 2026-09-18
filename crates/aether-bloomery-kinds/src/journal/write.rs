//! Fenced journal writes over mail: a pure head move and an atomic publish.
//!
//! Both commands carry `expected_seq`, the whole-journal fence: the journal's
//! last stored sequence as the caller observed it, or zero for an empty
//! journal. A stale fence writes nothing. Neither carries a journal cause;
//! every event `MoveHead` / `Publish` appends is uncaused. The native
//! bundle driver's caused records and head moves append instead through
//! [`crate::AppendRecords`] (`journal::append`).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use aether_data::{Cites, Kind, Storage, StorageError};

use crate::{Digest, EncodedArtifact, Head, HeadMoved, RecordedHead, RecordedHeadMove, Ref};

/// Move one typed head to an artifact the journal already stores.
///
/// Carries no artifact bytes. The journal refuses a destination that is
/// missing or whose stored prefix is not the head's kind.
#[aether_data::kind(name = "aether.bloomery.journal.move_head", eq, no_serde)]
pub struct MoveHead {
    head: RecordedHead,
    to: Digest,
    expected_seq: u64,
}

impl MoveHead {
    /// Point `head` at `to` if the journal's last stored sequence is still `expected_seq`.
    ///
    /// The head and destination must have the same kind:
    ///
    /// ```compile_fail
    /// use aether_bloomery_kinds::{Digest, Head, MoveHead, Program, Ref, Tree};
    /// let head = Head::<Program>::new("main");
    /// let _ = MoveHead::new(&head, Ref::<Tree>::from_digest(Digest::from_bytes([0; 32])), 0);
    /// ```
    #[must_use]
    pub fn new<K: Kind>(head: &Head<K>, to: Ref<K>, expected_seq: u64) -> Self {
        Self { head: RecordedHead::from(head), to: to.digest(), expected_seq }
    }

    /// Carry an already-built typed move under the whole-journal fence `expected_seq`.
    #[must_use]
    pub fn from_event<K: Kind>(event: &HeadMoved<K>, expected_seq: u64) -> Self {
        Self::new(event.head(), event.to(), expected_seq)
    }

    /// Head to move.
    #[must_use]
    pub const fn head(&self) -> &RecordedHead {
        &self.head
    }

    /// Destination digest. The expected prefix is the head's kind.
    #[must_use]
    pub const fn to(&self) -> Digest {
        self.to
    }

    /// Whole-journal fence: the last stored sequence the caller observed.
    #[must_use]
    pub const fn expected_seq(&self) -> u64 {
        self.expected_seq
    }

    /// Take the head, destination, and fence.
    #[must_use]
    pub fn into_parts(self) -> (RecordedHead, Digest, u64) {
        (self.head, self.to, self.expected_seq)
    }
}

/// Outcome of one fenced head move.
#[aether_data::kind(name = "aether.bloomery.journal.move_head_result", eq, no_serde)]
pub enum MoveHeadResult {
    /// The move event was appended at `seq`.
    Committed { seq: u64 },
    /// The supplied whole-journal fence was stale; nothing was written.
    Conflict { actual: u64 },
    /// Destination validation or the journal backend refused the append.
    Err { message: String },
}

/// Stage encoded artifacts and append head moves in one fenced journal append.
///
/// Moves may point at artifacts staged by the same command or already
/// stored. Every citation and destination is verified before anything is
/// written; any refusal rolls back every artifact and move.
#[aether_data::kind(name = "aether.bloomery.journal.publish", eq, no_serde)]
pub struct Publish {
    artifacts: Vec<EncodedArtifact>,
    moves: Vec<RecordedHeadMove>,
    expected_seq: u64,
}

impl Publish {
    /// Publish `artifacts` and append `moves` if the journal's last stored sequence is still `expected_seq`.
    #[must_use]
    pub fn new(artifacts: Vec<EncodedArtifact>, moves: Vec<RecordedHeadMove>, expected_seq: u64) -> Self {
        Self { artifacts, moves, expected_seq }
    }

    /// Encode `value` and move `head` to it in one publish.
    ///
    /// The head and value must have the same storage kind:
    ///
    /// ```compile_fail
    /// use aether_bloomery_kinds::{Head, Program, Publish, Tree};
    /// let head = Head::<Program>::new("main");
    /// let _ = Publish::head(&head, &Tree::empty(), 0);
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a storage error if `value` cannot be encoded.
    pub fn head<K: Storage + Clone + Cites>(
        head: &Head<K>,
        value: &K,
        expected_seq: u64,
    ) -> Result<Self, StorageError> {
        let artifact = EncodedArtifact::new(value)?;
        let moved = RecordedHeadMove::new(RecordedHead::from(head), artifact.digest());
        Ok(Self::new(vec![artifact], vec![moved], expected_seq))
    }

    /// Artifacts to stage, in request order.
    #[must_use]
    pub fn artifacts(&self) -> &[EncodedArtifact] {
        &self.artifacts
    }

    /// Head moves to append, in request order.
    #[must_use]
    pub fn moves(&self) -> &[RecordedHeadMove] {
        &self.moves
    }

    /// Whole-journal fence: the last stored sequence the caller observed.
    #[must_use]
    pub const fn expected_seq(&self) -> u64 {
        self.expected_seq
    }

    /// Take the artifacts, moves, and fence without copying payload bytes.
    #[must_use]
    pub fn into_parts(self) -> (Vec<EncodedArtifact>, Vec<RecordedHeadMove>, u64) {
        (self.artifacts, self.moves, self.expected_seq)
    }
}

/// Outcome of one fenced publish.
#[aether_data::kind(name = "aether.bloomery.journal.publish_result", eq, no_serde)]
pub enum PublishResult {
    /// Everything was written. `head` is the journal head after the append;
    /// the moves occupy `expected_seq + 1 ..= head`. `artifacts` are the
    /// staged digests in request order.
    Committed { head: u64, artifacts: Vec<Digest> },
    /// The supplied whole-journal fence was stale; nothing was written.
    Conflict { actual: u64 },
    /// Citation or destination validation or the journal backend refused the append.
    Err { message: String },
}
