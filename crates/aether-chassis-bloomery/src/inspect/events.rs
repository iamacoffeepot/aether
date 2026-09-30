//! The `Events` job: journal entries after a sequence, filtered by kind name,
//! each with its value decoded at depth 0.
//!
//! The job is sans-io like the `Artifact` job: it reads one journal page at a
//! time and stops when `limit` entries match, the head is reached,
//! [`MAX_SCANNED`] entries have been read, or the next value would pass
//! [`MAX_VALUES`]. `next_after` is the last sequence scanned, so the caller
//! pages on from it.

use std::collections::VecDeque;
use std::mem;

use aether_bloomery_journal::MAX_READ_EVENTS;
use aether_bloomery_kinds::{DeclarationsResult, JournalEntry, ReadEvents, ReadEventsResult};

use super::kinds::{InspectEvents, InspectEventsResult, InspectedEvent, MAX_SCANNED, MAX_VALUES};
use super::resolve::{Resolver, render};

/// What the actor performs next for one job.
pub enum Step {
    /// Read one journal page.
    Read(ReadEvents),
    /// Ask the driver for its program declarations.
    Declarations,
    /// Answer the request; the job is done.
    Answer(InspectEventsResult),
    /// Wait for a reply already asked for.
    Wait,
}

/// One `InspectEvents` in flight.
pub struct EventsJob {
    limit: usize,
    kinds: Vec<String>,
    resolver: Resolver,
    /// Whether the driver's declarations have been asked for.
    asked: bool,
    /// The last sequence scanned.
    cursor: u64,
    /// The head the last page saw.
    head: u64,
    /// The entries read but not yet scanned.
    pending: VecDeque<JournalEntry>,
    /// Whether the last page reached the head.
    at_head: bool,
    scanned: usize,
    values: usize,
    /// Whether the next value would pass [`MAX_VALUES`].
    full: bool,
    events: Vec<InspectedEvent>,
}

impl EventsJob {
    /// Start reading the page after `request.after`.
    pub fn start(request: InspectEvents) -> (Self, Step) {
        let InspectEvents { after, limit, kinds } = request;
        let job = Self {
            limit: limit.min(MAX_READ_EVENTS) as usize,
            kinds,
            resolver: Resolver::default(),
            asked: false,
            cursor: after,
            head: 0,
            pending: VecDeque::new(),
            at_head: false,
            scanned: 0,
            values: 0,
            full: false,
            events: Vec::new(),
        };
        let step = Step::Read(job.page());
        (job, step)
    }

    /// Feed one journal page.
    pub fn on_page(&mut self, result: ReadEventsResult) -> Step {
        match result {
            ReadEventsResult::Ok { head, entries, .. } => {
                self.head = head;
                self.at_head =
                    entries.len() < MAX_READ_EVENTS as usize || entries.last().is_none_or(|entry| entry.seq >= head);
                self.pending.extend(entries);
                self.advance()
            }
            ReadEventsResult::Err { message, .. } => Step::Answer(InspectEventsResult::Err { message }),
        }
    }

    /// Feed the driver's declarations.
    pub fn on_declarations(&mut self, result: DeclarationsResult) -> Step {
        self.resolver.declare(result);
        self.advance()
    }

    /// Scan the entries read, then read the next page or answer.
    fn advance(&mut self) -> Step {
        while !self.done() {
            let Some(entry) = self.pending.pop_front() else {
                break;
            };
            let Some(resolved) = self.resolver.resolve(entry.kind) else {
                self.pending.push_front(entry);
                if self.asked {
                    return Step::Wait;
                }
                self.asked = true;
                return Step::Declarations;
            };
            let kind = resolved.name();
            let kept = self.kinds.is_empty() || kind.as_ref().is_some_and(|kind| self.kinds.contains(kind));
            if !kept {
                self.scanned += 1;
                self.cursor = entry.seq;
                continue;
            }

            let rendered = render(entry.kind, &entry.bytes, &resolved, MAX_VALUES - self.values);
            if rendered.over_budget && !self.events.is_empty() {
                self.full = true;
                break;
            }
            self.values += rendered.values;
            self.scanned += 1;
            self.cursor = entry.seq;
            self.events.push(InspectedEvent {
                seq: entry.seq,
                cause: entry.cause,
                kind_id: entry.kind.0,
                kind,
                recorded_at_millis: entry.recorded_at_millis,
                value: rendered.json.to_string(),
            });
        }

        if self.done() || (self.pending.is_empty() && self.at_head) {
            return Step::Answer(InspectEventsResult::Ok {
                head: self.head,
                next_after: self.cursor,
                events: mem::take(&mut self.events),
            });
        }
        Step::Read(self.page())
    }

    /// Whether the reply is complete short of the head.
    fn done(&self) -> bool {
        self.events.len() >= self.limit || self.scanned >= MAX_SCANNED || self.full || self.values >= MAX_VALUES
    }

    /// The page after the cursor.
    fn page(&self) -> ReadEvents {
        ReadEvents { after: self.cursor, limit: MAX_READ_EVENTS }
    }
}
