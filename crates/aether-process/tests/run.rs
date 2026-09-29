//! The `aether.process` cap booted in `SubstrateHarness` and driven by mail:
//! each `run` goes through production dispatch, the cap's task queue, and the
//! `#[handler(task)]` completion that answers the caller.
#![cfg(unix)]

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_process::{ProcessCapability, ProcessConfig, ProcessError, ProcessParams, Run, RunResult};
use std::collections::HashSet;
use std::env;

/// One permitted binary and a single queue slot, so a refusal that leaks its
/// slot starves every later run.
fn harness() -> SubstrateHarness {
    let config = ProcessConfig {
        allowlist: HashSet::from(["cat=/bin/cat".to_owned()]),
        max_in_flight: 1,
        ..ProcessConfig::default()
    };
    SubstrateHarness::builder()
        .with_actor_configured::<ProcessCapability>(ProcessParams { work_root: env::temp_dir() }, config)
        .build()
        .expect("boot harness with the process cap")
}

fn run(binary: &str, stdin: &[u8]) -> Run {
    Run { binary: binary.to_owned(), args: Vec::new(), env: Vec::new(), stdin: stdin.to_vec(), timeout_millis: 0 }
}

/// An unlisted binary is refused, and the refusal frees its queue slot: with
/// `max_in_flight = 1`, a leaked slot would leave the second request queued
/// behind the first instead of refused.
#[test]
fn unlisted_binary_is_refused_without_leaking_a_slot() {
    let mut harness = harness();
    let process = harness.actor_ref::<ProcessCapability>();

    let result = harness
        .execute(vec![
            ("first", HarnessOp::send_and_await_reply(&process, &run("sh", b""))),
            ("second", HarnessOp::send_and_await_reply(&process, &run("sh", b""))),
        ])
        .expect("refused runs reply");

    for label in ["first", "second"] {
        match result.reply::<RunResult>(label).expect("decode run reply") {
            RunResult::Err { error: ProcessError::NotPermitted } => {}
            other => panic!("{label}: expected NotPermitted, got {other:?}"),
        }
    }
}

/// An allowlisted binary runs to completion off the dispatcher: `/bin/cat`
/// echoes its piped stdin, and the reply carries the captured stdout.
#[test]
fn allowlisted_cat_echoes_stdin() {
    let mut harness = harness();
    let process = harness.actor_ref::<ProcessCapability>();

    let reply = harness
        .execute(vec![("cat", HarnessOp::send_and_await_reply(&process, &run("cat", b"hello aether")))])
        .expect("cat run replies")
        .reply::<RunResult>("cat")
        .expect("decode run reply");

    match reply {
        RunResult::Ok { exit_code, stdout, stderr } => {
            assert_eq!(exit_code, Some(0));
            assert_eq!(stdout, b"hello aether");
            assert!(stderr.is_empty(), "cat wrote to stderr: {stderr:?}");
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}
