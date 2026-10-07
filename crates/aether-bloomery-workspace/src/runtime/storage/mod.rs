//! Every read and stage a task makes, carried as mail through its request's
//! source (ADR-0240 D7).
//!
//! The tar codec and the run sequence are synchronous and run on a worker
//! thread, which sends no mail. So each read or stage a worker needs is a
//! request its own actor sends for it, following the `aether-tcp` sidecar
//! shape (an `mpsc` channel plus a [`SelfWake`]) inside one actor:
//!
//! - [`StorageDesk`] lives in the actor's state. It holds the request
//!   receiver, the wake, and a table from each open task's ticket to that
//!   task's resolved source and answer sender. [`StorageDesk::open`] hands a
//!   task its [`StorageSession`].
//! - A [`StoragePort`], inside the session, moves onto the worker. A request
//!   pushes `(ticket, seq, call)` and wakes the actor; the worker then blocks
//!   on the port's own answer receiver. Dropping the port sends a close, so
//!   the desk forgets the ticket on its next turn and its table never holds
//!   more than the tasks still open.
//! - [`StorageDesk::drain`], on the wake, sends each request through its
//!   task's source with the ctx's request-context table carrying a
//!   [`StorageTicket`], and [`StorageDesk::answer`] hands the reply the actor
//!   took back to the waiting worker. An answer for a ticket that has closed
//!   is dropped: no worker waits for it.
//!
//! The table is the actor's own state and the payloads cross only between
//! its dispatcher and its own workers. When the actor closes, the answer
//! senders drop and a waiting worker's receive fails with
//! [`StorageError::Closed`], so its task ends `Failed` and never hangs.
//!
//! A session reads through [`SourceReader`] (closure windows read ahead of
//! the archive) and writes through [`StagingSink`] (bounded batches, one in
//! flight).

mod sink;
mod source;

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::mpsc;

use aether_actor::{ProtocolRef, ReplyMode};
use aether_bloomery_kinds::{
    ArtifactStorage, ClosureLimit, DigestMismatch, ReadArtifact, ReadArtifactResult, ReadArtifacts,
    ReadArtifactsResult, ReadClosure, ReadClosureResult, Stage, StageResult,
};
use aether_data::Digest;
use aether_substrate::actor::native::{BlobCheckIn, NativeCtx, SelfWake};

pub use sink::{StagingBlob, StagingSink};
pub use source::{SourceReader, StoredBlob};

use crate::StorageWake;
use sink::Staging;
use source::Fetched;

/// One request a worker asks its actor to send through its task's source.
pub enum StorageCall {
    Read(ReadArtifact),
    ReadMany(ReadArtifacts),
    ReadClosure(ReadClosure),
    Stage(Stage),
}

/// The source's answer to one [`StorageCall`], as the actor received it.
pub enum StorageAnswer {
    Read(ReadArtifactResult),
    ReadMany(ReadArtifactsResult),
    ReadClosure(ReadClosureResult),
    Stage(StageResult),
}

/// What a port pushes to its desk.
enum Message {
    /// Send `call` through the source of the task `ticket` names; its answer
    /// comes back under `seq`.
    Call { ticket: u64, seq: u64, call: StorageCall },
    /// The task `ticket` names is done: forget it.
    Close { ticket: u64 },
}

/// The request context one storage request is sent under, taken back from
/// its reply to find the task and the request it answers.
#[aether_data::kind(name = "aether.workspace.storage_ticket", copy, eq, no_serde)]
pub struct StorageTicket {
    ticket: u64,
    seq: u64,
}

/// One open task's source and the sender its answers go back through.
struct Open {
    source: ProtocolRef<ArtifactStorage>,
    answers: mpsc::Sender<(u64, StorageAnswer)>,
}

/// The actor's side of every open task's storage requests.
pub struct StorageDesk {
    requests: mpsc::Receiver<Message>,
    sender: mpsc::Sender<Message>,
    wake: SelfWake<StorageWake>,
    open: HashMap<u64, Open>,
    next_ticket: u64,
    /// The read budget each session holds or has asked for at most.
    prefetch: ClosureLimit,
}

impl StorageDesk {
    /// A desk whose ports wake the actor through `wake`, and whose sessions
    /// each hold or have asked for at most `prefetch` bytes at once.
    pub fn new(wake: SelfWake<StorageWake>, prefetch: ClosureLimit) -> Self {
        let (sender, requests) = mpsc::channel();
        Self { requests, sender, wake, open: HashMap::new(), next_ticket: 0, prefetch }
    }

    /// Open a session for one task over its resolved `source`. The task's
    /// worker checks in the bytes it stages through `check_in`.
    pub fn open(&mut self, source: ProtocolRef<ArtifactStorage>, check_in: BlobCheckIn) -> StorageSession {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let (answers, receiver) = mpsc::channel();
        self.open.insert(ticket, Open { source, answers });

        let port = StoragePort {
            ticket,
            requests: self.sender.clone(),
            wake: self.wake.clone(),
            answers: receiver,
            next_seq: 0,
            early: HashMap::new(),
        };
        StorageSession { port, fetched: Fetched::new(self.prefetch), staging: Staging::default(), check_in }
    }

    /// Send every queued request through its task's source, and forget every
    /// task that has closed. A request from a task that has already closed is
    /// not sent.
    pub fn drain<A, S, M: ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, A, S, M>) {
        while let Ok(message) = self.requests.try_recv() {
            let (ticket, seq, call) = match message {
                Message::Close { ticket } => {
                    self.open.remove(&ticket);
                    continue;
                }
                Message::Call { ticket, seq, call } => (ticket, seq, call),
            };
            let Some(open) = self.open.get(&ticket) else {
                continue;
            };
            let context = StorageTicket { ticket, seq };
            let _ = match call {
                StorageCall::Read(request) => ctx.send_detached_to_with_context(open.source, &request, context),
                StorageCall::ReadMany(request) => ctx.send_detached_to_with_context(open.source, &request, context),
                StorageCall::ReadClosure(request) => ctx.send_detached_to_with_context(open.source, &request, context),
                StorageCall::Stage(request) => ctx.send_detached_to_with_context(open.source, &request, context),
            };
        }
    }

    /// Hand `answer` to the worker waiting under `ticket`. An answer for a
    /// task that has closed is dropped, since no worker waits for it.
    pub fn answer(&self, ticket: StorageTicket, answer: StorageAnswer) {
        if let Some(open) = self.open.get(&ticket.ticket) {
            // A worker that has gone already sent its close, which the next
            // drain applies.
            let _ = open.answers.send((ticket.seq, answer));
        }
    }
}

/// A worker's end of its task's storage requests.
pub struct StoragePort {
    ticket: u64,
    requests: mpsc::Sender<Message>,
    wake: SelfWake<StorageWake>,
    answers: mpsc::Receiver<(u64, StorageAnswer)>,
    next_seq: u64,
    /// Answers that arrived while the worker waited for another.
    early: HashMap<u64, StorageAnswer>,
}

impl StoragePort {
    /// Queue `call` for the actor to send and wake it; its answer comes back
    /// under the returned sequence.
    fn send(&mut self, call: StorageCall) -> Result<u64, StorageError> {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.requests.send(Message::Call { ticket: self.ticket, seq, call }).map_err(|_| StorageError::Closed)?;
        self.wake.wake(&StorageWake);
        Ok(seq)
    }

    /// Block until the answer under `seq` arrives, keeping any other that
    /// arrives first.
    fn wait(&mut self, seq: u64) -> Result<StorageAnswer, StorageError> {
        if let Some(answer) = self.early.remove(&seq) {
            return Ok(answer);
        }
        loop {
            let (arrived, answer) = self.answers.recv().map_err(|_| StorageError::Closed)?;
            if arrived == seq {
                return Ok(answer);
            }
            self.early.insert(arrived, answer);
        }
    }

    /// The answer under `seq` if it has arrived, without blocking, keeping any
    /// other that arrived with it.
    fn poll(&mut self, seq: u64) -> Option<StorageAnswer> {
        while let Ok((arrived, answer)) = self.answers.try_recv() {
            self.early.insert(arrived, answer);
        }
        self.early.remove(&seq)
    }

    /// Send `call` and wait for its answer.
    fn call(&mut self, call: StorageCall) -> Result<StorageAnswer, StorageError> {
        self.send(call).and_then(|seq| self.wait(seq))
    }
}

impl Drop for StoragePort {
    fn drop(&mut self) {
        // An actor that has closed has nothing left to forget.
        let _ = self.requests.send(Message::Close { ticket: self.ticket });
        self.wake.wake(&StorageWake);
    }
}

/// One task's reads and stages over one port: what it has read, and what it
/// is staging.
pub struct StorageSession {
    port: StoragePort,
    fetched: Fetched,
    staging: Staging,
    check_in: BlobCheckIn,
}

impl StorageSession {
    /// Read through the session's source.
    pub fn reader(&mut self) -> SourceReader<'_> {
        SourceReader::new(&mut self.port, &mut self.fetched)
    }

    /// Stage through the session's source.
    pub fn sink(&mut self) -> StagingSink<'_> {
        StagingSink::new(&mut self.port, &mut self.staging, &self.check_in)
    }

    /// Send what is still filling and wait until every stage is answered.
    ///
    /// # Errors
    ///
    /// The first stage the source refused, or [`StorageError::Closed`].
    pub fn finish(&mut self) -> Result<(), StorageError> {
        self.sink().finish()
    }
}

/// Why a read or a stage through a source failed.
#[derive(Debug)]
pub enum StorageError {
    /// The source stores no artifact under the digest.
    Missing(Digest),
    /// The source answered with an error; the text is its own.
    Refused(String),
    /// The workspace closed before the answer arrived.
    Closed,
    /// The stored bytes do not hash to the digest they were read under.
    Mismatch(DigestMismatch),
    /// The artifact under the digest is stored as another kind.
    OtherKind(Digest),
    /// The artifact under the digest did not decode.
    Decode(Digest),
    /// A value the workspace built did not encode.
    Encode(String),
    /// The source answered a request with another request's row.
    Answer,
}

impl StorageError {
    /// The failure's class, for a reply the driver records: never the
    /// source's own words.
    #[must_use]
    pub fn class(&self) -> String {
        match self {
            Self::Missing(digest) => format!("the source stores no artifact {digest}"),
            Self::Refused(_) => "the source failed".to_owned(),
            Self::Closed => "the workspace closed".to_owned(),
            Self::Mismatch(_) => "the stored bytes do not hash to their digest".to_owned(),
            Self::OtherKind(_) => "the stored artifact is another kind".to_owned(),
            Self::Decode(_) => "the stored artifact did not decode".to_owned(),
            Self::Encode(_) => "a tree did not encode".to_owned(),
            Self::Answer => "the source answered another request".to_owned(),
        }
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(digest) => write!(f, "the source stores no artifact {digest}"),
            Self::Refused(message) => write!(f, "the source failed: {message}"),
            Self::Closed => f.write_str("the workspace closed before the source answered"),
            Self::Mismatch(mismatch) => mismatch.fmt(f),
            Self::OtherKind(digest) => write!(f, "the artifact {digest} is stored as another kind"),
            Self::Decode(digest) => write!(f, "the artifact {digest} did not decode"),
            Self::Encode(error) => write!(f, "a tree did not encode: {error}"),
            Self::Answer => f.write_str("the source answered with another request's row"),
        }
    }
}

impl Error for StorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mismatch(mismatch) => Some(mismatch),
            _ => None,
        }
    }
}
