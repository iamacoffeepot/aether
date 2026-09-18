//! The program core: a sans-io state machine over the journal folds.
//!
//! [`ProgramCore::start`] is the only constructor, and it returns the first
//! journal read, so no core exists that has not begun catching up.
//! [`ProgramCore::call`] accepts one [`Call`];
//! the shell stores its deferred reply under the returned [`CallerId`]
//! before it performs the commands. Each command kind has one typed reply
//! method, and a reply whose ticket the core is not waiting on returns no
//! commands.

mod command;
mod journal;
mod ticket;

use std::collections::{BTreeMap, VecDeque};

use aether_bloomery_kinds::{
    AppendRecordsResult, Call, CallOutcome, CallRefusal, ClosureLimit, Detail, Digest, DriverRecord, Fault,
    FaultReason, Invoked, ProgramRef, ReadArtifactResult, ReadClosureResult, ReadEvents, ReadEventsResult, Seq,
};

use crate::bundles::BundleTable;

pub use command::{Command, LoadOutcome};
pub use journal::EVENTS_PAGE;
pub use journal::{Journal, PendingWrite, RequestedClaim};
pub use ticket::{AppendTicket, ArtifactTicket, CallerId, ClosureTicket, EventsTicket, InvokeTicket, LoadTicket};

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
    pub(crate) limit: ClosureLimit,
    pub(crate) next_ticket: u64,
    pub(crate) recovered: bool,
    pub(crate) aborted: bool,
    pub(crate) held: VecDeque<(CallerId, Call)>,
    pub(crate) waiters: BTreeMap<u64, (u64, Vec<CallerId>)>,
    pub(crate) activations: BTreeMap<u64, Activation>,
    pub(crate) artifact_reads: BTreeMap<ArtifactTicket, Digest>,
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
            limit,
            next_ticket: 0,
            recovered: false,
            aborted: false,
            held: VecDeque::new(),
            waiters: BTreeMap::new(),
            activations: BTreeMap::new(),
            artifact_reads: BTreeMap::new(),
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

    /// Feed one journal page. Unknown tickets return no commands.
    pub fn on_events(&mut self, ticket: EventsTicket, result: ReadEventsResult) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        if self.journal.read_ticket() != Some(ticket) {
            return out;
        }
        self.journal.set_read_ticket(None);
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

    /// Feed one bundle artifact reply. Unknown tickets return no commands.
    pub fn on_artifact(&mut self, ticket: ArtifactTicket, result: ReadArtifactResult) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let Some(bundle) = self.artifact_reads.remove(&ticket) else {
            return out;
        };
        self.continue_artifact(bundle, result, &mut out);
        out
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
                if head <= self.journal.cursor() {
                    self.abort(
                        format!("journal committed an append at head {head} behind cursor {}", self.journal.cursor()),
                        &mut out,
                    );
                    return out;
                }
                if let PendingWrite::RequestedCall { callers, call } = write {
                    self.activations.insert(head, Activation { callers, call });
                }
                self.journal.set_target(head);
                self.emit_read(&mut out);
            }
            AppendRecordsResult::Conflict { actual } => {
                if actual <= self.journal.cursor() {
                    self.abort(
                        format!("journal reported conflict at {actual} behind cursor {}", self.journal.cursor()),
                        &mut out,
                    );
                    return out;
                }
                self.journal.queue_front(write);
                self.journal.set_target(actual);
                self.emit_read(&mut out);
            }
            AppendRecordsResult::Err { message } => match write {
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
                    self.pump(&mut out);
                }
                PendingWrite::Outcome { request, record: DriverRecord::Transition { .. }, .. } => {
                    let Some((program, input)) = self.request_data(request) else {
                        self.abort(format!("cannot re-record the refused transition for request {request}"), &mut out);
                        return out;
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
                    self.pump(&mut out);
                }
                PendingWrite::Outcome { .. } => {
                    self.abort(format!("journal refused a fault append: {message}"), &mut out);
                }
                PendingWrite::Startup => {
                    self.abort(format!("journal refused the startup batch: {message}"), &mut out);
                }
            },
        }
        out
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
        self.continue_loaded(bundle, outcome, &mut out);
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
    fn emit_read(&mut self, out: &mut Vec<Command>) {
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

    /// Activate committed requests, answer ready waiters, and drain held calls.
    pub(crate) fn on_recovered_synced(&mut self, out: &mut Vec<Command>) {
        self.activate_committed(out);
        if self.aborted {
            return;
        }
        self.answer_ready(out);
        self.drain_held(out);
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
            }
            if self.aborted {
                return;
            }
        }
    }
}
