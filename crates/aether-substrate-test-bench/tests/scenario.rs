//! Phase 3 substrate-feature scenarios (issue 430). Each test boots
//! a `TestBench` and exercises one substrate primitive — input
//! subscription, drop, capture_frame round-trip, replace_component
//! (all via `aether-test-fixture-probe`), or the IO sink's
//! read/write/delete/list round trips (which talk directly to the
//! chassis `aether.sink.io` via `TestBench::send_and_await_reply`).
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

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use aether_kinds::{
    Delete, DeleteResult, DropComponent, IoError, List, ListResult, LoadComponent, MailEnvelope,
    Read, ReadResult, ReplaceComponent, Write, WriteResult,
};
use aether_mail::{Kind, MailboxId, mailbox_id_from_name};
use aether_scenario::{decode_png, differs_from_background};
use aether_substrate_test_bench::TestBench;
use aether_test_fixture_probe::SetRender;

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary. Without the reference, the
// host-target rlib's descriptor symbols can be stripped by the linker
// and `aether_kinds::descriptors::all()` won't see fixture kinds.
use aether_test_fixture_probe as _;

/// Probe for any usable wgpu adapter.
fn has_wgpu_adapter() -> bool {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .is_ok()
}

/// Locate the fixture's wasm artifact under the workspace target dir.
/// Tries `release` first, then `debug` so either build profile works.
fn locate_fixture_wasm() -> Option<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root reachable from CARGO_MANIFEST_DIR");
    for profile in ["release", "debug"] {
        let path = workspace
            .join("target")
            .join("wasm32-unknown-unknown")
            .join(profile)
            .join("aether_test_fixture_probe.wasm");
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Common boot path: probe wgpu, locate the fixture wasm, return
/// both. `AETHER_REQUIRE_RUNTIME=1` turns either missing requirement
/// into a panic so CI failures are loud.
fn require_runtime() -> Option<PathBuf> {
    let strict = std::env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    if !has_wgpu_adapter() {
        assert!(
            !strict,
            "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available",
        );
        eprintln!("skipping: no wgpu adapter available");
        return None;
    }
    match locate_fixture_wasm() {
        Some(path) => Some(path),
        None => {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but aether_test_fixture_probe.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing this crate",
            );
            eprintln!(
                "skipping: aether_test_fixture_probe.wasm not built; run \
                 `cargo build --target wasm32-unknown-unknown -p aether-test-fixture-probe`",
            );
            None
        }
    }
}

const PROBE_NAME: &str = "probe";
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

/// Loads the probe into the bench. The load mail is queued and
/// processed during the next `advance` (the bench's queue is FIFO
/// and drains ahead of the chassis Advance event, so the freshly
/// instantiated probe is fully subscribed before any tick fans out).
fn load_probe(bench: &TestBench, wasm_path: &Path) {
    let wasm = std::fs::read(wasm_path).expect("read fixture wasm");
    bench
        .send_mail(
            "aether.control",
            &LoadComponent {
                wasm,
                name: Some(PROBE_NAME.to_owned()),
            },
        )
        .expect("dispatch load_component");
}

/// Subscribing the fixture to Tick yields exactly one
/// `tick_observed` broadcast per advance tick. Validates the
/// subscribe_input → tick fanout path end-to-end.
#[test]
fn input_subscription_yields_one_tick_observed_per_advance() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&bench, &wasm_path);

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
/// Validates that `aether.control.drop_component` removes the
/// mailbox from the input subscriber set so subsequent ticks don't
/// reach it (ADR-0021 + ADR-0038 actor lifecycle).
#[test]
fn drop_component_silences_tick_echoes() {
    let Some(wasm_path) = require_runtime() else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&bench, &wasm_path);

    bench.advance(3).expect("pre-drop advance");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        3,
        "expected 3 tick_observed before drop; observed kinds: {:?}",
        bench.observed_kinds(),
    );

    // Queue the drop. The same FIFO ordering that lets the load mail
    // beat the first tick fanout means the drop mail beats the next
    // tick fanout — by the time `Advance{1}`'s `run_frame` queries
    // subscribers, the probe's mailbox is already gone.
    let probe_mbox = MailboxId(mailbox_id_from_name(PROBE_NAME));
    bench
        .send_mail(
            "aether.control",
            &DropComponent {
                mailbox_id: probe_mbox,
            },
        )
        .expect("dispatch drop_component");
    bench.advance(1).expect("drop drain advance");

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
    let Some(wasm_path) = require_runtime() else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&bench, &wasm_path);
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
            PROBE_NAME,
            &SetRender {
                r: 200,
                g: 32,
                b: 32,
                visible: 1,
            },
        ),
        MailEnvelope {
            recipient_name: PROBE_NAME.to_owned(),
            kind_name: "aether.tick".to_owned(),
            payload: Vec::new(),
            count: 1,
        },
    ];
    // After-mail bundle dispatches *after* the readback. Flips
    // render state to invisible so the post-cleanup capture is back
    // at the chassis clear color.
    let after = vec![envelope(
        PROBE_NAME,
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
    let Some(wasm_path) = require_runtime() else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&bench, &wasm_path);

    bench.advance(3).expect("pre-replace advance");
    let pre_replace = bench.count_observed(TICK_OBSERVED);
    assert_eq!(
        pre_replace,
        3,
        "expected 3 tick_observed before replace; observed kinds: {:?}",
        bench.observed_kinds(),
    );

    let probe_mbox = MailboxId(mailbox_id_from_name(PROBE_NAME));
    let wasm = std::fs::read(&wasm_path).expect("re-read fixture wasm");
    bench
        .send_mail(
            "aether.control",
            &ReplaceComponent {
                mailbox_id: probe_mbox,
                wasm,
                drain_timeout_ms: None,
            },
        )
        .expect("dispatch replace_component");
    bench.advance(1).expect("replace drain advance");

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

/// Process-wide `save://` sandbox. `NamespaceRoots::from_env`
/// reads `AETHER_SAVE_DIR` once per chassis boot, so the env var
/// must be set before any TestBench boot. `OnceLock` linearises
/// the set; tests gate through `init_test_save_dir()` first.
static TEST_SAVE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Create the per-process sandbox (idempotent) and point
/// `AETHER_SAVE_DIR` at it. Subsequent `TestBench::start()` calls
/// see the env var and wire `save://` to this dir.
///
/// `set_var` is racy with concurrent `getenv` on POSIX, but
/// `OnceLock` linearises the set, and every IO test gates through
/// this helper before booting a TestBench — so by the time any
/// test thread reads env, the set has completed.
fn init_test_save_dir() -> &'static Path {
    TEST_SAVE_DIR.get_or_init(|| {
        let dir = std::env::temp_dir()
            .join(format!("aether-test-bench-io-tests-{}", std::process::id(),));
        std::fs::create_dir_all(&dir).expect("create test save dir");
        unsafe { std::env::set_var("AETHER_SAVE_DIR", &dir) };
        dir
    })
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

const IO_SINK: &str = "aether.sink.io";
const IO_NAMESPACE_SAVE: &str = "save";

/// `aether.io.write` followed by `aether.io.read` round-trips the
/// bytes through the local-file adapter (ADR-0041). Both replies
/// echo the originating namespace + path for correlation; the read
/// reply also carries the bytes verbatim.
#[test]
fn io_write_then_read_round_trips_in_save_namespace() {
    if !require_wgpu_only() {
        return;
    }
    init_test_save_dir();
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");

    let path = "io-roundtrip.bin".to_owned();
    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];

    let write_reply: WriteResult = bench
        .send_and_await_reply(
            IO_SINK,
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
            IO_SINK,
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

/// `aether.io.delete` removes a previously-written file; a
/// follow-up `aether.io.read` of the same path returns
/// `Err { NotFound }`.
#[test]
fn io_delete_removes_written_file() {
    if !require_wgpu_only() {
        return;
    }
    init_test_save_dir();
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");

    let path = "io-delete.bin".to_owned();
    let _: WriteResult = bench
        .send_and_await_reply(
            IO_SINK,
            &Write {
                namespace: IO_NAMESPACE_SAVE.to_owned(),
                path: path.clone(),
                bytes: vec![1, 2, 3],
            },
        )
        .expect("write reply");

    let delete_reply: DeleteResult = bench
        .send_and_await_reply(
            IO_SINK,
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
            IO_SINK,
            &Read {
                namespace: IO_NAMESPACE_SAVE.to_owned(),
                path: path.clone(),
            },
        )
        .expect("read-after-delete reply");
    match read_after_delete {
        ReadResult::Ok { .. } => panic!("read should not have found a deleted file"),
        ReadResult::Err {
            error: IoError::NotFound,
            ..
        } => {}
        ReadResult::Err { error, .. } => panic!("expected NotFound, got {error:?}"),
    }
}

/// `aether.io.list` enumerates entries under a prefix. After a
/// write to `<sandbox>/probe-list.bin`, listing the empty prefix
/// in `save` returns an entry list containing the bare filename.
#[test]
fn io_list_returns_written_path() {
    if !require_wgpu_only() {
        return;
    }
    init_test_save_dir();
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");

    let path = "probe-list.bin".to_owned();
    let _: WriteResult = bench
        .send_and_await_reply(
            IO_SINK,
            &Write {
                namespace: IO_NAMESPACE_SAVE.to_owned(),
                path: path.clone(),
                bytes: vec![0],
            },
        )
        .expect("write reply");

    let list_reply: ListResult = bench
        .send_and_await_reply(
            IO_SINK,
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
    init_test_save_dir();
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");

    let read_reply: ReadResult = bench
        .send_and_await_reply(
            IO_SINK,
            &Read {
                namespace: IO_NAMESPACE_SAVE.to_owned(),
                path: "nonexistent-do-not-create.bin".to_owned(),
            },
        )
        .expect("read reply");
    match read_reply {
        ReadResult::Ok { .. } => panic!("read should not have found a never-written file"),
        ReadResult::Err {
            error: IoError::NotFound,
            ..
        } => {}
        ReadResult::Err { error, .. } => panic!("expected NotFound, got {error:?}"),
    }
}

/// `aether.observation.frame_stats` is broadcast every 120 frames
/// (ADR-0023). Advancing exactly 120 ticks should yield one such
/// broadcast on the loopback. The bench emits this from its own
/// frame loop — no fixture component needed.
#[test]
fn frame_stats_broadcast_at_120_tick_cadence() {
    if !has_wgpu_adapter() {
        let strict = std::env::var("AETHER_REQUIRE_RUNTIME").is_ok();
        assert!(
            !strict,
            "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available",
        );
        eprintln!("skipping: no wgpu adapter available");
        return;
    }
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    bench.advance(120).expect("advance 120");
    let stats_count = bench.count_observed("aether.observation.frame_stats");
    assert_eq!(
        stats_count,
        1,
        "expected exactly one frame_stats broadcast at 120 ticks; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}
