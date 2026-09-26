//! Guest `invoke` owns decoding, staging, and the orphan rule.

use std::cell::Cell;
use std::error::Error;

use aether_bloomery_kinds::{
    ClosureArtifact, Detail, Digest, EncodedArtifact, ExecutorFault, Invoke, Invoked, Mode, OpaqueBytes, ProgramName,
    ReadArtifactResult, Ref, Refusal, Tree, Utf8Text, artifact_digest,
};
use aether_bloomery_program::{
    Async, AsyncProgram, AsyncSession, Env, Http, InjectedApi, Pending, PendingCall, PollResult, Process, Program,
    Started, Sync, SyncProgram, Workspace, invoke, start_async,
};
use aether_data::wire::{decode_from_slice, encode_to_vec};
use aether_data::{Cites, Kind, MAX_READ_BYTES, Storage};
use aether_http::{Fetch, FetchResult, HttpMethod};
use aether_process::{Run, RunResult};

fn program_name<P: Program>() -> ProgramName {
    ProgramName::new(P::NAME).expect("valid program name")
}

fn encoded<K: Storage + Clone + Cites>(value: &K) -> Result<EncodedArtifact, Box<dyn Error>> {
    Ok(EncodedArtifact::new(value)?)
}

fn closure_of<K: Storage + Clone + Cites>(value: &K) -> Result<ClosureArtifact, Box<dyn Error>> {
    let encoded = encoded(value)?;
    Ok(ClosureArtifact::new(encoded.kind(), encoded.bytes().to_vec()))
}

fn send<P: SyncProgram>(input: Digest, closure: Vec<ClosureArtifact>) -> Invoked {
    invoke::<P>(Invoke::new(7, program_name::<P>(), input, closure))
}

/// `artifact` decoded again after one byte of its claimed digest was flipped: the member a
/// sender that lies about its digest carries. Its bytes are unchanged, so they still decode.
fn with_altered_claim(artifact: &ClosureArtifact) -> Result<ClosureArtifact, Box<dyn Error>> {
    let mut bytes = encode_to_vec(artifact)?;
    bytes[0] ^= 0x01;
    let altered: ClosureArtifact = decode_from_slice(&bytes)?;
    assert_ne!(altered.claimed(), artifact.claimed(), "the flipped byte is the claim's");
    Ok(altered)
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.invoke_child")]
struct Child {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.invoke_input")]
struct CiteInput {
    child: Ref<Child>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.invoke_result")]
struct CiteResult {
    text: Ref<Utf8Text>,
}

struct Cite;

impl Program for Cite {
    const NAME: &'static str = "cite.child";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Read a closure child and stage cited text.";
    type Input = CiteInput;
    type Result = CiteResult;
}

impl SyncProgram for Cite {
    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        let _child = env.injected(input.child)?;
        Ok(CiteResult { text: env.stage_text("hello") })
    }
}

struct MissingInput;

impl Program for MissingInput {
    const NAME: &'static str = "missing.input";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Never runs; the input is absent.";
    type Input = Child;
    type Result = Child;
}

impl SyncProgram for MissingInput {
    fn run(input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Ok(input)
    }
}

struct MissingRead;

impl Program for MissingRead {
    const NAME: &'static str = "missing.read";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Read a digest the closure does not carry.";
    type Input = Child;
    type Result = Child;
}

impl SyncProgram for MissingRead {
    fn run(_input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        env.injected(Ref::<Child>::from_digest(Digest::from_bytes([9; 32])))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.invoke_pair")]
struct Pair {
    a: Ref<OpaqueBytes>,
    b: Ref<OpaqueBytes>,
}

struct Orphan;

impl Program for Orphan {
    const NAME: &'static str = "stage.orphan";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Stage a blob the result does not cite.";
    type Input = Child;
    type Result = Pair;
}

impl SyncProgram for Orphan {
    fn run(_input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        let a = env.stage_bytes(b"one");
        let _orphan = env.stage_bytes(b"two");
        Ok(Pair { a, b: a })
    }
}

struct Dedupe;

impl Program for Dedupe {
    const NAME: &'static str = "stage.dedupe";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Stage the same bytes twice.";
    type Input = Child;
    type Result = Pair;
}

impl SyncProgram for Dedupe {
    fn run(_input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        let a = env.stage_bytes(b"same");
        let b = env.stage_bytes(b"same");
        Ok(Pair { a, b })
    }
}

/// Reads its cited text with `injected_text` and stages it back whole.
struct EchoText;

impl Program for EchoText {
    const NAME: &'static str = "echo.text";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Read cited text and stage it back.";
    type Input = CiteResult;
    type Result = CiteResult;
}

impl SyncProgram for EchoText {
    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        let text = env.injected_text(input.text)?;
        Ok(CiteResult { text: env.stage_text(&text) })
    }
}

#[test]
fn a_member_whose_claim_was_altered_refuses_before_decode() -> Result<(), Box<dyn Error>> {
    // Catches a typed `injected` read or an `injected_text` read that decodes a member without
    // hashing it against the digest it was read under. Both payloads decode, so only
    // verification refuses them.
    let child = with_altered_claim(&closure_of(&Child { n: 1 })?)?;
    match send::<MissingInput>(child.claimed().unverified(), vec![child]) {
        Invoked::Refused { seq: 7, refusal: Refusal::InputDecode } => {}
        other => panic!("expected InputDecode for a typed read, got {other:?}"),
    }

    let text = with_altered_claim(&ClosureArtifact::new(Utf8Text::ID, b"hello".to_vec()))?;
    let input = closure_of(&CiteResult { text: Ref::from_digest(text.claimed().unverified()) })?;
    match send::<EchoText>(input.claimed().unverified(), vec![input, text]) {
        Invoked::Refused { seq: 7, refusal: Refusal::InputDecode } => Ok(()),
        other => panic!("expected InputDecode for a text read, got {other:?}"),
    }
}

#[test]
fn a_text_member_past_one_read_window_reads_back_whole() -> Result<(), Box<dyn Error>> {
    // Catches an `Env` read that truncates a member at a `MAX_READ_BYTES` window.
    let text = format!("{}z", "a".repeat(MAX_READ_BYTES));
    let member = ClosureArtifact::new(Utf8Text::ID, text.as_bytes().to_vec());
    let input = closure_of(&CiteResult { text: Ref::of_text(&text) })?;
    let Invoked::Completed { result, .. } = send::<EchoText>(input.claimed().unverified(), vec![input, member]) else {
        panic!("expected Completed");
    };
    assert_eq!(result, encoded(&CiteResult { text: Ref::of_text(&text) })?.digest());
    Ok(())
}

#[test]
fn absent_input_is_input_missing() {
    match send::<MissingInput>(Digest::from_bytes([1; 32]), Vec::new()) {
        Invoked::Refused { seq: 7, refusal: Refusal::InputMissing } => {}
        other => panic!("expected InputMissing, got {other:?}"),
    }
}

#[test]
fn wrong_kind_input_is_input_decode() -> Result<(), Box<dyn Error>> {
    let child = Child { n: 1 };
    let artifact = closure_of(&child)?;
    match send::<Cite>(artifact.claimed().unverified(), vec![artifact]) {
        Invoked::Refused { seq: 7, refusal: Refusal::InputDecode } => Ok(()),
        other => panic!("expected InputDecode, got {other:?}"),
    }
}

#[test]
fn undecodable_input_is_input_decode() {
    let artifact = ClosureArtifact::new(Child::ID, vec![0xff, 0xff, 0xff]);
    match send::<MissingInput>(artifact.claimed().unverified(), vec![artifact]) {
        Invoked::Refused { seq: 7, refusal: Refusal::InputDecode } => {}
        other => panic!("expected InputDecode, got {other:?}"),
    }
}

#[test]
fn read_outside_the_closure_is_input_missing() -> Result<(), Box<dyn Error>> {
    let child = Child { n: 1 };
    let artifact = closure_of(&child)?;
    match send::<MissingRead>(artifact.claimed().unverified(), vec![artifact]) {
        Invoked::Refused { seq: 7, refusal: Refusal::InputMissing } => Ok(()),
        other => panic!("expected InputMissing, got {other:?}"),
    }
}

#[test]
fn completed_reads_a_closure_child_and_stages_cited_text() -> Result<(), Box<dyn Error>> {
    let child = Child { n: 4 };
    let child_artifact = closure_of(&child)?;
    let input = CiteInput { child: Ref::of_encoded(&child)? };
    let input_artifact = closure_of(&input)?;
    let Invoked::Completed { seq, result, staged } =
        send::<Cite>(input_artifact.claimed().unverified(), vec![child_artifact, input_artifact])
    else {
        panic!("expected Completed");
    };
    assert_eq!(seq, 7);
    let expected = CiteResult { text: Ref::of_text("hello") };
    let expected_encoded = encoded(&expected)?;
    assert_eq!(result, artifact_digest(CiteResult::ID, expected_encoded.bytes()));
    assert_eq!(result, expected_encoded.digest());
    assert_eq!(staged, vec![EncodedArtifact::text("hello"), expected_encoded]);
    Ok(())
}

#[test]
fn unreachable_staged_blob_is_refused() -> Result<(), Box<dyn Error>> {
    let child = Child { n: 1 };
    let artifact = closure_of(&child)?;
    match send::<Orphan>(artifact.claimed().unverified(), vec![artifact]) {
        Invoked::Refused { seq: 7, refusal: Refusal::Refused { reason } } => {
            assert!(reason.as_str().contains("is not reachable from the result"), "{}", reason.as_str());
            Ok(())
        }
        other => panic!("expected Refused orphan, got {other:?}"),
    }
}

#[test]
fn staging_the_same_bytes_twice_yields_one_encoded_artifact() -> Result<(), Box<dyn Error>> {
    let child = Child { n: 1 };
    let artifact = closure_of(&child)?;
    let Invoked::Completed { staged, .. } = send::<Dedupe>(artifact.claimed().unverified(), vec![artifact]) else {
        panic!("expected Completed");
    };
    let bytes = EncodedArtifact::opaque_bytes(b"same");
    let pair = encoded(&Pair { a: Ref::of_bytes(b"same"), b: Ref::of_bytes(b"same") })?;
    assert_eq!(staged, vec![bytes, pair]);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.async_summarize.input")]
struct SummarizeInput {
    text: Ref<Utf8Text>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.async_summarize.result")]
struct SummarizeResult {
    text: Ref<Utf8Text>,
}

struct AsyncSummarize;

impl Program for AsyncSummarize {
    const NAME: &'static str = "async.summarize";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Fetch cited text and stage a derived summary.";
    type Input = SummarizeInput;
    type Result = SummarizeResult;
}

impl AsyncProgram for AsyncSummarize {
    async fn run(input: Self::Input, mut env: Env<Async>) -> Result<Self::Result, Refusal> {
        let text = env.read_text(input.text).await?;
        Ok(SummarizeResult { text: env.stage_text(&format!("summary:{text}")) })
    }
}

fn drive_async(input: Digest, closure: Vec<ClosureArtifact>, reply: Option<ReadArtifactResult>) -> Invoked {
    match start_async::<AsyncSummarize>(Invoke::new(7, program_name::<AsyncSummarize>(), input, closure)) {
        Started::Finished(invoked) => invoked,
        Started::Live { mut session, waiting } => {
            let Pending::Artifact(pending) = waiting.expect("a miss records one ReadArtifact") else {
                panic!("expected a journal ReadArtifact");
            };
            session.fulfill(pending, reply.expect("test supplies a journal reply"));
            match session.poll() {
                PollResult::Finished(invoked) => invoked,
                PollResult::NeedArtifact(_) | PollResult::NeedSend(_) | PollResult::Waiting => {
                    panic!("expected Finished after one journal reply")
                }
            }
        }
    }
}

#[test]
fn async_hit_path_does_not_fetch() -> Result<(), Box<dyn Error>> {
    let text = "hello";
    let text_artifact = ClosureArtifact::new(Utf8Text::ID, text.as_bytes().to_vec());
    let input = SummarizeInput { text: Ref::of_text(text) };
    let input_artifact = closure_of(&input)?;
    match drive_async(input_artifact.claimed().unverified(), vec![text_artifact, input_artifact], None) {
        Invoked::Completed { seq: 7, result, staged } => {
            let expected = SummarizeResult { text: Ref::of_text("summary:hello") };
            let expected_encoded = encoded(&expected)?;
            assert_eq!(result, expected_encoded.digest());
            assert_eq!(staged, vec![EncodedArtifact::text("summary:hello"), expected_encoded]);
            Ok(())
        }
        other => panic!("expected Completed on an injected hit, got {other:?}"),
    }
}

#[test]
fn async_miss_fetches_and_completes() -> Result<(), Box<dyn Error>> {
    let text = "hello";
    let input = SummarizeInput { text: Ref::of_text(text) };
    let input_artifact = closure_of(&input)?;
    let reply = ReadArtifactResult::Found { artifact: ClosureArtifact::new(Utf8Text::ID, text.as_bytes().to_vec()) };
    match drive_async(input_artifact.claimed().unverified(), vec![input_artifact], Some(reply)) {
        Invoked::Completed { seq: 7, result, staged } => {
            let expected = SummarizeResult { text: Ref::of_text("summary:hello") };
            let expected_encoded = encoded(&expected)?;
            assert_eq!(result, expected_encoded.digest());
            assert_eq!(staged, vec![EncodedArtifact::text("summary:hello"), expected_encoded]);
            Ok(())
        }
        other => panic!("expected Completed after Found, got {other:?}"),
    }
}

#[test]
fn async_miss_after_journal_missing_is_input_missing() -> Result<(), Box<dyn Error>> {
    let text = "hello";
    let input = SummarizeInput { text: Ref::of_text(text) };
    let input_artifact = closure_of(&input)?;
    let reply = ReadArtifactResult::Missing { digest: Ref::of_text(text).digest() };
    match drive_async(input_artifact.claimed().unverified(), vec![input_artifact], Some(reply)) {
        Invoked::Refused { seq: 7, refusal: Refusal::InputMissing } => Ok(()),
        other => panic!("expected InputMissing after Missing, got {other:?}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.sampled_http.input")]
struct HttpInput {
    marker: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.sampled_http.result")]
struct HttpResult {
    body: Ref<OpaqueBytes>,
}

struct SampledHttp;

impl Program for SampledHttp {
    const NAME: &'static str = "sampled.http";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Fetch and stage the response body.";
    type Input = HttpInput;
    type Result = HttpResult;
}

impl AsyncProgram for SampledHttp {
    async fn run(input: Self::Input, mut env: Env<Async>) -> Result<Self::Result, Refusal> {
        let _ = input;
        let mut http = Http::from_env(&mut env);
        match http
            .fetch(Fetch {
                request_id: 1,
                url: "https://example.test/body".into(),
                method: HttpMethod::Get,
                headers: Vec::new(),
                body: Vec::new(),
                timeout_ms: None,
            })
            .await?
        {
            FetchResult::Ok { body, .. } => Ok(HttpResult { body: env.stage_bytes(&body) }),
            FetchResult::Err { .. } => Err(Refusal::InputDecode),
        }
    }
}

#[test]
fn sampled_http_fetch_yields_need_send_then_completes() -> Result<(), Box<dyn Error>> {
    let input = HttpInput { marker: 1 };
    let input_artifact = closure_of(&input)?;
    match start_async::<SampledHttp>(Invoke::new(
        7,
        program_name::<SampledHttp>(),
        input_artifact.claimed().unverified(),
        vec![input_artifact],
    )) {
        Started::Finished(other) => panic!("expected Live NeedSend, got Finished {other:?}"),
        Started::Live { mut session, waiting } => {
            let Pending::Send(pending) = waiting.expect("first poll records one cap send") else {
                panic!("expected NeedSend to aether.http");
            };
            assert_eq!(pending.mailbox, "aether.http");
            assert_eq!(pending.kind_id, Fetch::ID);
            assert_eq!(pending.expected_reply, FetchResult::ID);
            let reply = FetchResult::Ok {
                request_id: 1,
                url: "https://example.test/body".into(),
                status: 200,
                headers: Vec::new(),
                body: b"hello".to_vec(),
            };
            session.fulfill_send(&pending, FetchResult::ID, reply.encode_into_bytes());
            match session.poll() {
                PollResult::Finished(Invoked::Completed { seq: 7, result, staged }) => {
                    let expected = HttpResult { body: Ref::of_bytes(b"hello") };
                    let expected_encoded = encoded(&expected)?;
                    assert_eq!(result, expected_encoded.digest());
                    assert_eq!(staged, vec![EncodedArtifact::opaque_bytes(b"hello"), expected_encoded]);
                    Ok(())
                }
                other => panic!("expected Completed after FetchResult, got {other:?}"),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.sampled_process.input")]
struct ProcessInput {
    marker: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.sampled_process.result")]
struct ProcessOut {
    stdout: Ref<OpaqueBytes>,
}

struct SampledProcess;

impl Program for SampledProcess {
    const NAME: &'static str = "sampled.process";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Run a binary and stage stdout.";
    type Input = ProcessInput;
    type Result = ProcessOut;
}

impl AsyncProgram for SampledProcess {
    async fn run(input: Self::Input, mut env: Env<Async>) -> Result<Self::Result, Refusal> {
        let _ = input;
        let mut process = Process::from_env(&mut env);
        match process
            .run(Run { binary: "echo".into(), args: Vec::new(), env: Vec::new(), stdin: Vec::new(), timeout_millis: 0 })
            .await?
        {
            RunResult::Ok { stdout, .. } => Ok(ProcessOut { stdout: env.stage_bytes(&stdout) }),
            RunResult::TimedOut { .. } | RunResult::Err { .. } => Err(Refusal::InputDecode),
        }
    }
}

#[test]
fn sampled_process_run_yields_need_send_then_completes() -> Result<(), Box<dyn Error>> {
    let input = ProcessInput { marker: 1 };
    let input_artifact = closure_of(&input)?;
    match start_async::<SampledProcess>(Invoke::new(
        7,
        program_name::<SampledProcess>(),
        input_artifact.claimed().unverified(),
        vec![input_artifact],
    )) {
        Started::Finished(other) => panic!("expected Live NeedSend, got Finished {other:?}"),
        Started::Live { mut session, waiting } => {
            let Pending::Send(pending) = waiting.expect("first poll records one cap send") else {
                panic!("expected NeedSend to aether.process");
            };
            assert_eq!(pending.mailbox, "aether.process");
            assert_eq!(pending.kind_id, Run::ID);
            assert_eq!(pending.expected_reply, RunResult::ID);
            let reply = RunResult::Ok { exit_code: Some(0), stdout: b"hello".to_vec(), stderr: Vec::new() };
            session.fulfill_send(&pending, RunResult::ID, reply.encode_into_bytes());
            match session.poll() {
                PollResult::Finished(Invoked::Completed { seq: 7, result, staged }) => {
                    let expected = ProcessOut { stdout: Ref::of_bytes(b"hello") };
                    let expected_encoded = encoded(&expected)?;
                    assert_eq!(result, expected_encoded.digest());
                    assert_eq!(staged, vec![EncodedArtifact::opaque_bytes(b"hello"), expected_encoded]);
                    Ok(())
                }
                other => panic!("expected Completed after RunResult, got {other:?}"),
            }
        }
    }
}

#[test]
fn invocation_child_has_no_run_kind_arm() {
    // Tripwire: Process shares Http's generic NeedSend pump. A per-kind `Run`
    // arm in the generated invocation child reopens the closed PendingSend enum.
    let child = include_str!("../../aether-bloomery-bundle-derive/src/expand/programs.rs");
    assert!(!child.contains("PendingSend::Process"), "invocation child must not grow a Process arm");
    assert!(!child.contains("Run =>"), "invocation child must not match on Run");
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.sampled_workspace.input")]
struct WorkspaceInput {
    tree: Ref<Tree>,
    environment: Ref<aether_workspace::Environment>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.sampled_workspace.result")]
struct WorkspaceOut {
    stdout: Ref<OpaqueBytes>,
}

thread_local! {
    /// Set when [`SampledWorkspace`]'s code after the run's await executes.
    static AFTER_AWAIT: Cell<bool> = const { Cell::new(false) };
}

struct SampledWorkspace;

impl Program for SampledWorkspace {
    const NAME: &'static str = "sampled.workspace";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Run one step and cite its stdout.";
    type Input = WorkspaceInput;
    type Result = WorkspaceOut;
}

impl AsyncProgram for SampledWorkspace {
    async fn run(input: Self::Input, mut env: Env<Async>) -> Result<Self::Result, Refusal> {
        let mut workspace = Workspace::from_env(&mut env);
        let step = aether_workspace::Step {
            tool: aether_workspace::ToolName::new("tool").map_err(|_| Refusal::InputDecode)?,
            args: vec!["target".to_owned()],
            env: Vec::new(),
            stdin: None,
        };
        let run = aether_workspace::Run {
            tree: input.tree,
            environment: input.environment,
            mounts: aether_workspace::Mounts::new(Vec::new()).map_err(|_| Refusal::InputDecode)?,
            steps: aether_workspace::Steps::new(vec![step]).map_err(|_| Refusal::InputDecode)?,
            scratch: aether_workspace::Scratch::new(Vec::new()).map_err(|_| Refusal::InputDecode)?,
            network: aether_workspace::Network::Off,
        };
        let answered = workspace.run(run).await?;
        AFTER_AWAIT.set(true);
        match answered {
            Ok(outcome) => Ok(WorkspaceOut { stdout: outcome.steps[0].stdout }),
            Err(refusal) => Err(Refusal::Refused { reason: Detail::new(format!("{refusal:?}")) }),
        }
    }
}

/// A started [`SampledWorkspace`] at seq 7 and the one call its first poll captured.
fn start_workspace() -> Result<(AsyncSession, PendingCall), Box<dyn Error>> {
    AFTER_AWAIT.set(false);
    let input = WorkspaceInput {
        tree: Ref::from_digest(Digest::from_bytes([1; 32])),
        environment: Ref::from_digest(Digest::from_bytes([2; 32])),
    };
    let input_artifact = closure_of(&input)?;
    let started = start_async::<SampledWorkspace>(Invoke::new(
        7,
        program_name::<SampledWorkspace>(),
        input_artifact.claimed().unverified(),
        vec![input_artifact],
    ));
    let Started::Live { session, waiting: Some(Pending::Send(pending)) } = started else {
        return Err("expected the first poll to capture one cap send".into());
    };
    Ok((session, pending))
}

/// Answer the captured run with `reply`, then poll once: the invocation's end,
/// and whether the program's code after the await ran.
fn answer_workspace(reply: &aether_workspace::RunResult) -> Result<(PollResult, bool), Box<dyn Error>> {
    let (mut session, pending) = start_workspace()?;
    session.fulfill_send(&pending, aether_workspace::RunResult::ID, reply.encode_into_bytes());
    Ok((session.poll(), AFTER_AWAIT.get()))
}

fn step_outcome(stdout: &[u8]) -> Result<aether_workspace::Outcome, Box<dyn Error>> {
    Ok(aether_workspace::Outcome {
        steps: vec![aether_workspace::StepOutcome {
            exit_code: Some(0),
            stdout: Ref::of_bytes(stdout),
            stderr: Ref::of_bytes(b""),
            tool: aether_workspace::ToolRecord {
                name: aether_workspace::ToolName::new("tool")?,
                path: aether_workspace::TreePath::new("usr/bin/tool")?,
                file: Ref::of_bytes(b"#!tool\n"),
            },
        }],
        tree: Ref::from_digest(Digest::from_bytes([3; 32])),
    })
}

#[test]
fn a_workspace_run_targets_the_workspace_and_hands_the_program_its_outcome_or_refusal() -> Result<(), Box<dyn Error>> {
    // Catches a wrong `api_target` row or reply kind, an outcome that does not
    // reach the program, and a workspace refusal that ends the invocation
    // instead of reaching the program as `Ok(Err(..))`.
    let (_, pending) = start_workspace()?;
    assert_eq!(pending.mailbox, "aether.workspace");
    assert_eq!(pending.kind_id, aether_workspace::Run::ID);
    assert_eq!(pending.expected_reply, aether_workspace::RunResult::ID);

    let (ran, _) = answer_workspace(&aether_workspace::RunResult::Ok(step_outcome(b"checked")?))?;
    let expected = encoded(&WorkspaceOut { stdout: Ref::of_bytes(b"checked") })?;
    let PollResult::Finished(Invoked::Completed { seq: 7, result, staged }) = ran else {
        return Err(format!("expected Completed after Ok, got {ran:?}").into());
    };
    assert_eq!((result, staged), (expected.digest(), vec![expected]), "the result cites stdout without staging it");

    let unknown = aether_workspace::Refusal::UnknownTool(aether_workspace::ToolName::new("tool")?);
    let (refused, after_await) = answer_workspace(&aether_workspace::RunResult::Refused(unknown))?;
    let PollResult::Finished(Invoked::Refused { seq: 7, refusal: Refusal::Refused { reason } }) = refused else {
        return Err(format!("expected the program's own refusal, got {refused:?}").into());
    };
    assert!(reason.as_str().contains("UnknownTool"), "{reason:?}");
    assert!(after_await, "the program saw the workspace's refusal");
    Ok(())
}

#[test]
fn an_exhausted_or_failed_run_ends_the_invocation_unseen_by_the_program() -> Result<(), Box<dyn Error>> {
    // Catches a fault the program can observe (its code after the await runs,
    // so it could record the fault as a result), a swapped time/memory
    // mapping, and a failure detail lost on the way to the driver.
    let detail = Detail::new("starting container c1: the Docker daemon answered 500");
    let cases = [
        (aether_workspace::RunResult::Exhausted(aether_workspace::Resource::Time), ExecutorFault::TimedOut),
        (aether_workspace::RunResult::Exhausted(aether_workspace::Resource::Memory), ExecutorFault::ResourceExhausted),
        (aether_workspace::RunResult::Failed { detail: detail.clone() }, ExecutorFault::Failed { reason: detail }),
    ];
    for (reply, expected) in cases {
        let (ended, after_await) = answer_workspace(&reply)?;
        let PollResult::Finished(Invoked::Faulted { seq: 7, fault }) = ended else {
            return Err(format!("expected Faulted for {reply:?}, got {ended:?}").into());
        };
        assert_eq!(fault, expected, "for {reply:?}");
        assert!(!after_await, "the program ran past the await of {reply:?}");
    }
    Ok(())
}
