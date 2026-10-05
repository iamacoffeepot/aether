//! The export walk, the wait's session attribution, and the call retry walk:
//! the first two read a scratch journal through the same [`Reads`] seam the
//! engine connection implements, and the last drives `settle` without one.

use std::collections::BTreeMap;
use std::fs;
use std::iter::once;
use std::path::{Path as HostPath, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{env, process};

use aether_bloomery_journal::{Batch, Journal};
use aether_bloomery_kinds::{
    Activated, ActivationRejected, CallOutcome, ClosureArtifact, Detail, EncodedArtifact, Fault, FaultReason,
    JournalEntry, Name, NativeOrigin, Node, Path, ProgramName, ProgramRef, ReactorName, ReactorSet, ReadArtifacts,
    ReadArtifactsResult, ReadEvents, ReadEventsResult, RecordedHead, RecordedHeadMove, RequestSource, Requested,
    RuleName, Seq, Transition, Tree, WatchHead, WatchHeadResult,
};
use aether_bloomery_muse::{
    ContinueInput, Echo, Endpoint, InputLimit, MUSE, ModelName, MuseTurn, OfferedTools, OpenInput, OutputBudget,
    ReasoningEffort, RequiredProofs, Session, SessionContinue, SessionKey, SessionOpen, SessionRecord, TurnLimit,
    TurnResult, TurnSettings,
};
use aether_bloomery_program::Program;
use aether_bloomery_workspace::EnvVar;
use aether_bloomery_workspace_programs::WORKSPACE_PROGRAMS;
use aether_codec::encode_storage_schema;
use aether_data::{Cites, Digest, Ref, Schema, Storage};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::activation::{MuseActivation, muse_activation, verdict_after};
use super::bind_programs::programs_publish;
use super::call::{MAX_ATTEMPTS, settle};
use super::export::{Action, Change, export};
use super::open::parse_test_env;
use super::wait::{Usage, follow};
use crate::bloomery::Reads;

/// A directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Result<Self> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("xtask-muse-{label}-{}-{unique}", process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    fn path(&self) -> &HostPath {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Answers each read exactly as the journal actor's handlers do. A watch
/// past the head refuses rather than parks, since nothing else appends.
struct JournalReads(Journal);

impl Reads for JournalReads {
    fn read_events(&mut self, request: ReadEvents) -> Result<ReadEventsResult> {
        let entries = self.0.read_cited(Seq(request.after), usize::try_from(request.limit)?)?;
        Ok(ReadEventsResult::Ok {
            after: request.after,
            head: self.0.head()?.0,
            entries: entries.into_iter().map(|(entry, cites)| JournalEntry::from_entry(&entry, cites)).collect(),
        })
    }

    fn watch_head(&mut self, request: WatchHead) -> Result<WatchHeadResult> {
        let head = self.0.head()?.0;
        if head <= request.after {
            bail!("the scratch journal has nothing past {}", request.after);
        }
        Ok(WatchHeadResult::Advanced { head })
    }

    fn read_artifacts(&mut self, request: &ReadArtifacts) -> Result<ReadArtifactsResult> {
        let mut artifacts = Vec::new();
        for digest in request.digests.as_slice() {
            match self.0.get_bytes(digest)? {
                Some((kind, payload)) => artifacts.push(ClosureArtifact::new(kind, payload)),
                None => return Ok(ReadArtifactsResult::Missing { digest: *digest }),
            }
        }
        Ok(ReadArtifactsResult::Found { artifacts })
    }
}

impl JournalReads {
    /// Append `batch`'s artifacts and events at the head.
    fn commit(&mut self, batch: &Batch) -> Result<()> {
        let head = self.0.head()?;
        self.0.append(head, batch)?;
        Ok(())
    }

    /// Append `event` alone under `cause`, and return its seq.
    fn push<K: Storage + Clone + Cites>(&mut self, event: &K, cause: Option<u64>) -> Result<u64> {
        let mut batch = Batch::new();
        batch.push_event(event, cause.map(Seq))?;
        let head = self.0.head()?;
        Ok(self.0.append(head, &batch)?.start.0)
    }

    /// Record a run of `P` over `input` that the session loop's rule
    /// triggered at `cause` requested and that faulted with `reason`; return
    /// the fault's seq.
    fn fault<P: Program>(&mut self, input: Digest, reason: FaultReason, cause: u64) -> Result<u64> {
        let program = ProgramRef::new(BUNDLE, ProgramName::new(P::NAME)?);
        let source = RequestSource::Reaction {
            bundle: BUNDLE,
            reactor: ReactorName::new("muse.session")?,
            rule: RuleName::new("call")?,
            ordinal: 0,
        };
        let requested = self.push(&Requested { program: program.clone(), input, source }, Some(cause))?;
        self.push(&Fault { program, input, reason }, Some(requested))
    }

    /// Record a run of `P` over `input` with `result`, requested natively
    /// for no `cause`, or by the session loop's rule triggered at `cause`;
    /// return the run's seq.
    fn run<P: Program>(&mut self, input: Digest, result: Digest, cause: Option<u64>) -> Result<u64> {
        let program = ProgramRef::new(BUNDLE, ProgramName::new(P::NAME)?);
        let source = match cause {
            None => RequestSource::Native { origin: NativeOrigin::new("test.muse")?, key: input.as_bytes()[0].into() },
            Some(_) => RequestSource::Reaction {
                bundle: BUNDLE,
                reactor: ReactorName::new("muse.session")?,
                rule: RuleName::new("call")?,
                ordinal: 0,
            },
        };
        let requested = self.push(&Requested { program: program.clone(), input, source }, cause)?;
        self.push(&Transition { program, input, result }, Some(requested))
    }
}

/// The digest every recorded run names as its bundle; nothing reads it.
const BUNDLE: Digest = Digest::from_bytes([0xb0; 32]);

/// A journal opened under a scratch root, removed with it.
fn scratch_journal() -> Result<(Scratch, JournalReads)> {
    let root = Scratch::new("journal")?;
    let journal = Journal::open(&root.path().join("journal"))?;
    Ok((root, JournalReads(journal)))
}

/// A tree of `entries`, staged in `batch`.
fn tree(batch: &mut Batch, entries: impl IntoIterator<Item = (&'static str, Node)>) -> Result<Ref<Tree>> {
    let entries = entries.into_iter().map(|(name, node)| Ok((Name::new(name)?, node))).collect::<Result<_>>()?;
    Ok(batch.stage_encoded(&Tree::new(entries))?)
}

#[test]
fn an_export_writes_exactly_the_added_flipped_and_deleted_files_in_path_order() -> Result<()> {
    // Catches a write-back that drops or misplaces an entry, loses an execute bit, leaves a deleted file behind,
    // or reports its changes out of path order.
    let (_root, mut reads) = scratch_journal()?;
    let mut batch = Batch::new();
    let [keep, script, gone, added] =
        [&b"keep\n"[..], b"#!/bin/sh\n", b"gone\n", b"added\n"].map(|bytes| batch.stage_bytes(bytes));
    let from_deep = tree(
        &mut batch,
        [("keep.txt", Node::File(keep)), ("run.sh", Node::File(script)), ("gone.txt", Node::File(gone))],
    )?;
    let to_deep = tree(
        &mut batch,
        [("keep.txt", Node::File(keep)), ("run.sh", Node::Executable(script)), ("new.txt", Node::File(added))],
    )?;
    let from_src = tree(&mut batch, [("deep", Node::Directory(from_deep))])?;
    let to_src = tree(&mut batch, [("deep", Node::Directory(to_deep))])?;
    let from = tree(&mut batch, [("src", Node::Directory(from_src)), ("top.txt", Node::File(keep))])?.digest();
    let to = tree(&mut batch, [("src", Node::Directory(to_src)), ("top.txt", Node::File(keep))])?.digest();
    let empty = batch.stage_encoded(&Tree::empty())?.digest();
    reads.commit(&batch)?;

    let into = Scratch::new("export")?;
    export(&mut reads, empty, from, into.path())?;
    let changes = export(&mut reads, from, to, into.path())?;

    let change = |path: &str, action| Change { path: path.to_owned(), action };
    assert_eq!(
        changes,
        [
            change("src/deep/gone.txt", Action::Deleted),
            change("src/deep/new.txt", Action::Added),
            change("src/deep/run.sh", Action::Modified),
        ]
    );
    let deep = into.path().join("src/deep");
    assert_eq!(fs::read(deep.join("new.txt"))?, b"added\n");
    assert_eq!(fs::read(deep.join("keep.txt"))?, b"keep\n");
    assert_eq!(fs::read(into.path().join("top.txt"))?, b"keep\n");
    assert!(!deep.join("gone.txt").exists(), "the deleted file is removed");
    assert_executable(&deep.join("run.sh"))?;
    Ok(())
}

#[cfg(unix)]
fn assert_executable(path: &HostPath) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    assert_ne!(fs::metadata(path)?.permissions().mode() & 0o111, 0, "{} is executable", path.display());
    Ok(())
}

#[cfg(not(unix))]
fn assert_executable(_path: &HostPath) -> Result<()> {
    Ok(())
}

/// `value`, written as JSON in `K`'s storage shape, as the artifact the
/// journal stores: a session record and a turn result have no constructor
/// outside their crate, and the decode here checks the fixture is a `K`.
fn stored<K: Storage + Schema>(value: &Value) -> Result<EncodedArtifact> {
    let payload = encode_storage_schema(value, &K::SCHEMA)?;
    K::decode_storage(&payload).map_err(|error| anyhow!("the {} fixture: {error}", K::NAME))?;
    Ok(EncodedArtifact::uncited(K::ID, &payload))
}

/// A digest as the JSON of a reference.
fn reference(digest: Digest) -> Value {
    json!(digest.as_bytes().to_vec())
}

/// A turn that completed after reporting `usage`.
fn turn(batch: &mut Batch, usage: Usage) -> Result<Digest> {
    let outcome = json!({ "Completed": {
        "reasoning": [],
        "text": reference(Ref::of_text("done").digest()),
        "usage": {
            "input_tokens": usage.input,
            "cached_input_tokens": usage.cached,
            "output_tokens": usage.output,
            "reasoning_tokens": usage.reasoning,
        },
    }});
    let body = reference(Ref::of_bytes(b"{}").digest());
    let result = json!({ "reply": { "Received": { "status": 200, "body": body, "outcome": outcome } } });
    Ok(batch.stage_artifact(stored::<TurnResult>(&result)?))
}

/// A turn the vendor answered without usage.
fn unreported_turn(batch: &mut Batch) -> Result<Digest> {
    let body = reference(Ref::of_bytes(b"{}").digest());
    let result = json!({ "reply": { "Received": { "status": 200, "body": body, "outcome": "Rejected" } } });
    Ok(batch.stage_artifact(stored::<TurnResult>(&result)?))
}

/// A session that completed on `tree`.
fn session(batch: &mut Batch, tree: Ref<Tree>) -> Result<Digest> {
    let message =
        |role: &str, text: &str| json!({ "Message": { "role": role, "text": reference(Ref::of_text(text).digest()) } });
    let value = json!({
        "settings": {
            "endpoint": "https://example.test/v1/responses",
            "model": "muse",
            "tools": [],
            "max_output_tokens": 512,
            "reasoning": "Low",
            "input_limit": 1_048_576,
        },
        "items": [message("User", "hi"), message("Assistant", "done")],
        "rested": "Completed",
        "tree": reference(tree.digest()),
    });
    Ok(batch.stage_artifact(stored::<Session>(&value)?))
}

/// A tree holding one symlink to `target`, so trees differ without blobs.
fn marker(batch: &mut Batch, target: &str) -> Result<Ref<Tree>> {
    let entries = BTreeMap::from([(Name::new("marker")?, Node::Symlink(Path::new(target)?))]);
    Ok(batch.stage_encoded(&Tree::new(entries))?)
}

const fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Usage {
    Usage { input, cached, output, reasoning }
}

#[test]
fn test_env_splits_each_value_at_its_first_equals_and_refuses_a_repeated_key() {
    // Catches a `KEY=VALUE` split at the last `=`, a value without one accepted, and two values for one
    // variable passed to the bound.
    let env = parse_test_env(&["AETHER_A=1", "AETHER_B=x=y"]).expect("split at the first `=`");
    let want = [EnvVar::new("AETHER_A", "1").expect("variable"), EnvVar::new("AETHER_B", "x=y").expect("variable")];
    assert_eq!(env.as_slice(), want);

    let repeated = parse_test_env(&["AETHER_A=1", "AETHER_A=2"]).expect_err("a repeated key is refused");
    assert!(repeated.to_string().contains("AETHER_A"), "the error names the key: {repeated}");
    parse_test_env(&["AETHER_A"]).expect_err("a value without `=` is refused");
}

#[test]
fn a_wait_sums_only_its_own_sessions_turns_and_stops_at_its_own_rest() -> Result<()> {
    // Catches a summary that mixes two interleaved sessions' turns, one that reads past its own rest into a later
    // activation, a continue attributed to the wrong session, and a tool run the loop retried reported as a fault.
    let (_root, mut reads) = scratch_journal()?;
    let mut batch = Batch::new();
    let (a0, a1) = (marker(&mut batch, "a0")?, marker(&mut batch, "a1")?);
    let (b0, b1, b2) = (marker(&mut batch, "b0")?, marker(&mut batch, "b1")?, marker(&mut batch, "b2")?);
    let settings = TurnSettings::new(
        Endpoint::new("https://example.test/v1/responses")?,
        ModelName::new("muse")?,
        OfferedTools::default(),
        OutputBudget::new(512)?,
        ReasoningEffort::Low,
        InputLimit::new(u64::MAX)?,
    );
    let user = batch.stage_text("hi");
    let instructions = batch.stage_text("rules");
    let open_a = batch.stage_encoded(&OpenInput::new(
        settings.clone(),
        instructions,
        user,
        TurnLimit::new(4)?,
        a0,
        Vec::new(),
        RequiredProofs::default(),
    ))?;
    let open_b = batch.stage_encoded(&OpenInput::new(
        settings,
        instructions,
        user,
        TurnLimit::new(4)?,
        b0,
        Vec::new(),
        RequiredProofs::default(),
    ))?;
    let (session_a, session_b, session_b2) =
        (session(&mut batch, a1)?, session(&mut batch, b1)?, session(&mut batch, b2)?);
    let a_called = turn(&mut batch, usage(10, 1, 100, 5))?;
    let a_rejected = unreported_turn(&mut batch)?;
    let a_completed = turn(&mut batch, usage(20, 2, 200, 6))?;
    let b_completed = turn(&mut batch, usage(1_000, 100, 10_000, 500))?;
    let b_resumed = turn(&mut batch, usage(30, 3, 300, 7))?;
    reads.commit(&batch)?;

    let opened = Digest::from_bytes([1; 32]);
    let key_a = reads.run::<SessionOpen>(open_a.digest(), opened, None)?;
    let key_b = reads.run::<SessionOpen>(open_b.digest(), opened, None)?;
    let (a, b) = (SessionKey::new(key_a), SessionKey::new(key_b));
    let sent = Digest::from_bytes([2; 32]);
    let b_turn = reads.run::<MuseTurn>(sent, b_completed, Some(key_b))?;
    let a_first = reads.run::<MuseTurn>(sent, a_called, Some(key_a))?;
    let a_middle = reads.run::<MuseTurn>(sent, a_rejected, Some(a_first))?;
    reads.fault::<Echo>(sent, FaultReason::TimedOut, a_middle)?;
    let a_last = reads.run::<MuseTurn>(sent, a_completed, Some(a_middle))?;
    let b_record = reads.run::<SessionRecord>(sent, session_b, Some(b_turn))?;
    let b_rested = reads.push(&b.head().move_to(Ref::from_digest(session_b)), Some(b_record))?;
    let a_record = reads.run::<SessionRecord>(sent, session_a, Some(a_last))?;
    reads.push(&a.head().move_to(Ref::from_digest(session_a)), Some(a_record))?;

    let mut batch = Batch::new();
    let resume = ContinueInput::new(b, Ref::from_digest(session_b), None, None, TurnLimit::new(1)?);
    let resume = batch.stage_encoded(&resume)?;
    reads.commit(&batch)?;
    let continued = reads.run::<SessionContinue>(resume.digest(), sent, None)?;
    let b_resumed_turn = reads.run::<MuseTurn>(sent, b_resumed, Some(continued))?;
    let b_record = reads.run::<SessionRecord>(sent, session_b2, Some(b_resumed_turn))?;
    reads.push(&b.head().move_to(Ref::from_digest(session_b2)), Some(b_record))?;

    let rested = follow(&mut reads, a, 0)?;
    let usages = rested.turns.iter().map(|turn| turn.usage).collect::<Vec<_>>();
    assert_eq!(usages, [Some(usage(10, 1, 100, 5)), None, Some(usage(20, 2, 200, 6))]);
    let seqs = rested.turns.iter().map(|turn| turn.seq).collect::<Vec<_>>();
    assert_eq!(seqs, [a_first, a_middle, a_last]);
    assert_eq!(rested.usage, usage(30, 3, 300, 11));
    assert!(rested.faults.is_empty(), "a tool run the loop retries is no fault of the session");
    assert_eq!((rested.from, rested.session.tree()), (Some(a0.digest()), a1));

    let rested = follow(&mut reads, b, 0)?;
    let usages = rested.turns.iter().map(|turn| turn.usage).collect::<Vec<_>>();
    assert_eq!(usages, [Some(usage(1_000, 100, 10_000, 500))], "B stops at its first rest");
    assert_eq!(rested.usage, usage(1_000, 100, 10_000, 500));
    assert_eq!((rested.from, rested.session.tree()), (Some(b0.digest()), b1));

    let rested = follow(&mut reads, b, b_rested)?;
    let usages = rested.turns.iter().map(|turn| turn.usage).collect::<Vec<_>>();
    assert_eq!(usages, [Some(usage(30, 3, 300, 7))], "the continue's turns are B's");
    assert_eq!(rested.usage, usage(30, 3, 300, 7));
    assert_eq!((rested.from, rested.session.tree()), (Some(b1.digest()), b2));
    assert!(rested.faults.is_empty());
    Ok(())
}

/// The muse head brought live for `BUNDLE` from `live_from`, at `cause`.
fn activated(reads: &mut JournalReads, live_from: u64, cause: Option<u64>) -> Result<u64> {
    reads.push(&Activated::new(MUSE, BUNDLE, Seq(live_from))?, cause)
}

/// The muse head's activation rejected for `reason`, at `cause`.
fn rejected(reads: &mut JournalReads, reason: &str, cause: u64) -> Result<u64> {
    reads.push(&ActivationRejected { head: MUSE, bundle: BUNDLE, reason: Detail::new(reason) }, Some(cause))
}

#[test]
fn an_owed_muse_head_refuses_with_the_rejection_reason_and_the_remedy() -> Result<()> {
    // Catches a fold that keeps reporting the earlier activation as live, or loses the reason the driver recorded.
    let (_root, mut reads) = scratch_journal()?;
    let first = activated(&mut reads, 1, None)?;
    let owed = rejected(&mut reads, "cannot decode session 7", first)?;

    let state = muse_activation(&mut reads)?;
    let error = state.refuse_unless_live("primary").expect_err("not live").to_string();

    assert!(matches!(state, MuseActivation::Rejected { seq, .. } if seq == owed));
    assert!(error.contains("cannot decode session 7"), "{error}");
    assert!(error.contains(&format!("entry {owed}")), "{error}");
    assert!(error.contains("--bloomery-units primary=<dir>"), "{error}");
    Ok(())
}

#[test]
fn a_reactivated_muse_head_is_live_again() -> Result<()> {
    // Catches a reader that latches the last rejection after the driver brought the reactor live again.
    let (_root, mut reads) = scratch_journal()?;
    let first = activated(&mut reads, 1, None)?;
    let owed = rejected(&mut reads, "cannot decode session 7", first)?;
    activated(&mut reads, owed, Some(owed))?;

    let state = muse_activation(&mut reads)?;

    assert_eq!(state, MuseActivation::Live { bundle: BUNDLE });
    state.refuse_unless_live("primary")
}

#[test]
fn the_verdict_after_a_fence_ignores_an_earlier_rejection() -> Result<()> {
    // Catches a bind that fails on a rejection recorded before its own publish, after a rebind that came live.
    let (_root, mut reads) = scratch_journal()?;
    let first = activated(&mut reads, 1, None)?;
    let owed = rejected(&mut reads, "cannot decode session 7", first)?;
    activated(&mut reads, owed, Some(owed))?;

    let verdict = verdict_after(&mut reads, first)?;

    assert_eq!(verdict, MuseActivation::Live { bundle: BUNDLE });
    Ok(())
}

#[test]
fn a_journal_never_bound_refuses_naming_bind() -> Result<()> {
    // Catches an open that proceeds, or blames a rejection, on a journal where the muse head was never activated.
    let (_root, mut reads) = scratch_journal()?;

    let state = muse_activation(&mut reads)?;
    let error = state.refuse_unless_live("primary").expect_err("not live").to_string();

    assert_eq!(state, MuseActivation::Never);
    assert!(error.contains("muse bind"), "{error}");
    Ok(())
}

/// Store `artifact` and move `head` to it in one batch, and return its digest.
fn bound(reads: &mut JournalReads, head: RecordedHead, artifact: EncodedArtifact) -> Result<Digest> {
    let mut batch = Batch::new();
    let digest = batch.stage_artifact(artifact);
    batch.push_event(&RecordedHeadMove::new(head, digest), None)?;
    reads.commit(&batch)?;
    Ok(digest)
}

#[test]
fn binding_programs_moves_only_their_head_and_is_unchanged_on_a_rebind() -> Result<()> {
    // Catches a bind that rewrites the muse head or the reactor set, or one that appends a head move on every rerun.
    let (_root, mut reads) = scratch_journal()?;
    bound(&mut reads, RecordedHead::from(&MUSE), EncodedArtifact::opaque_bytes(b"muse"))?;
    let set = EncodedArtifact::new(&ReactorSet::new(vec![MUSE])?)?;
    bound(&mut reads, RecordedHead::from(&ReactorSet::ROOT), set)?;
    let fence = reads.0.head()?.0;

    let programs = EncodedArtifact::opaque_bytes(b"programs").digest();
    let publish = programs_publish(&mut reads, programs, fence)?.expect("an unbound head is owed a move");
    let moved: Vec<_> = publish.moves().iter().map(|moved| (moved.head().clone(), moved.to())).collect();
    assert_eq!(moved, [(RecordedHead::from(&WORKSPACE_PROGRAMS), programs)]);
    assert_eq!(publish.expected_seq(), fence);

    bound(&mut reads, RecordedHead::from(&WORKSPACE_PROGRAMS), EncodedArtifact::opaque_bytes(b"programs"))?;
    let rebind = programs_publish(&mut reads, programs, fence + 1)?;
    assert!(rebind.is_none(), "a head that names the bundle owes no move");
    Ok(())
}

/// The input the call retry outcomes name.
const TEST_INPUT: Digest = Digest::from_bytes([0x01; 32]);

/// The program the call retry outcomes name.
fn test_program() -> Result<ProgramRef> {
    Ok(ProgramRef::new(BUNDLE, ProgramName::new("test.program")?))
}

/// A fault outcome recorded at `seq`.
fn fault_outcome(seq: u64) -> Result<CallOutcome> {
    Ok(CallOutcome::Fault {
        key: 0,
        seq,
        fault: Fault { program: test_program()?, input: TEST_INPUT, reason: FaultReason::TimedOut },
    })
}

/// A transition outcome recorded at `seq`.
fn transition_outcome(seq: u64) -> Result<CallOutcome> {
    Ok(CallOutcome::Transition {
        key: 0,
        seq,
        transition: Transition { program: test_program()?, input: TEST_INPUT, result: Digest::from_bytes([0x02; 32]) },
    })
}

#[test]
fn a_replayed_fault_is_asked_again_and_returns_the_transition() -> Result<()> {
    // Catches returning the replayed fault instead of asking again under the next key.
    let mut scripted = [fault_outcome(5)?, transition_outcome(12)?].into_iter();
    let mut asked = Vec::new();
    let outcome = settle(10, |attempt| {
        asked.push(attempt);
        Ok(scripted.next().expect("asked past the script"))
    })?;

    assert!(matches!(outcome, CallOutcome::Transition { seq: 12, .. }), "the walk returns the fresh transition");
    assert_eq!(asked, vec![0, 1]);
    Ok(())
}

#[test]
fn a_fresh_fault_returns_without_another_ask() -> Result<()> {
    // Catches re-asking a fresh fault in one command.
    let mut scripted = once(fault_outcome(11)?);
    let mut asked = Vec::new();
    let outcome = settle(10, |attempt| {
        asked.push(attempt);
        Ok(scripted.next().expect("asked past the script"))
    })?;

    assert!(matches!(outcome, CallOutcome::Fault { seq: 11, .. }), "the fresh fault is this command's outcome");
    assert_eq!(asked, vec![0]);
    Ok(())
}

#[test]
fn a_recorded_transition_returns_after_the_first_ask() -> Result<()> {
    // Catches breaking the replay of a recorded success by asking again.
    let mut scripted = once(transition_outcome(5)?);
    let mut asked = Vec::new();
    let outcome = settle(10, |attempt| {
        asked.push(attempt);
        Ok(scripted.next().expect("asked past the script"))
    })?;

    assert!(matches!(outcome, CallOutcome::Transition { seq: 5, .. }), "the recorded transition is replayed");
    assert_eq!(asked, vec![0]);
    Ok(())
}

#[test]
fn replayed_faults_on_every_attempt_fail_after_the_cap() -> Result<()> {
    // Catches an unbounded walk over replayed faults.
    let mut asked = Vec::new();
    let error = settle(10, |attempt| {
        asked.push(attempt);
        fault_outcome(5)
    })
    .expect_err("replayed faults on every attempt fail");

    assert_eq!(asked.len(), usize::try_from(MAX_ATTEMPTS)?);
    assert!(error.to_string().contains(&MAX_ATTEMPTS.to_string()), "the error names the count: {error}");
    Ok(())
}
