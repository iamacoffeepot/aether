//! The core's outbox: one [`Command`] per requested effect.

use aether_bloomery_kinds::{
    ApiCallResult, AppendRecords, CallOutcome, Digest, Event, Invoke, Processed, ReadArtifact, ReadArtifactResult,
    ReadClosure, ReadEvents, Warm, WatchHead,
};

use super::ticket::{
    ApiTicket, AppendTicket, ArtifactTicket, CallerId, ClosureTicket, EvaluateTicket, EventsTicket, InvokeTicket,
    LoadTicket, StatusTicket, WarmTicket, WatchTicket,
};

/// One effect the shell performs on the core's behalf.
///
/// `ReadEvents`, `ReadArtifact`, `ReadClosure`, `Append`, `Load`,
/// `Invoke`, `WatchHead`, `Warm`, `Evaluate`, `QueryStatus`, `Fetch`, and
/// `RunWorkspace` each carry the ticket the shell hands back with the reply.
/// `Answer` delivers a [`Call`'s](aether_bloomery_kinds::Call) one outcome
/// to a waiting caller, `Processed` delivers an
/// [`AwaitProcessed`](aether_bloomery_kinds::AwaitProcessed) barrier reply,
/// `Fetched` delivers a bundle root's fetch-on-miss answer, `ApiAnswered`
/// delivers the answer to a program API call a bundle root relayed, and `Abort`
/// reports that the core's journal view cannot be trusted or a required
/// record cannot be written; the shell maps it to `fatal_abort` (ADR-0063).
///
/// It carries [`Invoke`] and [`ReadArtifactResult`], whose closure members
/// hold `Blob`s, so it has no equality: tests compare fields.
#[derive(Debug, Clone)]
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
        /// Ticket the matching [`ReadArtifactResult`] arrives under.
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
    /// Load a bundle's wasm under the unit's bundle name for its digest.
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
        /// Digest of the loaded bundle whose root runs the program.
        bundle: Digest,
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
        /// Digest of the loaded bundle whose reactor root warms.
        bundle: Digest,
        /// The warmup batch.
        request: Warm,
    },
    /// Evaluate one live journal entry on a reactor root.
    Evaluate {
        /// Ticket the matching [`Evaluated`](aether_bloomery_kinds::Evaluated) reply arrives under.
        ticket: EvaluateTicket,
        /// Digest of the loaded bundle whose reactor root evaluates.
        bundle: Digest,
        /// The live entry.
        request: Event,
    },
    /// Ask one reactor root for its cursor and poison flag.
    QueryStatus {
        /// Ticket the matching [`Status`](aether_bloomery_kinds::Status) reply arrives under.
        ticket: StatusTicket,
        /// Digest of the loaded bundle whose reactor root answers.
        bundle: Digest,
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
    /// Deliver one fetch-on-miss answer, from the cache or a shared read.
    Fetched {
        /// Fetch to answer.
        caller: CallerId,
        /// The artifact read's result.
        result: ReadArtifactResult,
    },
    /// Send one program's relayed `Http` call to the http capability.
    Fetch {
        /// Ticket the matching [`FetchResult`](aether_http::FetchResult) arrives under.
        ticket: ApiTicket,
        /// The program's request.
        request: aether_http::Fetch,
    },
    /// Send one program's relayed `Workspace` call to the unit's workspace.
    RunWorkspace {
        /// Ticket the matching [`RunResult`](aether_bloomery_workspace::RunResult) arrives under.
        ticket: ApiTicket,
        /// The program's run.
        request: aether_bloomery_workspace::Run,
    },
    /// Deliver the answer to one relayed program API call.
    ApiAnswered {
        /// The relayed call to answer.
        caller: CallerId,
        /// The provider's reply, or the driver's refusal.
        result: ApiCallResult,
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
/// does not depend on `aether-kinds`. The core names a loaded bundle by its
/// digest alone; the shell keeps the loaded root's proven reference, taken
/// from the load reply's stamped sender, keyed by that digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    /// The bundle loaded; the shell holds its root.
    Loaded,
    /// The load failed; the digest becomes permanently unavailable.
    Failed {
        /// Human-readable failure.
        error: String,
    },
}

/// A provider's reply to [`Command::Fetch`] or [`Command::RunWorkspace`],
/// which the core relays back to the program as its kind and bytes.
#[derive(Debug, Clone)]
pub enum ApiReply {
    /// The http capability's answer to a relayed `Http` call.
    Fetch(aether_http::FetchResult),
    /// The workspace's answer to a relayed `Workspace` call.
    Workspace(aether_bloomery_workspace::RunResult),
}
