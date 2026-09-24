//! Scripted services driving [`ProgramCore`](aether_bloomery_driver::ProgramCore) without I/O.
//!
//! [`World`] holds the core plus a fake journal truth, stored artifacts,
//! scripted closures, loads, and invocations. [`World::drive`] feeds commands
//! back through those doubles in a loop: journal reads and appends answer
//! automatically from the scripted truth, scripted services answer from
//! their maps, and answers and aborts are collected. Commands no double can
//! answer come back for the test to feed by hand, which is how tests stage
//! interleavings (a second call before the first invoke answers, an append
//! failure between two commits).

mod reactor;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::mem::take;

use aether_bloomery_driver::{CallerId, Command, EvaluateTicket, LoadOutcome, ProgramCore, WatchTicket};
use aether_bloomery_kinds::{
    Activated, ActivationRejected, AppendRecords, AppendRecordsResult, CallOutcome, ClosureArtifact, ClosureLimit,
    Digest, DriverRecord, EncodedArtifact, Evaluated, Head, Invoke, Invoked, JournalEntry, OpaqueBytes, Processed,
    ReactionFailed, ReadArtifact, ReadArtifactResult, ReadClosure, ReadClosureResult, ReadEvents, ReadEventsResult,
    RecordedHead, RecordedHeadMove, Status, Warmed, WatchHeadResult, artifact_digest,
};
use aether_data::{Kind, KindId, Storage, StorageData};
use reactor::Reactor;

/// Byte budget every test core starts under: 1 MiB.
pub const LIMIT_BYTES: u64 = 1024 * 1024;

/// One journal entry's truth: stored kind, cause, and storage bytes.
type Truth = (KindId, Option<u64>, Vec<u8>);

fn seq_no(seq: usize) -> u64 {
    u64::try_from(seq).expect("test seq fits in u64")
}

/// A scripted world: the core plus every service it talks to.
pub struct World {
    /// The core under test; public so tests can feed replies by hand.
    pub core: ProgramCore,
    /// Journal truth; entry seqs are positions plus one.
    journal: Vec<Truth>,
    /// Stored artifacts by digest: kind plus unprefixed payload.
    artifacts: HashMap<Digest, (KindId, Vec<u8>)>,
    /// Scripted closures by root digest.
    pub closures: HashMap<Digest, Vec<ClosureArtifact>>,
    /// Roots whose closure read answers `TooLarge`.
    pub oversized: HashSet<Digest>,
    /// Scripted load outcomes by bundle digest.
    pub loads: HashMap<Digest, Result<(), String>>,
    /// Scripted invoke replies by request seq.
    pub invokes: HashMap<u64, Invoked>,
    /// Closure budget the core started with, echoed in `TooLarge` replies.
    pub limit: ClosureLimit,
    /// Answers collected from the core, in arrival order.
    pub answers: Vec<(CallerId, CallOutcome)>,
    /// Abort reason, when the core aborted.
    pub abort: Option<String>,
    /// Every append command the core emitted, including refused ones.
    pub appends: Vec<AppendRecords>,
    /// Every append the journal committed, in commit order.
    pub committed: Vec<AppendRecords>,
    /// Bundle digests the core asked to read, in order.
    pub reads_seen: Vec<Digest>,
    /// Closure roots the core asked to read, in order.
    pub closures_seen: Vec<Digest>,
    /// Bundle digests the core asked to load, in order.
    pub loads_seen: Vec<Digest>,
    /// Invokes the core emitted, with the bundle they address, in order.
    pub invokes_seen: Vec<(Digest, Invoke)>,
    /// When set, the next append answers `Conflict` instead of committing.
    pub conflict_next: Option<u64>,
    /// When set, the next append answers `Err` instead of committing.
    pub fail_next: Option<String>,
    /// When set, every journal read answers `Err`.
    pub fail_reads: Option<String>,
    /// Parked watches: ticket and `after`, in arrival order.
    pub parked: Vec<(WatchTicket, u64)>,
    /// Every `WatchHead` boundary the core emitted, in order.
    pub watches_seen: Vec<u64>,
    /// Every `ReadEvents` boundary the core emitted, in order.
    pub events_seen: Vec<u64>,
    /// `events_seen.len()` at each `Warm` the core emitted, in order.
    pub warm_marks: Vec<usize>,
    /// Scripted reactor roots by bundle digest, holding cursors and recordings.
    pub reactors: HashMap<Digest, Reactor>,
    /// Bundles behind held `Evaluate` commands, so hand-fed replies sequence.
    pub eval_roots: BTreeMap<EvaluateTicket, Digest>,
    /// Barrier replies collected from the core, in arrival order.
    pub processed: Vec<(CallerId, Processed)>,
    /// Scripted warm replies by batch first seq.
    pub warm_pages: BTreeMap<u64, Warmed>,
    /// Default warm reply when the first seq is unscripted; `None` folds through the batch end.
    pub warm_default: Option<Warmed>,
    /// Scripted evaluation replies by event seq.
    pub evaluates: BTreeMap<u64, Evaluated>,
    /// Scripted status reply for every resync.
    pub status: Option<Status>,
}

impl World {
    /// Start a core under [`LIMIT_BYTES`] over an empty journal truth.
    ///
    /// # Panics
    ///
    /// Panics if [`LIMIT_BYTES`] ever leaves the valid closure-limit range.
    #[must_use]
    pub fn open() -> (Self, Vec<Command>) {
        let limit = ClosureLimit::new(LIMIT_BYTES).expect("test limit is valid");
        let (core, commands) = ProgramCore::start(limit);
        let world = Self {
            core,
            journal: Vec::new(),
            artifacts: HashMap::new(),
            closures: HashMap::new(),
            oversized: HashSet::new(),
            loads: HashMap::new(),
            invokes: HashMap::new(),
            limit,
            answers: Vec::new(),
            abort: None,
            appends: Vec::new(),
            committed: Vec::new(),
            reads_seen: Vec::new(),
            closures_seen: Vec::new(),
            loads_seen: Vec::new(),
            invokes_seen: Vec::new(),
            conflict_next: None,
            fail_next: None,
            fail_reads: None,
            parked: Vec::new(),
            watches_seen: Vec::new(),
            events_seen: Vec::new(),
            warm_marks: Vec::new(),
            reactors: HashMap::new(),
            eval_roots: BTreeMap::new(),
            processed: Vec::new(),
            warm_pages: BTreeMap::new(),
            warm_default: None,
            evaluates: BTreeMap::new(),
            status: None,
        };
        (world, commands)
    }

    /// Current journal head: entries recorded so far.
    #[must_use]
    pub fn head(&self) -> u64 {
        seq_no(self.journal.len())
    }

    /// Seed one typed record into the journal truth. Tests seed history
    /// before driving the core's initial read.
    ///
    /// # Panics
    ///
    /// Panics if the record does not storage-encode.
    pub fn seed<K: Kind + Storage + Clone>(&mut self, cause: Option<u64>, value: &K) {
        let bytes = K::encode_storage(&StorageData::from_value(value.clone())).expect("encode seeded record");
        self.journal.push((K::ID, cause, bytes));
    }

    /// Seed a head move into the journal truth.
    pub fn seed_move(&mut self, head: &'static str, to: Digest) {
        let recorded = RecordedHeadMove::new(RecordedHead::from(&program_head(head)), to);
        self.seed(None, &recorded);
    }

    /// Store one artifact the core can read back.
    #[must_use]
    pub fn store(&mut self, kind: KindId, payload: &[u8]) -> Digest {
        let digest = artifact_digest(kind, payload);
        self.artifacts.insert(digest, (kind, payload.to_vec()));
        digest
    }

    /// Drive commands through the doubles to quiescence, returning the
    /// commands no double could answer (unscripted loads and invokes).
    pub fn drive(&mut self, commands: Vec<Command>) -> Vec<Command> {
        let mut queue: VecDeque<Command> = commands.into();
        let mut manual = Vec::new();
        while let Some(command) = queue.pop_front() {
            match self.step(command) {
                Step::More(next) => queue.extend(next),
                Step::Manual(command) => manual.push(command),
            }
        }
        manual
    }

    fn step(&mut self, command: Command) -> Step {
        match command {
            Command::ReadEvents { ticket, request } => {
                self.events_seen.push(request.after);
                let result = self.page(&request);
                Step::More(self.core.on_events(ticket, result))
            }
            Command::ReadArtifact { ticket, request } => {
                self.reads_seen.push(request.digest);
                let result = self.artifact(&request);
                Step::More(self.core.on_artifact(ticket, result))
            }
            Command::ReadClosure { ticket, request } => {
                self.closures_seen.push(request.root);
                let result = self.closure(&request);
                Step::More(self.core.on_closure(ticket, result))
            }
            Command::Append { ticket, request } => {
                let result = self.commit(&request);
                let head = self.head();
                if matches!(result, AppendRecordsResult::Committed { .. }) {
                    self.committed.push(request.clone());
                }
                self.appends.push(request);
                let mut next = self.core.on_appended(ticket, result);
                next.extend(self.wake_watches(head));
                Step::More(next)
            }
            Command::Load { ticket, bundle, wasm } => {
                self.loads_seen.push(bundle);
                match self.loads.get(&bundle).cloned() {
                    Some(Ok(())) => Step::More(self.core.on_loaded(ticket, LoadOutcome::Loaded)),
                    Some(Err(error)) => Step::More(self.core.on_loaded(ticket, LoadOutcome::Failed { error })),
                    None => Step::Manual(Command::Load { ticket, bundle, wasm }),
                }
            }
            Command::Invoke { ticket, bundle, request } => {
                self.invokes_seen.push((bundle, request.clone()));
                match self.invokes.remove(&request.seq()) {
                    Some(invoked) => Step::More(self.core.on_invoked(ticket, invoked)),
                    None => Step::Manual(Command::Invoke { ticket, bundle, request }),
                }
            }
            Command::WatchHead { ticket, request } => {
                self.watches_seen.push(request.after);
                if self.head() > request.after {
                    let head = self.head();
                    Step::More(self.core.on_watched(ticket, WatchHeadResult::Advanced { head }))
                } else {
                    self.parked.push((ticket, request.after));
                    Step::More(Vec::new())
                }
            }
            Command::Warm { ticket, bundle, request } => {
                self.warm_marks.push(self.events_seen.len());
                let first = request.entries().first();
                let last = request.entries().last();
                let scripted = self.warm_pages.get(&first).cloned();
                let default = self.warm_default.clone();
                let warmed = self.reactors.entry(bundle).or_default().warm(first, last, scripted, default.as_ref());
                Step::More(self.core.on_warmed(ticket, warmed))
            }
            Command::Evaluate { ticket, bundle, request } => {
                let seq = request.entry().seq;
                let scripted = self.evaluates.get(&seq).cloned();
                let reactor = self.reactors.entry(bundle).or_default();
                if let Some(auto) = reactor.check_event(seq) {
                    Step::More(self.core.on_evaluated(ticket, auto))
                } else if let Some(scripted) = scripted {
                    reactor.note_evaluated(&scripted);
                    Step::More(self.core.on_evaluated(ticket, scripted))
                } else {
                    self.eval_roots.insert(ticket, bundle);
                    Step::Manual(Command::Evaluate { ticket, bundle, request })
                }
            }
            Command::QueryStatus { ticket, bundle } => match self.status {
                Some(status) => Step::More(self.core.on_status(ticket, &status)),
                None => Step::Manual(Command::QueryStatus { ticket, bundle }),
            },
            Command::Answer { caller, outcome } => {
                self.answers.push((caller, outcome));
                Step::More(Vec::new())
            }
            Command::Processed { caller, reply } => {
                self.processed.push((caller, reply));
                Step::More(Vec::new())
            }
            Command::Abort { reason } => {
                self.abort = Some(reason);
                Step::More(Vec::new())
            }
        }
    }

    /// Answer one page read from the journal truth.
    fn page(&self, request: &ReadEvents) -> ReadEventsResult {
        if let Some(message) = &self.fail_reads {
            return ReadEventsResult::Err { after: request.after, message: message.clone() };
        }
        let after = usize::try_from(request.after).expect("test boundary fits in usize");
        let entries = self
            .journal
            .iter()
            .skip(after)
            .take(request.limit as usize)
            .enumerate()
            .map(|(index, (kind, cause, bytes))| JournalEntry {
                seq: seq_no(after + index + 1),
                kind: *kind,
                cause: *cause,
                recorded_at_millis: 0,
                bytes: bytes.clone(),
            })
            .collect();
        ReadEventsResult::Ok { after: request.after, head: self.head(), entries }
    }

    /// Answer one artifact read from the stored artifacts.
    fn artifact(&self, request: &ReadArtifact) -> ReadArtifactResult {
        match self.artifacts.get(&request.digest) {
            Some((kind, bytes)) => {
                ReadArtifactResult::Found { digest: request.digest, kind: *kind, bytes: bytes.clone() }
            }
            None => ReadArtifactResult::Missing { digest: request.digest },
        }
    }

    /// Answer one closure read from the scripted closures.
    fn closure(&self, request: &ReadClosure) -> ReadClosureResult {
        if self.oversized.contains(&request.root) {
            return ReadClosureResult::TooLarge { root: request.root, limit_bytes: self.limit };
        }
        let Some(artifacts) = self.closures.get(&request.root) else {
            return ReadClosureResult::Missing { root: request.root, digest: request.root };
        };
        ReadClosureResult::Found { root: request.root, artifacts: artifacts.clone() }
    }

    /// Apply one fenced append to the journal truth, honouring injections.
    fn commit(&mut self, request: &AppendRecords) -> AppendRecordsResult {
        if let Some(actual) = self.conflict_next.take() {
            return AppendRecordsResult::Conflict { actual };
        }
        if let Some(message) = self.fail_next.take() {
            return AppendRecordsResult::Err { message };
        }
        if request.expected_seq() != self.head() {
            return AppendRecordsResult::Conflict { actual: self.head() };
        }
        for artifact in request.artifacts() {
            self.artifacts.insert(artifact.digest(), (artifact.kind(), artifact.bytes().to_vec()));
        }
        for record in request.records() {
            let (cause, kind, bytes) = encode_record(record);
            self.journal.push((kind, cause, bytes));
        }
        AppendRecordsResult::Committed {
            head: self.head(),
            artifacts: request.artifacts().iter().map(EncodedArtifact::digest).collect(),
        }
    }

    /// Wake every parked watch whose boundary the head has passed.
    pub(crate) fn wake_watches(&mut self, head: u64) -> Vec<Command> {
        let mut tickets = Vec::new();
        let mut remaining = Vec::new();
        for (ticket, after) in take(&mut self.parked) {
            if head > after {
                tickets.push(ticket);
            } else {
                remaining.push((ticket, after));
            }
        }
        self.parked = remaining;
        let mut next = Vec::new();
        for ticket in tickets {
            next.extend(self.core.on_watched(ticket, WatchHeadResult::Advanced { head }));
        }
        next
    }
}

/// One stepped command: follow-up commands, or one for the test to feed by hand.
enum Step {
    More(Vec<Command>),
    Manual(Command),
}

/// Encode one driver record as journal truth: cause, stored kind, and bytes.
fn encode_record(record: &DriverRecord) -> (Option<u64>, KindId, Vec<u8>) {
    fn encode<K: Kind + Storage + Clone>(value: &K) -> (KindId, Vec<u8>) {
        let bytes = K::encode_storage(&StorageData::from_value(value.clone())).expect("encode journal record");
        (K::ID, bytes)
    }
    match record {
        DriverRecord::Requested { cause, record } => {
            let (kind, bytes) = encode(record);
            (*cause, kind, bytes)
        }
        DriverRecord::Transition { cause, record } => {
            let (kind, bytes) = encode(record);
            (Some(*cause), kind, bytes)
        }
        DriverRecord::Fault { cause, record } => {
            let (kind, bytes) = encode(record);
            (Some(*cause), kind, bytes)
        }
        DriverRecord::Activated { cause, record } => {
            let (kind, bytes) = encode::<Activated>(record);
            (Some(*cause), kind, bytes)
        }
        DriverRecord::ActivationRejected { cause, record } => {
            let (kind, bytes) = encode::<ActivationRejected>(record);
            (Some(*cause), kind, bytes)
        }
        DriverRecord::ReactionFailed { cause, record } => {
            let (kind, bytes) = encode::<ReactionFailed>(record);
            (Some(*cause), kind, bytes)
        }
        DriverRecord::HeadMoved { cause, record } => {
            let (kind, bytes) = encode::<RecordedHeadMove>(record);
            (Some(*cause), kind, bytes)
        }
    }
}

/// Digest of 32 equal bytes: readable, deterministic test identity.
#[must_use]
pub fn digest(byte: u8) -> Digest {
    Digest::from_bytes([byte; 32])
}

/// One program head over opaque bundle bytes.
#[must_use]
pub fn program_head(name: &'static str) -> Head<OpaqueBytes> {
    Head::new(name)
}

/// One wasm module carrying the given custom sections: name plus raw payload each.
#[must_use]
pub fn wasm_module(sections: &[(&str, &[u8])]) -> Vec<u8> {
    fn push_leb(mut value: u32, out: &mut Vec<u8>) {
        loop {
            let byte = u8::try_from(value & 0x7f).expect("masked byte fits");
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }
    let mut wasm = b"\0asm".to_vec();
    wasm.extend_from_slice(&1u32.to_le_bytes());
    for (name, data) in sections {
        let name = name.as_bytes();
        let mut section = Vec::new();
        push_leb(u32::try_from(name.len()).expect("test section name fits"), &mut section);
        section.extend_from_slice(name);
        section.extend_from_slice(data);
        wasm.push(0);
        push_leb(u32::try_from(section.len()).expect("test section fits"), &mut wasm);
        wasm.extend_from_slice(&section);
    }
    wasm
}

/// Concatenated program declaration records: name, input kind, result kind, intent.
#[must_use]
pub fn program_records(records: &[(&str, KindId, KindId, &str)]) -> Vec<u8> {
    let mut data = Vec::new();
    for (name, input, result, intent) in records {
        let name = name.as_bytes();
        let intent = intent.as_bytes();
        data.push(1u8);
        data.extend_from_slice(&u16::try_from(name.len()).expect("test name fits").to_le_bytes());
        data.extend_from_slice(name);
        data.extend_from_slice(&input.0.to_le_bytes());
        data.extend_from_slice(&result.0.to_le_bytes());
        data.push(0u8);
        data.extend_from_slice(&u16::try_from(intent.len()).expect("test intent fits").to_le_bytes());
        data.extend_from_slice(intent);
    }
    data
}

/// Concatenated reactor declaration records, each with one `on_event` rule over opaque bytes.
#[must_use]
pub fn reactor_records(names: &[&str]) -> Vec<u8> {
    let mut data = Vec::new();
    for name in names {
        let name = name.as_bytes();
        let rule = b"on_event";
        data.push(1u8);
        data.extend_from_slice(&u16::try_from(name.len()).expect("test name fits").to_le_bytes());
        data.extend_from_slice(name);
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&u16::try_from(rule.len()).expect("test rule fits").to_le_bytes());
        data.extend_from_slice(rule);
        data.extend_from_slice(&OpaqueBytes::ID.0.to_le_bytes());
        data.extend_from_slice(&OpaqueBytes::ID.0.to_le_bytes());
    }
    data
}

/// One wasm bundle declaring the given programs and reactors, labelled for a distinct digest.
///
/// Each role section is emitted only when its list is non-empty; the
/// `test.label` section carries `label` so that labels give distinct digests.
#[must_use]
pub fn bundle_wasm(programs: &[(&str, KindId, KindId, &str)], reactors: &[&str], label: &[u8]) -> Vec<u8> {
    let program_bytes = program_records(programs);
    let reactor_bytes = reactor_records(reactors);
    let mut sections: Vec<(&str, &[u8])> = Vec::new();
    if !programs.is_empty() {
        sections.push(("aether.bloomery.programs", &program_bytes));
    }
    if !reactors.is_empty() {
        sections.push(("aether.bloomery.reactors", &reactor_bytes));
    }
    sections.push(("test.label", label));
    wasm_module(&sections)
}
