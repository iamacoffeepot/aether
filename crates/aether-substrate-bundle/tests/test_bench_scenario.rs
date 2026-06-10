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
    visual::{background_top_left, bounding_box, centroid, coverage, decode_png},
};
use aether_test_fixtures::{Bump, CountQuery, CountReport, SetRender};

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
/// The `/`-rendered lineage a loaded component registers at (ADR-0099
/// §4): the component host `aether.component` `/`-joined to the
/// trampoline node — exactly what `LoadResult.name` reports.
fn probe_address() -> String {
    use aether_actor::Actor;
    format!(
        "aether.component/{}:{}",
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

/// Load the `cube` fixture into the bench, blocking on `LoadResult`
/// so the subsequent advance sees a tick-subscribed component. Mirrors
/// `load_probe`; the cube scenario only needs the load to succeed (it
/// captures rather than mailing the component), so the returned
/// `MailboxId` is discarded.
fn load_cube(bench: &mut TestBench, wasm_path: &Path) {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some("cube".to_owned()),
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
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load_component(cube): {error}"),
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
/// `aether.embedded:ui.panel.0` (Counter discriminator), and
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

    // ADR-0099 §3/§4: a spawned sibling nests under its spawner, so the
    // Panel registers at the `/`-rendered lineage path — the RootManager's
    // name with the sibling's trampoline segment appended — and its id is
    // the lineage fold of that path, not `hash("…trampoline:ui.panel.0")`.
    let panel_name = format!("{root_name}/aether.embedded:ui.panel.0");
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
                BenchOp::send_mail::<Ping>(panel_name.as_str(), &Ping { seq: 1 }),
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
/// pre-mail bundle flips the fixture's render state to "visible red";
/// the probe then paints one large triangle, so the captured PNG must
/// show a coverage fraction inside a sane band (neither all-background
/// nor all-filled) with a centroid sitting in the frame interior. The
/// after-mail bundle flips render back to invisible; a follow-up
/// advance + plain capture must produce a frame back at the clear
/// color — near-zero coverage — proving the after-mail cleanup ran.
#[test]
#[allow(clippy::cast_precision_loss)]
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
    let bg = background_top_left(&img);
    let tolerance = 5;
    // The probe draws one large triangle (NDC verts spanning ±0.9),
    // covering roughly 40% of the frame. A coverage band rules out the
    // two ways the old single-pixel `differs_from_background` check went
    // placebo: an all-background miss (drew nothing) and an all-filled
    // frame (clear color itself diverging from the sampled corner).
    let drawn = coverage(&img, bg, tolerance);
    assert!(
        (0.05..0.95).contains(&drawn),
        "probe triangle coverage {drawn} fell outside the expected band (0.05, 0.95); \
         the captured frame is effectively empty or entirely filled",
    );
    // The triangle is centered on the middle column and weighted toward
    // the lower half, so its centroid lands well inside the frame rather
    // than hugging an edge.
    let (center_x, center_y) = centroid(&img, bg, tolerance).expect("a lit frame has a centroid");
    let (width, height) = (img.width as f32, img.height as f32);
    assert!(
        center_x > 0.1 * width
            && center_x < 0.9 * width
            && center_y > 0.1 * height
            && center_y < 0.9 * height,
        "triangle centroid ({center_x}, {center_y}) should sit in the frame interior \
         of the {}x{} capture",
        img.width,
        img.height,
    );

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
    let cleaned_coverage = coverage(&img2, background_top_left(&img2), 5);
    assert!(
        cleaned_coverage < 0.01,
        "after after-mail cleanup the captured frame should be uniform clear color, \
         but coverage was {cleaned_coverage} (cleanup did not run)",
    );
}

/// Render-pipeline proof: load the `cube` fixture, drive one tick, and
/// capture. The fixture publishes a fixed `Camera { view_proj }` and a
/// twelve-triangle world-space unit cube, so the captured frame puts
/// every stage on the line at once — camera, `view_proj`, world-space
/// geometry, the depth test that orders the cube's faces, and GPU
/// readback. The existing `capture_frame_round_trip` scenario only
/// draws a flat NDC triangle at identity `view_proj`, so this is the
/// first capture that actually projects geometry through a camera.
///
/// The assertions use the #1513 silhouette reductions against the
/// known framing matrix: the cube's lit bounding box must sit centered
/// and inset from the frame edges (not a corner speck, not full-bleed),
/// and coverage must land in the cube's band. The bounds below were
/// tuned against the real captured frame at this size and `view_proj`.
#[test]
#[allow(clippy::cast_precision_loss)]
fn cube_render_projects_centered_silhouette() {
    let Some(wasm_path) = require_runtime("cube") else {
        return;
    };
    // 128×96 matches the fixture's `view_proj` aspect (4:3), so the
    // silhouette projects undistorted.
    let (width, height) = (128u32, 96u32);
    let mut bench = TestBench::start_with_size(width, height).expect("boot");
    load_cube(&mut bench, &wasm_path);

    // Priming advance subscribes the cube to ticks; the next tick (run
    // inside `capture`) drives the cube's camera + geometry emission so
    // the readback sees a fully-formed frame.
    let captured = bench
        .execute(vec![
            ("prime", BenchOp::advance(1)),
            ("snap", BenchOp::capture()),
        ])
        .expect("prime + capture");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    let bg = background_top_left(&img);
    let tolerance = 5;

    // Coverage band: the cube fills a healthy fraction of the frame but
    // leaves the clear color showing in the corners. The fixed
    // `view_proj` makes this deterministic; the observed fraction is
    // ~0.18, so the band brackets it with margin while still ruling out
    // an empty frame (drew nothing) and a full-bleed frame (clear-color
    // mismatch or runaway geometry).
    let drawn = coverage(&img, bg, tolerance);
    assert!(
        (0.10..0.30).contains(&drawn),
        "cube coverage {drawn} fell outside the expected band (0.10, 0.30); \
         the captured frame is effectively empty or entirely filled",
    );

    // The silhouette must be centered and inset from every edge —
    // proving the cube projected to the middle of the frame, not into a
    // corner and not bleeding past the borders.
    let silhouette = bounding_box(&img, bg, tolerance).expect("a lit frame has a bounding box");
    let (frame_width, frame_height) = (img.width as f32, img.height as f32);
    let min_x = silhouette.min_x as f32;
    let min_y = silhouette.min_y as f32;
    let max_x = silhouette.max_x as f32;
    let max_y = silhouette.max_y as f32;
    assert!(
        min_x > 0.05 * frame_width
            && max_x < 0.95 * frame_width
            && min_y > 0.05 * frame_height
            && max_y < 0.95 * frame_height,
        "cube silhouette {silhouette:?} should be inset from the edges of the \
         {}x{} frame (not full-bleed)",
        img.width,
        img.height,
    );
    assert!(
        min_x < 0.45 * frame_width
            && max_x > 0.55 * frame_width
            && min_y < 0.45 * frame_height
            && max_y > 0.55 * frame_height,
        "cube silhouette {silhouette:?} should straddle the center of the \
         {}x{} frame (not a corner speck)",
        img.width,
        img.height,
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

/// ADR-0101: a multi-actor module's entry export carries state across
/// `replace_component` through the `on_dehydrate` / `on_rehydrate`
/// hooks, now `FfiActor` defaults rather than an opt-in subtrait. Loads
/// the `stateful_replace` fixture (`export!(Counter, Sidecar)`), bumps
/// the entry `Counter`'s in-memory count to 3, replaces the wasm at the
/// same mailbox id with the same binary, then re-queries the count.
/// Because the boxed `ErasedFfiActor` now forwards the hooks, the count
/// survives the swap — before this change the multi-actor arm shipped
/// the hooks as no-ops and the replacement booted fresh at 0.
#[test]
fn replace_preserves_multi_actor_state_via_dehydrate_rehydrate() {
    use aether_actor::Actor;

    const FIXTURE_NAME: &str = "stateful_replace";

    let Some(wasm_path) = require_runtime(FIXTURE_NAME) else {
        return;
    };
    let addr = format!(
        "aether.component/{}:{FIXTURE_NAME}",
        aether_capabilities::WasmTrampoline::NAMESPACE,
    );

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load the entry export (Counter) and capture its mailbox id.
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(FIXTURE_NAME.to_owned()),
                    config: Vec::new(),
                    export: None,
                },
            ),
        )])
        .expect("load sequence");
    let mailbox_id = match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("stateful_replace load failed: {error}"),
    };

    // Bump the counter to 3, then read it back. `send_mail` is
    // fire-and-settle, so the three bumps land before the query.
    let pre = bench
        .execute(vec![
            ("bump_a", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_b", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_c", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("query", BenchOp::send_and_await(addr.as_str(), &CountQuery)),
        ])
        .expect("bump + query sequence");
    let pre_count = pre
        .reply::<CountReport>("query")
        .expect("decode pre-replace CountReport");
    assert_eq!(
        pre_count,
        CountReport { count: 3 },
        "three bumps should leave the counter at 3 before the replace",
    );

    // Replace the wasm at the same mailbox id with the same binary.
    // `on_dehydrate` saves the count on the old instance; `on_rehydrate`
    // restores it on the new one.
    let wasm = fs::read(&wasm_path).expect("re-read fixture wasm");
    let swapped = bench
        .execute(vec![(
            "swap",
            BenchOp::send_and_await(
                "aether.component",
                &ReplaceComponent {
                    mailbox_id,
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

    // The new instance booted fresh (init count = 0) and then rehydrated
    // from the saved bundle. Query it: the count must still be 3.
    let post = bench
        .execute(vec![(
            "query",
            BenchOp::send_and_await(addr.as_str(), &CountQuery),
        )])
        .expect("post-replace query sequence");
    let post_count = post
        .reply::<CountReport>("query")
        .expect("decode post-replace CountReport");
    assert_eq!(
        post_count,
        CountReport { count: 3 },
        "the counter must survive the multi-actor replace via on_dehydrate / on_rehydrate; \
         got {post_count:?} (0 means the hooks did not run through the boxed instance)",
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
