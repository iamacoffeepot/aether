//! `Run` over the unit's journal: the Engine API sequence, the sandbox, provisioning, refusals, and the output tree.

use std::error::Error;
use std::fs;
use std::thread;

use aether_bloomery_journal::Batch;
use aether_bloomery_kinds::{Digest, Node, OpaqueBytes, ReadArtifacts, Ref, Tree, artifact_blob};
use aether_bloomery_workspace::testing::{
    MountScript, RUN_CONTAINER, RUN_HELPER, RUN_VOLUME, RunScript, StubDaemon, StubReply, StubRequest, TarWriter,
    pointer_reply,
};
use aether_bloomery_workspace::{
    EnvVar, Environment, ImageRef, Import, ImportResult, Mount, Mounts, Network, Outcome, Platform, Provides, Refusal,
    Resource, Run, RunError, RunRequest, RunResult, RustToolchain, Scratch, Step, Steps, Tool, ToolName, Tools,
    TreePath,
};
use aether_data::Kind;
use aether_data::wire::encode_to_vec;
use aether_harness_bloomery::BloomeryHarness;

use crate::support::{
    CONTAINER, IMAGE, TestResult, answering, boot, boot_without_daemon, directory, large_payload, lines, serving,
    stored, tree_of,
};

const TOOL: &[u8] = b"#!tool\n";
const LOGS: &[(u8, &[u8])] = &[(1, b"checked\n"), (2, b"warning: unused\n")];

/// The workspace every run scenario boots: two cores, both given to each run, 1 GiB per step, and 64 processes.
pub const FLAGS: &[&str] = &[
    "--workspace-cpuset",
    "2-3",
    "--workspace-run-cores",
    "2",
    "--workspace-default-memory-bytes",
    "1073741824",
    "--workspace-pids-limit",
    "64",
];

/// A seed holding an environment whose root holds `usr/bin/tool` (executable) and `usr/bin/text` (not), providing
/// Rust 1.97.1 with clippy, and a run tree of `src/main.rs` plus its extra root files.
pub struct Inputs {
    batch: Batch,
    environment: Ref<Environment>,
    tree: Ref<Tree>,
}

impl Inputs {
    pub(crate) fn new(extra: Vec<(&str, &[u8])>) -> Result<Self, Box<dyn Error>> {
        let mut batch = Batch::new();
        let tool_file = batch.stage_bytes(TOOL);
        let text_file = batch.stage_bytes(b"x");
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

        let main = Node::File(batch.stage_bytes(b"fn main() {}\n"));
        let src = directory(&mut batch, vec![("main.rs", main)])?;
        let mut entries = vec![("src", Node::Directory(src))];
        for (name, content) in extra {
            entries.push((name, Node::File(batch.stage_bytes(content))));
        }
        let tree = directory(&mut batch, entries)?;
        Ok(Self { batch, environment, tree })
    }

    /// A one-step run of `tool` over the seed's tree, with `scratch` left out of its output.
    pub(crate) fn request(&self, tool: &str, scratch: &str) -> Result<RunRequest, Box<dyn Error>> {
        let step = Step { tool: ToolName::new(tool)?, args: vec!["--check".to_owned()], env: Vec::new(), stdin: None };
        Ok(RunRequest {
            tree: self.tree,
            environment: self.environment,
            mounts: Mounts::new(Vec::new())?,
            steps: Steps::new(vec![step])?,
            scratch: Scratch::new(vec![TreePath::new(scratch)?])?,
            network: Network::Off,
        })
    }

    /// The environment digest in hex, which the image label carries.
    pub(crate) fn hex(&self) -> String {
        self.environment.digest().to_string()
    }

    /// Boot over the seed with the workspace dialing `stub`.
    pub(crate) fn boot(self, stub: &StubDaemon, flags: &[&str]) -> Result<BloomeryHarness, Box<dyn Error>> {
        boot(vec![self.batch], &stub.endpoint(), flags)
    }
}

/// `request` over the harness's unit journal.
pub fn over(harness: &BloomeryHarness, request: RunRequest) -> Run {
    Run { source: harness.source(), request }
}

fn tool(name: &str, path: &str) -> Result<Tool, Box<dyn Error>> {
    Ok(Tool { name: ToolName::new(name)?, path: TreePath::new(path)? })
}

/// `/work` as the daemon archives it after a step that wrote `out.txt`: the input, the new file, and the `target`
/// tmpfs as an empty directory.
pub fn built_work() -> Vec<u8> {
    TarWriter::new()
        .directory("work/")
        .file("work/out.txt", b"built\n")
        .directory("work/src/")
        .file("work/src/main.rs", b"fn main() {}\n")
        .directory("work/target/")
        .finish()
}

pub fn script<'a>(environment: &'a str, output: &'a [u8]) -> RunScript<'a> {
    RunScript { environment, logs: LOGS, exit_code: 0, output }
}

/// A one-entry vendor tree staged into `inputs`' batch, and the request
/// mounting it at `vendor`.
fn vendored(
    inputs: &mut Inputs,
    request: &RunRequest,
    content: &[u8],
) -> Result<(RunRequest, Ref<Tree>), Box<dyn Error>> {
    let file = Node::File(inputs.batch.stage_bytes(content));
    let vendor = directory(&mut inputs.batch, vec![("crate.rs", file)])?;
    let mounted = RunRequest {
        mounts: Mounts::new(vec![Mount { at: TreePath::new("vendor")?, tree: vendor }])?,
        ..request.clone()
    };
    Ok((mounted, vendor))
}

/// The request lines of a single-step run over environment `hex`: the platform and image inspects and the `/work`
/// volume create, then `mounts`, then the step's create, `/work` write, start, stats, wait, inspect, logs, and output
/// read, then `removals`.
fn run_lines(hex: &str, mounts: &[String], removals: &[String]) -> Vec<String> {
    let prelude = [
        "GET /v1.44/info".to_owned(),
        format!("GET /v1.44/images/aether-workspace-environment:{hex}/json"),
        "POST /v1.44/volumes/create".to_owned(),
    ];
    let step = [
        "POST /v1.44/containers/create".to_owned(),
        format!("PUT /v1.44/containers/{RUN_CONTAINER}/archive?path=/work"),
        format!("POST /v1.44/containers/{RUN_CONTAINER}/start"),
        format!("GET /v1.44/containers/{RUN_CONTAINER}/stats?stream=true"),
        format!("POST /v1.44/containers/{RUN_CONTAINER}/wait"),
        format!("GET /v1.44/containers/{RUN_CONTAINER}/json"),
        format!("GET /v1.44/containers/{RUN_CONTAINER}/logs?stdout=1&stderr=1"),
        format!("GET /v1.44/containers/{RUN_CONTAINER}/logs?stdout=1&stderr=0"),
        format!("GET /v1.44/containers/{RUN_CONTAINER}/logs?stdout=0&stderr=1"),
        format!("GET /v1.44/containers/{RUN_CONTAINER}/archive?path=/work"),
    ];
    [&prelude[..], mounts, &step, removals].concat()
}

/// The removal lines of a run: the helper container when one was created, the step container, then `/work`.
fn removal_lines(helper: bool) -> Vec<String> {
    let helper = helper.then(|| format!("DELETE /v1.44/containers/{RUN_HELPER}?force=true&v=true"));
    helper
        .into_iter()
        .chain([
            format!("DELETE /v1.44/containers/{RUN_CONTAINER}?force=true&v=true"),
            format!("DELETE /v1.44/volumes/{RUN_VOLUME}?force=true"),
        ])
        .collect()
}

/// Boot over `inputs`, run `request` once while a fresh stub serves `replies`, and answer the result, the requests
/// the stub read, and the harness.
fn run_against(
    inputs: Inputs,
    request: RunRequest,
    replies: Vec<StubReply>,
    flags: &[&str],
) -> Result<(RunResult, Vec<StubRequest>, BloomeryHarness), Box<dyn Error>> {
    let stub = StubDaemon::bind()?;
    let mut harness = inputs.boot(&stub, flags)?;
    let run = over(&harness, request);
    let (answer, requests) = serving(stub, replies, || harness.run(&run))?;
    Ok((answer, requests, harness))
}

pub fn outcome(answer: RunResult) -> Result<Outcome, Box<dyn Error>> {
    match answer {
        RunResult::Ok(outcome) => Ok(outcome),
        other @ RunResult::Err(_) => Err(format!("expected an outcome, got {other:?}").into()),
    }
}

fn detail(answer: &RunResult) -> Result<&str, Box<dyn Error>> {
    match answer {
        RunResult::Err(RunError::Failed { detail }) => Ok(detail.as_str()),
        other => Err(format!("expected Failed, got {other:?}").into()),
    }
}

#[test]
fn a_run_answers_its_result_and_holds_settlement_until_it_is_done() -> TestResult {
    // Catches `on_run` answering on the dispatcher or detached from the caller's chain (settlement would come back
    // while the worker still talks to the daemon), or its result routed to the import completion.
    let inputs = Inputs::new(Vec::new())?;
    let (hex, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let stub = StubDaemon::bind()?;
    let mut harness = inputs.boot(&stub, FLAGS)?;
    let run = over(&harness, request);
    let output = built_work();
    let replies = script(&hex, &output).replies();
    let expected = replies.len();

    let (answer, read) = thread::scope(|scope| -> Result<_, Box<dyn Error>> {
        let served = scope.spawn(|| stub.answer(replies));
        let pending = harness.settle_run(&run);
        let read = stub.requests_read();
        let answer = harness.wait(pending);
        served.join().map_err(|_| "the stub daemon thread panicked")??;
        Ok((answer, read))
    })?;

    assert_eq!(read, expected, "the chain settled before the run's last request was served");
    outcome(answer)?;
    Ok(())
}

#[test]
fn a_second_run_waits_for_the_first_holding_its_settlement_and_both_answer() -> TestResult {
    // Catches a queued run dropped or starved (its reply never comes), a queued run started beside the first on a
    // budget with room for one (the stub would see the two runs' requests interleave), and a queued run whose
    // settlement hold was not taken at accept: its chain would settle while the first run still held the cores.
    let inputs = Inputs::new(Vec::new())?;
    let (hex, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let stub = StubDaemon::bind()?;
    let mut harness = inputs.boot(&stub, FLAGS)?;
    let run = over(&harness, request);
    let output = built_work();
    let per_run = script(&hex, &output).replies();
    let back_to_back: Vec<StubReply> = per_run.iter().cloned().chain(per_run.iter().cloned()).collect();
    let expected = back_to_back.len();

    let (first, second, read, requests) = thread::scope(|scope| -> Result<_, Box<dyn Error>> {
        let served = scope.spawn(|| stub.answer(back_to_back));
        let first = harness.send_run(&run);
        let second = harness.settle_run(&run);
        let read = stub.requests_read();
        let (first, second) = (harness.wait(first), harness.wait(second));
        let requests = served.join().map_err(|_| "the stub daemon thread panicked")??;
        Ok((first, second, read, requests))
    })?;

    assert_eq!(read, expected, "the queued run's chain settled before its requests were served");
    outcome(first)?;
    outcome(second)?;
    let lines = lines(&requests);
    assert_eq!(lines[..per_run.len()], lines[per_run.len()..], "the second run started only after the first ended");
    Ok(())
}

#[test]
fn a_retry_after_an_out_of_memory_kill_gets_twice_the_memory() -> TestResult {
    // Catches an out-of-memory kill read as an ordinary exit (137 would become an outcome the program sees), and the
    // estimate not wired to the run's completion: the retry would get the same memory that already ran out.
    let inputs = Inputs::new(Vec::new())?;
    let (hex, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let stub = StubDaemon::bind()?;
    let flags = [FLAGS, &["--workspace-budget-memory-bytes", "68719476736"][..]].concat();
    let mut harness = inputs.boot(&stub, &flags)?;
    let run = over(&harness, request);
    let mut killed = RunScript { environment: &hex, logs: &[], exit_code: 137, output: &[] }.replies();
    killed.truncate(8);
    killed.extend([
        StubReply::with_length(200, r#"{"State":{"ExitCode":137,"OOMKilled":true}}"#),
        StubReply::with_length(204, ""),
        StubReply::with_length(204, ""),
    ]);
    let both = killed.iter().cloned().chain(killed.iter().cloned()).collect();

    let (answers, requests) = serving(stub, both, || [harness.run(&run), harness.run(&run)])?;

    assert!(
        answers.iter().all(|answer| *answer == RunResult::Err(RunError::Exhausted(Resource::Memory))),
        "{answers:?}"
    );
    let memory = |request: &StubRequest| -> Result<serde_json::Value, Box<dyn Error>> {
        let spec: serde_json::Value = serde_json::from_slice(&request.body)?;
        Ok(spec["HostConfig"]["Memory"].clone())
    };
    let creates: Vec<&StubRequest> =
        requests.iter().filter(|request| request.line() == "POST /v1.44/containers/create").collect();
    let [first, retry] = creates.as_slice() else {
        return Err(format!("two step containers were created: {creates:?}").into());
    };
    assert_eq!(memory(first)?, serde_json::json!(1u64 << 30));
    assert_eq!(memory(retry)?, serde_json::json!(2u64 << 30));
    Ok(())
}

#[test]
fn a_run_resolves_creates_writes_starts_waits_reads_logs_and_output_then_removes() -> TestResult {
    // Catches a reordered or skipped call: the tree written after `start`, logs read before the wait, the output read
    // from a container other than the last, or a container or volume never removed. Also catches outputs staged
    // under other bytes than the daemon sent, a reply sent before the last stage is answered, and a tool record that
    // names a different file than the one resolved.
    let inputs = Inputs::new(Vec::new())?;
    let (hex, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let output = built_work();

    let (answer, requests, harness) = run_against(inputs, request, script(&hex, &output).replies(), FLAGS)?;

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
            format!("GET /v1.44/containers/{RUN_CONTAINER}/stats?stream=true"),
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
    assert!(harness.stores(&step.stderr.digest()), "the stderr blob is staged");
    stored::<Tree>(&harness, outcome.tree.digest())?;
    Ok(())
}

#[test]
fn a_missing_pointer_writes_a_new_data_volume_and_creates_the_pointer_after() -> TestResult {
    // Catches a run trusting a half-written volume: with no pointer (an orphaned data volume from a crashed write is
    // invisible; only the pointer proves completeness), the run writes a fresh data volume through the helper and
    // creates the pointer only after the write, keeping both out of per-run removal while `/work` is removed. Also
    // catches a helper mounting the miss anywhere but its path, and a pointer whose labels do not name the digest and
    // its data volume.
    let mut inputs = Inputs::new(Vec::new())?;
    let base = inputs.request("tool", "target")?;
    let (request, vendor) = vendored(&mut inputs, &base, b"pub fn vendor() {}\n")?;
    let hex = inputs.hex();
    let digest = vendor.digest().to_string();
    let pointer = format!("aether-workspace-mount-{digest}");
    let output = built_work();
    let miss = MountScript::Miss { hex: &digest, data_volume: "da7a0001", winner: None };

    let (answer, requests, _) = run_against(inputs, request, script(&hex, &output).mount_replies(&[miss]), FLAGS)?;

    outcome(answer)?;
    let mounts = [
        format!("GET /v1.44/volumes/{pointer}"),
        "POST /v1.44/volumes/create".to_owned(),
        "POST /v1.44/containers/create".to_owned(),
        format!("PUT /v1.44/containers/{RUN_HELPER}/archive?path=/vendor"),
        "POST /v1.44/volumes/create".to_owned(),
    ];
    assert_eq!(lines(&requests), run_lines(&hex, &mounts, &removal_lines(true)));
    let body = |index: usize| -> Result<serde_json::Value, Box<dyn Error>> {
        Ok(serde_json::from_slice(&requests.get(index).ok_or("the run made the request")?.body)?)
    };
    assert_eq!(body(4)?, serde_json::json!({ "Labels": { "aether.workspace.mount": digest } }));
    let helper = body(5)?;
    assert_eq!(helper["Image"], serde_json::json!(format!("aether-workspace-environment:{hex}")));
    assert_eq!(
        helper["HostConfig"]["Mounts"],
        serde_json::json!([{
            "Type": "volume",
            "Source": "da7a0001",
            "Target": "/vendor",
            "ReadOnly": false,
            "VolumeOptions": { "NoCopy": true },
        }])
    );
    assert_eq!(
        body(7)?,
        serde_json::json!({
            "Name": pointer,
            "Labels": { "aether.workspace.mount": digest, "aether.workspace.mount.data": "da7a0001" },
        })
    );
    assert!(mounts_read_only(&step_spec(&requests)?, "da7a0001", "/vendor"), "the step mounts the written tree");
    Ok(())
}

#[test]
fn a_second_run_with_the_same_digest_reuses_the_mount_without_writing() -> TestResult {
    // Catches the regression this cache exists for: the second run citing the same mount-tree digest inspects the
    // pointer and mounts its data volume, with no helper, no `PUT …/archive` for the mount, and no volume create
    // beyond `/work`.
    let mut inputs = Inputs::new(Vec::new())?;
    let base = inputs.request("tool", "target")?;
    let (request, vendor) = vendored(&mut inputs, &base, b"pub fn vendor() {}\n")?;
    let hex = inputs.hex();
    let digest = vendor.digest().to_string();
    let output = built_work();
    let first = script(&hex, &output).mount_replies(&[MountScript::Miss {
        hex: &digest,
        data_volume: "da7a0001",
        winner: None,
    }]);
    let second = script(&hex, &output).mount_replies(&[MountScript::Hit { hex: &digest, data_volume: "da7a0001" }]);
    let first_len = first.len();
    let stub = StubDaemon::bind()?;
    let mut harness = inputs.boot(&stub, FLAGS)?;
    let run = over(&harness, request);

    let (answers, requests) =
        serving(stub, first.into_iter().chain(second).collect(), || [harness.run(&run), harness.run(&run)])?;

    for answer in answers {
        outcome(answer)?;
    }
    let second = &requests[first_len..];
    let mounts = [format!("GET /v1.44/volumes/aether-workspace-mount-{digest}")];
    assert_eq!(lines(second), run_lines(&hex, &mounts, &removal_lines(false)));
    assert!(mounts_read_only(&step_spec(second)?, "da7a0001", "/vendor"), "the second step mounts the same tree");
    Ok(())
}

#[test]
fn a_lost_pointer_race_mounts_the_winner_and_removes_its_own_write() -> TestResult {
    // Catches a race that leaks or mounts the wrong volume: answered 409 on the pointer create, the run re-inspects,
    // mounts the winner's data volume read-only, and removes the data volume it wrote itself, which no pointer names.
    let mut inputs = Inputs::new(Vec::new())?;
    let base = inputs.request("tool", "target")?;
    let (request, vendor) = vendored(&mut inputs, &base, b"pub fn vendor() {}\n")?;
    let hex = inputs.hex();
    let digest = vendor.digest().to_string();
    let pointer = format!("aether-workspace-mount-{digest}");
    let output = built_work();
    let miss = MountScript::Miss { hex: &digest, data_volume: "da7a0003", winner: Some("da7af00d") };

    let (answer, requests, _) = run_against(inputs, request, script(&hex, &output).mount_replies(&[miss]), FLAGS)?;

    outcome(answer)?;
    let mounts = [
        format!("GET /v1.44/volumes/{pointer}"),
        "POST /v1.44/volumes/create".to_owned(),
        "POST /v1.44/containers/create".to_owned(),
        format!("PUT /v1.44/containers/{RUN_HELPER}/archive?path=/vendor"),
        "POST /v1.44/volumes/create".to_owned(),
        format!("GET /v1.44/volumes/{pointer}"),
    ];
    let removals = [removal_lines(true), vec!["DELETE /v1.44/volumes/da7a0003?force=true".to_owned()]].concat();
    assert_eq!(lines(&requests), run_lines(&hex, &mounts, &removals));
    assert!(mounts_read_only(&step_spec(&requests)?, "da7af00d", "/vendor"), "the step mounts the winner");
    Ok(())
}

#[test]
fn a_pointer_labelled_for_another_digest_is_refused_before_any_step() -> TestResult {
    // Catches a skipped label check running in the wrong filesystem: a pointer whose mount label does not name the
    // cited digest refuses as MountUnavailable before any container is created, while `/work` is still removed.
    let mut inputs = Inputs::new(Vec::new())?;
    let base = inputs.request("tool", "target")?;
    let (request, vendor) = vendored(&mut inputs, &base, b"pub fn vendor() {}\n")?;
    let hex = inputs.hex();
    let digest = vendor.digest().to_string();
    let replies = vec![
        StubReply::with_length(200, r#"{"Architecture":"x86_64","OSType":"linux"}"#),
        StubReply::with_length(200, format!(r#"{{"Config":{{"Labels":{{"aether.workspace.environment":"{hex}"}}}}}}"#)),
        StubReply::with_length(201, format!(r#"{{"Name":"{RUN_VOLUME}"}}"#)),
        pointer_reply(&digest, "00", "v9left"),
        StubReply::with_length(204, Vec::new()),
    ];

    let (answer, requests, _) = run_against(inputs, request, replies, FLAGS)?;

    assert_eq!(answer, RunResult::Err(RunError::Refused(Refusal::MountUnavailable)));
    assert_eq!(
        lines(&requests),
        [
            "GET /v1.44/info".to_owned(),
            format!("GET /v1.44/images/aether-workspace-environment:{hex}/json"),
            "POST /v1.44/volumes/create".to_owned(),
            format!("GET /v1.44/volumes/aether-workspace-mount-{digest}"),
            format!("DELETE /v1.44/volumes/{RUN_VOLUME}?force=true"),
        ]
    );
    Ok(())
}

/// The `containers/create` body of the step container: the last container a single-step run creates.
fn step_spec(requests: &[StubRequest]) -> Result<serde_json::Value, Box<dyn Error>> {
    let create = requests
        .iter()
        .rfind(|request| request.line() == "POST /v1.44/containers/create")
        .ok_or("a step container was created")?;
    Ok(serde_json::from_slice(&create.body)?)
}

/// Whether `spec` mounts `source` read-only at `target`.
fn mounts_read_only(spec: &serde_json::Value, source: &str, target: &str) -> bool {
    spec["HostConfig"]["Mounts"].as_array().is_some_and(|mounts| {
        mounts.iter().any(|mount| mount["Source"] == source && mount["Target"] == target && mount["ReadOnly"] == true)
    })
}

#[test]
fn a_transport_failure_mid_run_still_removes_everything_and_answers_failed_naming_the_call() -> TestResult {
    // Catches a failure path that skips cleanup (a container and a volume left per failed run), a failure reported as
    // a refusal, and a detail that does not say which call broke.
    let inputs = Inputs::new(Vec::new())?;
    let (hex, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let mut replies = script(&hex, &[]).replies();
    replies.truncate(5);
    replies.extend([StubReply::hang_up(), StubReply::with_length(204, ""), StubReply::with_length(204, "")]);

    let (answer, requests, _) = run_against(inputs, request, replies, FLAGS)?;

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
fn a_stored_blob_forged_on_disk_fails_the_run_before_any_step_starts_and_still_cleans_up() -> TestResult {
    // Catches a change that drops the workspace's own check on the blobs it streams into `/work`, believing the
    // journal checked them: the journal answers the stored file under the digest it was read by, unhashed, so only
    // the workspace's check stands between a forged file and a container.
    let inputs = Inputs::new(vec![("forged.txt", b"original bytes")])?;
    let (hex, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let forged = Ref::of_bytes(b"original bytes").digest().to_string();
    let mut replies = script(&hex, &[]).replies();
    replies.truncate(5);
    replies.extend([StubReply::with_length(204, ""), StubReply::with_length(204, "")]);
    let stub = StubDaemon::bind()?;
    let mut harness = inputs.boot(&stub, FLAGS)?;
    // Same length and kind prefix as the original, so only a digest check over the bytes catches it.
    fs::write(
        harness.journal_path().join("blobs").join(&forged[..2]).join(&forged),
        artifact_blob(OpaqueBytes::ID, b"modified bytes"),
    )?;

    let run = over(&harness, request);
    let (answer, requests) = serving(stub, replies, || harness.run(&run))?;

    let detail = detail(&answer)?;
    assert!(detail.contains("reading a blob failed"), "{detail}");
    assert_eq!(
        lines(&requests[4..]),
        [
            format!("PUT /v1.44/containers/{RUN_CONTAINER}/archive?path=/work"),
            format!("DELETE /v1.44/containers/{RUN_CONTAINER}?force=true&v=true"),
            format!("DELETE /v1.44/volumes/{RUN_VOLUME}?force=true"),
        ],
        "no step started, and cleanup still removed the container and its volume"
    );
    Ok(())
}

#[test]
fn a_step_past_the_deadline_is_killed_and_answers_exhausted_time() -> TestResult {
    // Catches a wait with no deadline (the scenario would hang on the held response), a timed-out step left running,
    // and a timeout reported as a failure or an outcome.
    let inputs = Inputs::new(Vec::new())?;
    let (hex, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let mut replies = script(&hex, &[]).replies();
    replies.truncate(7);
    replies.extend([
        StubReply::hold(),
        StubReply::with_length(204, ""),
        StubReply::with_length(204, ""),
        StubReply::with_length(204, ""),
    ]);
    let flags = [FLAGS, &["--workspace-default-deadline-millis", "300"][..]].concat();

    let (answer, requests, _) = run_against(inputs, request, replies, &flags)?;

    assert_eq!(answer, RunResult::Err(RunError::Exhausted(Resource::Time)));
    assert_eq!(
        lines(&requests[7..]),
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
fn an_image_labelled_for_another_environment_is_refused_before_anything_is_created() -> TestResult {
    // Catches the label check skipped (a run in the wrong root filesystem) or answered as a failure, and volumes
    // created before it.
    let inputs = Inputs::new(Vec::new())?;
    let request = inputs.request("tool", "target")?;
    let replies = vec![
        StubReply::with_length(200, r#"{"Architecture":"x86_64","OSType":"linux"}"#),
        StubReply::with_length(200, r#"{"Config":{"Labels":{"aether.workspace.environment":"00"}}}"#),
    ];

    let (answer, requests, _) = run_against(inputs, request, replies, FLAGS)?;

    assert_eq!(answer, RunResult::Err(RunError::Refused(Refusal::EnvironmentUnavailable)));
    assert_eq!(requests.len(), 2, "{:?}", lines(&requests));
    Ok(())
}

#[test]
fn a_toolchain_file_asking_for_more_than_the_environment_provides_is_refused_without_the_daemon() -> TestResult {
    // Catches a subset test turned around or skipped, and a toolchain check that runs after contacting the daemon:
    // the daemon is gone, so a check behind it would answer Failed. The neighbour asks only for what is provided and
    // gets past resolution, to the daemon.
    let wants = b"[toolchain]\nchannel = \"1.97.1\"\ncomponents = [\"rustfmt\", \"clippy\"]\n";
    let fits = b"[toolchain]\nchannel = \"1.97.1\"\ncomponents = [\"clippy\"]\nprofile = \"minimal\"\n";
    let refused = Inputs::new(vec![("rust-toolchain.toml", wants)])?;
    let accepted = Inputs::new(vec![("rust-toolchain.toml", fits)])?;
    let (refused_request, accepted_request) = (refused.request("tool", "target")?, accepted.request("tool", "target")?);
    let mut refused_harness = boot_without_daemon(vec![refused.batch], FLAGS)?;
    let mut accepted_harness = boot_without_daemon(vec![accepted.batch], FLAGS)?;

    let answer = refused_harness.run(&over(&refused_harness, refused_request));
    let neighbour = accepted_harness.run(&over(&accepted_harness, accepted_request));

    let provided = RustToolchain::new("1.97.1", vec!["clippy".to_owned()], Vec::new())?;
    let tree_wants = RustToolchain::new("1.97.1", vec!["clippy".to_owned(), "rustfmt".to_owned()], Vec::new())?;
    assert_eq!(
        answer,
        RunResult::Err(RunError::Refused(Refusal::ToolchainMismatch {
            tree_wants,
            environment_provides: Some(provided)
        }))
    );
    assert!(detail(&neighbour)?.starts_with("reading the daemon's platform:"), "{neighbour:?}");
    Ok(())
}

#[test]
fn a_tool_that_is_not_an_executable_in_the_root_is_unknown() -> TestResult {
    // Catches tool resolution that accepts any node at the tool's path, so a plain file would run, or that trusts the
    // table without walking the root.
    let inputs = Inputs::new(Vec::new())?;
    let (plain, absent) = (inputs.request("text", "target")?, inputs.request("cargo", "target")?);
    let mut harness = boot_without_daemon(vec![inputs.batch], FLAGS)?;

    let plain = harness.run(&over(&harness, plain));
    let absent = harness.run(&over(&harness, absent));

    assert_eq!(plain, RunResult::Err(RunError::Refused(Refusal::UnknownTool(ToolName::new("text")?))));
    assert_eq!(absent, RunResult::Err(RunError::Refused(Refusal::UnknownTool(ToolName::new("cargo")?))));
    Ok(())
}

#[test]
fn an_environment_the_journal_lacks_is_input_missing() -> TestResult {
    // Catches a missing input answered as an executor failure, which the driver would retry forever instead of
    // recording, and a missing input found only after contacting the daemon, which is gone here.
    let inputs = Inputs::new(Vec::new())?;
    let absent = Digest::from_bytes([9; 32]);
    let request = RunRequest { environment: Ref::from_digest(absent), ..inputs.request("tool", "target")? };
    let mut harness = boot_without_daemon(vec![inputs.batch], FLAGS)?;

    let answer = harness.run(&over(&harness, request));

    assert_eq!(answer, RunResult::Err(RunError::Refused(Refusal::InputMissing(absent))));
    Ok(())
}

#[test]
fn a_run_tree_the_journal_lacks_is_input_missing_before_the_daemon() -> TestResult {
    // Catches resolve losing the pre-daemon refusal now that it no longer reads the tree's closure: the tree is read
    // only as it is written, so without the root check a missing tree would reach the daemon, which is gone here, and
    // answer Failed.
    let inputs = Inputs::new(Vec::new())?;
    let absent = Digest::from_bytes([7; 32]);
    let request = RunRequest { tree: Ref::from_digest(absent), ..inputs.request("tool", "target")? };
    let mut harness = boot_without_daemon(vec![inputs.batch], FLAGS)?;

    let answer = harness.run(&over(&harness, request));

    assert_eq!(answer, RunResult::Err(RunError::Refused(Refusal::InputMissing(absent))));
    Ok(())
}

#[test]
fn an_output_holding_a_fifo_answers_failed_naming_it_and_still_cleans_up() -> TestResult {
    // Catches an output decoded under the userland rules or with the refusal swallowed, and a failed output that
    // skips the removals.
    let inputs = Inputs::new(Vec::new())?;
    let (hex, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let output = TarWriter::new().directory("work/").fifo("work/pipe").finish();

    let (answer, requests, _) = run_against(inputs, request, script(&hex, &output).replies(), FLAGS)?;

    let detail = detail(&answer)?;
    assert!(detail.contains("work/pipe"), "{detail}");
    assert_eq!(requests.len(), 15, "cleanup still ran: {:?}", lines(&requests));
    Ok(())
}

#[test]
fn a_nested_scratch_directory_is_absent_from_the_output_tree() -> TestResult {
    // Catches scratch left in the output (a result that depends on build state), and a removal that rebuilds the
    // wrong ancestors: `crates/app` must keep `src` beside the removed `target`, and `crates` its sibling.
    let inputs = Inputs::new(Vec::new())?;
    let (hex, request) = (inputs.hex(), inputs.request("tool", "crates/app/target")?);
    let output = TarWriter::new()
        .directory("work/")
        .directory("work/crates/")
        .file("work/crates/README", b"r\n")
        .directory("work/crates/app/")
        .file("work/crates/app/src", b"s\n")
        .directory("work/crates/app/target/")
        .finish();

    let (answer, _, harness) = run_against(inputs, request, script(&hex, &output).replies(), FLAGS)?;

    let app = tree_of(vec![("src", Node::File(Ref::of_bytes(b"s\n")))])?;
    let crates =
        tree_of(vec![("README", Node::File(Ref::of_bytes(b"r\n"))), ("app", Node::Directory(Ref::of_encoded(&app)?))])?;
    let work = tree_of(vec![("crates", Node::Directory(Ref::of_encoded(&crates)?))])?;
    let tree = outcome(answer)?.tree;
    assert_eq!(tree, Ref::of_encoded(&work)?);
    assert_eq!(stored::<Tree>(&harness, tree.digest())?, work, "the rebuilt root is staged");
    Ok(())
}

#[test]
fn the_same_run_twice_answers_byte_equal_results() -> TestResult {
    // Catches a result that is not a function of its inputs: a container id, a volume name, a timestamp, or map order
    // reaching the result or the output tree.
    let inputs = Inputs::new(Vec::new())?;
    let (hex, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let stub = StubDaemon::bind()?;
    let mut harness = inputs.boot(&stub, FLAGS)?;
    let run = over(&harness, request);
    let output = built_work();
    let both = script(&hex, &output).replies().into_iter().chain(script(&hex, &output).replies()).collect();

    let ([first, second], _) = serving(stub, both, || [harness.run(&run), harness.run(&run)])?;

    assert!(matches!(first, RunResult::Ok(_)), "{first:?}");
    assert_eq!(encode_to_vec(&first)?, encode_to_vec(&second)?);
    Ok(())
}

#[test]
fn a_step_with_stdin_attaches_before_start_and_streams_the_whole_blob() -> TestResult {
    // Catches stdin attached after `start` (a fast process would read an empty stdin), bytes other than the stored
    // blob's, and a stream never closed, which would leave the process waiting on stdin: the stub records the attach
    // only once the client closes its writing half.
    let inputs = Inputs::new(Vec::new())?;
    let hex = inputs.hex();
    let mut request = inputs.request("tool", "target")?;
    let mut steps = request.steps.as_slice().to_vec();
    steps[0].stdin = Some(Ref::of_bytes(b"fn main() {}\n"));
    request.steps = Steps::new(steps)?;
    let output = built_work();
    let mut replies = script(&hex, &output).replies();
    replies.insert(5, StubReply::upgrade());

    let (answer, requests, _) = run_against(inputs, request, replies, FLAGS)?;

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
    let spec = step_spec(&requests)?;
    assert_eq!((&spec["OpenStdin"], &spec["StdinOnce"]), (&true.into(), &true.into()));
    Ok(())
}

#[test]
fn the_step_container_carries_the_sandbox_pins_with_the_environment_overlaid_in_order() -> TestResult {
    // Catches a dropped pin (a writable root, the network on, swap beyond the memory limit, capabilities kept, no
    // tmpfs over scratch, the container free to use every host core) and an overlay in the wrong order: the step's
    // PATH must win over the environment's, and nothing may move SOURCE_DATE_EPOCH. The CPU, memory, and pids
    // settings must come from the run's allotment and the configured pids limit, not a constant.
    let inputs = Inputs::new(Vec::new())?;
    let hex = inputs.hex();
    let mut request = inputs.request("tool", "target")?;
    let mut steps = request.steps.as_slice().to_vec();
    steps[0].env = vec![EnvVar::new("PATH", "/opt/bin")?, EnvVar::new("SOURCE_DATE_EPOCH", "1")?];
    request.steps = Steps::new(steps)?;
    let output = built_work();

    let (_, requests, _) = run_against(inputs, request, script(&hex, &output).replies(), FLAGS)?;

    let spec = step_spec(&requests)?;
    let host = &spec["HostConfig"];
    assert_eq!(spec["Cmd"], serde_json::json!(["/usr/bin/tool", "--check"]));
    assert_eq!(spec["Env"], serde_json::json!(["PATH=/opt/bin", "SOURCE_DATE_EPOCH=315532800"]));
    assert_eq!((&spec["WorkingDir"], &spec["User"]), (&"/work".into(), &"0:0".into()));
    assert_eq!(host["ReadonlyRootfs"], true);
    assert_eq!(host["NetworkMode"], "none");
    assert_eq!(host["CpusetCpus"], "2-3");
    assert_eq!(host["NanoCpus"], 2_000_000_000u64);
    assert_eq!((&host["Memory"], &host["MemorySwap"]), (&(1u64 << 30).into(), &(1u64 << 30).into()));
    assert_eq!(host["PidsLimit"], 64);
    assert_eq!(host["CapDrop"], serde_json::json!(["ALL"]));
    assert_eq!(host["Tmpfs"], serde_json::json!({ "/work/target": "rw,exec" }));
    Ok(())
}

#[test]
fn a_stats_stream_that_hangs_up_leaves_the_answer_unchanged() -> TestResult {
    // Catches an observation that changes a result: a stats connection the daemon drops must neither fail the run nor
    // alter its outcome.
    let inputs = Inputs::new(Vec::new())?;
    let (hex, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let stub = StubDaemon::bind()?;
    let mut harness = inputs.boot(&stub, FLAGS)?;
    let run = over(&harness, request);
    let output = built_work();
    let mut dropped = script(&hex, &output).replies();
    dropped[6] = StubReply::hang_up();
    let both = script(&hex, &output).replies().into_iter().chain(dropped).collect();

    let ([sampled, unsampled], requests) = serving(stub, both, || [harness.run(&run), harness.run(&run)])?;

    assert!(matches!(sampled, RunResult::Ok(_)), "{sampled:?}");
    assert_eq!(encode_to_vec(&unsampled)?, encode_to_vec(&sampled)?);
    assert_eq!(requests.len(), 30, "the run went on past the dropped stats call: {:?}", lines(&requests));
    Ok(())
}

/// A tree three directories deep with files at every level, one of them over 1 MiB: a closure several reads deep.
fn nested_tree(batch: &mut Batch) -> Result<Ref<Tree>, Box<dyn Error>> {
    let [large, leaf_file, middle_file, sibling_file, top_file] =
        [&large_payload()[..], b"leaf\n", b"middle\n", b"sibling\n", b"top\n"]
            .map(|bytes| Node::File(batch.stage_bytes(bytes)));
    let leaf = directory(batch, vec![("large.bin", large), ("leaf.txt", leaf_file)])?;
    let middle = directory(batch, vec![("leaf", Node::Directory(leaf)), ("middle.txt", middle_file)])?;
    let sibling = directory(batch, vec![("sibling.txt", sibling_file)])?;
    directory(
        batch,
        vec![("middle", Node::Directory(middle)), ("sibling", Node::Directory(sibling)), ("top.txt", top_file)],
    )
}

/// A tree holding one directory of one more distinct small file than one batched read may name, so its blobs take
/// two batches.
fn wide_tree(batch: &mut Batch) -> Result<Ref<Tree>, Box<dyn Error>> {
    let names = (0..=ReadArtifacts::MAX_ARTIFACTS).map(|index| format!("f{index:05}")).collect::<Vec<_>>();
    let files =
        names.iter().map(|name| (name.as_str(), Node::File(batch.stage_bytes(name.as_bytes())))).collect::<Vec<_>>();
    let wide = directory(batch, files)?;
    directory(batch, vec![("wide", Node::Directory(wide))])
}

/// Stages a run's tree into a seed batch.
type StageTree = fn(&mut Batch) -> Result<Ref<Tree>, Box<dyn Error>>;

/// The `/work` archive a run over the tree `tree` stages writes into its first container, booted with `flags`.
fn work_archive(tree: StageTree, flags: &[&str]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut inputs = Inputs::new(Vec::new())?;
    inputs.tree = tree(&mut inputs.batch)?;
    let (hex, request) = (inputs.hex(), inputs.request("tool", "target")?);
    let output = built_work();

    let (answer, mut requests, _) = run_against(inputs, request, script(&hex, &output).replies(), flags)?;

    outcome(answer)?;
    Ok(requests.swap_remove(4).body)
}

#[test]
fn a_tree_over_the_prefetch_budget_writes_the_same_archive_as_one_that_fits() -> TestResult {
    // Catches a descend path that skips or reorders members: under an eight-byte budget no closure fits, so every
    // directory is read on its own and every blob on demand, and the archive must still be byte-equal to the one the
    // read-ahead writes when every window fits. Under 1 MiB, more than any small member and less than the large one,
    // the reservations throttle the read-ahead: one sibling waits for budget while an oversized directory is read
    // node alone, so an answer applied to the wrong directory, a released blob handed out again, or a throttled read
    // never sent would change or stall the archive.
    let fitting = work_archive(nested_tree, FLAGS)?;
    let descended = work_archive(nested_tree, &[FLAGS, &["--workspace-prefetch-bytes", "8"][..]].concat())?;
    let throttled = work_archive(nested_tree, &[FLAGS, &["--workspace-prefetch-bytes", "1048576"][..]].concat())?;

    assert!(fitting.len() > large_payload().len(), "the archive holds the large file");
    assert!(fitting == descended, "the descended archive differs from the fitting one");
    assert!(fitting == throttled, "the throttled archive differs from the fitting one");
    Ok(())
}

#[test]
fn a_directory_wider_than_one_batched_read_writes_the_same_archive_as_one_that_fits() -> TestResult {
    // Catches a second batched read that skips or repeats the first batch's last member: under an eight-byte budget
    // the wide directory's blobs are read in batches as the archive reaches them, and one batch cannot name them all.
    let fitting = work_archive(wide_tree, FLAGS)?;
    let batched = work_archive(wide_tree, &[FLAGS, &["--workspace-prefetch-bytes", "8"][..]].concat())?;

    assert!(fitting == batched, "the batched archive differs from the fitting one");
    Ok(())
}

#[test]
fn an_imported_tree_written_into_a_container_imports_back_to_the_same_tree() -> TestResult {
    // Catches a source that hands the encoder other bytes than the source stores (a blob cut at a read window, a
    // length from the wrong member, a tree loaded under the wrong digest) or a sink that stages other bytes than it
    // decoded: the archive a run writes for an imported tree, imported again, must name the same tree, whose digest
    // hashes every byte of every blob and tree.
    let inputs = Inputs::new(Vec::new())?;
    let hex = inputs.hex();
    let template = inputs.request("tool", "target")?;
    let stub = StubDaemon::bind()?;
    let mut harness = inputs.boot(&stub, FLAGS)?;
    let export = TarWriter::new()
        .directory("bin/")
        .executable("bin/tool", TOOL)
        .file("large.bin", &large_payload())
        .symlink("link", "bin/tool")
        .finish();
    let import = Import { image: ImageRef::new(IMAGE)?, source: harness.source() };
    let output = built_work();
    let replies = [StubReply::import_script(IMAGE, CONTAINER, &export), script(&hex, &output).replies()].concat();

    let ((imported, ran), requests) = answering(&stub, replies, || {
        let imported = harness.import(&import);
        let tree = match &imported {
            ImportResult::Ok { tree } => *tree,
            ImportResult::Err(_) => return (imported, None),
        };
        (imported, Some(harness.run(&over(&harness, RunRequest { tree, ..template }))))
    })?;
    let ImportResult::Ok { tree } = imported else {
        return Err(format!("the first import failed: {imported:?}").into());
    };
    outcome(ran.ok_or("the run was sent")?)?;
    let archive = &requests.get(9).ok_or("the run wrote /work")?.body;

    let replies = StubReply::import_script(IMAGE, CONTAINER, archive);
    let (reimported, _) = answering(&stub, replies, || harness.import(&import))?;
    assert_eq!(reimported, ImportResult::Ok { tree });
    Ok(())
}
