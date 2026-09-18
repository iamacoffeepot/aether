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

use std::collections::{HashMap, HashSet, VecDeque};

use aether_bloomery_driver::{CallerId, Command, LoadOutcome, ProgramCore};
use aether_bloomery_kinds::{
    Activated, ActivationRejected, AppendRecords, AppendRecordsResult, Call, CallOutcome, ClosureArtifact,
    ClosureLimit, Digest, DriverRecord, EncodedArtifact, Fault, FaultReason, Head, Invoke, Invoked, JournalEntry,
    NativeOrigin, OpaqueBytes, ProgramName, ProgramRef, ReactionFailed, ReadArtifact, ReadArtifactResult, ReadClosure,
    ReadClosureResult, ReadEvents, ReadEventsResult, RecordedHead, RecordedHeadMove, RequestSource, Requested,
    Transition, artifact_digest,
};
use aether_data::{Kind, KindId, MailboxId, Storage, StorageData};

/// Byte budget every test core starts under: 1 MiB.
pub const LIMIT_BYTES: u64 = 1024 * 1024;

/// Mailbox the scripted loads hand out unless a test overrides them.
pub const ROOT: MailboxId = MailboxId(7);

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
    closures: HashMap<Digest, Vec<ClosureArtifact>>,
    /// Roots whose closure read answers `TooLarge`.
    oversized: HashSet<Digest>,
    /// Scripted load outcomes by bundle digest.
    loads: HashMap<Digest, Result<MailboxId, String>>,
    /// Scripted invoke replies by request seq.
    invokes: HashMap<u64, Invoked>,
    /// Closure budget the core started with, echoed in `TooLarge` replies.
    limit: ClosureLimit,
    /// Answers collected from the core, in arrival order.
    pub answers: Vec<(CallerId, CallOutcome)>,
    /// Abort reason, when the core aborted.
    pub abort: Option<String>,
    /// Every append command the core emitted, including refused ones.
    pub appends: Vec<AppendRecords>,
    /// Bundle digests the core asked to read, in order.
    pub reads_seen: Vec<Digest>,
    /// Closure roots the core asked to read, in order.
    pub closures_seen: Vec<Digest>,
    /// Bundle digests the core asked to load, in order.
    pub loads_seen: Vec<Digest>,
    /// Invokes the core emitted, in order.
    pub invokes_seen: Vec<(MailboxId, Invoke)>,
    /// When set, the next append answers `Conflict` instead of committing.
    pub conflict_next: Option<u64>,
    /// When set, the next append answers `Err` instead of committing.
    pub fail_next: Option<String>,
    /// When set, every journal read answers `Err`.
    pub fail_reads: Option<String>,
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
            reads_seen: Vec::new(),
            closures_seen: Vec::new(),
            loads_seen: Vec::new(),
            invokes_seen: Vec::new(),
            conflict_next: None,
            fail_next: None,
            fail_reads: None,
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

    /// Store one wasm bundle and answer its loads successfully.
    #[must_use]
    pub fn store_bundle(&mut self, wasm: &[u8]) -> Digest {
        let digest = self.store(OpaqueBytes::ID, wasm);
        self.loads.insert(digest, Ok(ROOT));
        digest
    }

    /// Script one input's closure.
    pub fn script_closure(&mut self, root: Digest, artifacts: Vec<ClosureArtifact>) {
        self.closures.insert(root, artifacts);
    }

    /// Script one input's closure as too large.
    pub fn script_oversized(&mut self, root: Digest) {
        self.oversized.insert(root);
    }

    /// Leave one bundle's load unanswered so the test feeds it by hand.
    pub fn hold_load(&mut self, bundle: Digest) {
        self.loads.remove(&bundle);
    }

    /// Script one request's invoke reply.
    pub fn script_invoke(&mut self, seq: u64, invoked: Invoked) {
        self.invokes.insert(seq, invoked);
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
                self.appends.push(request);
                Step::More(self.core.on_appended(ticket, result))
            }
            Command::Load { ticket, bundle, wasm } => {
                self.loads_seen.push(bundle);
                match self.loads.get(&bundle).cloned() {
                    Some(Ok(root)) => Step::More(self.core.on_loaded(ticket, LoadOutcome::Loaded { root })),
                    Some(Err(error)) => Step::More(self.core.on_loaded(ticket, LoadOutcome::Failed { error })),
                    None => Step::Manual(Command::Load { ticket, bundle, wasm }),
                }
            }
            Command::Invoke { ticket, root, request } => {
                self.invokes_seen.push((root, request.clone()));
                match self.invokes.remove(&request.seq()) {
                    Some(invoked) => Step::More(self.core.on_invoked(ticket, invoked)),
                    None => Step::Manual(Command::Invoke { ticket, root, request }),
                }
            }
            Command::Answer { caller, outcome } => {
                self.answers.push((caller, outcome));
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

/// One validated program name.
///
/// # Panics
///
/// Panics if the name breaks [`ProgramName`] rules.
#[must_use]
pub fn program_name(name: &str) -> ProgramName {
    ProgramName::new(name).expect("valid test program name")
}

/// One validated native origin.
///
/// # Panics
///
/// Panics if the name breaks [`NativeOrigin`] rules.
#[must_use]
pub fn origin(name: &str) -> NativeOrigin {
    NativeOrigin::new(name).expect("valid test origin")
}

/// One native call.
#[must_use]
pub fn call(program: &'static str, name: &str, input: Digest, origin: &str, key: u64) -> Call {
    Call { program: program_head(program), name: program_name(name), input, origin: self::origin(origin), key }
}

/// One recorded request over a pinned bundle.
#[must_use]
pub fn requested(bundle: Digest, name: &str, input: Digest, origin: &str, key: u64) -> Requested {
    Requested {
        program: ProgramRef::new(bundle, program_name(name)),
        input,
        source: RequestSource::Native { origin: self::origin(origin), key },
    }
}

/// One recorded execution.
#[must_use]
pub fn transition(bundle: Digest, name: &str, input: Digest, result: Digest) -> Transition {
    Transition { program: ProgramRef::new(bundle, program_name(name)), input, result }
}

/// One recorded fault.
#[must_use]
pub fn fault(bundle: Digest, name: &str, input: Digest, reason: FaultReason) -> Fault {
    Fault { program: ProgramRef::new(bundle, program_name(name)), input, reason }
}
