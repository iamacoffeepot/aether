use std::{env, fs};

use aether_actor::Addressable;
use aether_component::ComponentHostCapability;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult, LogTailResult};
use aether_resolve_actor_dogfood::{ProbeWorker, Setup, SpawnWorker};

fn main() {
    let wasm_path = env::args().nth(1).expect("usage: runner <component.wasm>");
    let wasm = fs::read(&wasm_path).expect("read component wasm");
    let mut harness = SubstrateHarness::builder()
        .with_component_host()
        .log_ring_capacity(Some(256))
        .build()
        .expect("boot SubstrateHarness with component host");

    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent {
                    wasm,
                    name: Some("resolve-dogfood".to_owned()),
                    config: Vec::new(),
                    export: None,
                },
            ),
        )])
        .expect("load operation");
    let (mailbox_id, name) = match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, name, capabilities } => {
            println!("LOAD_OK mailbox_id={mailbox_id:?} name={name} capabilities={capabilities:?}");
            (mailbox_id, name)
        }
        LoadResult::Err { error } => panic!("component load failed: {error}"),
    };

    harness
        .execute(vec![
            ("setup", HarnessOp::send_and_settle(&name, &Setup {})),
            ("spawn-worker", HarnessOp::send_and_settle(&name, &SpawnWorker {})),
            (
                "resolve-send-observe",
                HarnessOp::send_and_settle(&name, &ProbeWorker { value: 41 }),
            ),
        ])
        .expect("drive setup, named child spawn, typed recovery, and reply");

    let branch_name = format!("{name}/aether.embedded:branch");
    let worker_name = format!("{branch_name}/aether.embedded:alpha");
    let branch_logs = harness.log_tail(&branch_name, None, None);
    let worker_logs = harness.log_tail(&worker_name, None, None);
    let branch_text = format!("{branch_logs:#?}");
    let worker_text = format!("{worker_logs:#?}");

    println!("BRANCH_LOGS mailbox={branch_name}\n{branch_text}");
    println!("WORKER_LOGS mailbox={worker_name}\n{worker_text}");

    assert!(matches!(branch_logs, LogTailResult::Ok { .. }), "branch log query failed");
    assert!(matches!(worker_logs, LogTailResult::Ok { .. }), "worker log query failed");
    assert!(
        branch_text.contains("spawned worker alpha and deliberately discarded its mailbox id"),
        "branch did not record deliberate MailboxId discard"
    );
    assert!(
        branch_text.contains("resolved worker alpha by typed key and sent request"),
        "branch did not record typed keyed recovery"
    );
    assert!(
        branch_text.contains("observed worker alpha reply") && branch_text.contains("42"),
        "branch did not observe the worker's typed reply value"
    );
    assert!(
        worker_text.contains("worker alpha received request") && worker_text.contains("41"),
        "worker did not receive the typed request"
    );

    println!("DOGFOOD_SUCCESS entry_mailbox={mailbox_id:?} reply_value=42");
}
