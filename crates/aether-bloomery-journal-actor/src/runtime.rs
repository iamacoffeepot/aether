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
}
