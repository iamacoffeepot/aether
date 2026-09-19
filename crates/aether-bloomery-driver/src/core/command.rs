//! The core's outbox: one [`Command`] per requested effect.

use aether_bloomery_kinds::{
    AppendRecords, CallOutcome, Digest, Event, Invoke, Processed, ReadArtifact, ReadClosure, ReadEvents, Warm,
    WatchHead,
};
use aether_data::MailboxId;

use super::ticket::{
    AppendTicket, ArtifactTicket, CallerId, ClosureTicket, EvaluateTicket, EventsTicket, InvokeTicket, LoadTicket,
    StatusTicket, WarmTicket, WatchTicket,
};

/// Which role a bundle digest serves. A digest serves one role; the bundle
/// table makes the other role unrepresentable once one is claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleRole {
    /// A program bundle, invoked per request.
    Program,
    /// A reactor bundle, warmed once and evaluated per seq.
    Reactor,
}

/// One effect the shell performs on the core's behalf.
///
/// `ReadEvents`, `ReadArtifact`, `ReadClosure`, `Append`, `Load`,
/// `Invoke`, `WatchHead`, `Warm`, `Evaluate`, and `QueryStatus` each carry
/// the ticket the shell hands back with the reply. `Answer` delivers a
/// [`Call`'s](aether_bloomery_kinds::Call) one outcome to a waiting caller,
/// `Processed` delivers an
/// [`AwaitProcessed`](aether_bloomery_kinds::AwaitProcessed) barrier reply,
/// and `Abort` reports that the core's journal view cannot be trusted or a
/// required record cannot be written; the shell maps it to `fatal_abort`
/// (ADR-0063).
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
        /// Which role the digest serves.
        role: BundleRole,
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
    /// Watch the journal head until it passes the request boundary.
    WatchHead {
        /// Ticket the matching [`WatchHeadResult`](aether_bloomery_kinds::WatchHeadResult) arrives under.
        ticket: WatchTicket,
        /// The watch request.
        request: WatchHead,
    },
    /// Warm one reactor root with a fold-only journal prefix.
    Warm {
        /// Ticket the matching [`Warmed`](aether_bloomery_kinds::Warmed) reply arrives under.
        ticket: WarmTicket,
        /// Mailbox of the digest's loaded reactor root.
        root: MailboxId,
        /// The warmup batch.
        request: Warm,
    },
    /// Evaluate one live journal entry on a reactor root.
    Evaluate {
        /// Ticket the matching [`Evaluated`](aether_bloomery_kinds::Evaluated) reply arrives under.
        ticket: EvaluateTicket,
        /// Mailbox of the digest's loaded reactor root.
        root: MailboxId,
        /// The live entry.
        request: Event,
    },
    /// Ask one reactor root for its cursor and poison flag.
    QueryStatus {
        /// Ticket the matching [`Status`](aether_bloomery_kinds::Status) reply arrives under.
        ticket: StatusTicket,
        /// Mailbox of the digest's loaded reactor root.
        root: MailboxId,
    },
    /// Deliver one caller's exactly-once outcome.
    Answer {
        /// Caller to answer.
        caller: CallerId,
        /// The recorded outcome.
        outcome: CallOutcome,
    },
    /// Deliver one barrier caller's processed head.
    Processed {
        /// Caller to answer.
        caller: CallerId,
        /// The observed journal head.
        reply: Processed,
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
