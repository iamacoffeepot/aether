//! The `aether.workspace` actor composed on a [`SubstrateHarness`] over a temp
//! journal's artifact store, with its endpoint pointed at the stub daemon.

#![cfg(unix)]

use std::error::Error;
use std::thread;

use aether_bloomery_journal::Journal;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_workspace::testing::{StubDaemon, StubReply, TarWriter};
use aether_workspace::{ImageRef, Import, ImportResult, WorkspaceCapability, WorkspaceConfig, WorkspaceParams};

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
