//! Explicit generated-coordinator feed state and live receipt bookkeeping.

use alloc::string::String;

use aether_bloomery_kinds::JournalEntry;
use aether_data::{MailboxId, mailbox_id_from_path};

use crate::{BeginWarmup, LiveQueue};

/// Maximum historical page requested by a generated views owner.
pub const WARMUP_PAGE_LIMIT: u32 = 32;

/// One outstanding, correlated historical journal page.
pub struct PendingRead {
    /// Host-minted request correlation.
    pub correlation: u64,
    /// Expected journal actor mailbox.
    pub source: MailboxId,
    /// Exclusive read boundary.
    pub after: u64,
    /// Exact requested entry count.
    pub limit: u32,
}

impl PendingRead {
    /// Remember the exact journal actor and host-minted request identity.
    #[must_use]
    pub fn new(correlation: u64, address: &str, after: u64, limit: u32) -> Self {
        Self { correlation, source: mailbox_id_from_path(address), after, limit }
    }
}

/// Historical read phase before live preparation starts.
pub struct Warming {
    /// Bound stream.
    pub stream: String,
    /// Runtime journal address supplied by the owner.
    pub journal_address: String,
    /// Inclusive fold-only boundary.
    pub historical_through: u64,
    /// Last selected live sequence received independently of fold progress.
    pub received_live_through: u64,
    /// `Some` while a page request is outstanding; `None` before the first
    /// request and after a correlated reply is taken for folding.
    pub pending: Option<PendingRead>,
}

/// Intake state of a generated views coordinator.
pub enum FeedMode {
    /// Legacy immediate `Event` and fold-only `EventBatch` handling.
    Direct,
    /// Historical pages are being folded; selected live inputs are buffered.
    Warming(Warming),
    /// Historical boundary reached; live input is prepared through the FIFO.
    Feeding { stream: String, received_live_through: u64 },
    /// A managed historical read or fold failed.
    Failed,
}

/// State and queue owned by one generated coordinator instance.
pub struct ManagedFeed {
    /// Explicit intake mode.
    pub mode: FeedMode,
    /// Lossless selected-live FIFO.
    pub queue: LiveQueue,
}

impl ManagedFeed {
    /// New coordinator starts in the legacy direct mode.
    #[must_use]
    pub fn new() -> Self {
        Self { mode: FeedMode::Direct, queue: LiveQueue::new() }
    }

    /// Begin an explicitly bounded historical warmup.
    ///
    /// # Errors
    ///
    /// Returns a static reason when the feed is already managed or invalid.
    pub fn begin(
        &mut self,
        stream: String,
        journal_address: String,
        historical_through: u64,
    ) -> Result<(), &'static str> {
        if !matches!(self.mode, FeedMode::Direct) {
            return Err("warmup already started");
        }
        let begin = BeginWarmup::new(stream, journal_address, historical_through);
        begin.validate()?;
        let BeginWarmup { stream, journal_address, historical_through } = begin;
        self.mode = if historical_through == 0 {
            FeedMode::Feeding { stream, received_live_through: 0 }
        } else {
            FeedMode::Warming(Warming {
                stream,
                journal_address,
                historical_through,
                received_live_through: historical_through,
                pending: None,
            })
        };
        Ok(())
    }

    /// Admit a selected live envelope at exactly the next received sequence.
    /// The caller fatally aborts on every error from this method.
    ///
    /// # Errors
    ///
    /// Returns a reason for wrong stream or noncontiguous receipt.
    pub fn push_live(&mut self, stream: &str, entry: JournalEntry) -> Result<(), &'static str> {
        let (bound, received) = match &mut self.mode {
            FeedMode::Warming(state) => (&state.stream, &mut state.received_live_through),
            FeedMode::Feeding { stream, received_live_through } => (stream, received_live_through),
            _ => return Err("live input outside managed feed"),
        };
        if stream != bound {
            return Err("live stream mismatch");
        }
        if entry.seq != received.checked_add(1).ok_or("live sequence overflow")? {
            return Err("live sequence gap, duplicate, or regression");
        }
        *received = entry.seq;
        self.queue.push(entry);
        Ok(())
    }

    /// Switch from historical folding to queued live preparation at H.
    pub fn finish_history(&mut self) {
        if let FeedMode::Warming(state) = &self.mode {
            self.mode =
                FeedMode::Feeding { stream: state.stream.clone(), received_live_through: state.received_live_through };
        }
    }

    /// Bound stream for a managed feed, when one exists.
    #[must_use]
    pub fn stream(&self) -> Option<&str> {
        match &self.mode {
            FeedMode::Warming(state) => Some(&state.stream),
            FeedMode::Feeding { stream, .. } => Some(stream),
            _ => None,
        }
    }
}

impl Default for ManagedFeed {
    fn default() -> Self {
        Self::new()
    }
}
