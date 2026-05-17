//! End-to-end coverage for issue 776's substrate-side
//! `LogCapability` ring + `aether.log.read` / `aether.log.read_result`
//! kinds. Boots a `TestBench` (which registers `LogCapability` on the
//! `aether.log` mailbox), pushes a synthesised `LogBatch` through the
//! cap's handler, and drives the read surface via
//! `bench.send_and_await_reply`.
//!
//! The substrate-side actor-aware subscriber (issue #601) does NOT
//! route out-of-actor `tracing::*` events into the mail pipeline —
//! host-emitted events go to stderr only and don't reach
//! `engine_logs`. The route this test exercises is the same one any
//! in-actor `tracing::error!` lands on: a `LogBatch` mail to
//! `"aether.log"`. Driving the cap directly with a synthesised batch
//! is the same byte-shape (the actor-aware drain would emit the
//! identical mail) without standing up a fixture wasm just to make a
//! `LogEvent`.
//!
//! Skipped when no wgpu adapter is available (driverless Linux
//! runners without `mesa-vulkan-drivers`); CI's
//! `AETHER_REQUIRE_RUNTIME=1` flips that into a hard panic.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]

use aether_kinds::{LogBatch, LogEvent, LogRead, LogReadResult};
use aether_substrate_bundle::test_bench::{TestBench, test_helpers::has_wgpu_adapter};

const LOG_MAILBOX: &str = "aether.log";

fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = std::env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(
        !strict,
        "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available",
    );
    eprintln!("skipping: no wgpu adapter available");
    false
}

/// Read `LogRead` against the bench and unwrap the `Ok` arm. Tests
/// panic-and-print on `Err` rather than threading match across every
/// site — the cap's healthy path only returns `Ok`.
// `request` is owned for the same ergonomic reason `send_mail<K>()`
// takes `K` by value — tests build the request inline at the call
// site and immediately hand it off.
#[allow(clippy::needless_pass_by_value)]
fn read(
    bench: &mut TestBench,
    request: LogRead,
) -> (Vec<aether_kinds::LogEntry>, u64, Option<u64>) {
    let result: LogReadResult = bench
        .send_and_await_reply(LOG_MAILBOX, &request)
        .expect("log.read reply");
    match result {
        LogReadResult::Ok {
            entries,
            next_since,
            truncated_before,
        } => (entries, next_since, truncated_before),
        LogReadResult::Err { error } => panic!("LogReadResult::Err: {error}"),
    }
}

fn batch(level: u8, target: &str, message: &str) -> LogBatch {
    LogBatch {
        entries: vec![LogEvent {
            level,
            target: target.to_owned(),
            message: message.to_owned(),
        }],
    }
}

/// A synthesised `LogBatch` mailed to `"aether.log"` surfaces in the
/// ring with the right `level`, `target`, and `message` on the next
/// `LogRead`. Validates the chassis dispatch → cap handler → reply
/// correlation round-trip end-to-end.
#[test]
fn batch_mail_surfaces_in_engine_logs() {
    if !require_wgpu_only() {
        return;
    }
    let mut bench = TestBench::start_with_size(32, 32).expect("boot");

    // Establish a baseline cursor so the assertion below sees only
    // entries pushed by this test, not any boot-time chatter the
    // cap might have absorbed before we got here.
    let (_, baseline_cursor, _) = read(
        &mut bench,
        LogRead {
            max: 1_000,
            min_level: None,
            since: None,
        },
    );

    bench
        .send_mail(
            LOG_MAILBOX,
            &batch(4, "engine_logs_e2e_target", "marker payload"),
        )
        .expect("send LogBatch");

    let (entries, next_since, truncated_before) = read(
        &mut bench,
        LogRead {
            max: 1_000,
            min_level: None,
            since: Some(baseline_cursor),
        },
    );

    let hit = entries
        .iter()
        .find(|e| e.target == "engine_logs_e2e_target" && e.message == "marker payload")
        .unwrap_or_else(|| {
            panic!(
                "expected the synthesised LogBatch to surface in entries; observed: {entries:#?}"
            )
        });
    assert_eq!(hit.level, 4, "expected error level=4; got {}", hit.level);
    assert!(
        hit.sequence > baseline_cursor,
        "new entry must advance the cursor"
    );
    assert_eq!(
        next_since, hit.sequence,
        "next_since should match the highest sequence in the returned slice",
    );
    assert_eq!(
        truncated_before, None,
        "the ring should not have evicted between baseline and the post-emit read",
    );
}

/// `min_level: Some(3)` (warn+) filters out info-level entries on
/// the server side, even when both are present in the ring. Pushes
/// one info + one warn batch; the filtered read returns only the
/// warn, and `next_since` skips ahead to the warn's sequence.
#[test]
fn engine_logs_level_filter_drops_below_threshold() {
    if !require_wgpu_only() {
        return;
    }
    let mut bench = TestBench::start_with_size(32, 32).expect("boot");

    let (_, baseline_cursor, _) = read(
        &mut bench,
        LogRead {
            max: 1_000,
            min_level: None,
            since: None,
        },
    );

    bench
        .send_mail(LOG_MAILBOX, &batch(2, "level_filter_e2e", "info-level"))
        .expect("send info batch");
    bench
        .send_mail(LOG_MAILBOX, &batch(3, "level_filter_e2e", "warn-level"))
        .expect("send warn batch");

    // Unfiltered: both info + warn present.
    let (unfiltered, _, _) = read(
        &mut bench,
        LogRead {
            max: 1_000,
            min_level: None,
            since: Some(baseline_cursor),
        },
    );
    let unfiltered_count = unfiltered
        .iter()
        .filter(|e| e.target == "level_filter_e2e")
        .count();
    assert_eq!(unfiltered_count, 2, "both entries should be in the ring");

    // Filtered: warn only.
    let (filtered, _, _) = read(
        &mut bench,
        LogRead {
            max: 1_000,
            min_level: Some(3),
            since: Some(baseline_cursor),
        },
    );
    let filtered_entries: Vec<_> = filtered
        .iter()
        .filter(|e| e.target == "level_filter_e2e")
        .collect();
    assert_eq!(
        filtered_entries.len(),
        1,
        "min_level=3 should drop the info entry; observed: {filtered_entries:#?}",
    );
    assert_eq!(filtered_entries[0].message, "warn-level");
}
