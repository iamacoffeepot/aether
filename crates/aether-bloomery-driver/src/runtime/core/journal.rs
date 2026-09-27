//! Journal view: the folds, their shared cursor, and the fenced write queue.
//!
//! The core learns the journal only from [`ReadEvents`](aether_bloomery_kinds::ReadEvents)
//! pages. Each entry folds into [`Heads`], [`HeadHistory`], [`Requests`], and
//! [`Activations`]; pages continue until the cursor reaches the page's `head`.
//! The history answers the `Heads` of any seq up to the cursor, so routing
//! never rereads the journal to rebuild them. At most one
//! [`AppendRecords`](aether_bloomery_kinds::AppendRecords) is in flight,
//! fenced at the cursor, and writing decisions come from a FIFO of pending
//! writes, made only when the core is caught up with no write in flight. A
//! fold error aborts: it reports a history the driver could not have
//! produced, so the view cannot be trusted.

use std::collections::VecDeque;

use aether_bloomery_kinds::{Call, Digest, DriverRecord, EncodedArtifact, JournalEntry, ReactorName, SetHead};
use aether_bloomery_view::{Activations, HeadHistory, Heads, Requests};

use super::ticket::{AppendTicket, CallerId, EventsTicket};

/// Entries per journal page. Matches the journal owner's `MAX_READ_EVENTS`.
pub const EVENTS_PAGE: u32 = 128;

/// One planned routing record: ready to append, or a `SetHead` awaiting its
/// compare-and-swap at derivation.
#[derive(Debug, Clone)]
pub enum PlannedRecord {
    /// A record decided during routing, appended unchanged.
    Ready(DriverRecord),
    /// A `SetHead` intent whose destination is stored under the head's kind.
    /// Derivation checks the compare-and-swap against the journal view's
    /// `Heads` plus earlier moves in the same batch; a pass becomes
    /// `HeadMoved`, a failure becomes `ReactionFailed` for this intent alone.
    SetHead {
        /// Trigger or catch-up seq causing the move or its failure.
        cause: u64,
        /// Bundle whose rule returned the intent.
        bundle: Digest,
        /// Reactor that returned the intent.
        reactor: ReactorName,
        /// The move to attempt.
        set_head: SetHead,
    },
}

/// One write waiting for, or holding, the fenced append slot.
#[derive(Debug)]
pub enum PendingWrite {
    /// A `Call`'s `Requested`. Re-derived from the refolded state on conflict:
    /// the dedup lookup and head resolution run again.
    RequestedCall {
        /// Callers waiting on this request, in arrival order.
        callers: Vec<CallerId>,
        /// The call to record.
        call: Call,
    },
    /// One request's outcome. Re-appended unchanged on conflict unless the
    /// refolded state already shows an outcome, in which case it is dropped.
    Outcome {
        /// The `Requested` seq this outcome answers.
        request: u64,
        /// Staged artifacts carried with the record, if any.
        artifacts: Vec<EncodedArtifact>,
        /// The caused record.
        record: DriverRecord,
    },
    /// One startup batch. Recomputed from the outstanding set on conflict.
    Startup,
    /// One routing batch. Re-derived from the carried plan on conflict, so
    /// each `SetHead` swap is checked again against the refolded view.
    Routing {
        /// Trigger seq whose records this batch carries.
        trigger: u64,
        /// Records in append order.
        plan: Vec<PlannedRecord>,
    },
}

/// How a new call relates to an already-queued `Requested`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedClaim {
    /// Same name and input as the queued write; the caller was attached to it.
    Attached,
    /// Same `(origin, key)` but a different request: a caller bug.
    Reused,
    /// No queued write carries this call's key.
    Absent,
}

/// The folds over the journal prefix `1..=cursor`, plus the write machinery.
#[derive(Debug)]
pub struct Journal {
    heads: Heads,
    history: HeadHistory,
    requests: Requests,
    activations: Activations,
    cursor: u64,
    target: Option<u64>,
    read_ticket: Option<EventsTicket>,
    append_ticket: Option<AppendTicket>,
    in_flight: Option<PendingWrite>,
    pending: VecDeque<PendingWrite>,
}

impl Journal {
    pub fn new() -> Self {
        Self {
            heads: Heads::new(),
            history: HeadHistory::new(),
            requests: Requests::new(),
            activations: Activations::new(),
            cursor: 0,
            target: None,
            read_ticket: None,
            append_ticket: None,
            in_flight: None,
            pending: VecDeque::new(),
        }
    }

    pub fn heads(&self) -> &Heads {
        &self.heads
    }

    /// Every head move through the cursor, answering `Heads` at any earlier seq.
    pub fn history(&self) -> &HeadHistory {
        &self.history
    }

    pub fn requests(&self) -> &Requests {
        &self.requests
    }

    pub fn activations(&self) -> &Activations {
        &self.activations
    }

    /// Last folded sequence; the fence for the next write.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// No read is outstanding and the cursor has reached the observed head.
    pub fn synced(&self) -> bool {
        self.read_ticket.is_none() && self.target == Some(self.cursor)
    }

    pub fn read_ticket(&self) -> Option<EventsTicket> {
        self.read_ticket
    }

    pub fn append_ticket(&self) -> Option<AppendTicket> {
        self.append_ticket
    }

    pub fn set_read_ticket(&mut self, ticket: Option<EventsTicket>) {
        self.read_ticket = ticket;
    }

    pub fn set_target(&mut self, head: u64) {
        self.target = Some(head);
    }

    /// Fold one page in stored order. On success the cursor advances past the
    /// page; on a fold error the returned string is the abort reason. (The
    /// core dies on a fold error, so the folds need no cross-pair atomicity.)
    pub fn apply_page(&mut self, entries: &[JournalEntry]) -> Result<(), String> {
        for entry in entries {
            let folded = entry.to_entry();
            self.heads
                .apply(&folded)
                .map_err(|error| format!("heads fold rejected journal entry {}: {error}", entry.seq))?;
            self.history
                .apply(&folded)
                .map_err(|error| format!("head history fold rejected journal entry {}: {error}", entry.seq))?;
            self.requests
                .apply(&folded)
                .map_err(|error| format!("requests fold rejected journal entry {}: {error}", entry.seq))?;
            self.activations
                .apply(&folded)
                .map_err(|error| format!("activations fold rejected journal entry {}: {error}", entry.seq))?;
            self.cursor = entry.seq;
        }
        Ok(())
    }

    pub fn queue_back(&mut self, write: PendingWrite) {
        self.pending.push_back(write);
    }

    pub fn queue_front(&mut self, write: PendingWrite) {
        self.pending.push_front(write);
    }

    pub fn has_startup_queued(&self) -> bool {
        self.pending.iter().any(|write| matches!(write, PendingWrite::Startup))
    }

    /// Whether a routing write is queued or in flight.
    pub fn has_routing_write(&self) -> bool {
        matches!(self.in_flight, Some(PendingWrite::Routing { .. }))
            || self.pending.iter().any(|write| matches!(write, PendingWrite::Routing { .. }))
    }

    /// Pop the next write when one may be decided: caught up, none in flight.
    pub fn next_write(&mut self) -> Option<PendingWrite> {
        if !self.synced() || self.append_ticket.is_some() {
            return None;
        }
        self.pending.pop_front()
    }

    pub fn set_append(&mut self, ticket: AppendTicket, write: PendingWrite) {
        self.append_ticket = Some(ticket);
        self.in_flight = Some(write);
    }

    /// Clear the append slot, returning the write that held it, if any.
    pub fn clear_append(&mut self) -> Option<PendingWrite> {
        self.append_ticket = None;
        self.in_flight.take()
    }

    /// Relate `call` to the queued or in-flight `Requested` writes.
    ///
    /// When the call repeats a queued write's `(origin, key)` with the same
    /// name and input, the caller joins that write and waits on the one
    /// request; the fold would reject a duplicate `Requested`, so no second
    /// write is queued.
    pub fn claim_requested(&mut self, call: &Call, caller: CallerId) -> RequestedClaim {
        let queued = self.in_flight.iter_mut().chain(self.pending.iter_mut()).find_map(|write| {
            if let PendingWrite::RequestedCall { callers, call: queued } = write {
                (queued.origin == call.origin && queued.key == call.key).then_some((queued, callers))
            } else {
                None
            }
        });
        match queued {
            None => RequestedClaim::Absent,
            Some((queued, callers)) if queued.name == call.name && queued.input == call.input => {
                callers.push(caller);
                RequestedClaim::Attached
            }
            Some(_) => RequestedClaim::Reused,
        }
    }
}
