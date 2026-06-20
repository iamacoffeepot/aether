//! Issue 1958: end-to-end proof that a WASM guest's `WasmCtx::source_mailbox()`
//! correctly surfaces the inbound mail's component origin.
//!
//! Uses the `source_observer` test-fixture component — a single-actor module
//! whose `on_source_query` manual handler reads `ctx.source_mailbox()`, logs
//! the raw id, and broadcasts `SourceReport { mailbox_id }` to the observer.
//!
//! Two invariants are checked:
//!
//! 1. **Session source returns `None`**: the bench sends `SourceQuery`
//!    directly (as a Session origin) via `send_and_await`; the decoded reply
//!    must carry `mailbox_id: 0`.
//!
//! 2. **Component source returns the sender's `MailboxId`**: a second
//!    instance ("sender") is loaded; the bench triggers it with
//!    `SendSourceQuery { to: reader_mailbox.0 }`. The sender forwards
//!    `SourceQuery` to the reader (component-origin mail). The reader logs
//!    `"source_mailbox={id}"`. After the chain settles, `log_tail` on the
//!    reader confirms the id equals the sender's `MailboxId`.
//!
//! This file is an integration test that requires a pre-built
//! `source_observer.wasm` fixture. CI builds component wasm before invoking
//! `cargo nextest`; `AETHER_REQUIRE_RUNTIME=1` flips the skip into a hard
//! panic so a missing pre-build is loud.

// Skip diagnostics emit via stderr so `cargo nextest` surfaces a visible
// "skipping: ..." line alongside `test ... ok`.
#![allow(clippy::print_stderr)]

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

use std::fs;

use aether_actor::Addressable;
use aether_capabilities::ComponentHostCapability;
use aether_data::MailboxId;
use aether_kinds::{LoadComponent, LoadResult, LogTailResult};
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::require_runtime};
use aether_test_fixtures_kinds::{SendSourceQuery, SourceQuery, SourceReport};

const SOURCE_OBSERVER: &str = "aether_test_fixtures_bundle";

fn load_source_observer(bench: &mut TestBench, wasm: Vec<u8>, name: &str) -> (MailboxId, String) {
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent {
                    wasm,
                    name: Some(name.to_owned()),
                    config: Vec::new(),
                    // `SourceObserver` is a non-entry actor in the bundle.
                    export: Some("test.source_observer".to_owned()),
                },
            ),
        )])
        .expect("load source_observer");
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok {
            mailbox_id,
            name: full_name,
            ..
        } => (mailbox_id, full_name),
        LoadResult::Err { error } => panic!("load_component {name}: {error}"),
    }
}

/// Session-source case: the bench sends `SourceQuery` directly to the reader.
/// `source_mailbox()` must return `None` (no component origin) → `SourceReport
/// { mailbox_id: 0 }`.
#[test]
fn session_source_returns_none() {
    let Some(wasm_path) = require_runtime(SOURCE_OBSERVER) else {
        return;
    };
    let mut bench = match TestBench::start_with_size(64, 48) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: TestBench boot failed (likely no wgpu adapter): {e}");
            return;
        }
    };
    let wasm = fs::read(&wasm_path).expect("read source_observer wasm");
    let (_, reader_addr) = load_source_observer(&mut bench, wasm, "reader");

    let result = bench
        .execute(vec![(
            "query",
            BenchOp::send_and_await(&reader_addr, &SourceQuery),
        )])
        .expect("send_and_await SourceQuery");

    let report = result
        .reply::<SourceReport>("query")
        .expect("decode SourceReport");

    assert_eq!(
        report.mailbox_id, 0,
        "session-origin source_mailbox() must be None (mailbox_id 0), got {:#x}",
        report.mailbox_id,
    );
}

/// Component-source case: a sender component forwards `SourceQuery` to the
/// reader. `source_mailbox()` must return `Some(sender_mailbox)`. Verified by
/// checking the value the reader logged via `log_tail`.
#[test]
fn component_source_returns_sender_mailbox() {
    let Some(wasm_path) = require_runtime(SOURCE_OBSERVER) else {
        return;
    };
    let mut bench = match TestBench::start_with_size(64, 48) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: TestBench boot failed (likely no wgpu adapter): {e}");
            return;
        }
    };

    let wasm = fs::read(&wasm_path).expect("read source_observer wasm");
    let (reader_mailbox, reader_addr) = load_source_observer(&mut bench, wasm.clone(), "reader");
    let (sender_mailbox, sender_addr) = load_source_observer(&mut bench, wasm, "sender");

    // Fire-and-settle: the whole chain (sender → reader handler) settles
    // before `execute` returns, so the log entry is already in the ring.
    bench
        .execute(vec![(
            "trigger",
            BenchOp::send_mail(
                &sender_addr,
                &SendSourceQuery {
                    to: reader_mailbox.0,
                },
            ),
        )])
        .expect("SendSourceQuery to sender");

    // Read the reader's log ring — the handler logs
    // "source_mailbox={id}" on every `SourceQuery` dispatch.
    let expected_msg = format!("source_mailbox={}", sender_mailbox.0);
    let logs = bench.log_tail(&reader_addr, None);
    let found = match &logs {
        LogTailResult::Ok { entries, .. } => entries.iter().any(|e| e.message == expected_msg),
        LogTailResult::Err { error } => panic!("log_tail on reader failed: {error}"),
    };

    assert!(
        found,
        "reader did not log the expected source_mailbox id;\n\
         expected message: {expected_msg:?}\n\
         sender_mailbox:   {sender_mailbox:?}\n\
         reader_mailbox:   {reader_mailbox:?}\n\
         reader_addr:      {reader_addr:?}\n\
         sender_addr:      {sender_addr:?}\n\
         log entries: {logs:?}",
    );
}
