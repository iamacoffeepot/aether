//! Issue 1958: end-to-end proof that a WASM guest's `WasmCtx::sender()`
//! correctly surfaces the inbound mail's component origin.
//!
//! Uses the `source_observer` test-fixture component, whose `on_source_query`
//! manual handler reads `ctx.sender()` and replies a `SourceReport` carrying
//! whether it returned a proof. The reply lands on the origin the host stamped
//! on the query, the one `sender()` reads.
//!
//! Two invariants are checked:
//!
//! 1. **Session source returns `None`**: the harness sends `SourceQuery`
//!    directly (as a Session origin) via `send_and_await_reply`; the decoded reply
//!    must carry `had_sender: false`.
//!
//! 2. **Component source returns the sender**: the observer is loaded under
//!    its default name and a `source_forwarder` — which declares the observer
//!    as a dependency, so the load order is load-bearing — beside it. The
//!    harness triggers the forwarder with the fieldless `SendSourceQuery`; the
//!    forwarder sends `SourceQuery` through the reference it minted from that
//!    declaration (component-origin mail). The observer replies its report to
//!    the stamped origin, and the forwarder logs its arrival with the report's
//!    `had_sender` verdict and whether its response handler's `ctx.sender()` is
//!    the observer. After the chain settles, `log_tail` on the forwarder
//!    confirms the report reached it with `had_sender=true` and
//!    `replier_is_observer=true` — so the observer's sender was a proof of the
//!    forwarder, and the forwarder read the replier as its own sender.
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

use aether_actor::ErasedActorRef;
use aether_data::ErasedActorPath;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LogTailResult};
use aether_test_fixtures_bundle::{SourceForwarder, SourceObserver};
use aether_test_fixtures_kinds::{SendSourceQuery, SourceQuery, SourceReport};

const SOURCE_OBSERVER: &str = "aether_test_fixtures_bundle";

/// Load one non-entry actor out of the fixture bundle, under the actor's own
/// namespace, which is where a declared dependency looks for it.
fn load_fixture(harness: &mut SubstrateHarness, wasm: Vec<u8>, export: &str) -> (ErasedActorRef, ErasedActorPath) {
    harness
        .load_any(&LoadComponent { wasm, name: None, config: Vec::new(), export: Some(export.to_owned()) })
        .unwrap_or_else(|error| panic!("load_component {export}: {error}"))
}

/// Session-source case: the harness sends `SourceQuery` directly to the reader.
/// `sender()` must return `None` (no component origin) → `SourceReport
/// { had_sender: false }`.
#[test]
fn session_source_returns_none() {
    let Some(wasm_path) = require_wasm(SOURCE_OBSERVER) else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read source_observer wasm");
    let reader = harness
        .load::<SourceObserver>(LoadComponent { wasm, name: None, config: Vec::new(), export: None })
        .unwrap_or_else(|error| panic!("load_component test.source_observer: {error}"));

    let result = harness
        .execute(vec![("query", HarnessOp::send_and_await_reply(&reader, &SourceQuery))])
        .expect("send_and_await_reply SourceQuery");

    let report = result.reply::<SourceReport>("query").expect("decode SourceReport");

    assert!(!report.had_sender, "session-origin sender() must be None");
}

/// Component-source case: a forwarder component sends `SourceQuery` to the
/// observer through the reference its declared dependency minted.
/// `sender()` must return a proof, and the observer's reply to the stamped
/// origin must arrive at the forwarder. Verified by the forwarder's log.
#[test]
fn component_source_returns_sender_mailbox() {
    let Some(wasm_path) = require_wasm(SOURCE_OBSERVER) else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    // The reader loads under its own namespace and first: the forwarder
    // declares it as a dependency, so a load in the other order is refused.
    let wasm = fs::read(&wasm_path).expect("read source_observer wasm");
    let (_, reader_path) = load_fixture(&mut harness, wasm.clone(), "test.source_observer");
    let sender = harness
        .load::<SourceForwarder>(LoadComponent { wasm, name: None, config: Vec::new(), export: None })
        .unwrap_or_else(|error| panic!("load_component test.source_forwarder: {error}"));
    let sender_path = harness.actor_path(&sender);

    // `send_and_settle`: the whole chain (forwarder → reader → forwarder)
    // settles before `execute` returns, so the log entry is already in the ring.
    harness
        .execute(vec![("trigger", HarnessOp::send_and_settle(&sender, &SendSourceQuery))])
        .expect("SendSourceQuery to the forwarder");

    let logs = harness.log_tail(&sender, None, None);
    let found = match &logs {
        LogTailResult::Ok { entries, .. } => {
            entries.iter().any(|e| e.message == "source_report_received had_sender=true replier_is_observer=true")
        }
        LogTailResult::Err { error } => panic!("log_tail on forwarder failed: {error}"),
    };

    assert!(
        found,
        "the reader's report did not reach the forwarder with a sender proof;\n\
         reader: {reader_path}\n\
         sender: {sender_path}\n\
         forwarder log entries: {logs:?}",
    );
}
