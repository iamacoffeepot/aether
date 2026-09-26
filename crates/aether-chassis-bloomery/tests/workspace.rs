//! End-to-end: a `Workspace` program on the shipped bloomery composition, its run answered by a scripted Engine API
//! daemon. A clean run records a transition citing the stored step output; an out-of-memory run and an unreachable
//! daemon record the matching executor fault and no transition.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::thread;

use aether_bloomery_journal::{Batch, JournalReader, Seq};
use aether_bloomery_kinds::{
    Call, CallOutcome, Digest, Fault, FaultReason, Head, Name, NativeOrigin, Node, OpaqueBytes, ProgramName,
    ProgramRef, RecordedHead, RecordedHeadMove, Ref, RequestSource, Requested, Tree,
};
use aether_chassis_bloomery::BloomeryCli;
use aether_harness_bloomery::{BloomeryHarness, Record, SeededJournal};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_workspace::testing::{RunScript, StubDaemon, StubReply, TarWriter};
use aether_workspace::{Environment, Platform, Provides, Tool, ToolName, Tools, TreePath};
use clap::Parser;

/// The `programs` head the seed binds to the fixture bundle.
const PROGRAMS: Head<OpaqueBytes> = Head::new("programs");

/// The fixture's one program.
const PROGRAM: &str = "test.program.workspace.run";

/// What the step writes: one stdout frame and one stderr frame.
const LOGS: &[(u8, &[u8])] = &[(1, b"checked\n"), (2, b"warning: unused\n")];

/// A local mirror of the fixture's input kind.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.workspace.run.input")]
struct RunInput {
    tree: Ref<Tree>,
    environment: Ref<Environment>,
}

/// A local mirror of the fixture's result kind.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.workspace.run.result")]
struct RunOutput {
    exit_code: Option<i32>,
    stdout: Ref<OpaqueBytes>,
    stderr: Ref<OpaqueBytes>,
    tree: Ref<Tree>,
}

/// The call a seed answers, the `Requested` it records, and the environment the stub's image label must name.
struct WorkspaceSeed {
    call: Call,
    requested: Requested,
    environment: Digest,
}

/// A batch holding the fixture bundle under [`PROGRAMS`], an environment whose root holds `tool` as an executable,
/// a run tree, and the program's input over both; or `None` when the bundle wasm is not built.
fn seed() -> Result<Option<(Batch, WorkspaceSeed)>, Box<dyn Error>> {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_program_workspace") else {
        return Ok(None);
    };
    let wasm = fs::read(&wasm_path)?;
    let mut batch = Batch::new();
    let bundle = batch.stage_bytes(&wasm).digest();
    batch.push_event(&RecordedHeadMove::new(RecordedHead::from(&PROGRAMS), bundle), None)?;

    let tool = Node::Executable(batch.stage_bytes(b"#!tool\n"));
    let root = batch.stage_encoded(&Tree::new(BTreeMap::from([(Name::new("tool")?, tool)])))?;
    let environment = batch.stage_encoded(&Environment {
        root,
        platform: Platform::new("x86_64-unknown-linux-gnu")?,
        provides: Provides { rust: None },
        tools: Tools::new(vec![Tool { name: ToolName::new("tool")?, path: TreePath::new("tool")? }])?,
        env: Vec::new(),
    })?;
    let main = Node::File(batch.stage_bytes(b"fn main() {}\n"));
    let tree = batch.stage_encoded(&Tree::new(BTreeMap::from([(Name::new("main.rs")?, main)])))?;
    let input = batch.stage_encoded(&RunInput { tree, environment })?.digest();

    let origin = NativeOrigin::new("test.workspace")?;
    let name = ProgramName::new(PROGRAM)?;
    let call = Call { program: PROGRAMS, name: name.clone(), input, origin: origin.clone(), key: 1 };
    let requested =
        Requested { program: ProgramRef::new(bundle, name), input, source: RequestSource::Native { origin, key: 1 } };
    Ok(Some((batch, WorkspaceSeed { call, requested, environment: environment.digest() })))
}

/// Boot the bloomery over `batch` with the workspace actor dialing `endpoint`.
fn boot(batch: Batch, endpoint: &str) -> Result<BloomeryHarness, Box<dyn Error>> {
    let cli = BloomeryCli::try_parse_from(["aether-bloomery", "--workspace-endpoint", endpoint])?;
    Ok(SeededJournal::new([batch]).boot_with_argv(cli))
}

/// Make the seed's call while `stub` answers `replies`.
fn call_against(
    harness: &mut BloomeryHarness,
    call: &Call,
    stub: StubDaemon,
    replies: Vec<StubReply>,
) -> Result<CallOutcome, Box<dyn Error>> {
    thread::scope(|scope| {
        let served = scope.spawn(|| stub.serve(replies));
        let outcome = harness.call(call);
        served.join().map_err(|_| "the stub daemon thread panicked")??;
        Ok(outcome)
    })
}

/// The one fault the call recorded, checked to be caused by its `Requested` with no transition beside it.
fn recorded_fault(harness: &BloomeryHarness, seed: &WorkspaceSeed, outcome: CallOutcome) -> FaultReason {
    let CallOutcome::Fault { key: 1, seq: 3, fault } = outcome else {
        panic!("expected the call's Fault at seq 3, got {outcome:?}");
    };
    let requested = &seed.requested;
    let expected = Fault { program: requested.program.clone(), input: requested.input, reason: fault.reason.clone() };
    harness.assert_appended(Seq(1), &[Record::equal(None, requested.clone()), Record::equal(Some(Seq(2)), expected)]);
    fault.reason
}

#[test]
fn a_workspace_run_records_a_transition_citing_the_stored_step_output() -> Result<(), Box<dyn Error>> {
    // Catches an invocation that does not declare `WorkspaceCapability` (the bundle's load is refused), a run
    // reply that does not reach the program, and step output the result cites but the journal does not store.
    let Some((batch, seed)) = seed()? else {
        return Ok(());
    };
    let stub = StubDaemon::bind()?;
    let mut harness = boot(batch, &stub.endpoint())?;
    let hex = seed.environment.to_string();
    let output = TarWriter::new().directory("work/").file("work/main.rs", b"fn main() {}\n").finish();
    let replies = RunScript { environment: &hex, logs: LOGS, exit_code: 0, output: &output }.replies();

    let outcome = call_against(&mut harness, &seed.call, stub, replies)?;

    let CallOutcome::Transition { key: 1, seq: 3, transition } = outcome else {
        panic!("expected the call's Transition at seq 3, got {outcome:?}");
    };
    harness.assert_appended(
        Seq(1),
        &[Record::equal(None, seed.requested.clone()), Record::equal(Some(Seq(2)), transition.clone())],
    );
    let result = JournalReader::open(harness.journal_path())?
        .get::<RunOutput>(&transition.result)?
        .ok_or("the transition cites a stored result")?;
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.stdout, Ref::of_bytes(b"checked\n"));
    assert_eq!(result.stderr, Ref::of_bytes(b"warning: unused\n"));
    for cited in [result.stdout.digest(), result.stderr.digest(), result.tree.digest()] {
        assert!(harness.stores(&cited), "the workspace stored {cited}");
    }
    Ok(())
}

#[test]
fn an_out_of_memory_run_records_a_resource_exhausted_fault() -> Result<(), Box<dyn Error>> {
    // Catches an `Exhausted` reply that does not cross the bundle root as `Faulted`, and one the program sees
    // (it would refuse, recording `Refused`) or the driver maps to another reason.
    let Some((batch, seed)) = seed()? else {
        return Ok(());
    };
    let stub = StubDaemon::bind()?;
    let mut harness = boot(batch, &stub.endpoint())?;
    let hex = seed.environment.to_string();
    let mut replies = RunScript { environment: &hex, logs: LOGS, exit_code: 0, output: &[] }.replies();
    replies.truncate(8);
    replies.extend([
        StubReply::with_length(200, r#"{"State":{"ExitCode":137,"OOMKilled":true}}"#),
        StubReply::with_length(204, ""),
        StubReply::with_length(204, ""),
    ]);

    let outcome = call_against(&mut harness, &seed.call, stub, replies)?;

    assert_eq!(recorded_fault(&harness, &seed, outcome), FaultReason::ResourceExhausted);
    Ok(())
}

#[test]
fn an_unreachable_daemon_records_an_executor_failure_without_the_endpoint() -> Result<(), Box<dyn Error>> {
    // Catches a host path reaching the journal: the recorded reason must name the failed call and never the
    // socket the workspace dialed. The stub is dropped before boot, so its socket and directory are gone.
    let Some((batch, seed)) = seed()? else {
        return Ok(());
    };
    let endpoint = StubDaemon::bind()?.endpoint();
    let socket = endpoint.strip_prefix("unix://").ok_or("a unix endpoint")?;
    let (socket_dir, socket_name) = socket.rsplit_once('/').ok_or("a socket inside a directory")?;
    let mut harness = boot(batch, &endpoint)?;

    let outcome = harness.call(&seed.call);

    let FaultReason::ExecutorFailed { reason } = recorded_fault(&harness, &seed, outcome) else {
        panic!("expected an executor failure");
    };
    assert!(reason.as_str().starts_with("reading the daemon's platform:"), "{reason:?}");
    assert!(!reason.as_str().contains(socket_dir), "{reason:?} names the socket's directory");
    assert!(!reason.as_str().contains(socket_name), "{reason:?} names the socket");
    Ok(())
}
