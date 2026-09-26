//! The program core: a sans-io state machine over the journal folds.
//!
//! [`ProgramCore::start`] is the only constructor, and it returns the first
//! journal read, so no core exists that has not begun catching up.
//! [`ProgramCore::call`] accepts one [`Call`], and
//! [`ProgramCore::fetch_artifact`] one bundle root's fetch-on-miss;
//! the shell stores its deferred reply under the returned [`CallerId`]
//! before it performs the commands. Each command kind has one typed reply
//! method, and a reply whose ticket the core is not waiting on returns no
//! commands.

mod artifacts;
mod command;
mod journal;
mod ticket;

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, VecDeque};

use aether_bloomery_kinds::{
    AppendRecordsResult, Call, CallOutcome, CallRefusal, ClosureLimit, Detail, Digest, DriverRecord, Fault,
    FaultReason, Invoked, ProgramRef, ReadArtifact, ReadArtifactResult, ReadClosureResult, ReadEvents,
    ReadEventsResult, Seq,
};

use self::artifacts::{ARTIFACT_CACHE_BYTES, ArtifactCache};
use crate::bundles::BundleTable;
use crate::programs::DigestQueue;
use crate::reactors::{CommittedRouting, Routing};

pub use command::{Command, LoadOutcome};
pub use journal::EVENTS_PAGE;
pub use journal::{Journal, PendingWrite, PlannedRecord, RequestedClaim};
pub use ticket::{
    AppendTicket, ArtifactTicket, CallerId, ClosureTicket, EvaluateTicket, EventsTicket, InvokeTicket, LoadTicket,
    StatusTicket, WarmTicket, WatchTicket,
};

/// Why the core read one artifact.
#[derive(Debug, Clone, Copy)]
pub enum ArtifactRead {
    /// A bundle's wasm, read once for every role.
    Bundle(Digest),
    /// A reactor set's stored membership.
    ReactorSet(Digest),
    /// The destination of the `SetHead` the current seq is checking.
    SetHeadDestination,
    /// One digest bundle roots fetched on a miss, shared by every waiting fetch.
    Fetch(Digest),
}

/// One committed `Requested` whose pipeline has not started yet.
#[derive(Debug)]
pub struct Activation {
    /// Callers waiting on this request, in arrival order.
    pub(crate) callers: Vec<CallerId>,
    /// The call as made, verified against the fold at activation.
    pub(crate) call: Call,
}

/// Sans-io program core: calls and typed replies in, commands out.
#[derive(Debug)]
pub struct ProgramCore {
    pub(crate) journal: Journal,
    pub(crate) bundles: BundleTable,
    pub(crate) queues: BTreeMap<Digest, DigestQueue>,
    pub(crate) routing: Routing,
    pub(crate) limit: ClosureLimit,
    pub(crate) next_ticket: u64,
    pub(crate) recovered: bool,
    pub(crate) aborted: bool,
    pub(crate) held: VecDeque<(CallerId, Call)>,
    pub(crate) waiters: BTreeMap<u64, (u64, Vec<CallerId>)>,
    pub(crate) activations: BTreeMap<u64, Activation>,
    pub(crate) artifact_reads: BTreeMap<ArtifactTicket, ArtifactRead>,
    /// Found artifacts reused by fetches and `SetHead` destination checks.
    pub(crate) artifacts: ArtifactCache,
    /// Fetches waiting on each digest's one in-flight read, in arrival order.
    pub(crate) fetching: BTreeMap<Digest, Vec<CallerId>>,
    pub(crate) closure_reads: BTreeMap<ClosureTicket, (Digest, u64)>,
    pub(crate) loads: BTreeMap<LoadTicket, Digest>,
    pub(crate) invokes: BTreeMap<InvokeTicket, (Digest, u64)>,
}

impl ProgramCore {
    /// Begin catching up from the empty prefix under a closure byte budget.
    #[must_use]
    pub fn start(limit: ClosureLimit) -> (Self, Vec<Command>) {
        let mut core = Self {
            journal: Journal::new(),
            bundles: BundleTable::default(),
            queues: BTreeMap::new(),
            routing: Routing::new(),
            limit,
            next_ticket: 0,
            recovered: false,
            aborted: false,
            held: VecDeque::new(),
            waiters: BTreeMap::new(),
            activations: BTreeMap::new(),
            artifact_reads: BTreeMap::new(),
            artifacts: ArtifactCache::with_budget(ARTIFACT_CACHE_BYTES),
            fetching: BTreeMap::new(),
            closure_reads: BTreeMap::new(),
            loads: BTreeMap::new(),
            invokes: BTreeMap::new(),
        };
        let mut out = Vec::new();
        core.emit_read(&mut out);
        (core, out)
    }

    /// Accept one call. The returned [`CallerId`] identifies the caller for
    /// its exactly-once [`Answer`](Command::Answer).
    pub fn call(&mut self, call: Call) -> (CallerId, Vec<Command>) {
        let caller = self.mint(CallerId::mint);
        let mut out = Vec::new();
        if self.aborted {
            return (caller, out);
        }
        if self.journal.synced() && self.recovered && self.held.is_empty() {
            self.handle_call(caller, call, &mut out);
            if !self.aborted {
                self.pump(&mut out);
            }
        } else {
            self.held.push_back((caller, call));
        }
        (caller, out)
    }

    /// Accept one bundle root's fetch-on-miss. The returned [`CallerId`]
    /// identifies the fetch for its exactly-once [`Fetched`](Command::Fetched).
    ///
    /// A cached artifact answers at once. Otherwise the fetch waits on the
    /// digest's one journal read, which it issues when none is in flight.
    /// Fetches do not wait for the journal fold.
    pub fn fetch_artifact(&mut self, request: ReadArtifact) -> (CallerId, Vec<Command>) {
        let caller = self.mint(CallerId::mint);
        let mut out = Vec::new();
        if self.aborted {
            return (caller, out);
        }
        let digest = request.digest;
        if let Some(artifact) = self.artifacts.get(digest) {
            let result = ReadArtifactResult::Found { artifact: artifact.clone() };
            out.push(Command::Fetched { caller, result });
            return (caller, out);
        }
        match self.fetching.entry(digest) {
            Entry::Occupied(waiters) => waiters.into_mut().push(caller),
            Entry::Vacant(waiters) => {
                waiters.insert(vec![caller]);
                let ticket = self.mint(ArtifactTicket::mint);
                self.artifact_reads.insert(ticket, ArtifactRead::Fetch(digest));
                out.push(Command::ReadArtifact { ticket, request });
            }
        }
        (caller, out)
    }

    /// Feed one journal page. Unknown tickets return no commands.
    pub fn on_events(&mut self, ticket: EventsTicket, result: ReadEventsResult) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        if self.journal.read_ticket() == Some(ticket) {
            self.journal.set_read_ticket(None);
        } else if let Some(read) = self.routing.read.take_if(|read| read.ticket == ticket) {
            self.continue_routing_events(&read, result, &mut out);
            return out;
        } else {
            return out;
        }
        match result {
            ReadEventsResult::Err { message, .. } => {
                self.abort(format!("journal read failed: {message}"), &mut out);
            }
            ReadEventsResult::Ok { after, head, entries } => {
                if after != self.journal.cursor() {
                    self.abort(
                        format!("journal page starts after {after} but the cursor is {}", self.journal.cursor()),
                        &mut out,
                    );
                    return out;
                }
                if head < self.journal.cursor() {
                    self.abort(format!("journal head {head} moved behind cursor {}", self.journal.cursor()), &mut out);
                    return out;
                }
                if let Err(reason) = self.journal.apply_page(&entries) {
                    self.abort(reason, &mut out);
                    return out;
                }
                if self.journal.cursor() > head {
                    self.abort(format!("journal returned entries past its head {head}"), &mut out);
                    return out;
                }
                self.journal.set_target(head);
                if entries.is_empty() && !self.journal.synced() {
                    self.abort(format!("journal returned an empty page below its head {head}"), &mut out);
                    return out;
                }
                if self.journal.synced() {
                    self.on_synced(&mut out);
                } else {
                    self.emit_read(&mut out);
                }
            }
        }
        out
    }

    /// Feed one artifact reply. Unknown tickets return no commands.
    pub fn on_artifact(&mut self, ticket: ArtifactTicket, result: ReadArtifactResult) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let Some(read) = self.artifact_reads.remove(&ticket) else {
            return out;
        };
        match read {
            ArtifactRead::Bundle(bundle) => {
                self.continue_bundle_artifact(bundle, result, &mut out);
            }
            ArtifactRead::ReactorSet(digest) => {
                self.continue_set_artifact(digest, result, &mut out);
            }
            ArtifactRead::SetHeadDestination => {
                self.continue_destination_artifact(result, &mut out);
            }
            ArtifactRead::Fetch(digest) => {
                self.continue_fetch(digest, result, &mut out);
            }
        }
        out
    }

    /// Answer every fetch waiting on `digest`, in arrival order, and cache a
    /// found artifact. A missing or failed read is answered and not kept.
    fn continue_fetch(&mut self, digest: Digest, result: ReadArtifactResult, out: &mut Vec<Command>) {
        for caller in self.fetching.remove(&digest).unwrap_or_default() {
            out.push(Command::Fetched { caller, result: result.clone() });
        }
        if let ReadArtifactResult::Found { artifact } = result {
            self.artifacts.insert(digest, artifact);
        }
    }

    /// Feed one closure reply. Unknown tickets return no commands.
    pub fn on_closure(&mut self, ticket: ClosureTicket, result: ReadClosureResult) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let Some((bundle, seq)) = self.closure_reads.remove(&ticket) else {
            return out;
        };
        self.continue_closure(bundle, seq, result, &mut out);
        out
    }

    /// Feed one fenced append outcome. Unknown tickets return no commands.
    pub fn on_appended(&mut self, ticket: AppendTicket, result: AppendRecordsResult) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        if self.journal.append_ticket() != Some(ticket) {
            return out;
        }
        let Some(write) = self.journal.clear_append() else {
            self.abort("append reply arrived with no write outstanding".to_string(), &mut out);
            return out;
        };
        match result {
            AppendRecordsResult::Committed { head, .. } => {
                self.append_committed(write, head, &mut out);
            }
            AppendRecordsResult::Conflict { actual } => {
                self.append_conflict(write, actual, &mut out);
            }
            AppendRecordsResult::Err { message } => {
                self.append_refused(write, message, &mut out);
            }
        }
        out
    }

    /// Record one committed append and read back its range.
    fn append_committed(&mut self, write: PendingWrite, head: u64, out: &mut Vec<Command>) {
        if head <= self.journal.cursor() {
            self.abort(
                format!("journal committed an append at head {head} behind cursor {}", self.journal.cursor()),
                out,
            );
            return;
        }
        match write {
            PendingWrite::RequestedCall { callers, call } => {
                self.activations.insert(head, Activation { callers, call });
            }
            PendingWrite::Routing { trigger, .. } => {
                let Some(records) = self.routing.appending.take() else {
                    self.abort(format!("routing batch for {trigger} committed with no derived records"), out);
                    return;
                };
                let start = self.journal.cursor() + 1;
                if u64::try_from(records.len()).ok() != Some(head - self.journal.cursor()) {
                    self.abort(
                        format!("routing batch for {trigger} committed at head {head} for {} records", records.len()),
                        out,
                    );
                    return;
                }
                self.routing.committed = Some(CommittedRouting { records, start });
            }
            _ => {}
        }
        self.journal.set_target(head);
        self.emit_read(out);
    }

    /// Requeue one conflicted append at the front and refold to the actual head.
    fn append_conflict(&mut self, write: PendingWrite, actual: u64, out: &mut Vec<Command>) {
        if actual <= self.journal.cursor() {
            self.abort(format!("journal reported conflict at {actual} behind cursor {}", self.journal.cursor()), out);
            return;
        }
        if matches!(write, PendingWrite::Routing { .. }) {
            self.routing.appending = None;
        }
        self.journal.queue_front(write);
        self.journal.set_target(actual);
        self.emit_read(out);
    }

    /// Answer one refused append: refuse its callers, re-record its transition, or abort.
    fn append_refused(&mut self, write: PendingWrite, message: String, out: &mut Vec<Command>) {
        match write {
            PendingWrite::RequestedCall { callers, call } => {
                let reason = Detail::new(&message);
                for caller in callers {
                    out.push(Command::Answer {
                        caller,
                        outcome: CallOutcome::Refused {
                            key: call.key,
                            reason: CallRefusal::Journal { reason: reason.clone() },
                        },
                    });
                }
                self.pump(out);
            }
            PendingWrite::Outcome { request, record: DriverRecord::Transition { .. }, .. } => {
                let Some((program, input)) = self.request_data(request) else {
                    self.abort(format!("cannot re-record the refused transition for request {request}"), out);
                    return;
                };
                let record = DriverRecord::Fault {
                    cause: request,
                    record: Fault {
                        program,
                        input,
                        reason: FaultReason::ProtocolViolation { reason: Detail::new(message) },
                    },
                };
                self.journal.queue_front(PendingWrite::Outcome { request, artifacts: Vec::new(), record });
                self.pump(out);
            }
            PendingWrite::Outcome { .. } => {
                self.abort(format!("journal refused a fault append: {message}"), out);
            }
            PendingWrite::Startup => {
                self.abort(format!("journal refused the startup batch: {message}"), out);
            }
            PendingWrite::Routing { trigger, .. } => {
                self.abort(format!("journal refused the routing batch for {trigger}: {message}"), out);
            }
        }
    }

    /// Feed one load outcome. Unknown tickets return no commands.
    pub fn on_loaded(&mut self, ticket: LoadTicket, outcome: LoadOutcome) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let Some(bundle) = self.loads.remove(&ticket) else {
            return out;
        };
        self.continue_bundle_loaded(bundle, outcome, &mut out);
        out
    }

    /// Feed one invocation reply. Unknown tickets return no commands.
    pub fn on_invoked(&mut self, ticket: InvokeTicket, invoked: Invoked) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let Some((bundle, seq)) = self.invokes.remove(&ticket) else {
            return out;
        };
        self.continue_invoked(bundle, seq, invoked, &mut out);
        out
    }

    /// Mint one ticket or caller id from the crate-private counter.
    pub(crate) fn mint<T>(&mut self, mint: impl FnOnce(u64) -> T) -> T {
        let id = self.next_ticket;
        self.next_ticket += 1;
        mint(id)
    }

    /// The recorded program and input of one request, if folded.
    pub(crate) fn request_data(&self, seq: u64) -> Option<(ProgramRef, Digest)> {
        let found = self.journal.requests().get(Seq(seq))?;
        Some((found.requested().program.clone(), found.requested().input))
    }

    /// Fail the core with one abort command. The shell maps it to `fatal_abort`.
    pub(crate) fn abort(&mut self, reason: String, out: &mut Vec<Command>) {
        self.aborted = true;
        out.push(Command::Abort { reason });
    }

    /// Issue the next journal page read from the cursor.
    pub(crate) fn emit_read(&mut self, out: &mut Vec<Command>) {
        let ticket = self.mint(EventsTicket::mint);
        self.journal.set_read_ticket(Some(ticket));
        out.push(Command::ReadEvents {
            ticket,
            request: ReadEvents { after: self.journal.cursor(), limit: EVENTS_PAGE },
        });
    }

    /// Run the caught-up transition: recovery, then the recovered pass and the pump.
    fn on_synced(&mut self, out: &mut Vec<Command>) {
        if !self.recovered {
            if self.journal.requests().outstanding().next().is_some() {
                if !self.journal.has_startup_queued() {
                    self.journal.queue_back(PendingWrite::Startup);
                }
            } else {
                self.recovered = true;
            }
        }
        if self.recovered {
            self.on_recovered_synced(out);
            if self.aborted {
                return;
            }
        }
        self.pump(out);
    }

    /// Activate committed requests, advance routing over its read-back, answer
    /// ready waiters, drain held calls, and drive routing while work is ready.
    pub(crate) fn on_recovered_synced(&mut self, out: &mut Vec<Command>) {
        self.activate_committed(out);
        if self.aborted {
            return;
        }
        self.routing_read_back(out);
        if self.aborted {
            return;
        }
        self.answer_ready(out);
        self.drain_held(out);
        if self.aborted {
            return;
        }
        self.drive_routing(out);
    }

    /// Handle every call held while catching up or recovering, in FIFO order.
    fn drain_held(&mut self, out: &mut Vec<Command>) {
        while let Some((caller, call)) = self.held.pop_front() {
            self.handle_call(caller, call, out);
            if self.aborted {
                return;
            }
        }
    }

    /// Derive queued writes while one may be decided.
    pub(crate) fn pump(&mut self, out: &mut Vec<Command>) {
        while let Some(write) = self.journal.next_write() {
            match write {
                PendingWrite::RequestedCall { callers, call } => {
                    self.derive_requested(callers, call, out);
                }
                PendingWrite::Outcome { request, artifacts, record } => {
                    self.derive_outcome(request, artifacts, record, out);
                }
                PendingWrite::Startup => self.derive_startup(out),
                PendingWrite::Routing { trigger, plan } => self.derive_routing(trigger, plan, out),
            }
            if self.aborted {
                return;
            }
        }
    }
}
