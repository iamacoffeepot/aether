//! Journal following: routing page reads, the single `WatchHead`, and `AwaitProcessed` (ADR-0226 decisions 10 and 11).

use std::mem::take;

use aether_bloomery_kinds::{
    AwaitProcessed, JournalEntry, Processed, ReadEvents, ReadEventsResult, WatchHead, WatchHeadResult,
};

use crate::core::{ArtifactRead, CallerId, Command, EVENTS_PAGE, EventsTicket, ProgramCore, WatchTicket};
use crate::reactors::{Delivery, PendingRead, RoutingRead, SeqPhase, SeqWork};

impl ProgramCore {
    /// Park one barrier waiter and answer it when quiescent through its bound.
    pub fn await_processed(&mut self, request: AwaitProcessed) -> (CallerId, Vec<Command>) {
        let caller = self.mint(CallerId::mint);
        let mut out = Vec::new();
        if self.aborted {
            return (caller, out);
        }
        self.routing.awaiters.push((caller, request.through));
        self.check_processed(&mut out);
        (caller, out)
    }

    /// Feed one watch result. Unknown tickets return no commands.
    pub fn on_watched(&mut self, ticket: WatchTicket, result: WatchHeadResult) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted || self.routing.watch.take_if(|watch| *watch == ticket).is_none() {
            return out;
        }
        match result {
            WatchHeadResult::Err { message } => {
                self.abort(format!("journal watch failed: {message}"), &mut out);
            }
            WatchHeadResult::Advanced { head } => {
                if head > self.journal.cursor() {
                    self.journal.set_target(head);
                    if self.journal.read_ticket().is_none() {
                        self.emit_read(&mut out);
                    }
                }
                self.drive_routing(&mut out);
            }
        }
        out
    }

    /// Continue one routing page read, trimmed to the journal-view cursor.
    pub(crate) fn continue_routing_events(
        &mut self,
        read: &PendingRead,
        result: ReadEventsResult,
        out: &mut Vec<Command>,
    ) {
        let entries = match result {
            ReadEventsResult::Err { message, .. } => {
                self.abort(format!("routing journal read failed: {message}"), out);
                return;
            }
            ReadEventsResult::Ok { after, .. } if after != read.after => {
                self.abort(format!("routing page starts after {after} but the read asked after {}", read.after), out);
                return;
            }
            ReadEventsResult::Ok { entries, .. } => entries,
        };
        let fence = self.journal.cursor();
        let entries: Vec<JournalEntry> = entries.into_iter().filter(|entry| entry.seq <= fence).collect();
        match read.purpose {
            RoutingRead::Steady => self.routing.page = entries.into(),
            RoutingRead::Warm => self.continue_warm_page(entries, out),
            RoutingRead::CatchUp => self.continue_catch_up_page(entries, out),
            RoutingRead::RestartWarm => self.continue_restart_warm_page(entries, out),
        }
        self.drive_routing(out);
    }

    /// Drive routing one step at a time while nothing it waits on is
    /// outstanding, then answer every barrier the progress satisfied.
    ///
    /// Each step either changes routing state or issues the reply routing
    /// then waits on, so the loop ends.
    pub(crate) fn drive_routing(&mut self, out: &mut Vec<Command>) {
        while !self.aborted && self.recovered && self.journal.synced() && self.routing_quiet() {
            if self.routing.restart.is_some() {
                self.drive_restart(out);
            } else if !self.routing.started {
                self.begin_restart();
            } else if self.routing.current.is_some() {
                self.drive_current(out);
            } else if self.routing.cursor() < self.journal.cursor() {
                self.start_next(out);
            } else {
                self.ensure_watch(out);
                break;
            }
        }
        self.check_processed(out);
    }

    /// Whether no routing reply, read, write, or awaited load is outstanding.
    ///
    /// A shared read or load holds routing only while the activation or
    /// restart warm waits on that same digest; one issued for the program
    /// role alone never blocks routing.
    fn routing_quiet(&self) -> bool {
        let routing = &self.routing;
        // A bundle root's fetch read is left out on purpose: routing never
        // waits on a program's fetch-on-miss.
        let reading = self
            .artifact_reads
            .values()
            .any(|read| matches!(read, ArtifactRead::ReactorSet(_) | ArtifactRead::SetHeadDestination));
        routing.read.is_none()
            && routing.deliveries.is_empty()
            && routing.warms.is_empty()
            && routing.statuses.is_empty()
            && routing.committed.is_none()
            && !self.journal.has_routing_write()
            && !reading
            && !routing.awaited().is_some_and(|digest| self.bundles.pending(&digest))
    }

    /// Take one step on the in-progress seq.
    fn drive_current(&mut self, out: &mut Vec<Command>) {
        let Some(phase) = self.routing.current.as_ref().map(|work| work.phase) else {
            return;
        };
        match phase {
            SeqPhase::Evaluating => self.begin_activations(out),
            SeqPhase::Activating => self.drive_activations(out),
            SeqPhase::Planning => self.drive_planning(out),
        }
    }

    /// Start routing `N = R + 1`: deliver `N` to every live digest, then fold it into routing `Heads`.
    fn start_next(&mut self, out: &mut Vec<Command>) {
        if !self.ensure_set_cached(out) {
            return;
        }
        let next = self.routing.cursor() + 1;
        let Some(entry) = self.routing.page.pop_front() else {
            self.emit_routing_read(self.routing.cursor(), RoutingRead::Steady, out);
            return;
        };
        if entry.seq != next {
            self.abort(format!("routing page holds {} where {next} is next", entry.seq), out);
            return;
        }
        let prev = self.selection();
        let live = self.live_digests(&prev);
        for digest in live.keys() {
            let cursor = self.routing.instances.get(digest).map(|instance| instance.cursor);
            if cursor.map(|cursor| cursor + 1) != Some(next) {
                self.abort(format!("live instance {digest} is at {cursor:?}, not ready for seq {next}"), out);
                return;
            }
            self.deliver(*digest, Delivery::Live { digest: *digest }, entry.clone(), out);
        }
        if let Err(error) = self.routing.heads.apply(&entry.to_entry()) {
            self.abort(format!("routing heads rejected entry {next}: {error}"), out);
            return;
        }
        self.routing.current = Some(SeqWork::new(next, entry, prev, live));
    }

    /// Answer every barrier waiter whose bound is quiescent.
    ///
    /// Routing `Heads` fold `N` when `N` starts, so the seq in progress is
    /// not yet routed; a batch counts once it is appended and read back.
    pub(crate) fn check_processed(&mut self, out: &mut Vec<Command>) {
        if self.aborted || !self.routing.started || self.routing.committed.is_some() || self.journal.has_routing_write()
        {
            return;
        }
        let routed = self.routing.current.as_ref().map_or_else(|| self.routing.cursor(), |work| work.n - 1);
        let head = self.journal.cursor();
        let (ready, waiting): (Vec<_>, Vec<_>) =
            take(&mut self.routing.awaiters).into_iter().partition(|&(_, through)| {
                routed >= through && !self.journal.requests().outstanding().any(|request| request.seq().0 <= through)
            });
        self.routing.awaiters = waiting;
        for (caller, _) in ready {
            out.push(Command::Processed { caller, reply: Processed { head } });
        }
    }

    /// Issue one routing page read after `after` for `purpose`.
    pub(crate) fn emit_routing_read(&mut self, after: u64, purpose: RoutingRead, out: &mut Vec<Command>) {
        let ticket = self.mint(EventsTicket::mint);
        self.routing.read = Some(PendingRead { ticket, after, purpose });
        out.push(Command::ReadEvents { ticket, request: ReadEvents { after, limit: EVENTS_PAGE } });
    }

    /// Hold exactly one watch while routing is idle at the journal head.
    fn ensure_watch(&mut self, out: &mut Vec<Command>) {
        if self.routing.watch.is_some() {
            return;
        }
        let ticket = self.mint(WatchTicket::mint);
        self.routing.watch = Some(ticket);
        out.push(Command::WatchHead { ticket, request: WatchHead { after: self.journal.cursor() } });
    }
}
