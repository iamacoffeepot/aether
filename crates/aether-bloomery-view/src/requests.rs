//! Cursor-bearing fold: outstanding program requests and their dedup key.

use alloc::collections::{BTreeMap, BTreeSet};
use core::error::Error;
use core::fmt;

use crate::sequence::{SequenceError, check_next};
use crate::view::View;
use aether_bloomery_kinds::{
    DecodeError, Digest, Entry, Fault, ProgramRef, ReactionFailed, RecordedHeadMove, RequestSource, Requested, Seq,
    Transition,
};
use aether_data::Kind;

/// Outstanding and completed program requests over a contiguous log prefix.
///
/// The cursor is the last applied [`Seq`]. It starts at `Seq(0)`, the empty
/// prefix. [`Self::apply`] requires the next contiguous sequence, including
/// unrelated entries, so the cursor is an exact statement about the observed
/// prefix rather than a best-effort watermark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requests {
    cursor: Seq,
    by_seq: BTreeMap<Seq, Request>,
    index: BTreeMap<(Option<Seq>, RequestSource), Seq>,
    outstanding: BTreeSet<Seq>,
    reaction_watermark: Seq,
}

impl Requests {
    /// Empty fold: cursor `Seq(0)`, no requests, watermark `Seq(0)`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cursor: Seq(0),
            by_seq: BTreeMap::new(),
            index: BTreeMap::new(),
            outstanding: BTreeSet::new(),
            reaction_watermark: Seq(0),
        }
    }

    /// Last applied sequence, or `Seq(0)` when nothing has been applied.
    #[must_use]
    pub const fn cursor(&self) -> Seq {
        self.cursor
    }

    /// Apply `entry` as the next contiguous sequence.
    ///
    /// Unrelated kinds advance the cursor. A [`Requested`] indexes the
    /// request under its `(cause, source)` dedup key (decision 8). A
    /// [`Transition`] or [`Fault`] records that request's outcome and clears
    /// it from [`Self::outstanding`] (decision 3). A [`ReactionFailed`] or a
    /// caused [`RecordedHeadMove`] raises [`Self::reaction_watermark`]
    /// (decision 9).
    ///
    /// # Errors
    ///
    /// [`RequestFoldError::Sequence`] when `entry.seq` is not the next
    /// contiguous sequence. [`RequestFoldError::Decode`] when a recognized
    /// entry does not decode. The other variants report a history this fold's
    /// only writer, the driver, could not have produced. On error, this fold
    /// is unchanged.
    pub fn apply(&mut self, entry: &Entry) -> Result<(), RequestFoldError> {
        check_next(self.cursor, entry.seq)?;

        if entry.kind == Requested::ID {
            self.apply_requested(entry)?;
        } else if entry.kind == Transition::ID {
            self.apply_transition(entry)?;
        } else if entry.kind == Fault::ID {
            self.apply_fault(entry)?;
        } else if entry.kind == ReactionFailed::ID {
            self.apply_reaction_failed(entry)?;
        } else if entry.kind == RecordedHeadMove::ID {
            self.apply_head_move(entry);
        }

        self.cursor = entry.seq;
        Ok(())
    }

    /// The request recorded under dedup key `(cause, source)`, if any (decision 8, decision 11).
    #[must_use]
    pub fn find(&self, cause: Option<Seq>, source: &RequestSource) -> Option<&Request> {
        let seq = self.index.get(&(cause, source.clone()))?;
        self.by_seq.get(seq)
    }

    /// The request recorded at `seq`, if any.
    #[must_use]
    pub fn get(&self, seq: Seq) -> Option<&Request> {
        self.by_seq.get(&seq)
    }

    /// Requests with no recorded outcome, in seq order (decision 9's restart pass).
    pub fn outstanding(&self) -> impl Iterator<Item = &Request> {
        self.outstanding.iter().filter_map(move |seq| self.by_seq.get(seq))
    }

    /// The highest cause among reaction-sourced records folded so far, or
    /// `Seq(0)` when none has been seen (decision 9's restart watermark).
    #[must_use]
    pub const fn reaction_watermark(&self) -> Seq {
        self.reaction_watermark
    }

    fn apply_requested(&mut self, entry: &Entry) -> Result<(), RequestFoldError> {
        let requested: Requested = entry.decode()?;
        let cause = entry.cause;
        if let Some(cause) = cause {
            check_cause_in_range(entry.seq, cause)?;
        }

        let is_reaction = matches!(requested.source, RequestSource::Reaction { .. });
        if is_reaction != cause.is_some() {
            return Err(RequestFoldError::SourceCause { seq: entry.seq });
        }

        let key = (cause, requested.source.clone());
        if let Some(&first) = self.index.get(&key) {
            return Err(RequestFoldError::DuplicateRequest { seq: entry.seq, first });
        }

        self.by_seq.insert(entry.seq, Request { seq: entry.seq, cause, requested, outcome: None });
        self.index.insert(key, entry.seq);
        self.outstanding.insert(entry.seq);
        if is_reaction {
            self.raise_watermark(cause.expect("reaction source carries a cause, checked above"));
        }
        Ok(())
    }

    fn apply_transition(&mut self, entry: &Entry) -> Result<(), RequestFoldError> {
        let transition: Transition = entry.decode()?;
        let outcome = Outcome::Transition { seq: entry.seq, transition: transition.clone() };
        self.apply_outcome(entry, &transition.program, transition.input, outcome)
    }

    fn apply_fault(&mut self, entry: &Entry) -> Result<(), RequestFoldError> {
        let fault: Fault = entry.decode()?;
        let outcome = Outcome::Fault { seq: entry.seq, fault: fault.clone() };
        self.apply_outcome(entry, &fault.program, fault.input, outcome)
    }

    fn apply_outcome(
        &mut self,
        entry: &Entry,
        program: &ProgramRef,
        input: Digest,
        outcome: Outcome,
    ) -> Result<(), RequestFoldError> {
        let cause = entry.cause.ok_or(RequestFoldError::UncausedOutcome { seq: entry.seq })?;
        check_cause_in_range(entry.seq, cause)?;

        let request = self.by_seq.get(&cause).ok_or(RequestFoldError::UnknownRequest { seq: entry.seq, cause })?;
        if &request.requested.program != program || request.requested.input != input {
            return Err(RequestFoldError::OutcomeMismatch { seq: entry.seq, request: cause });
        }
        if let Some(existing) = &request.outcome {
            return Err(RequestFoldError::SecondOutcome { seq: entry.seq, request: cause, first: existing.seq() });
        }

        let request = self.by_seq.get_mut(&cause).expect("looked up above");
        request.outcome = Some(outcome);
        self.outstanding.remove(&cause);
        Ok(())
    }

    fn apply_reaction_failed(&mut self, entry: &Entry) -> Result<(), RequestFoldError> {
        let _reaction_failed: ReactionFailed = entry.decode()?;
        let cause = entry.cause.ok_or(RequestFoldError::UncausedReactionFailure { seq: entry.seq })?;
        check_cause_in_range(entry.seq, cause)?;
        self.raise_watermark(cause);
        Ok(())
    }

    fn apply_head_move(&mut self, entry: &Entry) {
        // The move's payload is not decoded here; decoding it is `Heads`' job.
        if let Some(cause) = entry.cause {
            self.raise_watermark(cause);
        }
    }

    fn raise_watermark(&mut self, cause: Seq) {
        if cause.0 > self.reaction_watermark.0 {
            self.reaction_watermark = cause;
        }
    }
}

impl Default for Requests {
    fn default() -> Self {
        Self::new()
    }
}

impl View for Requests {
    type Error = RequestFoldError;

    fn empty() -> Self {
        Self::new()
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error> {
        for entry in entries {
            self.apply(entry)?;
        }
        Ok(())
    }
}

/// One program request, recorded before its attempt.
///
/// Only [`Requests::apply`] builds a value of this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    seq: Seq,
    cause: Option<Seq>,
    requested: Requested,
    outcome: Option<Outcome>,
}

impl Request {
    /// The `Requested` entry's own sequence: the request id (decision 3).
    #[must_use]
    pub const fn seq(&self) -> Seq {
        self.seq
    }

    /// The trigger seq for a `Reaction` source, or `None` for a `Native` source.
    #[must_use]
    pub const fn cause(&self) -> Option<Seq> {
        self.cause
    }

    /// The recorded request.
    #[must_use]
    pub const fn requested(&self) -> &Requested {
        &self.requested
    }

    /// This request's one recorded outcome, if it has completed.
    #[must_use]
    pub fn outcome(&self) -> Option<&Outcome> {
        self.outcome.as_ref()
    }
}

/// The one recorded outcome of a [`Request`] (decision 3, decision 11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The attempt ran and produced an execution.
    Transition {
        /// The `Transition` entry's own sequence.
        seq: Seq,
        /// The recorded transition.
        transition: Transition,
    },
    /// The attempt ended without an execution, `Interrupted` included.
    Fault {
        /// The `Fault` entry's own sequence.
        seq: Seq,
        /// The recorded fault.
        fault: Fault,
    },
}

impl Outcome {
    /// The seq of the entry that recorded this outcome.
    #[must_use]
    pub const fn seq(&self) -> Seq {
        match self {
            Self::Transition { seq, .. } | Self::Fault { seq, .. } => *seq,
        }
    }
}

/// Failure to fold one journal entry into [`Requests`].
#[derive(Debug)]
pub enum RequestFoldError {
    /// `entry.seq` was not the next contiguous sequence.
    Sequence(SequenceError),
    /// A recognized entry's payload did not decode.
    Decode(DecodeError),
    /// A cause did not name an earlier entry in this prefix.
    CauseOutOfRange {
        /// The entry that named the cause.
        seq: Seq,
        /// The cause it named.
        cause: Seq,
    },
    /// A `Requested`'s source did not match whether it carried a cause.
    SourceCause {
        /// The `Requested` entry.
        seq: Seq,
    },
    /// A `Requested` repeated a dedup key already recorded.
    DuplicateRequest {
        /// The repeating `Requested` entry.
        seq: Seq,
        /// The request first recorded under that key.
        first: Seq,
    },
    /// A `Transition` or `Fault` carried no cause.
    UncausedOutcome {
        /// The outcome entry.
        seq: Seq,
    },
    /// An outcome's cause named no recorded `Requested`.
    UnknownRequest {
        /// The outcome entry.
        seq: Seq,
        /// The cause it named.
        cause: Seq,
    },
    /// An outcome's program or input did not match its request's.
    OutcomeMismatch {
        /// The outcome entry.
        seq: Seq,
        /// The request it claimed to answer.
        request: Seq,
    },
    /// A request already had a recorded outcome.
    SecondOutcome {
        /// The second outcome entry.
        seq: Seq,
        /// The request it claimed to answer.
        request: Seq,
        /// The first recorded outcome for that request.
        first: Seq,
    },
    /// A `ReactionFailed` carried no cause.
    UncausedReactionFailure {
        /// The `ReactionFailed` entry.
        seq: Seq,
    },
}

impl fmt::Display for RequestFoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sequence(error) => write!(f, "{error}"),
            Self::Decode(error) => write!(f, "{error}"),
            Self::CauseOutOfRange { seq, cause } => {
                write!(f, "entry {seq} names cause {cause}, which is not an earlier entry in this prefix")
            }
            Self::SourceCause { seq } => write!(f, "entry {seq}'s request source does not match its cause"),
            Self::DuplicateRequest { seq, first } => {
                write!(f, "entry {seq} repeats the dedup key first recorded at {first}")
            }
            Self::UncausedOutcome { seq } => write!(f, "entry {seq} is an outcome with no cause"),
            Self::UnknownRequest { seq, cause } => {
                write!(f, "entry {seq}'s cause {cause} names no recorded request")
            }
            Self::OutcomeMismatch { seq, request } => {
                write!(f, "entry {seq}'s program or input does not match request {request}'s")
            }
            Self::SecondOutcome { seq, request, first } => {
                write!(f, "entry {seq} is a second outcome for request {request}, first recorded at {first}")
            }
            Self::UncausedReactionFailure { seq } => write!(f, "entry {seq} is a reaction failure with no cause"),
        }
    }
}

impl Error for RequestFoldError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sequence(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::CauseOutOfRange { .. }
            | Self::SourceCause { .. }
            | Self::DuplicateRequest { .. }
            | Self::UncausedOutcome { .. }
            | Self::UnknownRequest { .. }
            | Self::OutcomeMismatch { .. }
            | Self::SecondOutcome { .. }
            | Self::UncausedReactionFailure { .. } => None,
        }
    }
}

impl From<SequenceError> for RequestFoldError {
    fn from(error: SequenceError) -> Self {
        Self::Sequence(error)
    }
}

impl From<DecodeError> for RequestFoldError {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

fn check_cause_in_range(seq: Seq, cause: Seq) -> Result<(), RequestFoldError> {
    if cause.0 == 0 || cause.0 >= seq.0 {
        Err(RequestFoldError::CauseOutOfRange { seq, cause })
    } else {
        Ok(())
    }
}
