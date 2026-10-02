//! The export walk and the wait's session attribution, over a scratch
//! journal read through the same [`Reads`] seam the engine connection
//! implements.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path as HostPath, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{env, process};

use aether_bloomery_journal::{Batch, Journal};
use aether_bloomery_kinds::{
    ClosureArtifact, Digest, EncodedArtifact, Fault, FaultReason, JournalEntry, Name, NativeOrigin, Node, Path,
    ProgramName, ProgramRef, ReactorName, ReadArtifacts, ReadArtifactsResult, ReadEvents, ReadEventsResult, Ref,
    RequestSource, Requested, RuleName, Seq, Transition, Tree, WatchHead, WatchHeadResult,
};
use aether_bloomery_muse::{
    ContinueInput, Echo, Endpoint, InputLimit, ModelName, MuseTurn, OfferedTools, OpenInput, OutputBudget,
    ReasoningEffort, Session, SessionContinue, SessionKey, SessionOpen, SessionRecord, TurnLimit, TurnResult,
    TurnSettings,
};
use aether_bloomery_program::Program;
use aether_bloomery_workspace::EnvVar;
use aether_codec::encode_storage_schema;
use aether_data::{Cites, Schema, Storage};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

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
    ))?;
    let open_b =
        batch.stage_encoded(&OpenInput::new(settings, instructions, user, TurnLimit::new(4)?, b0, Vec::new()))?;
    let (session_a, session_b, session_b2) =
        (session(&mut batch, a1)?, session(&mut batch, b1)?, session(&mut batch, b2)?);
    let a_called = turn(&mut batch, usage(10, 1, 100, 5))?;
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
    let a_turn = reads.run::<MuseTurn>(sent, a_called, Some(key_a))?;
    reads.fault::<Echo>(sent, FaultReason::TimedOut, a_turn)?;
    let a_turn = reads.run::<MuseTurn>(sent, a_completed, Some(a_turn))?;
    let b_record = reads.run::<SessionRecord>(sent, session_b, Some(b_turn))?;
    let b_rested = reads.push(&b.head().move_to(Ref::from_digest(session_b)), Some(b_record))?;
    let a_record = reads.run::<SessionRecord>(sent, session_a, Some(a_turn))?;
    reads.push(&a.head().move_to(Ref::from_digest(session_a)), Some(a_record))?;

    let mut batch = Batch::new();
    let resume = ContinueInput::new(b, Ref::from_digest(session_b), None, None, TurnLimit::new(1)?);
    let resume = batch.stage_encoded(&resume)?;
    reads.commit(&batch)?;
    let continued = reads.run::<SessionContinue>(resume.digest(), sent, None)?;
    let b_turn = reads.run::<MuseTurn>(sent, b_resumed, Some(continued))?;
    let b_record = reads.run::<SessionRecord>(sent, session_b2, Some(b_turn))?;
    reads.push(&b.head().move_to(Ref::from_digest(session_b2)), Some(b_record))?;

    let rested = follow(&mut reads, a, 0)?;
    assert_eq!((rested.turns, rested.usage), (2, usage(30, 3, 300, 11)));
    assert!(rested.faults.is_empty(), "a tool run the loop retries is no fault of the session");
    assert_eq!((rested.from, rested.session.tree()), (Some(a0.digest()), a1));

    let rested = follow(&mut reads, b, 0)?;
    assert_eq!((rested.turns, rested.usage), (1, usage(1_000, 100, 10_000, 500)), "B stops at its first rest");
    assert_eq!((rested.from, rested.session.tree()), (Some(b0.digest()), b1));

    let rested = follow(&mut reads, b, b_rested)?;
    assert_eq!((rested.turns, rested.usage), (1, usage(30, 3, 300, 7)), "the continue's turns are B's");
    assert_eq!((rested.from, rested.session.tree()), (Some(b1.digest()), b2));
    assert!(rested.faults.is_empty());
    Ok(())
}
