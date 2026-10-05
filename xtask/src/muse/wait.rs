//! `muse wait`: follow one session from a boundary to its next rest, listing
//! each turn.
//!
//! The follow reads events after `--after` and blocks on the journal head
//! whenever it has read them all. An entry belongs to the session when its
//! cause chain reaches the session's open run (whose seq is the session key)
//! or a continue run naming the session, so many sessions on one journal are
//! told apart without a view of their own. The follow stops at the move of
//! the session's head, the step after its `muse.session.record` run, so a
//! `continue` sent after `wait` returns always finds that record.
//!
//! A fault or failed reaction in the session's chain is reported after the
//! rest it led to, and `wait` exits non-zero naming it. A second one in the
//! same chain, or a failed head move, means the session cannot record itself
//! (its failed record failed too), so the follow stops there instead of
//! waiting for a rest that will not come. A tool run that ran out of time or
//! memory is neither: the loop runs it again or answers the call saying so,
//! and the session goes on.

use std::collections::HashSet;

use aether_bloomery_kinds::{Fault, JournalEntry, ReactionFailed, RecordedHead, RecordedHeadMove, Transition};
use aether_bloomery_muse::{
    ContinueInput, Exhaustion, Failure, MuseTurn, OpenInput, RestReason, Role, Session, SessionContinue,
    SessionExhausted, SessionGate, SessionKey, SessionOpen, SessionRecord, TurnItem, TurnOutcome, TurnResult,
    TurnUsage,
};
use aether_bloomery_program::Program;
use aether_data::{Digest, Kind, Utf8Text};
use anyhow::{Result, bail};
use clap::Args;

use super::EngineArgs;
use crate::bloomery::{Reads, decode_entry, load, page, read_each, read_value, wait_past};

/// Arguments for `cargo xtask muse wait`.
#[derive(Args, Debug)]
pub(super) struct WaitArgs {
    #[command(flatten)]
    engine: EngineArgs,
    /// The session to follow: the key `open` printed.
    #[arg(long)]
    session: u64,
    /// The boundary to read from: the `after=` `open` or `continue` printed.
    #[arg(long)]
    after: u64,
}

/// Follow the session to its next rest and print it.
pub(super) fn run(args: &WaitArgs) -> Result<()> {
    let mut engine = args.engine.connect()?;
    let rested = follow(&mut engine, SessionKey::new(args.session), args.after)?;

    for turn in &rested.turns {
        let line = turn.usage.map_or_else(
            || format!("unreported turn={}", turn.seq),
            |usage| {
                format!(
                    "turn={} input={} cached={} output={} reasoning={}",
                    turn.seq, usage.input, usage.cached, usage.output, usage.reasoning
                )
            },
        );
        println!("{line}");
    }
    println!("rested {} turns={}", reason(rested.session.rested()), rested.turns.len());
    let from = rested.from.map_or_else(|| "unknown".to_owned(), |from| from.to_string());
    println!("from={from} to={}", rested.session.tree().digest());
    let usage = rested.usage;
    println!(
        "usage input={} cached={} output={} reasoning={}",
        usage.input, usage.cached, usage.output, usage.reasoning
    );
    if let (
        RestReason::Completed | RestReason::Blocked | RestReason::Asked,
        Some(TurnItem::Message { role: Role::Assistant, text }),
    ) = (rested.session.rested(), rested.session.items().last())
    {
        println!("{}", text_of(&mut engine, text.digest())?);
    }

    if !rested.faults.is_empty() {
        bail!("session {}'s chain recorded {}", args.session, rested.faults.join("; "));
    }
    Ok(())
}

/// A session at its next rest, and what its activation reported on the way.
pub(super) struct Rested {
    /// The record the session's head moved to.
    pub(super) session: Session,
    /// The tree the activation started from, when its open or continue run
    /// was read; `None` when the follow began after it.
    pub(super) from: Option<Digest>,
    /// The `muse.turn` runs of the session read before the rest, in journal
    /// order.
    pub(super) turns: Vec<Turn>,
    /// The usage those turns reported, summed.
    pub(super) usage: Usage,
    /// Each fault or failed reaction in the session's chain.
    pub(super) faults: Vec<String>,
}

/// One `muse.turn` run the follow read, and the usage its outcome reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Turn {
    /// The run's journal seq.
    pub(super) seq: u64,
    /// The usage the turn reported, when it reported any.
    pub(super) usage: Option<Usage>,
}

/// Token counts summed over a session's turns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Usage {
    pub(super) input: u64,
    pub(super) cached: u64,
    pub(super) output: u64,
    pub(super) reasoning: u64,
}

impl From<&TurnUsage> for Usage {
    fn from(usage: &TurnUsage) -> Self {
        Self {
            input: usage.input_tokens(),
            cached: usage.cached_input_tokens(),
            output: usage.output_tokens(),
            reasoning: usage.reasoning_tokens(),
        }
    }
}

impl Usage {
    fn add(&mut self, usage: Self) {
        self.input += usage.input;
        self.cached += usage.cached;
        self.output += usage.output;
        self.reasoning += usage.reasoning;
    }
}

/// Follow session `key` from the boundary `after` to the next move of its
/// head, reading and waiting through `reads`.
///
/// # Errors
/// A read failed, an entry or artifact did not decode, or the session's
/// chain failed twice or failed to move its head, so it cannot rest.
pub(super) fn follow(reads: &mut impl Reads, key: SessionKey, after: u64) -> Result<Rested> {
    let mut chain = Chain::new(key);
    let mut cursor = after;
    loop {
        let (head, entries) = page(reads, cursor)?;
        for entry in &entries {
            if let Some(session) = chain.fold(reads, entry)? {
                return Ok(chain.rested(session));
            }
        }

        cursor = entries.last().map_or(cursor, |entry| entry.seq);
        if cursor >= head {
            wait_past(reads, cursor)?;
        }
    }
}

/// What the follow has learned about one session so far.
struct Chain {
    key: SessionKey,
    head: RecordedHead,
    /// Every entry seq whose cause chain reaches the session's open or
    /// continue run.
    linked: HashSet<u64>,
    /// The seqs of the session's `muse.session.record` runs.
    records: HashSet<u64>,
    from: Option<Digest>,
    turns: Vec<Turn>,
    usage: Usage,
    faults: Vec<String>,
}

impl Chain {
    fn new(key: SessionKey) -> Self {
        Self {
            key,
            head: RecordedHead::from(&key.head()),
            linked: HashSet::new(),
            records: HashSet::new(),
            from: None,
            turns: Vec::new(),
            usage: Usage::default(),
            faults: Vec::new(),
        }
    }

    fn rested(self, session: Session) -> Rested {
        Rested { session, from: self.from, turns: self.turns, usage: self.usage, faults: self.faults }
    }

    /// Fold `entry` into the chain, and return the record the session's
    /// head moved to when `entry` is that move.
    fn fold(&mut self, reads: &mut impl Reads, entry: &JournalEntry) -> Result<Option<Session>> {
        if let Some(moved) = decode_entry::<RecordedHeadMove>(entry)? {
            return if *moved.head() == self.head {
                read_value::<Session>(reads, moved.to()).map(Some)
            } else {
                Ok(None)
            };
        }
        if let Some(run) = decode_entry::<Transition>(entry)? {
            self.ran(reads, entry, &run)?;
            return Ok(None);
        }

        if !self.link(entry) {
            return Ok(None);
        }
        if let Some(fault) = decode_entry::<Fault>(entry)? {
            let retried = retried(&fault);
            if !retried {
                let program = fault.program.name().as_str();
                self.fail(format!("{program} faulted at seq {}: {:?}", entry.seq, fault.reason))?;
            }
        } else if let Some(failure) = decode_entry::<ReactionFailed>(entry)? {
            let reactor = failure.reactor.as_ref().map_or("the bundle", |reactor| reactor.as_str());
            let message = format!("{reactor} failed a reaction at seq {}: {}", entry.seq, failure.reason.as_str());
            if entry.cause.is_some_and(|cause| self.records.contains(&cause)) {
                bail!("session {} cannot rest: its head move failed: {message}", self.key.get());
            }
            self.fail(message)?;
        }
        Ok(None)
    }

    /// Fold one program run: an open or continue of this session starts its
    /// chain, and a linked turn adds its usage.
    fn ran(&mut self, reads: &mut impl Reads, entry: &JournalEntry, run: &Transition) -> Result<()> {
        let name = run.program.name().as_str();
        if name == SessionOpen::NAME {
            if entry.seq == self.key.get() {
                self.start(entry.seq, read_value::<OpenInput>(reads, run.input)?.tree().digest());
            }
        } else if name == SessionContinue::NAME {
            let input = read_value::<ContinueInput>(reads, run.input)?;
            if input.session() == self.key {
                self.start(entry.seq, read_value::<Session>(reads, input.from().digest())?.tree().digest());
            }
        } else if self.link(entry) {
            if name == MuseTurn::NAME {
                let usage = usage(read_value::<TurnResult>(reads, run.result)?.outcome()).map(Usage::from);
                if let Some(usage) = usage {
                    self.usage.add(usage);
                }
                self.turns.push(Turn { seq: entry.seq, usage });
            } else if name == SessionRecord::NAME {
                self.records.insert(entry.seq);
            }
        }
        Ok(())
    }

    /// The run at `seq` starts an activation of this session on `tree`. The
    /// first one read is the activation the next rest ends.
    fn start(&mut self, seq: u64, tree: Digest) {
        self.linked.insert(seq);
        self.from.get_or_insert(tree);
    }

    /// Link `entry` to the session when its cause is linked.
    fn link(&mut self, entry: &JournalEntry) -> bool {
        let linked = entry.cause.is_some_and(|cause| self.linked.contains(&cause));
        if linked {
            self.linked.insert(entry.seq);
        }
        linked
    }

    /// Record one failure in the chain; a second means the failed record
    /// failed too, and the session is dropped without resting.
    fn fail(&mut self, message: String) -> Result<()> {
        self.faults.push(message);
        if self.faults.len() > 1 {
            bail!("session {} cannot rest: {}", self.key.get(), self.faults.join("; "));
        }
        Ok(())
    }
}

/// Whether the loop retries or answers `fault` and the session goes on: a run
/// of a tool, never one of the loop's own programs, that ran out of time or
/// memory.
fn retried(fault: &Fault) -> bool {
    let program = fault.program.name().as_str();
    let loops_own = [MuseTurn::NAME, SessionRecord::NAME, SessionExhausted::NAME, SessionGate::NAME].contains(&program);
    let exhausted = Exhaustion::of(&fault.reason).is_some();
    exhausted && !loops_own
}

/// The usage a turn's outcome reported, when it reported any.
const fn usage(outcome: &TurnOutcome) -> Option<&TurnUsage> {
    match outcome {
        TurnOutcome::Completed { usage, .. }
        | TurnOutcome::Called { usage, .. }
        | TurnOutcome::Incomplete { usage, .. }
        | TurnOutcome::Declined { usage, .. } => Some(usage),
        TurnOutcome::Rejected | TurnOutcome::Transient { .. } | TurnOutcome::Unreadable => None,
    }
}

/// Why a session rests, in one phrase.
fn reason(rested: &RestReason) -> String {
    match rested {
        RestReason::Completed => "completed".to_owned(),
        RestReason::Blocked => "blocked".to_owned(),
        RestReason::Asked => "asked".to_owned(),
        RestReason::Declined => "declined".to_owned(),
        RestReason::Incomplete => "incomplete".to_owned(),
        RestReason::TurnLimit => "turn-limit".to_owned(),
        RestReason::ContextFull => "context-full".to_owned(),
        RestReason::Failed(Failure::Faulted { program, reason }) => {
            format!("failed: {} faulted: {reason:?}", program.as_str())
        }
        RestReason::Failed(Failure::Reaction { reason }) => format!("failed: a rule failed: {}", reason.as_str()),
        RestReason::Failed(Failure::Turn { result }) => format!("failed: the vendor ended turn {}", result.digest()),
        RestReason::Failed(Failure::Unbuilt { reason }) => format!("failed: unbuilt: {}", reason.as_str()),
    }
}

/// The text the artifact `digest` holds.
fn text_of(reads: &mut impl Reads, digest: Digest) -> Result<String> {
    let mut text = String::new();
    read_each(reads, &[digest], |digest, artifact| {
        if artifact.kind() != Utf8Text::ID {
            bail!("artifact {digest} is not text");
        }
        text = String::from_utf8(load(artifact, digest)?)?;
        Ok(())
    })?;
    Ok(text)
}
