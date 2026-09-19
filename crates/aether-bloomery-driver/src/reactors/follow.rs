//! Journal following: routing page reads, the single `WatchHead`, and `AwaitProcessed` (ADR-0226 decisions 10 and 11).

use std::collections::BTreeMap;
use std::mem::take;

use aether_bloomery_kinds::{
    AwaitProcessed, Digest, Head, JournalEntry, OpaqueBytes, Processed, ReadEvents, ReadEventsResult, WatchHead,
    WatchHeadResult,
};

use crate::bundles::InstanceState;
use crate::core::{ArtifactRead, CallerId, Command, EVENTS_PAGE, EventsTicket, ProgramCore, WatchTicket};
use crate::reactors::{RoutingRead, SeqPhase, SeqWork};

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
        if self.aborted {
            return out;
        }
        if self.routing.watch != Some(ticket) {
            return out;
        }
        self.routing.watch = None;
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
        ticket: EventsTicket,
        result: ReadEventsResult,
        out: &mut Vec<Command>,
    ) {
        let Some(purpose) = self.routing.read_purpose.take() else {
            self.abort(format!("routing page arrived with no purpose for ticket {ticket:?}"), out);
            return;
        };
        let expected_after = match &purpose {
            RoutingRead::Steady { after }
            | RoutingRead::Warm { after, .. }
            | RoutingRead::CatchUp { after, .. }
            | RoutingRead::RestartFold { after, .. }
            | RoutingRead::RestartWarm { after, .. } => *after,
        };
        match result {
            ReadEventsResult::Err { message, .. } => {
                self.abort(format!("routing journal read failed: {message}"), out);
            }
            ReadEventsResult::Ok { after, head, entries } => {
                if after != expected_after {
                    self.abort(format!("routing page starts after {after} but expected {expected_after}"), out);
                    return;
                }
                if head < self.journal.cursor() {
                    self.abort(format!("journal head {head} moved behind cursor {}", self.journal.cursor()), out);
                    return;
                }
                let fence = self.journal.cursor();
                let trimmed: Vec<JournalEntry> = entries.into_iter().filter(|entry| entry.seq <= fence).collect();
                match purpose {
                    RoutingRead::Steady { .. } => {
                        self.routing.page = trimmed;
                        self.drive_routing(out);
                    }
                    RoutingRead::Warm { digest, head, trigger, live_from, .. } => {
                        self.continue_warm_page(digest, head, trigger, live_from, trimmed, out);
                    }
                    RoutingRead::CatchUp { digest, head, trigger, live_from, .. } => {
                        self.continue_catchup_page(digest, &head, trigger, live_from, trimmed, out);
                    }
                    RoutingRead::RestartFold { watermark, .. } => {
                        self.continue_restart_fold(watermark, trimmed, out);
                    }
                    RoutingRead::RestartWarm { digest, watermark, .. } => {
                        self.continue_restart_warm_page(digest, watermark, trimmed, out);
                    }
                }
            }
        }
    }

    /// Drive routing while work is ready, iteratively.
    pub(crate) fn drive_routing(&mut self, out: &mut Vec<Command>) {
        loop {
            if self.aborted {
                return;
            }
            if !self.recovered || !self.journal.synced() {
                return;
            }
            if !self.routing_quiet() {
                return;
            }
            if self.routing.restart.is_some() {
                self.drive_restart(out);
                if self.aborted || !self.routing_quiet() {
                    return;
                }
                continue;
            }
            if !self.routing.started {
                self.begin_restart(out);
                if self.aborted || !self.routing_quiet() {
                    return;
                }
                if !self.routing.started {
                    return;
                }
                continue;
            }
            if self.routing.current.is_some() {
                self.drive_current(out);
                if self.aborted || !self.routing_quiet() {
                    return;
                }
                continue;
            }
            let before = self.routing.cursor();
            let view = self.journal.cursor();
            if before < view {
                self.start_next(out);
                if self.aborted || !self.routing_quiet() {
                    return;
                }
                if self.routing.cursor() > before || self.routing.current.is_some() {
                    continue;
                }
                return;
            }
            self.ensure_watch(out);
            return;
        }
    }

    /// Whether no routing reply, read, write, or load is outstanding.
    fn routing_quiet(&self) -> bool {
        if self.routing.read_ticket.is_some()
            || !self.routing.evaluates.is_empty()
            || !self.routing.warms.is_empty()
            || !self.routing.statuses.is_empty()
            || self.journal.has_routing_write()
            || self.routing.committed.is_some()
        {
            return false;
        }
        if self.artifact_reads.values().any(|read| {
            matches!(
                read,
                ArtifactRead::ReactorSet(_) | ArtifactRead::ReactorBundle(_) | ArtifactRead::SetHeadDestination { .. }
            )
        }) {
            return false;
        }
        !self.loads.values().any(|digest| self.bundles.instance(digest).is_some())
    }

    /// Drive the in-progress seq toward its queued batch.
    fn drive_current(&mut self, out: &mut Vec<Command>) {
        let Some(work) = self.routing.current.as_ref() else {
            return;
        };
        match work.phase {
            SeqPhase::Evaluating => {
                if !self.routing.evaluates.is_empty() || !self.routing.statuses.is_empty() {
                    return;
                }
                self.begin_activations(out);
            }
            SeqPhase::Activating => {
                self.drive_activations(out);
            }
            SeqPhase::Planning => {
                self.drive_planning(out);
            }
        }
    }

    /// Start routing `N = R + 1` from the buffered page, reading when absent.
    fn start_next(&mut self, out: &mut Vec<Command>) {
        let next = self.routing.cursor() + 1;
        let view = self.journal.cursor();
        if next > view {
            return;
        }
        let Some(entry) = self.routing.page.iter().find(|entry| entry.seq == next).cloned() else {
            let after = self.routing.cursor();
            self.emit_routing_read(after, RoutingRead::Steady { after }, out);
            return;
        };
        if !self.ensure_set_cached(&self.routing.heads.clone(), out) {
            return;
        }
        let prev = self.selection_at(&self.routing.heads.clone());
        let live = self.live_digests(&prev);
        if let Err(reason) = self.check_live_cursors(next, &live) {
            self.abort(reason, out);
            return;
        }
        if live.is_empty() {
            self.begin_quiet_seq(next, entry, prev, out);
        } else {
            self.begin_live_seq(next, entry, prev, live, out);
        }
    }

    /// Verify every live ready instance sits at `N - 1`.
    fn check_live_cursors(&self, next: u64, live: &BTreeMap<Digest, Vec<Head<OpaqueBytes>>>) -> Result<(), String> {
        for digest in live.keys() {
            let Some(instance) = self.bundles.instance(digest) else {
                continue;
            };
            if matches!(instance.state, InstanceState::Ready { .. }) && instance.cursor != next - 1 {
                return Err(format!(
                    "live instance {digest} cursor {} is not {} for seq {next}",
                    instance.cursor,
                    next - 1
                ));
            }
        }
        Ok(())
    }

    /// Start one seq with live digests: fan out, then apply the entry.
    fn begin_live_seq(
        &mut self,
        next: u64,
        entry: JournalEntry,
        prev: BTreeMap<Head<OpaqueBytes>, Digest>,
        live: BTreeMap<Digest, Vec<Head<OpaqueBytes>>>,
        out: &mut Vec<Command>,
    ) {
        if let Err(reason) = self.emit_live_events(next, &entry, &live, out) {
            self.abort(reason, out);
            return;
        }
        self.routing.current = Some(SeqWork::new(next, entry, prev, live, SeqPhase::Evaluating));
        self.apply_seq_entry(out);
    }

    /// Start one seq with no live digest: apply the entry, then activate changes.
    fn begin_quiet_seq(
        &mut self,
        next: u64,
        entry: JournalEntry,
        prev: BTreeMap<Head<OpaqueBytes>, Digest>,
        out: &mut Vec<Command>,
    ) {
        let folded = entry.to_entry();
        if let Err(error) = self.routing.heads.apply(&folded) {
            self.abort(format!("routing heads rejected entry {next}: {error}"), out);
            return;
        }
        if !self.ensure_set_cached(&self.routing.heads.clone(), out) {
            self.routing.current = Some(SeqWork::new(next, entry, prev, BTreeMap::new(), SeqPhase::Evaluating));
            return;
        }
        let curr = self.selection_at(&self.routing.heads.clone());
        let to_activate = Self::changed_heads(&prev, &curr);
        if to_activate.is_empty() {
            self.routing.page.retain(|kept| kept.seq != next);
            self.check_processed(out);
            return;
        }
        let mut work = SeqWork::new(next, entry, prev, BTreeMap::new(), SeqPhase::Activating);
        work.curr = Some(curr);
        work.to_activate = to_activate;
        self.routing.current = Some(work);
        self.drive_activations(out);
    }

    /// Apply the current seq's entry to routing `Heads` and compute its successor set.
    fn apply_seq_entry(&mut self, out: &mut Vec<Command>) {
        let Some(work) = self.routing.current.as_mut() else {
            return;
        };
        let next = work.n;
        let entry = work.entry.clone();
        let folded = entry.to_entry();
        if let Err(error) = self.routing.heads.apply(&folded) {
            self.abort(format!("routing heads rejected entry {next}: {error}"), out);
            return;
        }
        if !self.ensure_set_cached(&self.routing.heads.clone(), out) {
            return;
        }
        let curr = self.selection_at(&self.routing.heads.clone());
        let prev = self.routing.current.as_ref().expect("seq work").prev.clone();
        let to_activate = Self::changed_heads(&prev, &curr);
        if let Some(work) = self.routing.current.as_mut() {
            work.curr = Some(curr);
            work.to_activate = to_activate;
        }
    }

    /// Answer every barrier waiter whose bound is quiescent.
    pub(crate) fn check_processed(&mut self, out: &mut Vec<Command>) {
        if self.aborted || !self.routing.started {
            return;
        }
        if self.journal.has_routing_write() {
            return;
        }
        let reached = self.routing.cursor();
        let head = self.journal.cursor();
        let mut ready = Vec::new();
        let mut waiting = Vec::new();
        for (caller, through) in take(&mut self.routing.awaiters) {
            if reached < through {
                waiting.push((caller, through));
                continue;
            }
            let blocked = self.journal.requests().outstanding().any(|request| request.seq().0 <= through);
            if blocked {
                waiting.push((caller, through));
                continue;
            }
            ready.push((caller, through));
        }
        self.routing.awaiters = waiting;
        for (caller, _) in ready {
            out.push(Command::Processed { caller, reply: Processed { head } });
        }
    }

    /// Issue one routing page read after `after` for `purpose`.
    pub(crate) fn emit_routing_read(&mut self, after: u64, purpose: RoutingRead, out: &mut Vec<Command>) {
        let ticket = self.mint(EventsTicket::mint);
        self.routing.read_ticket = Some(ticket);
        self.routing.read_purpose = Some(purpose);
        out.push(Command::ReadEvents { ticket, request: ReadEvents { after, limit: EVENTS_PAGE } });
    }

    /// Hold exactly one watch while routing is idle at the journal head.
    pub(crate) fn ensure_watch(&mut self, out: &mut Vec<Command>) {
        if self.routing.watch.is_some() {
            return;
        }
        let ticket = self.mint(WatchTicket::mint);
        self.routing.watch = Some(ticket);
        out.push(Command::WatchHead { ticket, request: WatchHead { after: self.journal.cursor() } });
    }
}
