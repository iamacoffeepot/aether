//! The run sequence against the stub daemon and a temp journal root.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use aether_bloomery_journal::{ArtifactBatch, ArtifactStore, Journal};
use aether_bloomery_kinds::{Digest, Name, Node, OpaqueBytes, Ref, Tree};
use aether_bloomery_tar::Limits;
use aether_data::wire::encode_to_vec;
use tempfile::TempDir;

use super::{Allotment, Runner};
use crate::runtime::engine::{Endpoint, Engine};
use crate::runtime::testing::{
    RUN_CONTAINER, RUN_VOLUME, RunScript, StubDaemon, StubReply, StubRequest, TarWriter, artifact_rows,
};
use crate::{
    EnvVar, Environment, Mounts, Network, Outcome, Platform, Provides, Refusal, Resource, Run, RunResult,
    RustToolchain, Scratch, Step, Steps, Tool, ToolName, Tools, TreePath,
};

type TestResult = Result<(), Box<dyn Error>>;

const TOOL: &[u8] = b"#!tool\n";
const LOGS: &[(u8, &[u8])] = &[(1, b"checked\n"), (2, b"warning: unused\n")];

/// A temp journal root holding one environment and one run tree.
struct Fixture {
    _temp: TempDir,
    path: PathBuf,
    store: ArtifactStore,
    environment: Ref<Environment>,
    tree: Ref<Tree>,
}

impl Fixture {
    /// An environment whose root holds `usr/bin/tool` (executable) and
    /// `usr/bin/text` (not), providing Rust 1.97.1 with clippy, and a run
    /// tree of `src/main.rs` plus `extra`.
    fn new(extra: Vec<(&str, &[u8])>) -> Result<Self, Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("journal");
        let store = Journal::open(&path)?.artifact_store();
        let mut batch = store.batch()?;

        let tool_file = blob(&mut batch, TOOL)?;
        let text_file = blob(&mut batch, b"x")?;
        let bin = directory(&mut batch, vec![("tool", Node::Executable(tool_file)), ("text", Node::File(text_file))])?;
        let usr = directory(&mut batch, vec![("bin", Node::Directory(bin))])?;
        let root = directory(&mut batch, vec![("usr", Node::Directory(usr))])?;
        let environment = batch.stage_encoded(&Environment {
            root,
            platform: Platform::new("x86_64-unknown-linux-gnu")?,
            provides: Provides { rust: Some(RustToolchain::new("1.97.1", vec!["clippy".to_owned()], Vec::new())?) },
            tools: Tools::new(vec![tool("tool", "usr/bin/tool")?, tool("text", "usr/bin/text")?])?,
            env: vec![EnvVar::new("PATH", "/usr/bin")?],
        })?;

        let main = Node::File(blob(&mut batch, b"fn main() {}\n")?);
        let src = directory(&mut batch, vec![("main.rs", main)])?;
        let mut entries = vec![("src", Node::Directory(src))];
        for (name, content) in extra {
            entries.push((name, Node::File(blob(&mut batch, content)?)));
        }
        let tree = directory(&mut batch, entries)?;
        batch.commit()?;
        Ok(Self { _temp: temp, path, store, environment, tree })
    }

    /// A one-step run of `tool` over the fixture's tree.
    fn run(&self, tool: &str, scratch: &str) -> Result<Run, Box<dyn Error>> {
        let step = Step { tool: ToolName::new(tool)?, args: vec!["--check".to_owned()], env: Vec::new(), stdin: None };
        Ok(Run {
            tree: self.tree,
            environment: self.environment,
            mounts: Mounts::new(Vec::new())?,
            steps: Steps::new(vec![step])?,
            scratch: Scratch::new(vec![TreePath::new(scratch)?])?,
            network: Network::Off,
        })
    }

    /// Run `run` against a stub serving `replies`, and return the answer and
    /// the requests the stub read.
    fn execute(
        &self,
        run: &Run,
        deadline: Duration,
        replies: Vec<StubReply>,
    ) -> Result<(RunResult, Vec<StubRequest>), Box<dyn Error>> {
        let stub = StubDaemon::bind()?;
        let runner = Runner {
            engine: Engine::new(Endpoint::from_config(&stub.config())?),
            artifacts: self.store.clone(),
            allotment: Allotment { deadline, memory_bytes: 1 << 30, pids: 64, output: Limits::new(1_000, 1 << 30)? },
        };
        thread::scope(|scope| {
            let served = scope.spawn(|| stub.serve(replies));
            let answer = runner.answer(run);
            let requests = served.join().map_err(|_| "the stub thread panicked")??;
            Ok((answer, requests))
        })
    }

    fn hex(&self) -> String {
        self.environment.digest().to_string()
    }

    fn rows(&self) -> Result<i64, Box<dyn Error>> {
        Ok(artifact_rows(&self.path)?)
    }
}

fn blob(batch: &mut ArtifactBatch, bytes: &[u8]) -> Result<Ref<OpaqueBytes>, Box<dyn Error>> {
    let mut file = batch.blob(bytes.len() as u64)?;
    file.write_chunk(bytes)?;
    Ok(file.finish()?)
}

fn directory(batch: &mut ArtifactBatch, entries: Vec<(&str, Node)>) -> Result<Ref<Tree>, Box<dyn Error>> {
    Ok(batch.stage_encoded(&tree_of(entries)?)?)
}

fn tree_of(entries: Vec<(&str, Node)>) -> Result<Tree, Box<dyn Error>> {
    let mut map = BTreeMap::new();
    for (name, node) in entries {
        map.insert(Name::new(name)?, node);
    }
    Ok(Tree::new(map))
}

fn tool(name: &str, path: &str) -> Result<Tool, Box<dyn Error>> {
    Ok(Tool { name: ToolName::new(name)?, path: TreePath::new(path)? })
}

fn lines(requests: &[StubRequest]) -> Vec<String> {
    requests.iter().map(StubRequest::line).collect()
}

fn outcome(answer: RunResult) -> Result<Outcome, Box<dyn Error>> {
    match answer {
        RunResult::Ok(outcome) => Ok(outcome),
        other => Err(format!("expected an outcome, got {other:?}").into()),
    }
}

fn detail(answer: &RunResult) -> Result<&str, Box<dyn Error>> {
    match answer {
        RunResult::Failed { detail } => Ok(detail.as_str()),
        other => Err(format!("expected Failed, got {other:?}").into()),
    }
}

/// `/work` as the daemon archives it after a step that wrote `out.txt`: the
/// input, the new file, and the `target` tmpfs as an empty directory.
fn built_work() -> Vec<u8> {
    TarWriter::new()
        .directory("work/")
        .file("work/out.txt", b"built\n")
        .directory("work/src/")
        .file("work/src/main.rs", b"fn main() {}\n")
        .directory("work/target/")
        .finish()
}

fn script<'a>(fixture: &'a str, output: &'a [u8]) -> RunScript<'a> {
    RunScript { environment: fixture, logs: LOGS, exit_code: 0, output }
}

const SECONDS: Duration = Duration::from_secs(10);

#[test]
fn a_run_resolves_creates_writes_starts_waits_reads_logs_and_output_then_removes() -> TestResult {
    // Catches a reordered or skipped call: the tree written after `start`,
    // logs read before the wait, the output read from a container other than
    // the last, or a container or volume never removed. Also catches outputs
    // stored under other bytes than the daemon sent and a tool record that
    // names a different file than the one resolved.
    let fixture = Fixture::new(Vec::new())?;
    let output = built_work();
    let hex = fixture.hex();

    let (answer, requests) =
        fixture.execute(&fixture.run("tool", "target")?, SECONDS, script(&hex, &output).replies())?;

    let outcome = outcome(answer)?;
    assert_eq!(
        lines(&requests),
        [
            "GET /v1.44/info".to_owned(),
            format!("GET /v1.44/images/aether-workspace-environment:{hex}/json"),
            "POST /v1.44/volumes/create".to_owned(),
            "POST /v1.44/containers/create".to_owned(),
            format!("PUT /v1.44/containers/{RUN_CONTAINER}/archive?path=/work"),
            format!("POST /v1.44/containers/{RUN_CONTAINER}/start"),
            format!("POST /v1.44/containers/{RUN_CONTAINER}/wait"),
            format!("GET /v1.44/containers/{RUN_CONTAINER}/json"),
            format!("GET /v1.44/containers/{RUN_CONTAINER}/logs?stdout=1&stderr=1"),
            format!("GET /v1.44/containers/{RUN_CONTAINER}/logs?stdout=1&stderr=0"),
            format!("GET /v1.44/containers/{RUN_CONTAINER}/logs?stdout=0&stderr=1"),
            format!("GET /v1.44/containers/{RUN_CONTAINER}/archive?path=/work"),
            format!("DELETE /v1.44/containers/{RUN_CONTAINER}?force=true&v=true"),
            format!("DELETE /v1.44/volumes/{RUN_VOLUME}?force=true"),
        ]
    );
    let [step] = outcome.steps.as_slice() else {
        panic!("one step ran: {:?}", outcome.steps)
    };
    assert_eq!(step.exit_code, Some(0));
    assert_eq!(step.stdout, Ref::of_bytes(b"checked\n"));
    assert_eq!(step.stderr, Ref::of_bytes(b"warning: unused\n"));
    assert_eq!((step.tool.path.as_str(), step.tool.file), ("usr/bin/tool", Ref::of_bytes(TOOL)));
    let reader = fixture.store.batch()?;
    assert!(reader.blob_reader(&step.stderr)?.is_some(), "the stderr blob is committed");
    assert!(reader.get::<Tree>(&outcome.tree.digest())?.is_some(), "the output tree is committed");
    Ok(())
}

#[test]
fn a_transport_failure_mid_run_still_removes_everything_and_answers_failed_naming_the_call() -> TestResult {
    // Catches a failure path that skips cleanup (a container and a volume
    // left per failed run), a failure reported as a refusal, and a detail
    // that does not say which call broke.
    let fixture = Fixture::new(Vec::new())?;
    let hex = fixture.hex();
    let mut replies = script(&hex, &[]).replies();
    replies.truncate(5);
    replies.extend([StubReply::hang_up(), StubReply::with_length(204, ""), StubReply::with_length(204, "")]);

    let (answer, requests) = fixture.execute(&fixture.run("tool", "target")?, SECONDS, replies)?;

    let detail = detail(&answer)?;
    assert!(detail.starts_with(&format!("starting container {RUN_CONTAINER}:")), "{detail}");
    assert_eq!(
        lines(&requests[5..]),
        [
            format!("POST /v1.44/containers/{RUN_CONTAINER}/start"),
            format!("DELETE /v1.44/containers/{RUN_CONTAINER}?force=true&v=true"),
            format!("DELETE /v1.44/volumes/{RUN_VOLUME}?force=true"),
        ]
    );
    Ok(())
}

#[test]
fn a_step_past_the_deadline_is_killed_and_answers_exhausted_time() -> TestResult {
    // Catches a wait with no deadline (the test would hang on the held
    // response), a timed-out step left running, and a timeout reported as a
    // failure or an outcome.
    let fixture = Fixture::new(Vec::new())?;
    let hex = fixture.hex();
    let mut replies = script(&hex, &[]).replies();
    replies.truncate(6);
    replies.extend([
        StubReply::hold(),
        StubReply::with_length(204, ""),
        StubReply::with_length(204, ""),
        StubReply::with_length(204, ""),
    ]);

    let (answer, requests) = fixture.execute(&fixture.run("tool", "target")?, Duration::from_millis(300), replies)?;

    assert_eq!(answer, RunResult::Exhausted(Resource::Time));
    assert_eq!(
        lines(&requests[6..]),
        [
            format!("POST /v1.44/containers/{RUN_CONTAINER}/wait"),
            format!("POST /v1.44/containers/{RUN_CONTAINER}/kill"),
            format!("DELETE /v1.44/containers/{RUN_CONTAINER}?force=true&v=true"),
            format!("DELETE /v1.44/volumes/{RUN_VOLUME}?force=true"),
        ]
    );
    Ok(())
}

#[test]
fn an_oom_killed_step_answers_exhausted_memory() -> TestResult {
    // Catches an out-of-memory kill read as an ordinary exit (137 would
    // become an outcome the program sees).
    let fixture = Fixture::new(Vec::new())?;
    let hex = fixture.hex();
    let mut replies = script(&hex, &[]).replies();
    replies.truncate(7);
    replies.extend([
        StubReply::with_length(200, r#"{"State":{"ExitCode":137,"OOMKilled":true}}"#),
        StubReply::with_length(204, ""),
        StubReply::with_length(204, ""),
    ]);

    let (answer, _) = fixture.execute(&fixture.run("tool", "target")?, SECONDS, replies)?;

    assert_eq!(answer, RunResult::Exhausted(Resource::Memory));
    Ok(())
}

#[test]
fn an_image_labelled_for_another_environment_is_refused_before_anything_is_created() -> TestResult {
    // Catches the label check skipped (a run in the wrong root filesystem)
    // or answered as a failure, and volumes created before it.
    let fixture = Fixture::new(Vec::new())?;
    let replies = vec![
        StubReply::with_length(200, r#"{"Architecture":"x86_64","OSType":"linux"}"#),
        StubReply::with_length(200, r#"{"Config":{"Labels":{"aether.workspace.environment":"00"}}}"#),
    ];

    let (answer, requests) = fixture.execute(&fixture.run("tool", "target")?, SECONDS, replies)?;

    assert_eq!(answer, RunResult::Refused(Refusal::EnvironmentUnavailable));
    assert_eq!(requests.len(), 2, "{:?}", lines(&requests));
    Ok(())
}

#[test]
fn a_toolchain_file_asking_for_more_than_the_environment_provides_is_refused_without_the_daemon() -> TestResult {
    // Catches a subset test turned around or skipped, and a toolchain check
    // that runs after contacting the daemon. The neighbour asks only for what
    // is provided and gets past resolution, to the daemon this stub never
    // serves.
    let wants = b"[toolchain]\nchannel = \"1.97.1\"\ncomponents = [\"rustfmt\", \"clippy\"]\n";
    let fits = b"[toolchain]\nchannel = \"1.97.1\"\ncomponents = [\"clippy\"]\nprofile = \"minimal\"\n";
    let refused = Fixture::new(vec![("rust-toolchain.toml", wants)])?;
    let accepted = Fixture::new(vec![("rust-toolchain.toml", fits)])?;

    let (answer, requests) = refused.execute(&refused.run("tool", "target")?, SECONDS, Vec::new())?;
    let (neighbour, _) = accepted.execute(&accepted.run("tool", "target")?, SECONDS, Vec::new())?;

    let provided = RustToolchain::new("1.97.1", vec!["clippy".to_owned()], Vec::new())?;
    let tree_wants = RustToolchain::new("1.97.1", vec!["clippy".to_owned(), "rustfmt".to_owned()], Vec::new())?;
    assert_eq!(
        answer,
        RunResult::Refused(Refusal::ToolchainMismatch { tree_wants, environment_provides: Some(provided) })
    );
    assert!(requests.is_empty(), "{:?}", lines(&requests));
    assert!(detail(&neighbour)?.starts_with("reading the daemon's platform:"), "{neighbour:?}");
    Ok(())
}

#[test]
fn a_tool_that_is_not_an_executable_in_the_root_is_unknown() -> TestResult {
    // Catches tool resolution that accepts any node at the tool's path, so a
    // plain file would run, or that trusts the table without walking the root.
    let fixture = Fixture::new(Vec::new())?;

    let (plain, _) = fixture.execute(&fixture.run("text", "target")?, SECONDS, Vec::new())?;
    let (absent, _) = fixture.execute(&fixture.run("cargo", "target")?, SECONDS, Vec::new())?;

    assert_eq!(plain, RunResult::Refused(Refusal::UnknownTool(ToolName::new("text")?)));
    assert_eq!(absent, RunResult::Refused(Refusal::UnknownTool(ToolName::new("cargo")?)));
    Ok(())
}

#[test]
fn an_environment_the_journal_lacks_is_input_missing() -> TestResult {
    // Catches a missing input answered as an executor failure, which the
    // driver would retry forever instead of recording.
    let fixture = Fixture::new(Vec::new())?;
    let absent = Digest::from_bytes([9; 32]);
    let run = Run { environment: Ref::from_digest(absent), ..fixture.run("tool", "target")? };

    let (answer, requests) = fixture.execute(&run, SECONDS, Vec::new())?;

    assert_eq!(answer, RunResult::Refused(Refusal::InputMissing(absent)));
    assert!(requests.is_empty());
    Ok(())
}

#[test]
fn an_output_holding_a_fifo_answers_failed_naming_it_and_commits_nothing() -> TestResult {
    // Catches an output decoded under the userland rules or with the refusal
    // swallowed, and stdout, stderr, or partial output rows committed for a
    // run that failed.
    let fixture = Fixture::new(Vec::new())?;
    let hex = fixture.hex();
    let output = TarWriter::new().directory("work/").fifo("work/pipe").finish();
    let rows = fixture.rows()?;

    let (answer, requests) =
        fixture.execute(&fixture.run("tool", "target")?, SECONDS, script(&hex, &output).replies())?;

    let detail = detail(&answer)?;
    assert!(detail.contains("work/pipe"), "{detail}");
    assert_eq!(fixture.rows()?, rows);
    assert_eq!(requests.len(), 14, "cleanup still ran: {:?}", lines(&requests));
    Ok(())
}

#[test]
fn a_nested_scratch_directory_is_absent_from_the_output_tree() -> TestResult {
    // Catches scratch left in the output (a result that depends on build
    // state), and a removal that rebuilds the wrong ancestors: `crates/app`
    // must keep `src` beside the removed `target`, and `crates` its sibling.
    let fixture = Fixture::new(Vec::new())?;
    let hex = fixture.hex();
    let output = TarWriter::new()
        .directory("work/")
        .directory("work/crates/")
        .file("work/crates/README", b"r\n")
        .directory("work/crates/app/")
        .file("work/crates/app/src", b"s\n")
        .directory("work/crates/app/target/")
        .finish();

    let (answer, _) =
        fixture.execute(&fixture.run("tool", "crates/app/target")?, SECONDS, script(&hex, &output).replies())?;

    let app = tree_of(vec![("src", Node::File(Ref::of_bytes(b"s\n")))])?;
    let crates =
        tree_of(vec![("README", Node::File(Ref::of_bytes(b"r\n"))), ("app", Node::Directory(Ref::of_encoded(&app)?))])?;
    let work = tree_of(vec![("crates", Node::Directory(Ref::of_encoded(&crates)?))])?;
    assert_eq!(outcome(answer)?.tree, Ref::of_encoded(&work)?);
    Ok(())
}

#[test]
fn the_same_run_twice_answers_byte_equal_results_and_adds_no_rows() -> TestResult {
    // Catches a result that is not a function of its inputs: a container id,
    // a volume name, a timestamp, or map order reaching the result or the
    // output tree, or rows inserted again for content already stored.
    let fixture = Fixture::new(Vec::new())?;
    let hex = fixture.hex();
    let output = built_work();
    let run = fixture.run("tool", "target")?;

    let (first, _) = fixture.execute(&run, SECONDS, script(&hex, &output).replies())?;
    let rows = fixture.rows()?;
    let (second, _) = fixture.execute(&run, SECONDS, script(&hex, &output).replies())?;

    assert!(matches!(first, RunResult::Ok(_)), "{first:?}");
    assert_eq!(encode_to_vec(&first)?, encode_to_vec(&second)?);
    assert_eq!(fixture.rows()?, rows);
    Ok(())
}

#[test]
fn a_step_with_stdin_attaches_before_start_and_streams_the_whole_blob() -> TestResult {
    // Catches stdin attached after `start` (a fast process would read an
    // empty stdin), bytes other than the stored blob's, and a stream never
    // closed, which would leave the process waiting on stdin: the stub records
    // the attach only once the client closes its writing half.
    let fixture = Fixture::new(Vec::new())?;
    let hex = fixture.hex();
    let output = built_work();
    let mut run = fixture.run("tool", "target")?;
    let mut steps = run.steps.as_slice().to_vec();
    steps[0].stdin = Some(Ref::of_bytes(b"fn main() {}\n"));
    run.steps = Steps::new(steps)?;
    let mut replies = script(&hex, &output).replies();
    replies.insert(5, StubReply::upgrade());

    let (answer, requests) = fixture.execute(&run, SECONDS, replies)?;

    outcome(answer)?;
    assert_eq!(
        lines(&requests[4..7]),
        [
            format!("PUT /v1.44/containers/{RUN_CONTAINER}/archive?path=/work"),
            format!("POST /v1.44/containers/{RUN_CONTAINER}/attach?stream=1&stdin=1"),
            format!("POST /v1.44/containers/{RUN_CONTAINER}/start"),
        ]
    );
    assert_eq!(requests[5].body, b"fn main() {}\n");
    let spec: serde_json::Value = serde_json::from_slice(&requests[3].body)?;
    assert_eq!((&spec["OpenStdin"], &spec["StdinOnce"]), (&true.into(), &true.into()));
    Ok(())
}

#[test]
fn the_step_container_carries_the_sandbox_pins_with_the_environment_overlaid_in_order() -> TestResult {
    // Catches a dropped pin (a writable root, the network on, swap beyond the
    // memory limit, capabilities kept, no tmpfs over scratch) and an overlay
    // in the wrong order: the step's PATH must win over the environment's,
    // and nothing may move SOURCE_DATE_EPOCH.
    let fixture = Fixture::new(Vec::new())?;
    let hex = fixture.hex();
    let output = built_work();
    let mut run = fixture.run("tool", "target")?;
    let mut steps = run.steps.as_slice().to_vec();
    steps[0].env = vec![EnvVar::new("PATH", "/opt/bin")?, EnvVar::new("SOURCE_DATE_EPOCH", "1")?];
    run.steps = Steps::new(steps)?;

    let (_, requests) = fixture.execute(&run, SECONDS, script(&hex, &output).replies())?;

    let spec: serde_json::Value = serde_json::from_slice(&requests[3].body)?;
    let host = &spec["HostConfig"];
    assert_eq!(spec["Cmd"], serde_json::json!(["/usr/bin/tool", "--check"]));
    assert_eq!(spec["Env"], serde_json::json!(["PATH=/opt/bin", "SOURCE_DATE_EPOCH=315532800"]));
    assert_eq!((&spec["WorkingDir"], &spec["User"]), (&"/work".into(), &"0:0".into()));
    assert_eq!(host["ReadonlyRootfs"], true);
    assert_eq!(host["NetworkMode"], "none");
    assert_eq!(host["Memory"], host["MemorySwap"]);
    assert_eq!(host["CapDrop"], serde_json::json!(["ALL"]));
    assert_eq!(host["Tmpfs"], serde_json::json!({ "/work/target": "rw,exec" }));
    Ok(())
}
