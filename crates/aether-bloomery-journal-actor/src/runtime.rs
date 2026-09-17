//! Native journal owner and ordinary request/reply handlers.

use std::path::PathBuf;

use aether_actor::actor;
use aether_bloomery_journal::Journal;
use aether_bloomery_kinds::{JournalEntry, ReadEvents, ReadEventsResult, ReadHead, ReadHeadResult, Seq};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

/// Maximum number of entries one read mail can return.
pub const MAX_READ_EVENTS: u32 = 128;

/// One independently named, file-backed journal owner.
pub struct JournalActor;

/// Private state retained by one actor instance.
pub struct JournalActorState {
    journal: Journal,
}

#[actor(instanced, root)]
impl NativeActor for JournalActor {
    type State = JournalActorState;
    type Config = PathBuf;

    const NAMESPACE: &'static str = "aether.bloomery.journal";

    fn init(path: PathBuf, _ctx: &mut NativeInitCtx<'_>) -> Result<JournalActorState, BootError> {
        Ok(JournalActorState { journal: Journal::open(&path).map_err(|error| BootError::Other(Box::new(error)))? })
    }

    #[handler::single]
    fn on_read_events(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, request: ReadEvents) -> ReadEventsResult {
        let ReadEvents { after, limit } = request;
        if !(1..=MAX_READ_EVENTS).contains(&limit) {
            return ReadEventsResult::Err { after, message: format!("limit must be between 1 and {MAX_READ_EVENTS}") };
        }

        match state
            .journal
            .read(Seq(after), limit as usize)
            .and_then(|entries| state.journal.head().map(|head| (entries, head)))
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
    fn on_read_head(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _request: ReadHead) -> ReadHeadResult {
        match state.journal.head() {
            Ok(head) => ReadHeadResult::Ok { head: head.0 },
            Err(error) => ReadHeadResult::Err { message: error.to_string() },
        }
    }
}
