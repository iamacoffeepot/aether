//! Phase 3 substrate-feature scenarios (issue 430). Each test boots
//! a `TestBench` and exercises one substrate primitive — input
//! subscription, drop, capture_frame round-trip, replace_component
//! (all via `aether-test-fixture-probe`), or the IO sink's
//! read/write/delete/list round trips (which talk directly to the
//! chassis `aether.fs` via `TestBench::send_and_await_reply`).
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The fixture's wasm hasn't been built — fixture-loading tests
//!   read `target/wasm32-unknown-unknown/{debug,release}/aether_test_fixture_probe.wasm`
//!   and skip with an `eprintln!` when it's absent. IO scenarios
//!   don't load the fixture, so they only need wgpu. CI builds the
//!   fixture wasm before invoking `cargo test`; setting
//!   `AETHER_REQUIRE_RUNTIME=1` (CI does) flips both skip points
//!   into hard panics so a missing pre-build is loud.
//!
//! All boot-time mechanics (wgpu probe, wasm locator, skip-or-panic
//! gate, `save://` sandbox) live in `aether_scenario::test_helpers`
//! (issue 460). Per issue 464, the sandbox flows in via
//! `TestBench::builder().namespace_roots(...)` rather than env-var
//! mutation.

use std::path::Path;

use aether_data::{Kind, mailbox_id_from_name};
use aether_kinds::{
    Delete, DeleteResult, DropComponent, FsError, List, ListResult, LoadComponent, MailEnvelope,
    Read, ReadResult, ReplaceComponent, Write, WriteResult,
};
use aether_scenario::test_helpers::{
    has_wgpu_adapter, init_save_sandbox, require_runtime, test_namespace_roots,
};
use aether_scenario::{decode_png, differs_from_background};
use aether_substrate_bundle::test_bench::TestBench;
use aether_test_fixture_probe::SetRender;

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary. Without the reference, the
// host-target rlib's descriptor symbols can be stripped by the linker
// and `aether_kinds::descriptors::all()` won't see fixture kinds.
use aether_test_fixture_probe as _;

/// Caller-supplied component name passed to `LoadComponent`.
const PROBE_NAME: &str = "probe";
/// Full trampoline address the substrate registers under post-issue-634
/// Phase 4. Mail destined for the loaded probe goes here, not to the
/// bare `PROBE_NAME` (which isn't a registered mailbox).
const PROBE_ADDRESS: &str = "aether.component.trampoline:probe";
const TICK_OBSERVED: &str = "aether.test_fixture.tick_observed";

/// Build a `MailEnvelope` for a `CaptureFrame` mail bundle. Uses
/// the kind's wire encoding (`encode_into_bytes`) so any K — cast
/// or postcard — packs correctly.
fn envelope<K: Kind>(recipient: &str, mail: &K) -> MailEnvelope {
    MailEnvelope {
        recipient_name: recipient.to_owned(),
        kind_name: K::NAME.to_owned(),
        payload: mail.encode_into_bytes(),
        count: 1,
    }
}

/// Loads the probe into the bench, blocking until the substrate
/// replies with `LoadResult` so subsequent `advance` calls see a
/// fully-instantiated and tick-subscribed component. Pre-Phase-4 of
/// issue 603 the bench's `aether.control` mailbox (renamed to
/// `aether.component` in issue 638 phase 3) served as a single FIFO
/// point for both load and advance; Phase 4 split advance onto
/// `aether.test_bench`, so load is no longer naturally ordered ahead
/// of advance — the test must await `LoadResult` explicitly.
fn load_probe(bench: &mut TestBench, wasm_path: &Path) {
    let wasm = std::fs::read(wasm_path).expect("read fixture wasm");
    let result: aether_kinds::LoadResult = bench
        .send_and_await_reply(
            "aether.component",
            &LoadComponent {
                wasm,
                name: Some(PROBE_NAME.to_owned()),
            },
        )
        .expect("await load_component reply");
    match result {
        aether_kinds::LoadResult::Ok { .. } => {}
        aether_kinds::LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

/// Subscribing the fixture to Tick yields exactly one
/// `tick_observed` broadcast per advance tick. Validates the
/// subscribe_input → tick fanout path end-to-end.
#[test]
fn input_subscription_yields_one_tick_observed_per_advance() {
    let Some(wasm_path) = require_runtime("aether_test_fixture_probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&mut bench, &wasm_path);

    bench.advance(5).expect("advance 5");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        5,
        "expected exactly 5 tick_observed broadcasts after advance(5); \
         observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// Dropping the probe stops further tick_observed broadcasts.
/// Validates that `aether.component.drop` removes the
/// mailbox from the input subscriber set so subsequent ticks don't
/// reach it (ADR-0021 + ADR-0038 actor lifecycle).
#[test]
fn drop_component_silences_tick_echoes() {
    let Some(wasm_path) = require_runtime("aether_test_fixture_probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&mut bench, &wasm_path);

    bench.advance(3).expect("pre-drop advance");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        3,
        "expected 3 tick_observed before drop; observed kinds: {:?}",
        bench.observed_kinds(),
    );

    // Phase 4 split advance off `aether.component` (formerly `aether.control`), so the drop mail no
    // longer naturally orders ahead of the next advance. Await
    // `DropResult` explicitly so the probe's mailbox is fully gone
    // before the next advance dispatches ticks.
    let probe_mbox = mailbox_id_from_name(PROBE_ADDRESS);
    let drop_result: aether_kinds::DropResult = bench
        .send_and_await_reply(
            "aether.component",
            &DropComponent {
                mailbox_id: probe_mbox,
            },
        )
        .expect("await drop_component reply");
    match drop_result {
        aether_kinds::DropResult::Ok => {}
        aether_kinds::DropResult::Err { error } => panic!("drop_component: {error}"),
    }

    let post_drop = bench.count_observed(TICK_OBSERVED);

    bench.advance(10).expect("post-drop advance");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        post_drop,
        "tick_observed count climbed after drop_component; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// `capture_frame` round-trip with non-empty mail bundles. The
/// pre-mail bundle flips the fixture's render state to "visible
/// red"; the captured PNG must contain at least one pixel that
/// diverges from the chassis clear color. The after-mail bundle
/// flips render back to invisible; a follow-up advance + plain
/// capture must produce a frame back at the clear color, proving
/// the after-mail cleanup ran.
#[test]
fn capture_frame_round_trip_runs_pre_and_after_mails() {
    let Some(wasm_path) = require_runtime("aether_test_fixture_probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&mut bench, &wasm_path);
    // Advance once so the probe is loaded + subscribed; the capture
    // path doesn't dispatch Tick on its own (`run_frame` runs with
    // dispatch_tick=false during capture), so we need at least one
    // prior tick to have wired the subscriber set.
    bench.advance(1).expect("priming advance");

    // Capture's `run_frame` runs with `dispatch_tick=false`, so the
    // probe won't auto-tick during the captured frame. The pre-mail
    // bundle wires it up manually: set_render flips state to "visible
    // red", and a synthesised `aether.tick` immediately drives the
    // probe's on_tick to emit a `DrawTriangle` into the bench's
    // frame_vertices buffer right before the GPU readback.
    let pre = vec![
        envelope(
            PROBE_ADDRESS,
            &SetRender {
                r: 200,
                g: 32,
                b: 32,
                visible: 1,
            },
        ),
        MailEnvelope {
            recipient_name: PROBE_ADDRESS.to_owned(),
            kind_name: "aether.tick".to_owned(),
            payload: Vec::new(),
            count: 1,
        },
    ];
    // After-mail bundle dispatches *after* the readback. Flips
    // render state to invisible so the post-cleanup capture is back
    // at the chassis clear color.
    let after = vec![envelope(
        PROBE_ADDRESS,
        &SetRender {
            r: 0,
            g: 0,
            b: 0,
            visible: 0,
        },
    )];
    let png = bench
        .capture_with_mails(pre, after)
        .expect("capture with mails");
    let img = decode_png(&png).expect("decode capture png");
    differs_from_background(&img, 5).expect("captured frame should contain a non-background pixel");

    // Cleanup ran: probe.render is now { visible: 0 }. Advance once
    // and capture again — the next tick won't emit DrawTriangle, so
    // the frame stays at clear color. (The next advance does fire a
    // tick against the probe, but with visible=0 the probe broadcasts
    // tick_observed without emitting any geometry.)
    bench.advance(1).expect("post-cleanup advance");
    let png2 = bench.capture().expect("plain capture after cleanup");
    let img2 = decode_png(&png2).expect("decode cleanup png");
    assert!(
        differs_from_background(&img2, 5).is_err(),
        "after after-mail cleanup the captured frame should be uniform clear color, \
         but at least one pixel still diverges (cleanup did not run)",
    );
}

/// `replace_component` preserves the mailbox identity across the
/// splice (ADR-0022 + ADR-0038). Loads the probe, lets it broadcast
/// N ticks, replaces the wasm at the same mailbox id with the same
/// fixture binary, and asserts the post-replace count climbs —
/// proving the new component instance inherits the input
/// subscriptions and continues receiving ticks at the original
/// mailbox.
#[test]
fn replace_component_preserves_mailbox_identity() {
    let Some(wasm_path) = require_runtime("aether_test_fixture_probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&mut bench, &wasm_path);

    bench.advance(3).expect("pre-replace advance");
    let pre_replace = bench.count_observed(TICK_OBSERVED);
    assert_eq!(
        pre_replace,
        3,
        "expected 3 tick_observed before replace; observed kinds: {:?}",
        bench.observed_kinds(),
    );

    // Phase 4 of issue 603 split advance off `aether.component`, so
    // replace mail no longer naturally orders ahead of the next
    // advance. Await `ReplaceResult` explicitly.
    let probe_mbox = mailbox_id_from_name(PROBE_ADDRESS);
    let wasm = std::fs::read(&wasm_path).expect("re-read fixture wasm");
    let replace_result: aether_kinds::ReplaceResult = bench
        .send_and_await_reply(
            "aether.component",
            &ReplaceComponent {
                mailbox_id: probe_mbox,
                wasm,
                drain_timeout_ms: None,
            },
        )
        .expect("await replace_component reply");
    match replace_result {
        aether_kinds::ReplaceResult::Ok { .. } => {}
        aether_kinds::ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    let post_replace_baseline = bench.count_observed(TICK_OBSERVED);
    bench.advance(4).expect("post-replace advance");
    let post_replace = bench.count_observed(TICK_OBSERVED);

    assert!(
        post_replace > post_replace_baseline,
        "tick_observed count did not climb after replace; \
         baseline={post_replace_baseline}, final={post_replace}; \
         observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// IO scenarios need wgpu (the bench unconditionally builds a
/// `Gpu` at boot) but not the fixture wasm. Skips on wgpu-less
/// runners and panics under `AETHER_REQUIRE_RUNTIME` so a
/// CI-side regression is loud.
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

const FS_MAILBOX: &str = "aether.fs";
const IO_NAMESPACE_SAVE: &str = "save";

/// `aether.fs.write` followed by `aether.fs.read` round-trips the
/// bytes through the local-file adapter (ADR-0041). Both replies
/// echo the originating namespace + path for correlation; the read
/// reply also carries the bytes verbatim.
#[test]
fn io_write_then_read_round_trips_in_save_namespace() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("test-bench-io");
    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let path = "io-roundtrip.bin".to_owned();
    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];

    let write_reply: WriteResult = bench
        .send_and_await_reply(
            FS_MAILBOX,
            &Write {
                namespace: IO_NAMESPACE_SAVE.to_owned(),
                path: path.clone(),
                bytes: payload.clone(),
            },
        )
        .expect("write reply");
    match write_reply {
        WriteResult::Ok {
            namespace,
            path: echoed_path,
        } => {
            assert_eq!(namespace, IO_NAMESPACE_SAVE);
            assert_eq!(echoed_path, path);
        }
        WriteResult::Err { error, .. } => panic!("write failed: {error:?}"),
    }

    let read_reply: ReadResult = bench
        .send_and_await_reply(
            FS_MAILBOX,
            &Read {
                namespace: IO_NAMESPACE_SAVE.to_owned(),
                path: path.clone(),
            },
        )
        .expect("read reply");
    match read_reply {
        ReadResult::Ok {
            namespace,
            path: echoed_path,
            bytes,
        } => {
            assert_eq!(namespace, IO_NAMESPACE_SAVE);
            assert_eq!(echoed_path, path);
            assert_eq!(bytes, payload);
        }
        ReadResult::Err { error, .. } => panic!("read failed: {error:?}"),
    }
}

/// `aether.fs.delete` removes a previously-written file; a
/// follow-up `aether.fs.read` of the same path returns
/// `Err { NotFound }`.
#[test]
fn io_delete_removes_written_file() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("test-bench-io");
    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let path = "io-delete.bin".to_owned();
    let _: WriteResult = bench
        .send_and_await_reply(
            FS_MAILBOX,
            &Write {
                namespace: IO_NAMESPACE_SAVE.to_owned(),
                path: path.clone(),
                bytes: vec![1, 2, 3],
            },
        )
        .expect("write reply");

    let delete_reply: DeleteResult = bench
        .send_and_await_reply(
            FS_MAILBOX,
            &Delete {
                namespace: IO_NAMESPACE_SAVE.to_owned(),
                path: path.clone(),
            },
        )
        .expect("delete reply");
    match delete_reply {
        DeleteResult::Ok { .. } => {}
        DeleteResult::Err { error, .. } => panic!("delete failed: {error:?}"),
    }

    let read_after_delete: ReadResult = bench
        .send_and_await_reply(
            FS_MAILBOX,
            &Read {
                namespace: IO_NAMESPACE_SAVE.to_owned(),
                path: path.clone(),
            },
        )
        .expect("read-after-delete reply");
    match read_after_delete {
        ReadResult::Ok { .. } => panic!("read should not have found a deleted file"),
        ReadResult::Err {
            error: FsError::NotFound,
            ..
        } => {}
        ReadResult::Err { error, .. } => panic!("expected NotFound, got {error:?}"),
    }
}

/// `aether.fs.list` enumerates entries under a prefix. After a
/// write to `<sandbox>/probe-list.bin`, listing the empty prefix
/// in `save` returns an entry list containing the bare filename.
#[test]
fn io_list_returns_written_path() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("test-bench-io");
    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let path = "probe-list.bin".to_owned();
    let _: WriteResult = bench
        .send_and_await_reply(
            FS_MAILBOX,
            &Write {
                namespace: IO_NAMESPACE_SAVE.to_owned(),
                path: path.clone(),
                bytes: vec![0],
            },
        )
        .expect("write reply");

    let list_reply: ListResult = bench
        .send_and_await_reply(
            FS_MAILBOX,
            &List {
                namespace: IO_NAMESPACE_SAVE.to_owned(),
                prefix: String::new(),
            },
        )
        .expect("list reply");
    match list_reply {
        ListResult::Ok { entries, .. } => {
            assert!(
                entries.iter().any(|e| e == &path),
                "expected entries to include {path:?}; got {entries:?}",
            );
        }
        ListResult::Err { error, .. } => panic!("list failed: {error:?}"),
    }
}

/// Reading a path that was never written returns
/// `Err { NotFound }`. Negative companion to the round-trip test.
#[test]
fn io_read_unknown_path_returns_not_found() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("test-bench-io");
    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let read_reply: ReadResult = bench
        .send_and_await_reply(
            FS_MAILBOX,
            &Read {
                namespace: IO_NAMESPACE_SAVE.to_owned(),
                path: "nonexistent-do-not-create.bin".to_owned(),
            },
        )
        .expect("read reply");
    match read_reply {
        ReadResult::Ok { .. } => panic!("read should not have found a never-written file"),
        ReadResult::Err {
            error: FsError::NotFound,
            ..
        } => {}
        ReadResult::Err { error, .. } => panic!("expected NotFound, got {error:?}"),
    }
}

// Pre-#775 the bench emitted `aether.observation.frame_stats` every
// 120 frames and a test verified one broadcast arrived after
// `advance(120)`. Issue 775 retired the broadcast cap, the frame_stats
// kind, and the helper that emitted it; this test went with them.
