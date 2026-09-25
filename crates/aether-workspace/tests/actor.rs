//! The `aether.workspace` actor composed on a [`SubstrateHarness`] over a temp
//! journal's artifact store, with its endpoint pointed at the stub daemon.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::error::Error;
use std::thread;

use aether_bloomery_journal::{ArtifactBatch, Journal};
use aether_bloomery_kinds::{Name, Node, Ref, Tree};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_workspace::testing::{RunScript, StubDaemon, StubReply, TarWriter};
use aether_workspace::{
    Environment, ImageRef, Import, ImportResult, Mounts, Network, Platform, Provides, Run, RunResult, Scratch, Step,
    Steps, Tool, ToolName, Tools, TreePath, WorkspaceCapability, WorkspaceConfig, WorkspaceParams,
};

type TestResult = Result<(), Box<dyn Error>>;

const IMAGE: &str = "debian@sha256:3333333333333333333333333333333333333333333333333333333333333333";

#[test]
fn an_import_answers_ok_and_holds_settlement_until_it_is_done() -> TestResult {
    // Catches the import run on the dispatcher or detached from the caller's
    // chain: settlement must not come back while the worker is still talking
    // to the daemon, and the reply must arrive through the task completion.
    // Also catches `init` choosing the canonical rules, which refuse the
    // export's absolute symlink.
    let temp = tempfile::tempdir()?;
    let store = Journal::open(&temp.path().join("journal"))?.artifact_store();
    let stub = StubDaemon::bind()?;
    let config = WorkspaceConfig { endpoint: Some(stub.endpoint()), ..WorkspaceConfig::default() };
    let mut harness = SubstrateHarness::builder()
        .with_actor_configured::<WorkspaceCapability>(WorkspaceParams { artifacts: Some(store) }, config)
        .build()?;
    let workspace = harness.actor_ref::<WorkspaceCapability>();
    let import = Import { image: ImageRef::new(IMAGE)? };
    // The absolute link decodes only under the userland rules `init` chooses.
    let export = TarWriter::new()
        .directory("etc/")
        .file("etc/hostname", b"ws\n")
        .symlink("etc/localtime", "/usr/share/zoneinfo/UTC")
        .finish();

    let served_before_settling = thread::scope(|scope| -> Result<bool, Box<dyn Error>> {
        let served = scope.spawn(|| stub.answer(StubReply::import_script(IMAGE, "c0ffee", &export)));
        harness.execute(vec![("settle", HarnessOp::send_and_settle(&workspace, &import))])?;
        let finished = served.is_finished();
        served.join().map_err(|_| "the stub thread panicked")??;
        Ok(finished)
    })?;
    assert!(served_before_settling, "the chain settled before the import's last request was served");

    let answer = thread::scope(|scope| -> Result<ImportResult, Box<dyn Error>> {
        let served = scope.spawn(|| stub.serve(StubReply::import_script(IMAGE, "c0ffee", &export)));
        let result = harness.execute(vec![("reply", HarnessOp::send_and_await_reply(&workspace, &import))])?;
        served.join().map_err(|_| "the stub thread panicked")??;
        Ok(result.reply::<ImportResult>("reply")?)
    })?;
    assert!(matches!(answer, ImportResult::Ok { .. }), "{answer:?}");
    Ok(())
}

#[test]
fn no_artifact_store_refuses_boot_naming_it() {
    // Catches a describe-only composition booting a workspace that would
    // answer every import with a failure, rather than refusing at boot.
    let error = SubstrateHarness::builder()
        .with_actor::<WorkspaceCapability>(WorkspaceParams { artifacts: None })
        .build()
        .err()
        .expect("boot without an artifact store must fail");

    let message = error.to_string();
    assert!(message.contains("artifact store"), "the refusal names the store: {message}");
}

#[test]
fn a_run_answers_its_result_and_holds_settlement_until_it_is_done() -> TestResult {
    // Catches `on_run` answering on the dispatcher or detached from the
    // caller's chain (settlement would come back while the worker still talks
    // to the daemon), or its result routed to the import completion.
    let temp = tempfile::tempdir()?;
    let store = Journal::open(&temp.path().join("journal"))?.artifact_store();
    let mut batch = store.batch()?;
    let (environment, tree) = stage_inputs(&mut batch)?;
    batch.commit()?;
    let stub = StubDaemon::bind()?;
    let config = WorkspaceConfig { endpoint: Some(stub.endpoint()), ..WorkspaceConfig::default() };
    let mut harness = SubstrateHarness::builder()
        .with_actor_configured::<WorkspaceCapability>(WorkspaceParams { artifacts: Some(store) }, config)
        .build()?;
    let workspace = harness.actor_ref::<WorkspaceCapability>();
    let step = Step { tool: ToolName::new("tool")?, args: Vec::new(), env: Vec::new(), stdin: None };
    let run = Run {
        tree,
        environment,
        mounts: Mounts::new(Vec::new())?,
        steps: Steps::new(vec![step])?,
        scratch: Scratch::new(Vec::new())?,
        network: Network::Off,
    };
    let hex = environment.digest().to_string();
    let output = TarWriter::new().directory("work/").file("work/out", b"o\n").finish();
    let script = RunScript { environment: &hex, logs: &[(1, b"ok\n")], exit_code: 0, output: &output };

    let served_before_settling = thread::scope(|scope| -> Result<bool, Box<dyn Error>> {
        let served = scope.spawn(|| stub.answer(script.replies()));
        harness.execute(vec![("settle", HarnessOp::send_and_settle(&workspace, &run))])?;
        let finished = served.is_finished();
        served.join().map_err(|_| "the stub thread panicked")??;
        Ok(finished)
    })?;
    assert!(served_before_settling, "the chain settled before the run's last request was served");

    let answer = thread::scope(|scope| -> Result<RunResult, Box<dyn Error>> {
        let served = scope.spawn(|| stub.serve(script.replies()));
        let result = harness.execute(vec![("reply", HarnessOp::send_and_await_reply(&workspace, &run))])?;
        served.join().map_err(|_| "the stub thread panicked")??;
        Ok(result.reply::<RunResult>("reply")?)
    })?;
    assert!(matches!(answer, RunResult::Ok(_)), "{answer:?}");
    Ok(())
}

/// Commit an environment whose root holds the executable `usr/bin/tool`,
/// and an empty run tree.
fn stage_inputs(batch: &mut ArtifactBatch) -> Result<(Ref<Environment>, Ref<Tree>), Box<dyn Error>> {
    let mut file = batch.blob(4)?;
    file.write_chunk(b"tool")?;
    let tool = file.finish()?;
    let bin = batch.stage_encoded(&tree(vec![("tool", Node::Executable(tool))])?)?;
    let usr = batch.stage_encoded(&tree(vec![("bin", Node::Directory(bin))])?)?;
    let root = batch.stage_encoded(&tree(vec![("usr", Node::Directory(usr))])?)?;
    let environment = batch.stage_encoded(&Environment {
        root,
        platform: Platform::new("x86_64-unknown-linux-gnu")?,
        provides: Provides { rust: None },
        tools: Tools::new(vec![Tool { name: ToolName::new("tool")?, path: TreePath::new("usr/bin/tool")? }])?,
        env: Vec::new(),
    })?;
    let work = batch.stage_encoded(&Tree::empty())?;
    Ok((environment, work))
}

fn tree(entries: Vec<(&str, Node)>) -> Result<Tree, Box<dyn Error>> {
    let mut map = BTreeMap::new();
    for (name, node) in entries {
        map.insert(Name::new(name)?, node);
    }
    Ok(Tree::new(map))
}
