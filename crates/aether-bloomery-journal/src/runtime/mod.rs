//! The `aether.bloomery.journal` runtime half (ADR-0122 split), compiled only
//! under `feature = "runtime"`: the append-only, single-writer log of typed
//! events over one journal root, and the native actor that owns it.
//!
//! A root is a directory holding `journal.sqlite` (the event log and one
//! row per stored artifact) and `blobs/<first two hex>/<digest hex>`, one
//! file per artifact holding exactly the bytes its digest hashes. Each blob
//! file is written to a temp file as it streams; at commit a batch fsyncs its
//! files side by side, renames each to its digest name, and fsyncs each
//! directory the renames touched once, all before the row that names the
//! file commits. [`Journal::open`] takes an exclusive lock on the
//! root, so a second open fails in any process, and sweeps `blobs/tmp/`.
//! [`Journal::open`] also stores the empty [`aether_bloomery_kinds::Tree`],
//! with no event, so a program can cite it without staging it.
//! [`JournalReader`] observes a root without the lock. See ADR-0220.
//!
//! Two layers, and the split is the point:
//!
//! - **The store stays raw.** Artifacts are content-addressed
//!   (`digest` = `sha256(bytes)`) and the store knows nothing about kinds.
//! - **An artifact is an abstraction above the store:** a blob whose bytes
//!   are an eight-byte [`aether_data::KindId`] prefix followed by a payload.
//!   The digest covers the kind, so a digest names one kind and one payload.
//!
//! Events are [`aether_data::Storage`] kinds. The only event write is a
//! [`Batch`] of staged artifacts plus events; [`Journal::append`] judges it.
//! Artifacts also land through a second door over the same root lock: an
//! [`ArtifactStore`] derived by [`Journal::artifact_store`] opens
//! [`ArtifactBatch`]es on any thread, which stream blobs into their files a
//! chunk at a time ([`BlobFile`]) and commit their rows through the same row
//! insert and citation check `append` runs (ADR-0237 open question 1).
//! Citations are typed [`Ref`](crate::Ref) values collected by a derive-emitted walk.
//! The one recognized exception is `bloomery.head_moved`: `append` decodes
//! that kind from the draft as [`RecordedHeadMove`]
//! and verifies that its destination exists with the recorded head's
//! eight-byte prefix. A missing citation walk cannot stand in for that
//! check. See ADR-0220.
//!
//! [`JournalIdentity`] is a process-local allocation token minted by each
//! constructor so a view registry can detect replacement. It is not persisted
//! and is not a SQL column.
//!
//! [`JournalActor`] is the native owner for one named journal root. It answers
//! read, head, and artifact mail plus three fenced writes while keeping its
//! journal handle inside the actor: [`MoveHead`] moves
//! one head to an already-stored artifact, [`Publish`]
//! stages encoded artifacts and appends head moves in one atomic batch, and
//! [`AppendRecords`] stages artifacts and appends the
//! native bundle driver's own records and head moves, each under its own
//! cause. All three fence on the whole journal's last stored sequence;
//! `MoveHead` / `Publish` carry no cause, and `AppendRecords` is the one
//! write that does. It also answers [`WatchHead`], a
//! bounded long poll that is answered once a committed write moves the head
//! past `after`, [`ReadClosure`], a read of an
//! artifact's transitive closure under a validated byte limit, and
//! [`ReadArtifacts`], a read of several named artifacts that answers the
//! prefix fitting its byte limit. [`ReadArtifact`], `ReadClosure`, and
//! `ReadArtifacts` all run off the actor's thread, each on its own task
//! queue (ADR-0093), so a large artifact, closure, or batch never holds up
//! the actor's other requests. All three reuse members the journal still
//! holds, under the [`ReadCacheBudget`], and read only the misses. None of
//! them hashes a member: each answers it claiming the digest it was stored
//! under, and the receiver verifies the bytes before relying on them
//! (ADR-0238 decision 11), so a stored file whose bytes do not hash to its
//! digest is answered and fails at the receiver's check.
//!
//! [`Stage`] is the one unfenced write: it stores encoded artifacts
//! content-addressed, with no event and no head move, through an
//! [`ArtifactBatch`] on a worker thread of its own task queue, streaming each
//! payload from its [`aether_data::Blob`] into its file. The answer lands
//! after the batch commits, and a dangling citation refuses the whole stage.
//! Those four rows, `ReadArtifact`, `ReadArtifacts`, `ReadClosure`, and
//! `Stage`, are the
//! [`aether_bloomery_kinds::ArtifactStorage`] protocol the owner covers.
//!
//! A blob stored for the first time records its citation edges in the
//! `citations` table inside the same append transaction, so the edges are
//! fixed when the blob is, and a later re-staging with a different citation
//! list cannot change them. [`Journal::read_closure`] walks those edges
//! breadth-first under a byte budget and never truncates (ADR-0226
//! decision 10). Artifacts stored before the table existed have no edges.
//!
//! Each entry's own citations are kept the same way, in `entry_citations`,
//! written in the append transaction: the typed `Ref`s its event cites, a
//! head move's destination, and a `Transition`'s input and result.
//! [`Journal::read_cited`] returns them beside each entry, and a
//! `ReadEvents` page carries them on each `JournalEntry`. Entries appended
//! before the table existed cite nothing.

mod artifact;
mod batch;
mod blobs;
mod cache;
mod clock;
mod closure;
mod draft;
mod journal;
mod reader;
mod store;
mod watch;
mod worker;

pub use artifact::split_artifact;
pub use batch::{Batch, BatchError};
pub use cache::ReadCacheBudget;
pub use clock::{Clock, SystemClock};
pub use closure::Closure;
pub use draft::{Draft, DraftError};
pub use journal::{AppendError, GetError, Journal, JournalError, JournalIdentity, MAX_CLOCK_BEHIND_MILLIS};
pub use reader::JournalReader;
pub use store::{ArtifactBatch, ArtifactStore, BlobFile, VerifiedBlob};

use std::ops::Range;

use aether_actor::runtime;
use aether_bloomery_kinds::{
    AppendRecords, AppendRecordsResult, ClosureArtifact, ClosureLimit, DriverRecord, EncodedArtifact, JournalEntry,
    MoveHead, MoveHeadResult, Publish, PublishResult, ReadArtifact, ReadArtifactResult, ReadArtifacts,
    ReadArtifactsResult, ReadClosure, ReadClosureResult, ReadEvents, ReadEventsResult, ReadHead, ReadHeadResult,
    RecordedHeadMove, Seq, Stage, StageResult, WatchHead, WatchHeadResult,
};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, Pending, TaskDone, TaskQueue};
use aether_substrate::chassis::error::BootError;

use crate::{Digest, JournalActor, MAX_HEAD_WATCHERS, MAX_READ_EVENTS};
use cache::ReadCache;
use watch::Watchers;
use worker::ReadPrefix;

/// Maximum number of closure walks running on worker threads at once. Each
/// walk can hold up to [`ClosureLimit::MAX_BYTES`] of checked-in members
/// resident until its reply is sent, so this bounds that memory; a closure
/// read over the bound waits its turn in arrival order.
const MAX_CLOSURE_READS_IN_FLIGHT: usize = 2;

/// Maximum number of single-artifact reads running on worker threads at
/// once, bounding worker threads and resident artifacts; a read over the
/// bound waits its turn in arrival order. Separate from the closure queue so
/// a small fetch never waits behind closure walks that can each pin
/// [`ClosureLimit::MAX_BYTES`].
const MAX_ARTIFACT_READS_IN_FLIGHT: usize = 4;

/// Maximum number of batched artifact reads running on worker threads at
/// once. Each can hold its byte limit of checked-in members resident until
/// its reply is sent, so this bounds that memory; a batch over the bound
/// waits its turn in arrival order. Separate from the closure and single-read
/// queues so a batch neither waits behind them nor holds them up.
const MAX_BATCH_READS_IN_FLIGHT: usize = 2;

/// Maximum number of stages writing on worker threads at once, bounding
/// worker threads and the fsyncs competing for the root; a stage over the
/// bound waits its turn in arrival order. Separate from the read queues so a
/// large stage never holds up a read.
const MAX_STAGES_IN_FLIGHT: usize = 2;

/// One independently named journal owner over its own journal root.
///
/// The composer opens the [`Journal`] and hands it over as the actor's params
/// (ADR-0156 §3), so the root is held, and a held root refused, before the
/// actor exists. Its config is the [`ReadCacheBudget`] that bounds the one
/// read cache its workers share, holding the members they checked in.
/// Stages write through the [`ArtifactStore`] taken from the journal at
/// `init`, which holds the same root lock.
pub struct JournalActorState {
    journal: Journal,
    store: ArtifactStore,
    watchers: Watchers,
    closures: TaskQueue<ReadClosureResult>,
    artifacts: TaskQueue<ReadArtifactResult>,
    batches: TaskQueue<ReadArtifactsResult>,
    stages: TaskQueue<StageResult>,
    cache: ReadCache,
}

#[runtime]
impl NativeActor for JournalActor {
    type State = JournalActorState;

    type Config = ReadCacheBudget;
    type Params = Journal;

    const NAMESPACE: &'static str = "aether.bloomery.journal";

    fn init(
        budget: ReadCacheBudget,
        journal: Journal,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<JournalActorState, BootError> {
        Ok(JournalActorState {
            store: journal.artifact_store(),
            journal,
            watchers: Watchers::new(),
            closures: TaskQueue::new(MAX_CLOSURE_READS_IN_FLIGHT),
            artifacts: TaskQueue::new(MAX_ARTIFACT_READS_IN_FLIGHT),
            batches: TaskQueue::new(MAX_BATCH_READS_IN_FLIGHT),
            stages: TaskQueue::new(MAX_STAGES_IN_FLIGHT),
            cache: ReadCache::with_budget(budget),
        })
    }

    #[handler::request]
    fn on_read_events(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, request: ReadEvents) -> ReadEventsResult {
        let ReadEvents { after, limit } = request;
        if !(1..=MAX_READ_EVENTS).contains(&limit) {
            return ReadEventsResult::Err { after, message: format!("limit must be between 1 and {MAX_READ_EVENTS}") };
        }

        match state
            .journal
            .read_cited(Seq(after), limit as usize)
            .and_then(|entries| state.journal.head().map(|head| (entries, head)))
        {
            Ok((entries, head)) => ReadEventsResult::Ok {
                after,
                head: head.0,
                entries: entries.into_iter().map(|(entry, cites)| JournalEntry::from_entry(&entry, cites)).collect(),
            },
            Err(error) => ReadEventsResult::Err { after, message: error.to_string() },
        }
    }

    #[handler::request]
    fn on_read_head(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _request: ReadHead) -> ReadHeadResult {
        match state.journal.head() {
            Ok(head) => ReadHeadResult::Ok { head: head.0 },
            Err(error) => ReadHeadResult::Err { message: error.to_string() },
        }
    }

    /// Read one stored artifact. A plain read: writes nothing and wakes no
    /// watcher.
    ///
    /// The read runs on a worker thread through the actor's artifact task
    /// queue (ADR-0093), on its own read-only connection, and checks the
    /// payload into the engine blob store there, read straight into one
    /// buffer of its exact length, so the reply carries it without copying.
    /// Every other request keeps being answered meanwhile, and the reply
    /// lands when the read finishes. The connection opens after every write
    /// this actor committed before the request was handled, so the read sees
    /// them all. A member the read cache still holds is answered from it
    /// without opening a connection.
    #[handler::request]
    fn on_read_artifact(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        request: ReadArtifact,
    ) -> Pending<ReadArtifactResult> {
        let digest = request.digest;
        let reader = state.journal.worker_reader();
        let check_in = ctx.blob_check_in();
        let cache = state.cache.clone();
        state.artifacts.submit(ctx, move || artifact_reply(digest, reader.read_artifact(&digest, &check_in, &cache)))
    }

    /// Completion of an artifact read: the queue answers the request's own
    /// caller, then starts the next queued read in the freed slot.
    #[handler(task)]
    fn on_read_artifact_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<ReadArtifactResult>) {
        state.artifacts.complete(ctx, done);
    }

    /// Read several named artifacts in request order, answering the prefix
    /// whose stored length fits the requested byte limit, and always the
    /// first. A plain read: writes nothing and wakes no watcher.
    ///
    /// The read runs on a worker thread through the actor's batch task queue
    /// (ADR-0093), on one read-only connection, and checks the members the
    /// read cache misses into the engine blob store there as one slab, as a
    /// closure walk does. Every other request keeps being answered meanwhile.
    /// The connection opens after every write this actor committed before the
    /// request was handled, so the read sees them all.
    #[handler::request]
    fn on_read_artifacts(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        request: ReadArtifacts,
    ) -> Pending<ReadArtifactsResult> {
        let ReadArtifacts { digests, limit_bytes } = request;
        let reader = state.journal.worker_reader();
        let check_in = ctx.blob_check_in();
        let cache = state.cache.clone();
        state
            .batches
            .submit(ctx, move || batch_reply(reader.read_artifacts(digests.as_slice(), limit_bytes, &check_in, &cache)))
    }

    /// Completion of a batched read: the queue answers the request's own
    /// caller, then starts the next queued batch in the freed slot.
    #[handler(task)]
    fn on_read_artifacts_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<ReadArtifactsResult>) {
        state.batches.complete(ctx, done);
    }

    /// Read an artifact's transitive closure under the requested byte limit.
    /// A plain read: writes nothing and wakes no watcher.
    ///
    /// The walk runs on a worker thread through the actor's task queue
    /// (ADR-0093), on its own read-only connection, and checks the members
    /// into the engine blob store there as one slab: one allocation for the
    /// whole closure, each member file read straight into its region. Every
    /// other request keeps being answered meanwhile, and the reply lands when
    /// the walk finishes. The connection opens after every write this actor
    /// committed before the request was handled, so the walk sees them all.
    /// Members the read cache still holds are reused without reading their
    /// files, and only the misses go into the slab.
    #[handler::request]
    fn on_read_closure(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        request: ReadClosure,
    ) -> Pending<ReadClosureResult> {
        let ReadClosure { root, limit_bytes } = request;
        let reader = state.journal.worker_reader();
        let check_in = ctx.blob_check_in();
        let cache = state.cache.clone();
        state.closures.submit(ctx, move || {
            closure_reply(root, limit_bytes, reader.read_closure(&root, limit_bytes, &check_in, &cache))
        })
    }

    /// Completion of a closure walk: the queue answers the request's own
    /// caller, then starts the next queued read in the freed slot.
    #[handler(task)]
    fn on_read_closure_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<ReadClosureResult>) {
        state.closures.complete(ctx, done);
    }

    /// Store encoded artifacts content-addressed: no fence, no event, no head
    /// move, and no watcher woken.
    ///
    /// The write runs on a worker thread through the actor's stage task queue
    /// (ADR-0093): it opens an [`ArtifactBatch`], streams each payload into
    /// its blob file a window at a time, and commits every row together, so
    /// the actor's dispatcher never waits on a stage's fsyncs. The reply lands
    /// after the commit, so a read sent once it arrives finds every staged
    /// artifact; a citation naming neither a staged nor a stored artifact
    /// refuses the whole stage.
    #[handler::request]
    fn on_stage(state: &mut Self::State, ctx: &mut NativeCtx<'_>, request: Stage) -> Pending<StageResult> {
        let store = state.store.clone();
        state.stages.submit(ctx, move || stage_reply(stage(&store, request.into_artifacts())))
    }

    /// Completion of a stage: the queue answers the request's own caller,
    /// then starts the next queued stage in the freed slot.
    #[handler(task)]
    fn on_stage_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<StageResult>) {
        state.stages.complete(ctx, done);
    }

    #[handler::request]
    fn on_move_head(state: &mut Self::State, ctx: &mut NativeCtx<'_>, request: MoveHead) -> MoveHeadResult {
        let (head, to, expected_seq) = request.into_parts();
        let mut batch = Batch::new();
        if let Err(error) = batch.push_event(&RecordedHeadMove::new(head, to), None) {
            return MoveHeadResult::Err { message: error.to_string() };
        }

        match state.commit(ctx, Seq(expected_seq), &batch) {
            Ok(range) => MoveHeadResult::Committed { seq: range.start.0 },
            Err(AppendError::HeadMoved { actual }) => MoveHeadResult::Conflict { actual: actual.0 },
            Err(error) => MoveHeadResult::Err { message: error.to_string() },
        }
    }

    #[handler::request]
    fn on_publish(state: &mut Self::State, ctx: &mut NativeCtx<'_>, request: Publish) -> PublishResult {
        let (artifacts, moves, expected_seq) = request.into_parts();
        let mut batch = Batch::new();
        let artifacts = artifacts.into_iter().map(|artifact| batch.stage_artifact(artifact)).collect();
        for moved in &moves {
            if let Err(error) = batch.push_event(moved, None) {
                return PublishResult::Err { message: error.to_string() };
            }
        }

        // `append` returns `head+1 .. head+n+1`, and `head+1 .. head+1` for no events.
        match state.commit(ctx, Seq(expected_seq), &batch) {
            Ok(range) => PublishResult::Committed { head: range.end.0.saturating_sub(1), artifacts },
            Err(AppendError::HeadMoved { actual }) => PublishResult::Conflict { actual: actual.0 },
            Err(error) => PublishResult::Err { message: error.to_string() },
        }
    }

    #[handler::request]
    fn on_append_records(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        request: AppendRecords,
    ) -> AppendRecordsResult {
        let not_before_millis = request.not_before_millis();
        let (artifacts, records, expected_seq) = request.into_parts();

        for record in &records {
            let cause = match record {
                DriverRecord::Requested { cause, .. } => *cause,
                DriverRecord::Transition { cause, .. }
                | DriverRecord::Fault { cause, .. }
                | DriverRecord::Activated { cause, .. }
                | DriverRecord::ActivationRejected { cause, .. }
                | DriverRecord::ReactionFailed { cause, .. }
                | DriverRecord::HeadMoved { cause, .. } => Some(*cause),
            };
            if let Some(cause) = cause
                && !(1..=expected_seq).contains(&cause)
            {
                return AppendRecordsResult::Err {
                    message: format!("cause {cause} is outside the fenced prefix 1..={expected_seq}"),
                };
            }
        }

        let mut batch = Batch::new();
        batch.not_before(not_before_millis);
        let artifacts = artifacts.into_iter().map(|artifact| batch.stage_artifact(artifact)).collect();

        for record in records {
            let result = match record {
                DriverRecord::Requested { cause, record } => batch.push_event(&record, cause.map(Seq)),
                DriverRecord::Transition { cause, record } => batch.push_transition(&record, Seq(cause)),
                DriverRecord::Fault { cause, record } => batch.push_event(&record, Some(Seq(cause))),
                DriverRecord::Activated { cause, record } => batch.push_event(&record, Some(Seq(cause))),
                DriverRecord::ActivationRejected { cause, record } => batch.push_event(&record, Some(Seq(cause))),
                DriverRecord::ReactionFailed { cause, record } => batch.push_event(&record, Some(Seq(cause))),
                DriverRecord::HeadMoved { cause, record } => batch.push_event(&record, Some(Seq(cause))),
            };
            if let Err(error) = result {
                return AppendRecordsResult::Err { message: error.to_string() };
            }
        }

        match state.commit(ctx, Seq(expected_seq), &batch) {
            Ok(range) => AppendRecordsResult::Committed { head: range.end.0.saturating_sub(1), artifacts },
            Err(AppendError::HeadMoved { actual }) => AppendRecordsResult::Conflict { actual: actual.0 },
            Err(error) => AppendRecordsResult::Err { message: error.to_string() },
        }
    }

    /// Long-poll watch on the head (ADR-0226 decision 10): answered at once
    /// when the head is already past `after`, otherwise held until
    /// [`JournalActorState::commit`] wakes it. Every path answers through
    /// the one held ticket, so the row is `WatchHeadResult` (ADR-0243 §2).
    #[handler::request]
    fn on_watch_head(state: &mut Self::State, ctx: &mut NativeCtx<'_>, request: WatchHead) -> Pending<WatchHeadResult> {
        let (pending, held) = ctx.hold::<WatchHeadResult>();

        match state.journal.head() {
            Err(error) => held.answer(ctx, &WatchHeadResult::Err { message: error.to_string() }),
            Ok(current) if current.0 > request.after => {
                held.answer(ctx, &WatchHeadResult::Advanced { head: current.0 });
            }
            Ok(_) => {
                if let Err(held) = state.watchers.park(request.after, held) {
                    held.answer(
                        ctx,
                        &WatchHeadResult::Err { message: format!("watcher table is full (max {MAX_HEAD_WATCHERS})") },
                    );
                }
            }
        }

        pending
    }
}

impl JournalActorState {
    /// The one caller of [`Journal::append`]. Wakes every watcher the
    /// commit's head passed; never reached on a conflict or refusal, so a
    /// write that doesn't commit wakes nobody.
    fn commit<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        expected: Seq,
        batch: &Batch,
    ) -> Result<Range<Seq>, AppendError> {
        let range = self.journal.append(expected, batch)?;
        self.watchers.wake(ctx, range.end.0.saturating_sub(1));
        Ok(range)
    }
}

/// The reply a finished artifact read answers with.
fn artifact_reply(digest: Digest, outcome: Result<Option<ClosureArtifact>, JournalError>) -> ReadArtifactResult {
    match outcome {
        Ok(Some(artifact)) => ReadArtifactResult::Found { artifact },
        Ok(None) => ReadArtifactResult::Missing { digest },
        Err(error) => ReadArtifactResult::Err { digest, message: error.to_string() },
    }
}

/// The reply a finished batched read answers with.
fn batch_reply(outcome: Result<ReadPrefix, JournalError>) -> ReadArtifactsResult {
    match outcome {
        Ok(ReadPrefix::Found(artifacts)) => ReadArtifactsResult::Found { artifacts },
        Ok(ReadPrefix::Missing(digest)) => ReadArtifactsResult::Missing { digest },
        Err(error) => ReadArtifactsResult::Err { message: error.to_string() },
    }
}

/// Stream every artifact into one batch in order, then commit their rows together.
fn stage(store: &ArtifactStore, artifacts: Vec<EncodedArtifact>) -> Result<(), AppendError> {
    let mut staging = store.batch()?;
    for artifact in artifacts {
        let (kind, payload, carried) = artifact.into_parts();
        staging.stage_blob(kind, &payload, batch::citations(carried))?;
    }
    staging.commit()
}

/// The reply a finished stage answers with.
fn stage_reply(outcome: Result<(), AppendError>) -> StageResult {
    match outcome {
        Ok(()) => StageResult::Staged,
        Err(error) => StageResult::Err { message: error.to_string() },
    }
}

/// The reply a finished closure walk answers with.
fn closure_reply(root: Digest, limit_bytes: ClosureLimit, outcome: Result<Closure, JournalError>) -> ReadClosureResult {
    match outcome {
        Ok(Closure::Found(artifacts)) => ReadClosureResult::Found { root, artifacts },
        Ok(Closure::Missing(digest)) => ReadClosureResult::Missing { root, digest },
        Ok(Closure::TooLarge) => ReadClosureResult::TooLarge { root, limit_bytes },
        Err(error) => ReadClosureResult::Err { root, message: error.to_string() },
    }
}
