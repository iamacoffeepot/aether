//! The core's outbox: one [`Command`] per requested effect.

use aether_bloomery_kinds::{
    ApiCallResult, AppendRecords, CallOutcome, Event, Invoke, Processed, ReadArtifact, ReadArtifactResult,
    ReadArtifacts, ReadClosure, ReadEvents, Warm, WatchHead,
};
use aether_data::Digest;

use super::ticket::{
    ApiTicket, AppendTicket, ArtifactTicket, ArtifactsTicket, CallerId, ClosureTicket, EvaluateTicket, EventsTicket,
    InvokeTicket, LoadTicket, StatusTicket, WarmTicket, WatchTicket,
};

/// One effect the shell performs on the core's behalf.
///
/// `ReadEvents`, `ReadArtifact`, `ReadArtifacts`, `ReadClosure`, `Append`, `Load`,
/// `Invoke`, `WatchHead`, `Warm`, `Evaluate`, `QueryStatus`, `Fetch`, and
/// `RunWorkspace` each carry the ticket the shell hands back with the reply.
/// `Answer` delivers a [`Call`'s](aether_bloomery_kinds::Call) one outcome
/// to a waiting caller, `Processed` delivers an
/// [`AwaitProcessed`](aether_bloomery_kinds::AwaitProcessed) barrier reply,
/// `Fetched` delivers a bundle root's fetch-on-miss answer, `ApiAnswered`
/// delivers the answer to a program API call a bundle root relayed, `ArmTick`
/// asks for one clock tick, and `Abort` reports that the core's journal view
/// cannot be trusted or a required record cannot be written; the shell maps
/// it to `fatal_abort` (ADR-0063).
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
    /// Read the artifacts a routing page's entries cite, answered as a
    /// prefix under the core's byte budget.
    ReadArtifacts {
        /// Ticket the matching [`ReadArtifactsResult`](aether_bloomery_kinds::ReadArtifactsResult) arrives under.
        ticket: ArtifactsTicket,
        /// The batched artifact request.
        request: ReadArtifacts,
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
    /// Stand a bundle's root up under the unit's bundle name for its digest:
    /// publish its wasm, then spawn its root under the unit key.
    Load {
        /// Ticket the matching [`LoadOutcome`] arrives under.
        ticket: LoadTicket,
        /// Content digest the bundle loads under.
        bundle: Digest,
        /// The roles the bundle declares, which the shell casts its root to.
        roles: RootRoles,
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
    /// Send one program's relayed `Workspace` call to the workspace, over
    /// the unit's journal as its source.
    RunWorkspace {
        /// Ticket the matching [`RunResult`](aether_bloomery_workspace::RunResult) arrives under.
        ticket: ApiTicket,
        /// The program's run, which names no source: the shell adds its
        /// unit's.
        request: aether_bloomery_workspace::RunRequest,
    },
    /// Deliver the answer to one relayed program API call.
    ApiAnswered {
        /// The relayed call to answer.
        caller: CallerId,
        /// The provider's reply, or the driver's refusal.
        result: ApiCallResult,
    },
    /// Wait one tick period, then read the clock and feed it to
    /// [`ProgramCore::tick`](crate::ProgramCore::tick) (ADR-0245). The core
    /// asks for one tick at a time, and only while a timer is armed.
    ArmTick,
    /// The journal view cannot be trusted or a required record cannot be written.
    Abort {
        /// Human-readable reason.
        reason: String,
    },
}

/// The roles a loading bundle declares, as the shell needs them: which
/// protocols it casts the bundle's root to at the spawn reply. A root that
/// does not publish a role it declares fails its load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootRoles {
    /// The bundle declares programs, so its root is a
    /// [`ProgramRoot`](aether_bloomery_kinds::ProgramRoot).
    pub programs: bool,
    /// The bundle declares reactors, so its root is a
    /// [`ReactorRoot`](aether_bloomery_kinds::ReactorRoot).
    pub reactors: bool,
}

/// Shell-mapped outcome of [`Command::Load`].
///
/// The shell maps the component host's `PublishResult` and `SpawnResult`
/// onto this type so the core does not depend on `aether-kinds`. The core
/// names a loaded bundle by its digest alone; the shell keeps the root's
/// typed references, cast from the spawn reply's stamped sender once per
/// declared role, keyed by that digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    /// The spawn stood the bundle's root up; the shell holds it.
    Loaded,
    /// The engine already held the bundle's root live under the unit key, so
    /// nothing was stood up and the shell holds that root (ADR-0226 D9): a
    /// reactor root resumes from the cursor it reports, not from 0.
    Adopted,
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
