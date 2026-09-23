//! Issue 1958: end-to-end proof that a WASM guest's `WasmCtx::sender()`
//! correctly surfaces the inbound mail's component origin.
//!
//! Uses the `source_observer` test-fixture component, whose `on_source_query`
//! manual handler reads `ctx.sender()` and answers `SourceReport` through it
//! (or by reply, when there is no component sender).
//!
//! Two invariants are checked:
//!
//! 1. **Session source returns `None`**: the harness sends `SourceQuery`
//!    directly (as a Session origin) via `send_and_await_reply`; the decoded reply
//!    must carry `mailbox_id: 0`.
//!
//! 2. **Component source returns the sender**: the observer is loaded under
//!    its default name and a `source_forwarder` — which declares the observer
//!    as a dependency, so the load order is load-bearing — beside it. The
//!    harness triggers the forwarder with the fieldless `SendSourceQuery`; the
//!    forwarder sends `SourceQuery` through the reference it minted from that
//!    declaration (component-origin mail). The observer sends its report back
//!    through `ctx.sender()`, and the forwarder logs its arrival. After the
//!    chain settles, `log_tail` on the forwarder confirms the report reached
//!    it — so the observer's sender was the forwarder.
//!
//! This file is an integration test that requires a pre-built
//! `source_observer.wasm` fixture. CI builds component wasm before invoking
//! `cargo nextest`; `AETHER_REQUIRE_RUNTIME=1` flips the skip into a hard
//! panic so a missing pre-build is loud.

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

use std::fs;

use aether_actor::Addressable;
use aether_component::ComponentHostCapability;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult, LogTailResult};
use aether_test_fixtures_kinds::{SendSourceQuery, SourceQuery, SourceReport};

const SOURCE_OBSERVER: &str = "aether_test_fixtures_bundle";

/// Load one non-entry actor out of the fixture bundle, under `name` or — with
/// `None` — under the actor's own namespace, which is where a declared
/// dependency looks for it.
fn load_fixture(harness: &mut SubstrateHarness, wasm: Vec<u8>, export: &str, name: Option<&str>) -> String {
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent {
                    wasm,
                    name: name.map(str::to_owned),
                    config: Vec::new(),
                    export: Some(export.to_owned()),
                },
            ),
        )])
        .expect("load fixture actor");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { path, .. } => path.to_string(),
        LoadResult::Err { error } => panic!("load_component {export} as {name:?}: {error}"),
    }
}

fn load_source_observer(harness: &mut SubstrateHarness, wasm: Vec<u8>, name: &str) -> String {
    load_fixture(harness, wasm, "test.source_observer", Some(name))
}

/// Session-source case: the harness sends `SourceQuery` directly to the reader.
/// `sender()` must return `None` (no component origin) → `SourceReport
/// { mailbox_id: 0 }`.
#[test]
fn session_source_returns_none() {
    let Some(wasm_path) = require_wasm(SOURCE_OBSERVER) else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read source_observer wasm");
    let reader_addr = load_source_observer(&mut harness, wasm, "reader");

    let result = harness
        .execute(vec![("query", HarnessOp::send_and_await_reply(&reader_addr, &SourceQuery))])
        .expect("send_and_await_reply SourceQuery");

    let report = result.reply::<SourceReport>("query").expect("decode SourceReport");

    assert_eq!(
        report.mailbox_id, 0,
        "session-origin sender() must be None (mailbox_id 0), got {:#x}",
        report.mailbox_id,
    );
}

/// Component-source case: a forwarder component sends `SourceQuery` to the
/// observer through the reference its declared dependency minted.
/// `sender()` must return the forwarder, so the report the observer sends
/// through it arrives at the forwarder. Verified by the forwarder's log.
#[test]
fn component_source_returns_sender_mailbox() {
    let Some(wasm_path) = require_wasm(SOURCE_OBSERVER) else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    // The reader loads under its own namespace and first: the forwarder
    // declares it as a dependency, so a load in the other order is refused.
    let wasm = fs::read(&wasm_path).expect("read source_observer wasm");
    let reader_addr = load_fixture(&mut harness, wasm.clone(), "test.source_observer", None);
    let sender_addr = load_fixture(&mut harness, wasm, "test.source_forwarder", None);

    // `send_and_settle`: the whole chain (forwarder → reader → forwarder)
    // settles before `execute` returns, so the log entry is already in the ring.
    harness
        .execute(vec![("trigger", HarnessOp::send_and_settle(&sender_addr, &SendSourceQuery))])
        .expect("SendSourceQuery to the forwarder");

    let logs = harness.log_tail(&sender_addr, None, None);
    let found = match &logs {
        LogTailResult::Ok { entries, .. } => entries.iter().any(|e| e.message == "source_report_received"),
        LogTailResult::Err { error } => panic!("log_tail on forwarder failed: {error}"),
    };

    assert!(
        found,
        "the reader's report did not reach the forwarder through its sender;\n\
         reader_addr: {reader_addr:?}\n\
         sender_addr: {sender_addr:?}\n\
         forwarder log entries: {logs:?}",
    );
}
