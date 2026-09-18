//! The core's outbox: one [`Command`] per requested effect.

use aether_bloomery_kinds::{AppendRecords, CallOutcome, Digest, Invoke, ReadArtifact, ReadClosure, ReadEvents};
use aether_data::MailboxId;

use super::ticket::{AppendTicket, ArtifactTicket, CallerId, ClosureTicket, EventsTicket, InvokeTicket, LoadTicket};

/// One effect the shell performs on the core's behalf.
///
/// `ReadEvents`, `ReadArtifact`, `ReadClosure`, `Append`, `Load`, and
/// `Invoke` each carry the ticket the shell hands back with the reply.
/// `Answer` delivers a [`Call`'s](aether_bloomery_kinds::Call) one outcome to
/// a waiting caller, and `Abort` reports that the core's journal view cannot
/// be trusted or a required record cannot be written; the shell maps it to
/// `fatal_abort` (ADR-0063).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Read journal entries after the request boundary.
    ReadEvents {
        /// Ticket the matching [`ReadEventsResult`](aether_bloomery_kinds::ReadEventsResult) arrives under.
        ticket: EventsTicket,
        /// The page request.
        request: ReadEvents,
    },
    /// Read one content-addressed bundle artifact.
    ReadArtifact {
        /// Ticket the matching [`ReadArtifactResult`](aether_bloomery_kinds::ReadArtifactResult) arrives under.
        ticket: ArtifactTicket,
        /// The artifact request.
        request: ReadArtifact,
    },
    /// Read one input's transitive closure under the core's byte budget.
    ReadClosure {
        /// Ticket the matching [`ReadClosureResult`](aether_bloomery_kinds::ReadClosureResult) arrives under.
        ticket: ClosureTicket,
        /// The closure request.
        request: ReadClosure,
    },
    /// Append one fenced batch of driver records and staged artifacts.
    Append {
        /// Ticket the matching [`AppendRecordsResult`](aether_bloomery_kinds::AppendRecordsResult) arrives under.
        ticket: AppendTicket,
        /// The fenced write.
        request: AppendRecords,
    },
    /// Load a bundle's wasm under its digest name.
    Load {
        /// Ticket the matching [`LoadOutcome`] arrives under.
        ticket: LoadTicket,
        /// Content digest the bundle loads under.
        bundle: Digest,
        /// The bundle's wasm bytes.
        wasm: Vec<u8>,
    },
    /// Invoke one program on a loaded bundle root.
    Invoke {
        /// Ticket the matching [`Invoked`](aether_bloomery_kinds::Invoked) reply arrives under.
        ticket: InvokeTicket,
        /// Mailbox of the digest's loaded root.
        root: MailboxId,
        /// The invocation.
        request: Invoke,
    },
    /// Deliver one caller's exactly-once outcome.
    Answer {
        /// Caller to answer.
        caller: CallerId,
        /// The recorded outcome.
        outcome: CallOutcome,
    },
    /// The journal view cannot be trusted or a required record cannot be written.
    Abort {
        /// Human-readable reason.
        reason: String,
    },
}

/// Shell-mapped outcome of [`Command::Load`].
///
/// The shell maps the substrate's `LoadResult` onto this type so the core
/// does not depend on `aether-kinds`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    /// The bundle loaded; `root` is its digest-named mailbox.
    Loaded {
        /// Mailbox of the loaded root.
        root: MailboxId,
    },
    /// The load failed; the digest becomes permanently unavailable.
    Failed {
        /// Human-readable failure.
        error: String,
    },
}
