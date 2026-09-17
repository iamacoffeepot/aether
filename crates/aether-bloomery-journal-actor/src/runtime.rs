//! Native journal owner and ordinary request/reply handlers.

use std::path::PathBuf;

use aether_actor::actor;
use aether_bloomery_journal::{AppendError, Batch, Journal};
use aether_bloomery_kinds::{
    JournalEntry, ReactorLifecycleOutcome, ReadEvents, ReadEventsResult, ReadHead, ReadHeadResult,
    RecordReactorLifecycle, RecordReactorLifecycleResult, Seq,
};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

/// Maximum number of entries one read mail can return.
pub const MAX_READ_EVENTS: u32 = 128;

/// One independently named, file-backed journal owner.
pub struct JournalActor {
    journal: Journal,
}

#[actor(instanced, root)]
impl NativeActor for JournalActor {
    type Config = PathBuf;

    const NAMESPACE: &'static str = "aether.bloomery.journal";

    fn init(path: PathBuf, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { journal: Journal::open(&path).map_err(|error| BootError::Other(Box::new(error)))? })
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

    #[handler::single]
    fn on_record_reactor_lifecycle(
        &mut self,
        _ctx: &mut NativeCtx<'_>,
        request: RecordReactorLifecycle,
    ) -> RecordReactorLifecycleResult {
        let RecordReactorLifecycle { expect_head, cause, event, artifact_bytes } = request;
        let cited_artifact = match &event {
            ReactorLifecycleOutcome::Activated { artifact, .. } => Some(*artifact),
            ReactorLifecycleOutcome::Rejected { attempted, .. } => Some(*attempted),
            ReactorLifecycleOutcome::Retired { .. } => None,
        };

        let mut batch = Batch::new();
        if let Some(bytes) = artifact_bytes {
            let Some(expected) = cited_artifact else {
                return RecordReactorLifecycleResult::Err { message: "retirement cannot stage artifact bytes".into() };
            };
            if batch.stage_bytes(&bytes) != expected {
                return RecordReactorLifecycleResult::Err {
                    message: "staged artifact bytes do not match the lifecycle citation".into(),
                };
            }
        }

        if let Err(error) = batch.push_event(&event, cause.map(Seq)) {
            return RecordReactorLifecycleResult::Err { message: error.to_string() };
        }

        match self.journal.append(Seq(expect_head), &batch) {
            Ok(range) => RecordReactorLifecycleResult::Committed { seq: range.start.0 },
            Err(AppendError::HeadMoved { actual }) => RecordReactorLifecycleResult::HeadMoved { actual: actual.0 },
            Err(error) => RecordReactorLifecycleResult::Err { message: error.to_string() },
        }
    }
}
