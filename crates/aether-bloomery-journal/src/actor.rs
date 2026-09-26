//! Native journal owner and ordinary request/reply handlers.

use std::ops::Range;

use crate::cache::{ReadCache, ReadCacheBudget};
use crate::watch::Watchers;
use crate::{AppendError, Batch, Closure, Digest, Journal, JournalError};
use aether_actor::{Manual, actor};
use aether_bloomery_kinds::{
    AppendRecords, AppendRecordsResult, ClosureArtifact, ClosureLimit, DriverRecord, JournalEntry, MoveHead,
    MoveHeadResult, Publish, PublishResult, ReadArtifact, ReadArtifactResult, ReadClosure, ReadClosureResult,
    ReadEvents, ReadEventsResult, ReadHead, ReadHeadResult, RecordedHeadMove, Seq, WatchHead, WatchHeadResult,
};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, Pending, TaskDone, TaskQueue};
use aether_substrate::chassis::error::BootError;

/// Maximum number of entries one read mail can return.
pub const MAX_READ_EVENTS: u32 = 128;

/// Maximum number of parked `WatchHead` replies. Journal writes and watches
/// are unauthenticated (ADR-0226 Consequences), and the one expected watcher
/// is the ADR-0226 driver, so this bounds an otherwise easy leak rather than
/// anticipating real concurrent demand.
pub const MAX_HEAD_WATCHERS: usize = 64;

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

/// One independently named journal owner over its own journal root.
///
/// The composer opens the [`Journal`] and hands it over as the actor's params
/// (ADR-0156 §3), so the root is held, and a held root refused, before the
/// actor exists. Its config is the [`ReadCacheBudget`] that bounds the one
/// read cache its workers share, holding the members they checked in.
pub struct JournalActor {
    journal: Journal,
    watchers: Watchers,
    closures: TaskQueue,
    artifacts: TaskQueue,
    cache: ReadCache,
}

#[actor(instanced, root)]
impl NativeActor for JournalActor {
    type Config = ReadCacheBudget;
    type Params = Journal;

    const NAMESPACE: &'static str = "aether.bloomery.journal";

    fn init(budget: ReadCacheBudget, journal: Journal, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            journal,
            watchers: Watchers::new(),
            closures: TaskQueue::new(MAX_CLOSURE_READS_IN_FLIGHT),
            artifacts: TaskQueue::new(MAX_ARTIFACT_READS_IN_FLIGHT),
            cache: ReadCache::with_budget(budget),
        })
    }

    #[handler::single]
    fn on_read_events(&self, _ctx: &mut NativeCtx<'_>, request: ReadEvents) -> ReadEventsResult {
        let ReadEvents { after, limit } = request;
        if !(1..=MAX_READ_EVENTS).contains(&limit) {
            return ReadEventsResult::Err { after, message: format!("limit must be between 1 and {MAX_READ_EVENTS}") };
        }

        match self
            .journal
            .read(Seq(after), limit as usize)
            .and_then(|entries| self.journal.head().map(|head| (entries, head)))
        {
            Ok((entries, head)) => ReadEventsResult::Ok {
                after,
                head: head.0,
                entries: entries.iter().map(JournalEntry::from_entry).collect(),
            },
            Err(error) => ReadEventsResult::Err { after, message: error.to_string() },
        }
    }

    #[handler::single]
    fn on_read_head(&self, _ctx: &mut NativeCtx<'_>, _request: ReadHead) -> ReadHeadResult {
        match self.journal.head() {
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
    #[handler::single]
    fn on_read_artifact(&mut self, ctx: &mut NativeCtx<'_>, request: ReadArtifact) -> Pending<ReadArtifactResult> {
        let digest = request.digest;
        let reader = self.journal.worker_reader();
        let check_in = ctx.blob_check_in();
        let cache = self.cache.clone();
        self.artifacts.submit(ctx, move || artifact_reply(digest, reader.read_artifact(&digest, &check_in, &cache)))
    }

    /// ADR-0093 completion of an artifact read: reply to the request's own
    /// caller, then free the read's slot for the next queued read.
    #[handler(task)]
    fn on_read_artifact_done(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<ReadArtifactResult>) {
        done.resolve(ctx);
        self.artifacts.on_complete(ctx);
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
    #[handler::single]
    fn on_read_closure(&mut self, ctx: &mut NativeCtx<'_>, request: ReadClosure) -> Pending<ReadClosureResult> {
        let ReadClosure { root, limit_bytes } = request;
        let reader = self.journal.worker_reader();
        let check_in = ctx.blob_check_in();
        let cache = self.cache.clone();
        self.closures.submit(ctx, move || {
            closure_reply(root, limit_bytes, reader.read_closure(&root, limit_bytes, &check_in, &cache))
        })
    }

    /// ADR-0093 completion of a closure walk: reply to the request's own
    /// caller, then free the walk's slot for the next queued read.
    #[handler(task)]
    fn on_read_closure_done(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<ReadClosureResult>) {
        done.resolve(ctx);
        self.closures.on_complete(ctx);
    }

    #[handler::single]
    fn on_move_head(&mut self, ctx: &mut NativeCtx<'_>, request: MoveHead) -> MoveHeadResult {
        let (head, to, expected_seq) = request.into_parts();
        let mut batch = Batch::new();
        if let Err(error) = batch.push_event(&RecordedHeadMove::new(head, to), None) {
            return MoveHeadResult::Err { message: error.to_string() };
        }

        match self.commit(ctx, Seq(expected_seq), &batch) {
            Ok(range) => MoveHeadResult::Committed { seq: range.start.0 },
            Err(AppendError::HeadMoved { actual }) => MoveHeadResult::Conflict { actual: actual.0 },
            Err(error) => MoveHeadResult::Err { message: error.to_string() },
        }
    }

    #[handler::single]
    fn on_publish(&mut self, ctx: &mut NativeCtx<'_>, request: Publish) -> PublishResult {
        let (artifacts, moves, expected_seq) = request.into_parts();
        let mut batch = Batch::new();
        let artifacts = artifacts.into_iter().map(|artifact| batch.stage_artifact(artifact)).collect();
        for moved in &moves {
            if let Err(error) = batch.push_event(moved, None) {
                return PublishResult::Err { message: error.to_string() };
            }
        }

        // `append` returns `head+1 .. head+n+1`, and `head+1 .. head+1` for no events.
        match self.commit(ctx, Seq(expected_seq), &batch) {
            Ok(range) => PublishResult::Committed { head: range.end.0.saturating_sub(1), artifacts },
            Err(AppendError::HeadMoved { actual }) => PublishResult::Conflict { actual: actual.0 },
            Err(error) => PublishResult::Err { message: error.to_string() },
        }
    }

    #[handler::single]
    fn on_append_records(&mut self, ctx: &mut NativeCtx<'_>, request: AppendRecords) -> AppendRecordsResult {
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
        let artifacts = artifacts.into_iter().map(|artifact| batch.stage_artifact(artifact)).collect();

        for record in records {
            let result = match record {
                DriverRecord::Requested { cause, record } => batch.push_event(&record, cause.map(Seq)),
                DriverRecord::Transition { cause, record } => {
                    batch.require_artifact(record.input);
                    batch.require_artifact(record.result);
                    batch.push_event(&record, Some(Seq(cause)))
                }
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

        match self.commit(ctx, Seq(expected_seq), &batch) {
            Ok(range) => AppendRecordsResult::Committed { head: range.end.0.saturating_sub(1), artifacts },
            Err(AppendError::HeadMoved { actual }) => AppendRecordsResult::Conflict { actual: actual.0 },
            Err(error) => AppendRecordsResult::Err { message: error.to_string() },
        }
    }

    /// Long-poll watch on the head (ADR-0226 decision 10): answered at once
    /// when the head is already past `after`, otherwise parked until
    /// [`Self::commit`] wakes it.
    #[handler::manual]
    fn on_watch_head(&mut self, ctx: &mut NativeCtx<'_, aether_substrate::Erased, Manual>, request: WatchHead) {
        let owed = ctx.defer_reply_to(ctx.reply_target());

        let head = match self.journal.head() {
            Ok(head) => head,
            Err(error) => {
                owed.reply(ctx, &WatchHeadResult::Err { message: error.to_string() });
                return;
            }
        };

        if head.0 > request.after {
            owed.reply(ctx, &WatchHeadResult::Advanced { head: head.0 });
            return;
        }

        if let Err(owed) = self.watchers.park(request.after, owed) {
            owed.reply(
                ctx,
                &WatchHeadResult::Err { message: format!("watcher table is full (max {MAX_HEAD_WATCHERS})") },
            );
        }
    }
}

impl JournalActor {
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
        Err(JournalError::ArtifactDigestMismatch(_)) => ReadArtifactResult::Err {
            digest,
            message: "stored artifact bytes do not match the requested digest".into(),
        },
        Ok(None) => ReadArtifactResult::Missing { digest },
        Err(error) => ReadArtifactResult::Err { digest, message: error.to_string() },
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
