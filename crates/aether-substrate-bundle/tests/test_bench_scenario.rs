//! Phase 3 substrate-feature scenarios (issue 430). Each test boots
//! a `TestBench` and exercises one substrate primitive — input
//! subscription, drop, `capture_frame` round-trip, `replace_component`
//! (all via `aether-test-fixtures`'s `probe` cdylib), or the chassis `aether.fs`
//! adapter's read/write/delete/list round trips — driving every step
//! through `TestBench::execute` (issue 868).
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The fixture's wasm hasn't been built — fixture-loading tests
//!   read `target/wasm32-unknown-unknown/{debug,release}/examples/probe.wasm`
//!   and skip with an `eprintln!` when it's absent. fs scenarios
//!   don't load the fixture, so they only need wgpu. CI builds the
//!   fixture wasm before invoking `cargo test`; setting
//!   `AETHER_REQUIRE_RUNTIME=1` (CI does) flips both skip points
//!   into hard panics so a missing pre-build is loud.
//!
//! All boot-time mechanics (wgpu probe, wasm locator, skip-or-panic
//! gate, `save://` sandbox) live in
//! `aether_substrate_bundle::test_bench::test_helpers` (issues 460 +
//! 821). Per issue 464, the sandbox flows in via
//! `TestBench::builder().namespace_roots(...)` rather than env-var
//! mutation.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]

use std::path::Path;

use aether_data::{Kind, MailboxId};
use aether_kinds::{
    Delete, DeleteResult, DropComponent, DropResult, FsError, List, ListResult, LoadComponent,
    LoadResult, MailEnvelope, Ping, Read, ReadResult, ReplaceComponent, ReplaceResult, Write,
    WriteResult,
};
use aether_substrate_bundle::test_bench::{
    BenchOp, TestBench,
    test_helpers::{has_wgpu_adapter, init_save_sandbox, require_runtime, test_namespace_roots},
    visual::{decode_png, differs_from_background},
};
use aether_test_fixtures::SetRender;

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary. Without the reference, the
// host-target rlib's descriptor symbols can be stripped by the linker
// and `aether_kinds::descriptors::all()` won't see fixture kinds.
#[allow(unused_imports)]
use aether_test_fixtures as _;
use std::env;
use std::fs;

/// Caller-supplied component name passed to `LoadComponent`.
const PROBE_NAME: &str = "probe";
/// Full trampoline address the substrate registers under post-issue-634
/// Phase 4. Mail destined for the loaded probe goes here, not to the
/// bare `PROBE_NAME` (which isn't a registered mailbox). Built from
/// `WasmTrampoline::NAMESPACE` — the cap-owned single source of truth
/// post issue 654.
fn probe_address() -> String {
    use aether_actor::Actor;
    format!(
        "{}:{}",
        aether_capabilities::WasmTrampoline::NAMESPACE,
        PROBE_NAME,
    )
}
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

/// Load the probe into the bench via `execute`, blocking on the
/// `LoadResult` reply so subsequent `advance` ops see a
/// fully-instantiated and tick-subscribed component. Returns the
/// loaded component's `MailboxId` (the trampoline address), which
/// the drop / replace scenarios target. Pre-Phase-4 of issue 603 the
/// bench's `aether.control` mailbox (renamed to `aether.component` in
/// issue 638 phase 3) served as a single FIFO point for both load and
/// advance; Phase 4 split advance onto `aether.test_bench`, so load is
/// no longer naturally ordered ahead of advance — `SendAndAwait`
/// blocks on `LoadResult` before returning.
fn load_probe(bench: &mut TestBench, wasm_path: &Path) -> MailboxId {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(PROBE_NAME.to_owned()),
                    config: Vec::new(),
                    export: None,
                },
            ),
        )])
        .expect("load sequence");
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

/// Subscribing the fixture to Tick yields exactly one
/// `tick_observed` broadcast per advance tick. Validates the
/// `subscribe_input` → tick fanout path end-to-end.
#[test]
fn input_subscription_yields_one_tick_observed_per_advance() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&mut bench, &wasm_path);

    bench
        .execute(vec![("advance", BenchOp::advance(5))])
        .expect("advance 5");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        5,
        "expected exactly 5 tick_observed broadcasts after advance(5); \
         observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// ADR-0096: a multi-actor module (`export!(RootManager, Panel)`) loads
/// through the unmodified host, instantiating its entry export — the
/// first type in the list, `RootManager` — via the boxed
/// `ErasedFfiActor` path. Omitting `name` exercises the `aether.namespace`
/// section, which carries the entry type's `NAMESPACE` (`ui.root`), and
/// the `LoadResult` capabilities come from the entry type's
/// `aether.kinds.inputs` manifest. Proves init-through-the-box and the
/// multi-actor section emission end-to-end; selecting the `Panel` export
/// is the follow-on.
#[test]
fn multi_actor_module_loads_entry_export() {
    let Some(wasm_path) = require_runtime("multi_actor") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    // No name: resolve from the entry type's aether.namespace section.
                    name: None,
                    config: Vec::new(),
                    // No selector: load the entry export (RootManager).
                    export: None,
                },
            ),
        )])
        .expect("load sequence");
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok {
            name, capabilities, ..
        } => {
            assert!(
                name.ends_with(":ui.root"),
                "entry export should resolve to the first type's NAMESPACE (ui.root); got {name}",
            );
            assert!(
                !capabilities.handlers.is_empty(),
                "entry export RootManager declares a Ping handler; capabilities.handlers was empty",
            );
        }
        LoadResult::Err { error } => panic!("multi-actor load failed: {error}"),
    }
}

/// ADR-0096: passing `export: "ui.panel"` instantiates the non-entry
/// type from the same multi-actor module. The host resolves the
/// selector to the actor-type tag, `init_typed_p32` constructs `Panel`
/// (not the entry `RootManager`), the trampoline name defaults to the
/// selected type's namespace (`:ui.panel`), and the `LoadResult`
/// capabilities come from `Panel`'s `aether.kinds.inputs` group — which
/// carries a `#[fallback]` the entry type lacks, so the reply proves
/// the right group was selected.
#[test]
fn multi_actor_module_loads_selected_export() {
    let Some(wasm_path) = require_runtime("multi_actor") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    // No name: defaults to the selected export's namespace.
                    name: None,
                    config: Vec::new(),
                    export: Some("ui.panel".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok {
            name, capabilities, ..
        } => {
            assert!(
                name.ends_with(":ui.panel"),
                "selected export should resolve to Panel's NAMESPACE (ui.panel); got {name}",
            );
            assert!(
                capabilities.fallback.is_some(),
                "Panel declares a #[fallback]; selecting it must surface that group's capabilities, \
                 not the entry RootManager's strict-receiver group",
            );
        }
        LoadResult::Err { error } => panic!("multi-actor select-export load failed: {error}"),
    }
}

/// ADR-0096: an export selector that names no type the module exports
/// is a clean `LoadResult::Err`, not a silent fall-through to the entry
/// type. The error names the requested export.
#[test]
fn multi_actor_unknown_export_errors() {
    let Some(wasm_path) = require_runtime("multi_actor") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: None,
                    config: Vec::new(),
                    export: Some("ui.does_not_exist".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Err { error } => {
            assert!(
                error.contains("ui.does_not_exist"),
                "unknown-export error should name the requested export; got {error}",
            );
        }
        LoadResult::Ok { name, .. } => {
            panic!("unknown export should fail the load, not fall through; loaded {name}")
        }
    }
}

/// ADR-0097: a loaded `RootManager` spawns a `Panel` sibling at runtime
/// via `ctx.spawn_child::<Panel>`. Pinging `RootManager` triggers the
/// spawn; the spawned `Panel` registers at
/// `aether.component.trampoline:ui.panel.0` (Counter discriminator), and
/// pinging *it* makes it broadcast a `TickObserved` to the bench
/// observer — proving the spawned sibling is addressable and dispatches.
/// The fire-and-settle send blocks until the whole tree (including the
/// spawned trampoline's init) drains, so the panel is registered before
/// the second send routes.
#[test]
fn multi_actor_sibling_spawn() {
    let Some(wasm_path) = require_runtime("multi_actor") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: None,
                    config: Vec::new(),
                    export: None,
                },
            ),
        )])
        .expect("load sequence");
    let root_name = match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { name, .. } => name,
        LoadResult::Err { error } => panic!("multi-actor load failed: {error}"),
    };
    assert!(
        root_name.ends_with(":ui.root"),
        "entry load should resolve to ui.root; got {root_name}",
    );

    bench
        .execute(vec![
            // RootManager spawns a Panel sibling (Counter → ui.panel.0).
            (
                "spawn",
                BenchOp::send_mail::<Ping>(root_name.as_str(), &Ping { seq: 0 }),
            ),
            // The spawned Panel broadcasts TickObserved when pinged.
            (
                "ping_panel",
                BenchOp::send_mail::<Ping>(
                    "aether.component.trampoline:ui.panel.0",
                    &Ping { seq: 1 },
                ),
            ),
        ])
        .expect("spawn + ping sequence");

    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        1,
        "the spawned Panel (ui.panel.0) should have dispatched its ping and broadcast once; \
         observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// Dropping the probe stops further `tick_observed` broadcasts.
/// Validates that `aether.component.drop` removes the
/// mailbox from the input subscriber set so subsequent ticks don't
/// reach it (ADR-0021 + ADR-0038 actor lifecycle).
#[test]
fn drop_component_silences_tick_echoes() {
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let probe_mbox = load_probe(&mut bench, &wasm_path);

    bench
        .execute(vec![("warm", BenchOp::advance(3))])
        .expect("pre-drop advance");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        3,
        "expected 3 tick_observed before drop; observed kinds: {:?}",
        bench.observed_kinds(),
    );

    // Phase 4 split advance off `aether.component` (formerly
    // `aether.control`), so the drop mail no longer naturally orders
    // ahead of the next advance. `SendAndAwait` blocks on `DropResult`
    // so the probe's mailbox is fully gone before the next advance.
    let dropped = bench
        .execute(vec![(
            "drop",
            BenchOp::send_and_await(
                "aether.component",
                &DropComponent {
                    mailbox_id: probe_mbox,
                },
            ),
        )])
        .expect("drop sequence");
    match dropped
        .reply::<DropResult>("drop")
        .expect("decode DropResult")
    {
        DropResult::Ok => {}
        DropResult::Err { error } => panic!("drop_component: {error}"),
    }

    let post_drop = bench.count_observed(TICK_OBSERVED);

    bench
        .execute(vec![("post", BenchOp::advance(10))])
        .expect("post-drop advance");
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
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&mut bench, &wasm_path);

    // Capture's frame runs without a dispatched tick, so the probe
    // won't auto-tick during the captured frame. The pre-mail bundle
    // wires it up: `set_render` flips state to "visible red", and a
    // synthesised `aether.lifecycle.tick` drives the probe's on_tick
    // to emit a `DrawTriangle` into the frame buffer right before the
    // GPU readback. The after-mail bundle flips render back to
    // invisible after the readback.
    let pre = vec![
        envelope(
            &probe_address(),
            &SetRender {
                r: 200,
                g: 32,
                b: 32,
                visible: 1,
            },
        ),
        MailEnvelope {
            recipient_name: probe_address(),
            kind_name: "aether.lifecycle.tick".to_owned(),
            payload: Vec::new(),
            count: 1,
        },
    ];
    let after = vec![envelope(
        &probe_address(),
        &SetRender {
            r: 0,
            g: 0,
            b: 0,
            visible: 0,
        },
    )];

    // Priming advance subscribes the probe to ticks; the
    // capture-with-mails op then dispatches the pre bundle, reads
    // back, and dispatches the after bundle — all in one frame.
    let captured = bench
        .execute(vec![
            ("prime", BenchOp::advance(1)),
            ("snap", BenchOp::capture_with_mails(pre, after)),
        ])
        .expect("prime + capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    differs_from_background(&img, 5).expect("captured frame should contain a non-background pixel");

    // Cleanup ran: probe.render is now { visible: 0 }. Advance once
    // and capture again — the next tick won't emit DrawTriangle, so
    // the frame stays at clear color.
    let cleaned = bench
        .execute(vec![
            ("cleanup_advance", BenchOp::advance(1)),
            ("snap2", BenchOp::capture()),
        ])
        .expect("post-cleanup advance + capture");
    let png2 = cleaned.captured("snap2").expect("snap2 step ran");
    let img2 = decode_png(png2).expect("decode cleanup png");
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
    let Some(wasm_path) = require_runtime("probe") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let probe_mbox = load_probe(&mut bench, &wasm_path);

    bench
        .execute(vec![("warm", BenchOp::advance(3))])
        .expect("pre-replace advance");
    assert_eq!(
        bench.count_observed(TICK_OBSERVED),
        3,
        "expected 3 tick_observed before replace; observed kinds: {:?}",
        bench.observed_kinds(),
    );

    // Replace the wasm at the same mailbox id with the same fixture
    // binary. `SendAndAwait` blocks on `ReplaceResult` so the splice
    // completes before the post-replace baseline is sampled.
    let wasm = fs::read(&wasm_path).expect("re-read fixture wasm");
    let swapped = bench
        .execute(vec![(
            "swap",
            BenchOp::send_and_await(
                "aether.component",
                &ReplaceComponent {
                    mailbox_id: probe_mbox,
                    wasm,
                    drain_timeout_ms: None,
                    config: Vec::new(),
                },
            ),
        )])
        .expect("replace sequence");
    match swapped
        .reply::<ReplaceResult>("swap")
        .expect("decode ReplaceResult")
    {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    let post_replace_baseline = bench.count_observed(TICK_OBSERVED);
    bench
        .execute(vec![("post", BenchOp::advance(4))])
        .expect("post-replace advance");
    let post_replace = bench.count_observed(TICK_OBSERVED);

    assert!(
        post_replace > post_replace_baseline,
        "tick_observed count did not climb after replace; \
         baseline={post_replace_baseline}, final={post_replace}; \
         observed kinds: {:?}",
        bench.observed_kinds(),
    );
}

/// fs scenarios need wgpu (the bench unconditionally builds a
/// `Gpu` at boot) but not the fixture wasm. Skips on wgpu-less
/// runners and panics under `AETHER_REQUIRE_RUNTIME` so a
/// CI-side regression is loud.
fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(
        !strict,
        "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available",
    );
    eprintln!("skipping: no wgpu adapter available");
    false
}

const FS_MAILBOX: &str = "aether.fs";
const FS_NAMESPACE_SAVE: &str = "save";

/// `aether.fs.write` followed by `aether.fs.read` round-trips the
/// bytes through the local-file adapter (ADR-0041). Both replies
/// echo the originating namespace + path for correlation; the read
/// reply also carries the bytes verbatim.
#[test]
fn fs_write_then_read_round_trips_in_save_namespace() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("test-bench-fs");
    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let path = "fs-roundtrip.bin".to_owned();
    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];

    let result = bench
        .execute(vec![
            (
                "write",
                BenchOp::send_and_await(
                    FS_MAILBOX,
                    &Write {
                        namespace: FS_NAMESPACE_SAVE.to_owned(),
                        path: path.clone(),
                        bytes: payload.clone(),
                    },
                ),
            ),
            (
                "read",
                BenchOp::send_and_await(
                    FS_MAILBOX,
                    &Read {
                        namespace: FS_NAMESPACE_SAVE.to_owned(),
                        path: path.clone(),
                    },
                ),
            ),
        ])
        .expect("write + read");

    match result
        .reply::<WriteResult>("write")
        .expect("decode WriteResult")
    {
        WriteResult::Ok {
            namespace,
            path: echoed_path,
        } => {
            assert_eq!(namespace, FS_NAMESPACE_SAVE);
            assert_eq!(echoed_path, path);
        }
        WriteResult::Err { error, .. } => panic!("write failed: {error:?}"),
    }
    match result
        .reply::<ReadResult>("read")
        .expect("decode ReadResult")
    {
        ReadResult::Ok {
            namespace,
            path: echoed_path,
            bytes,
        } => {
            assert_eq!(namespace, FS_NAMESPACE_SAVE);
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
fn fs_delete_removes_written_file() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("test-bench-fs");
    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let path = "fs-delete.bin".to_owned();
    // A failed write would abort the sequence with `OpFailed`, so
    // reaching the asserts below means the write succeeded.
    let result = bench
        .execute(vec![
            (
                "write",
                BenchOp::send_and_await(
                    FS_MAILBOX,
                    &Write {
                        namespace: FS_NAMESPACE_SAVE.to_owned(),
                        path: path.clone(),
                        bytes: vec![1, 2, 3],
                    },
                ),
            ),
            (
                "delete",
                BenchOp::send_and_await(
                    FS_MAILBOX,
                    &Delete {
                        namespace: FS_NAMESPACE_SAVE.to_owned(),
                        path: path.clone(),
                    },
                ),
            ),
            (
                "read",
                BenchOp::send_and_await(
                    FS_MAILBOX,
                    &Read {
                        namespace: FS_NAMESPACE_SAVE.to_owned(),
                        path,
                    },
                ),
            ),
        ])
        .expect("write + delete + read");

    match result
        .reply::<DeleteResult>("delete")
        .expect("decode DeleteResult")
    {
        DeleteResult::Ok { .. } => {}
        DeleteResult::Err { error, .. } => panic!("delete failed: {error:?}"),
    }
    match result
        .reply::<ReadResult>("read")
        .expect("decode ReadResult")
    {
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
fn fs_list_returns_written_path() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("test-bench-fs");
    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let path = "probe-list.bin".to_owned();
    let result = bench
        .execute(vec![
            (
                "write",
                BenchOp::send_and_await(
                    FS_MAILBOX,
                    &Write {
                        namespace: FS_NAMESPACE_SAVE.to_owned(),
                        path: path.clone(),
                        bytes: vec![0],
                    },
                ),
            ),
            (
                "list",
                BenchOp::send_and_await(
                    FS_MAILBOX,
                    &List {
                        namespace: FS_NAMESPACE_SAVE.to_owned(),
                        prefix: String::new(),
                    },
                ),
            ),
        ])
        .expect("write + list");

    match result
        .reply::<ListResult>("list")
        .expect("decode ListResult")
    {
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
fn fs_read_unknown_path_returns_not_found() {
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("test-bench-fs");
    let mut bench = TestBench::builder()
        .size(64, 48)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let result = bench
        .execute(vec![(
            "read",
            BenchOp::send_and_await(
                FS_MAILBOX,
                &Read {
                    namespace: FS_NAMESPACE_SAVE.to_owned(),
                    path: "nonexistent-do-not-create.bin".to_owned(),
                },
            ),
        )])
        .expect("read");

    match result
        .reply::<ReadResult>("read")
        .expect("decode ReadResult")
    {
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
