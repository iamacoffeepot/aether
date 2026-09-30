//! Firing: the tick that pops due timers, and the paged batch of
//! `Transition`s that records them.

use aether_bloomery_kinds::{AppendRecords, DriverRecord, EncodedArtifact, Fired, Seq, Transition};

use super::Due;
use crate::runtime::core::{AppendTicket, Command, EVENTS_PAGE, PendingWrite, ProgramCore};

impl ProgramCore {
    /// Fire every armed timer due by `now_millis`, the clock the shell read.
    ///
    /// The due timers queue one write of their `Transition`s, and a tick is
    /// asked for again while any timer stays armed. The shell calls this
    /// once per [`Command::ArmTick`] it performed; comparing the clock with
    /// the heap on every tick means a forward jump or a resume from suspend
    /// fires every overdue timer on the next tick. The core never reads a
    /// clock itself.
    pub fn tick(&mut self, now_millis: u64) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let fired = self.timers.pop_due(now_millis);
        if !fired.is_empty() {
            self.journal.queue_back(PendingWrite::Fired { timers: fired });
            self.pump(&mut out);
        }
        if !self.aborted && self.timers.claim_tick() {
            out.push(Command::ArmTick);
        }
        out
    }

    /// Derive one queued fired batch against the synced view and append its
    /// first [`EVENTS_PAGE`] timers.
    ///
    /// A timer whose request already shows an outcome is dropped, and the rest
    /// of a longer batch waits at the front of the queue. Each timer becomes a
    /// `Transition` caused by its request, with its [`Fired`] result staged
    /// once per distinct due time, and the append is floored at the page's
    /// latest due time, so no `Transition` is stamped before its due time.
    pub(crate) fn derive_fired(&mut self, timers: Vec<Due>, out: &mut Vec<Command>) {
        let requests = self.journal.requests();
        let mut page: Vec<Due> = timers
            .into_iter()
            .filter(|due| requests.get(Seq(due.seq)).is_some_and(|request| request.outcome().is_none()))
            .collect();
        if page.is_empty() {
            self.answer_ready(out);
            return;
        }
        let rest = page.split_off(page.len().min(EVENTS_PAGE as usize));
        if !rest.is_empty() {
            self.journal.queue_front(PendingWrite::Fired { timers: rest });
        }

        let mut artifacts: Vec<EncodedArtifact> = Vec::new();
        let mut records = Vec::with_capacity(page.len());
        for due in &page {
            let Some((program, input)) = self.request_data(due.seq) else {
                self.abort(format!("fired clock request {} is missing from the journal fold", due.seq), out);
                return;
            };
            let fired = match EncodedArtifact::new(&Fired { due_millis: due.due_millis }) {
                Ok(fired) => fired,
                Err(error) => {
                    self.abort(format!("cannot encode the fired result of clock request {}: {error}", due.seq), out);
                    return;
                }
            };
            let result = fired.digest();
            if !artifacts.iter().any(|staged| staged.digest() == result) {
                artifacts.push(fired);
            }
            records.push(DriverRecord::Transition { cause: due.seq, record: Transition { program, input, result } });
        }

        let not_before_millis = page.iter().map(|due| due.due_millis).max().unwrap_or_default();
        let ticket = self.mint(AppendTicket::mint);
        let append = AppendRecords::new(artifacts, records, self.journal.cursor()).not_before(not_before_millis);
        self.journal.set_append(ticket, PendingWrite::Fired { timers: page });
        out.push(Command::Append { ticket, request: append });
    }
}
