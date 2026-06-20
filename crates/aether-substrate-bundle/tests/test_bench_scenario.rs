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
// Test reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]

use std::path::Path;

use aether_data::{Kind, MailboxId};
use aether_kinds::{
    CachedFontMetrics, Camera, CaptureFrame, CaptureFrameResult, CreateTexture,
    CreateTextureResult, Delete, DeleteResult, DrawSolidQuads, DrawText, DrawTexturedQuads,
    DropComponent, DropResult, FontMetricsRequest, FontMetricsResult, FontRef, FrameCheck,
    FrameCheckResult, FrameReduction, FsError, List, ListComponents, ListComponentsResult,
    ListResult, LoadComponent, LoadFont, LoadFontResult, LoadResult, MailEnvelope, Ping, QuadScale,
    QuadSpace, Read, ReadResult, ReplaceComponent, ReplaceResult, SolidQuad, TexturedQuad, UiBar,
    UiPanel, Write, WriteResult,
};
use aether_math::{Mat4, Vec3};
use aether_substrate_bundle::test_bench::{
    BenchOp, TestBench,
    test_helpers::{has_wgpu_adapter, init_save_sandbox, require_runtime, test_namespace_roots},
};
use aether_substrate_bundle::visual::{
    background_top_left, bounding_box, centroid, coverage, decode_png,
};
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, DespawnChild, INLINE_WHO_CHILD, INLINE_WHO_PARENT, InlineEcho,
    InlineProbe, SetRender,
};

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary. Without the reference, the
// host-target rlib's descriptor symbols can be stripped by the linker
// and `aether_kinds::descriptors::all()` won't see fixture kinds.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;
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
    use aether_actor::Addressable;
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
                    // `Cube` is a non-entry actor in the bundle.
                    export: Some("cube".to_owned()),
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

/// The engine-local loaded-components query (issue 2020) lists a
/// loaded component by its ADR-0099 lineage address. After loading the
/// probe, a fieldless `ListComponents` to the `aether.component` mailbox
/// replies with the probe's full trampoline address — the deterministic
/// registration snapshot a readiness poll consumes instead of inferring
/// liveness from a log-ring side channel.
#[test]
fn list_components_reports_loaded_probe_lineage() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    load_probe(&mut bench, &wasm_path);

    let listed = bench
        .execute(vec![(
            "list",
            BenchOp::send_and_await("aether.component", &ListComponents {}),
        )])
        .expect("list sequence");
    let result = listed
        .reply::<ListComponentsResult>("list")
        .expect("decode ListComponentsResult");
    assert!(
        result.names.contains(&probe_address()),
        "the loaded probe should be listed at its lineage address {}, got {:?}",
        probe_address(),
        result.names,
    );
}

/// Subscribing the fixture to Tick yields exactly one
/// `tick_observed` broadcast per advance tick. Validates the
/// `subscribe_input` → tick fanout path end-to-end.
#[test]
fn input_subscription_yields_one_tick_observed_per_advance() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
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

/// ADR-0096: a multi-actor module loads through the unmodified host,
/// instantiating its entry export — the first type in the `export!`
/// list, `Probe` — via the boxed `ErasedFfiActor` path. Omitting `name`
/// exercises the `aether.namespace` section, which carries the entry
/// type's `NAMESPACE` (`test_fixture_probe`), and the `LoadResult`
/// capabilities come from the entry type's `aether.kinds.inputs`
/// manifest. Proves init-through-the-box and the multi-actor section
/// emission end-to-end; selecting a non-entry export is the follow-on.
#[test]
fn multi_actor_module_loads_entry_export() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
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
                    // No selector: load the entry export (Probe).
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
                name.ends_with(":test_fixture_probe"),
                "entry export should resolve to the first type's NAMESPACE \
                 (test_fixture_probe); got {name}",
            );
            assert!(
                !capabilities.handlers.is_empty(),
                "entry export Probe declares handlers; capabilities.handlers was empty",
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
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
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
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
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
/// `aether.embedded:0` (Counter discriminator — a flat segment, no type
/// prefix), and pinging *it* makes it broadcast a `TickObserved` to the
/// bench observer — proving the spawned sibling is addressable and
/// dispatches. The fire-and-settle send blocks until the whole tree
/// (including the spawned trampoline's init) drains, so the panel is
/// registered before the second send routes.
#[test]
fn multi_actor_sibling_spawn() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
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
                    // `RootManager` is a non-entry actor in the bundle; select
                    // it by its `ui.root` export.
                    config: Vec::new(),
                    export: Some("ui.root".to_owned()),
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
        "selected export should resolve to ui.root; got {root_name}",
    );

    // ADR-0099 §3/§4: a spawned sibling nests under its spawner, so the
    // Panel registers at the `/`-rendered lineage path — the RootManager's
    // name with the sibling's trampoline segment appended — and its id is
    // the lineage fold of that path, not `hash("…trampoline:0")`.
    // The Counter discriminator is a flat segment ("0") — no type prefix.
    let panel_name = format!("{root_name}/aether.embedded:0");
    bench
        .execute(vec![
            // RootManager spawns a Panel sibling (Counter → 0).
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
        "the spawned Panel (0) should have dispatched its ping and broadcast once; \
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
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
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
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
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
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
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

/// ADR-0105 textured-quad surface: create an RGBA8 texture from raw
/// pixels, draw a `Screen`-space quad sampling it at a known pixel rect,
/// and assert the captured frame lights that rect. A second capture
/// after an advance with no resent quads asserts the immediate-mode
/// clear — the quad disappears, matching `aether.draw_triangle`.
///
/// No component is loaded; the quad is the only thing that can light a
/// pixel, so the silhouette reductions pin it directly. The pre-mail
/// bundle dispatches the `draw_textured_quads` into the accumulator
/// right before the readback, the same way the probe scenario
/// synthesises a tick.
#[test]
#[allow(clippy::cast_precision_loss)]
fn textured_quad_draws_screen_space_rect() {
    if !require_wgpu_only() {
        return;
    }
    let (frame_width, frame_height) = (64u32, 48u32);
    let mut bench = TestBench::start_with_size(frame_width, frame_height).expect("boot");

    // 8×8 checkerboard of opaque white and opaque red — both far from the
    // dark clear color, so every magnified texel of the quad reads as lit
    // regardless of which cell it samples.
    let texture_width = 8u32;
    let texture_height = 8u32;
    let mut pixels = Vec::with_capacity((texture_width * texture_height * 4) as usize);
    for y in 0..texture_height {
        for x in 0..texture_width {
            let white = (x / 2 + y / 2) % 2 == 0;
            if white {
                pixels.extend_from_slice(&[255, 255, 255, 255]);
            } else {
                pixels.extend_from_slice(&[255, 0, 0, 255]);
            }
        }
    }

    let created = bench
        .execute(vec![(
            "create",
            BenchOp::send_and_await(
                "aether.render",
                &CreateTexture {
                    width: texture_width,
                    height: texture_height,
                    pixels,
                },
            ),
        )])
        .expect("create_texture sequence");
    let texture_id = match created
        .reply::<CreateTextureResult>("create")
        .expect("decode CreateTextureResult")
    {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture failed: {error}"),
    };

    // Known screen rect: top-left (16, 12), size 24×18 → columns 16..40,
    // rows 12..30. Rasterized pixel centers give an inclusive lit box of
    // roughly [16, 39] × [12, 29].
    let (quad_x, quad_y, quad_w, quad_h) = (16.0f32, 12.0f32, 24.0f32, 18.0f32);
    let pre = vec![envelope(
        "aether.render",
        &DrawTexturedQuads {
            texture_id,
            space: QuadSpace::Screen,
            quads: vec![TexturedQuad {
                x: quad_x,
                y: quad_y,
                width: quad_w,
                height: quad_h,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                tint: [1.0, 1.0, 1.0, 1.0],
            }],
        },
    )];

    let captured = bench
        .execute(vec![("snap", BenchOp::capture_with_mails(pre, vec![]))])
        .expect("capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    let bg = background_top_left(&img);
    let tolerance = 5;

    // Coverage band around the quad's area fraction (24*18 / 64*48 ≈
    // 0.14) — rules out an empty frame and a full-bleed frame.
    let drawn = coverage(&img, bg, tolerance);
    assert!(
        (0.08..0.22).contains(&drawn),
        "quad coverage {drawn} fell outside the expected band (0.08, 0.22); \
         the captured frame is effectively empty or entirely filled",
    );

    // The lit box must land on the requested rect — proving the
    // screen-space ortho mapped pixels (16, 12)–(40, 30) to the frame.
    let silhouette = bounding_box(&img, bg, tolerance).expect("a lit frame has a bounding box");
    assert!(
        (14..=18).contains(&silhouette.min_x)
            && (37..=41).contains(&silhouette.max_x)
            && (10..=14).contains(&silhouette.min_y)
            && (27..=31).contains(&silhouette.max_y),
        "quad silhouette {silhouette:?} should bound the screen rect (16,12)-(40,30) \
         of the {frame_width}x{frame_height} frame",
    );

    // Immediate-mode contract: with no quad resent, an advance commits
    // the empty accumulator (clearing the cache) and the next capture is
    // back at clear color.
    let cleared = bench
        .execute(vec![
            ("clear_advance", BenchOp::advance(1)),
            ("snap2", BenchOp::capture()),
        ])
        .expect("advance + capture");
    let png2 = cleared.captured("snap2").expect("snap2 step ran");
    let img2 = decode_png(png2).expect("decode cleared png");
    let cleared_coverage = coverage(&img2, background_top_left(&img2), tolerance);
    assert!(
        cleared_coverage < 0.01,
        "after the quad stopped being sent the frame should be uniform clear color, \
         but coverage was {cleared_coverage} (immediate-mode clear did not run)",
    );
}

/// ADR-0107 §4 flat-fill primitive: a `draw_solid_quads` batch draws an
/// opaque screen-space rect in the overlay pass without a caller-created
/// texture. The test dispatches a single `SolidQuad` covering a known
/// pixel rect and asserts `coverage > 0` and `centroid` inside the rect.
/// A second capture after an advance with no resent quads asserts the
/// immediate-mode clear — exactly the same contract as
/// `textured_quad_draws_screen_space_rect`.
#[test]
#[allow(clippy::cast_precision_loss)]
fn solid_quad_draws_screen_space_rect() {
    if !require_wgpu_only() {
        return;
    }
    let (frame_width, frame_height) = (64u32, 48u32);
    let mut bench = TestBench::start_with_size(frame_width, frame_height).expect("boot");

    // Known screen rect: top-left (16, 12), size 24×18.
    let (quad_x, quad_y, quad_w, quad_h) = (16.0f32, 12.0f32, 24.0f32, 18.0f32);
    let pre = vec![envelope(
        "aether.render",
        &DrawSolidQuads {
            space: QuadSpace::Screen,
            quads: vec![SolidQuad {
                x: quad_x,
                y: quad_y,
                width: quad_w,
                height: quad_h,
                color: [1.0, 1.0, 1.0, 1.0],
            }],
        },
    )];

    let captured = bench
        .execute(vec![("snap", BenchOp::capture_with_mails(pre, vec![]))])
        .expect("capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    let bg = background_top_left(&img);
    let tolerance = 5;

    // Coverage band around the quad's area fraction (24*18 / 64*48 ≈ 0.14).
    let drawn = coverage(&img, bg, tolerance);
    assert!(
        (0.08..0.22).contains(&drawn),
        "solid quad coverage {drawn} fell outside the expected band (0.08, 0.22); \
         the captured frame is effectively empty or entirely filled",
    );

    // The lit centroid must land inside the requested rect — ruling out a misplaced fill.
    let (cx, cy) = centroid(&img, bg, tolerance).expect("a lit frame has a centroid");
    let pad = 4.0f32;
    assert!(
        cx >= quad_x - pad
            && cx <= quad_x + quad_w + pad
            && cy >= quad_y - pad
            && cy <= quad_y + quad_h + pad,
        "solid quad centroid ({cx}, {cy}) should sit inside the screen rect \
         ({quad_x},{quad_y})+({quad_w}x{quad_h}) of the {frame_width}x{frame_height} frame",
    );

    // Immediate-mode clear: advance with no quad resent, next capture returns to clear color.
    let cleared = bench
        .execute(vec![
            ("clear_advance", BenchOp::advance(1)),
            ("snap2", BenchOp::capture()),
        ])
        .expect("advance + capture");
    let png2 = cleared.captured("snap2").expect("snap2 step ran");
    let img2 = decode_png(png2).expect("decode cleared png");
    let cleared_coverage = coverage(&img2, background_top_left(&img2), tolerance);
    assert!(
        cleared_coverage < 0.01,
        "after the solid quad stopped being sent the frame should be uniform clear color, \
         but coverage was {cleared_coverage} (immediate-mode clear did not run)",
    );
}

/// iamacoffeepot/aether#1740: the locomotion HUD composes on the
/// `aether.ui` cap in screen space. This drives the same widget mail the
/// kit now sends — a dark `UiPanel` backing plate under a `UiBar` health
/// fill in a centered strip near the top edge — and asserts the bar lands
/// where a screen-anchored HUD should: a coverage band (a bar is drawn,
/// the frame is neither empty nor full) with the lit centroid sitting in
/// the top of the frame and horizontally centered. A second capture after
/// an advance with nothing resent asserts the immediate-mode clear.
#[test]
#[allow(clippy::cast_precision_loss)]
fn ui_hud_draws_screen_space_health_bar() {
    if !require_wgpu_only() {
        return;
    }
    let (frame_width, frame_height) = (128u32, 96u32);
    let mut bench = TestBench::start_with_size(frame_width, frame_height).expect("boot");

    // A centered strip near the top edge, mirroring the kit's HUD layout:
    // the dark plate, then the health fill inset within it.
    let plate_color = [0.10, 0.10, 0.13, 1.0];
    let pre = vec![
        envelope(
            "aether.ui",
            &UiPanel {
                rect: [29.0, 8.0, 70.0, 12.0],
                color: plate_color,
            },
        ),
        envelope(
            "aether.ui",
            &UiBar {
                rect: [31.0, 10.0, 66.0, 8.0],
                frac: 0.7,
                track_color: plate_color,
                // A bright fill so the bar reads clearly against the plate.
                fill_color: [0.25, 0.82, 0.32, 1.0],
            },
        ),
    ];

    let captured = bench
        .execute(vec![("snap", BenchOp::capture_with_mails(pre, vec![]))])
        .expect("capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    let bg = background_top_left(&img);
    let tolerance = 5;

    // A bar is drawn: coverage sits in a band, neither an empty frame nor a
    // fully filled one.
    let drawn = coverage(&img, bg, tolerance);
    assert!(
        (0.02..0.40).contains(&drawn),
        "HUD bar coverage {drawn} fell outside the expected band (0.02, 0.40); \
         the captured frame is effectively empty or entirely filled",
    );

    // Screen-anchored at the top: the lit centroid lands in the top of the
    // frame and stays horizontally centered.
    let (cx, cy) = centroid(&img, bg, tolerance).expect("a lit frame has a centroid");
    assert!(
        cy < frame_height as f32 * 0.4,
        "HUD centroid y={cy} should sit in the top of the {frame_height}-tall frame \
         (the bar is anchored near the top edge)",
    );
    assert!(
        (frame_width as f32 * 0.2..frame_width as f32 * 0.8).contains(&cx),
        "HUD centroid x={cx} should sit toward the horizontal center of the \
         {frame_width}-wide frame (the bar is centered)",
    );

    // Immediate-mode clear: advance with nothing resent, next capture
    // returns to the clear color.
    let cleared = bench
        .execute(vec![
            ("clear_advance", BenchOp::advance(1)),
            ("snap2", BenchOp::capture()),
        ])
        .expect("advance + capture");
    let png2 = cleared.captured("snap2").expect("snap2 step ran");
    let img2 = decode_png(png2).expect("decode cleared png");
    let cleared_coverage = coverage(&img2, background_top_left(&img2), tolerance);
    assert!(
        cleared_coverage < 0.01,
        "after the HUD widgets stopped being sent the frame should be uniform clear color, \
         but coverage was {cleared_coverage} (immediate-mode clear did not run)",
    );
}

/// iamacoffeepot/aether#1777: a `capture_frame` carrying a `checks`
/// request returns a substrate-side verdict scored on the exact RGBA
/// the PNG is built from — no caller-side PNG decode. Draws a known
/// solid quad as a capture pre-mail and asserts the verdict's
/// reductions (`not_all_black`, `coverage`, `centroid`, `bounding_box`)
/// land the same way the decode-based `solid_quad_draws_screen_space_rect`
/// scores them, but computed in the render thread.
#[test]
#[allow(clippy::cast_precision_loss)]
// A single long end-to-end scenario (build → draw → capture → assert each
// reduction); splitting it would scatter the one linear story.
#[allow(clippy::too_many_lines)]
fn capture_frame_checks_return_substrate_verdict() {
    if !require_wgpu_only() {
        return;
    }
    let (frame_width, frame_height) = (64u32, 48u32);
    let mut bench = TestBench::start_with_size(frame_width, frame_height).expect("boot");

    // Known screen rect: top-left (16, 12), size 24×18 — the same draw
    // `solid_quad_draws_screen_space_rect` decodes the PNG to score.
    let (quad_x, quad_y, quad_w, quad_h) = (16.0f32, 12.0f32, 24.0f32, 18.0f32);
    let draw = envelope(
        "aether.render",
        &DrawSolidQuads {
            space: QuadSpace::Screen,
            quads: vec![SolidQuad {
                x: quad_x,
                y: quad_y,
                width: quad_w,
                height: quad_h,
                color: [1.0, 1.0, 1.0, 1.0],
            }],
        },
    );
    let tolerance = 5u8;
    let mk_check = |reduction| FrameCheck {
        reduction,
        tolerance,
        // None → partition against the frame's top-left pixel (the clear
        // color), matching the decode-based scenarios' convention.
        background: None,
    };

    let result = bench
        .execute(vec![(
            "snap",
            BenchOp::send_and_await(
                "aether.render",
                &CaptureFrame {
                    mails: vec![draw],
                    after_mails: vec![],
                    checks: vec![
                        mk_check(FrameReduction::NotAllBlack),
                        mk_check(FrameReduction::Coverage),
                        mk_check(FrameReduction::Centroid),
                        mk_check(FrameReduction::BoundingBox),
                    ],
                    similarity: None,
                },
            ),
        )])
        .expect("send_and_await(CaptureFrame) with checks");
    let reply: CaptureFrameResult = result.reply("snap").expect("decode CaptureFrameResult");
    let verdict = match reply {
        CaptureFrameResult::Ok { png, verdict, .. } => {
            assert!(
                png.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
                "the PNG still rides back alongside the verdict",
            );
            verdict.expect("a checks request returns a verdict")
        }
        CaptureFrameResult::Err { error } => panic!("capture_frame replied Err: {error}"),
    };
    assert_eq!((verdict.width, verdict.height), (frame_width, frame_height));
    assert_eq!(verdict.results.len(), 4);

    match &verdict.results[0] {
        FrameCheckResult::NotAllBlack { passed, detail } => {
            assert!(passed, "the white quad lights pixels: {detail:?}");
        }
        other => panic!("expected NotAllBlack result, got {other:?}"),
    }
    match &verdict.results[1] {
        FrameCheckResult::Coverage { fraction, .. } => {
            // 24*18 / 64*48 ≈ 0.14 — the same band the decode test asserts.
            assert!(
                (0.08..0.22).contains(fraction),
                "solid quad coverage {fraction} fell outside the expected band",
            );
        }
        other => panic!("expected Coverage result, got {other:?}"),
    }
    match &verdict.results[2] {
        FrameCheckResult::Centroid { centroid, .. } => {
            let [cx, cy] = centroid.expect("a lit frame has a centroid");
            let pad = 4.0f32;
            assert!(
                cx >= quad_x - pad
                    && cx <= quad_x + quad_w + pad
                    && cy >= quad_y - pad
                    && cy <= quad_y + quad_h + pad,
                "verdict centroid ({cx}, {cy}) should sit inside the screen rect",
            );
        }
        other => panic!("expected Centroid result, got {other:?}"),
    }
    match &verdict.results[3] {
        FrameCheckResult::BoundingBox { rect, .. } => {
            let rect = rect.expect("a lit frame has a bounding box");
            let pad = 4.0f32;
            let (min_x, max_x) = (rect.min_x as f32, rect.max_x as f32);
            assert!(
                min_x >= quad_x - pad
                    && min_x <= quad_x + pad
                    && max_x <= quad_x + quad_w + pad
                    && max_x >= quad_x + quad_w - pad,
                "verdict bounding box {rect:?} should hug the drawn rect's x-extent",
            );
        }
        other => panic!("expected BoundingBox result, got {other:?}"),
    }
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
    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
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
                    export: None,
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
/// hooks, now `WasmActor` defaults rather than an opt-in subtrait. Loads
/// the `stateful_replace` fixture (`export!(Counter, Sidecar)`), bumps
/// the entry `Counter`'s in-memory count to 3, replaces the wasm at the
/// same mailbox id with the same binary, then re-queries the count.
/// Because the boxed `ErasedFfiActor` now forwards the hooks, the count
/// survives the swap — before this change the multi-actor arm shipped
/// the hooks as no-ops and the replacement booted fresh at 0.
#[test]
fn replace_preserves_multi_actor_state_via_dehydrate_rehydrate() {
    use aether_actor::Addressable;

    const FIXTURE_NAME: &str = "stateful_replace";

    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let addr = format!(
        "aether.component/{}:{FIXTURE_NAME}",
        aether_capabilities::WasmTrampoline::NAMESPACE,
    );

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load the `Counter` actor (a non-entry actor in the bundle) under the
    // `stateful_replace` name and capture its mailbox id.
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(FIXTURE_NAME.to_owned()),
                    config: Vec::new(),
                    export: Some("stateful.counter".to_owned()),
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
                    export: None,
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

/// ADR-0113: a single-actor component carries its declared `type State`
/// across `replace_component` through the macro-generated `on_dehydrate`
/// / `on_rehydrate` hooks — no hand-written hooks. Loads the
/// `stateful_replace_typed` fixture, bumps the counter to 3, replaces the
/// wasm at the same mailbox id with the same binary, then re-queries. The
/// generated `on_dehydrate` frames the `CounterState` via
/// `save_state_kind`; the generated `on_rehydrate` recovers it via
/// `as_kind`, so the count survives the swap.
#[test]
fn replace_preserves_state_via_typed_state_kind() {
    use aether_actor::Addressable;

    const FIXTURE_NAME: &str = "stateful_replace_typed";

    let Some(wasm_path) = require_runtime("aether_test_fixtures_stateful_typed") else {
        return;
    };
    let addr = format!(
        "aether.component/{}:{FIXTURE_NAME}",
        aether_capabilities::WasmTrampoline::NAMESPACE,
    );

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

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
        LoadResult::Err { error } => panic!("stateful_replace_typed load failed: {error}"),
    };

    // Bump the counter to 3, then read it back.
    let pre = bench
        .execute(vec![
            ("bump_a", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_b", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_c", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("query", BenchOp::send_and_await(addr.as_str(), &CountQuery)),
        ])
        .expect("bump + query sequence");
    assert_eq!(
        pre.reply::<CountReport>("query")
            .expect("decode pre-replace CountReport"),
        CountReport { count: 3 },
        "three bumps should leave the counter at 3 before the replace",
    );

    // Replace with the same binary; the generated hooks carry the count.
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
                    export: None,
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
        "the counter must survive the replace via the macro-generated typed-state hooks; \
         got {post_count:?} (0 means the generated hooks did not carry the state)",
    );
}

/// ADR-0113: when a replacement is compiled against a reshaped `type
/// State` kind (a different `Kind::ID`), the generated `on_rehydrate`
/// sees `PriorState::as_kind` miss the decode and boots fresh. Loads the
/// `stateful_replace_typed` fixture, bumps to 3, then replaces it with
/// `stateful_replace_reshaped` (same `NAMESPACE`, a `CounterState` that
/// gained a field). The recovered count is 0 — the fresh-`init` value —
/// because the saved bundle's leading id no longer matches. The warn the
/// generated hook emits on the decode-miss is covered host-side by
/// `aether-actor`'s `state_framing_roundtrip` test (the bench does not
/// route `aether.log` mail through its observed sinks).
#[test]
fn typed_state_decode_miss_boots_fresh() {
    use aether_actor::Addressable;

    const TYPED_NAME: &str = "stateful_replace_typed";

    let Some(typed_path) = require_runtime("aether_test_fixtures_stateful_typed") else {
        return;
    };
    let Some(reshaped_path) = require_runtime("aether_test_fixtures_stateful_reshaped") else {
        return;
    };
    let addr = format!(
        "aether.component/{}:{TYPED_NAME}",
        aether_capabilities::WasmTrampoline::NAMESPACE,
    );

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let typed_wasm = fs::read(&typed_path).expect("read typed fixture wasm");

    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: typed_wasm,
                    name: Some(TYPED_NAME.to_owned()),
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
        LoadResult::Err { error } => panic!("stateful_replace_typed load failed: {error}"),
    };

    let pre = bench
        .execute(vec![
            ("bump_a", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_b", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_c", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("query", BenchOp::send_and_await(addr.as_str(), &CountQuery)),
        ])
        .expect("bump + query sequence");
    assert_eq!(
        pre.reply::<CountReport>("query")
            .expect("decode pre-replace CountReport"),
        CountReport { count: 3 },
        "three bumps should leave the counter at 3 before the replace",
    );

    // Replace with the reshaped wasm: the saved bundle's leading id no
    // longer matches the new `CounterState::ID`, so rehydrate misses.
    let reshaped_wasm = fs::read(&reshaped_path).expect("read reshaped fixture wasm");
    let swapped = bench
        .execute(vec![(
            "swap",
            BenchOp::send_and_await(
                "aether.component",
                &ReplaceComponent {
                    mailbox_id,
                    wasm: reshaped_wasm,
                    drain_timeout_ms: None,
                    config: Vec::new(),
                    export: None,
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
        CountReport { count: 0 },
        "a reshaped state kind must boot fresh on rehydrate (decode-miss); \
         got {post_count:?} (3 would mean the stale bundle decoded against the new shape)",
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

/// ADR-0105 text surface end to end: load a real OFL TTF through the
/// `assets` namespace, draw a `Screen`-space string, and assert the
/// captured frame lights a region in the upper-left where top-left-
/// anchored text lands. No component is loaded — the text is the only
/// thing that can light a pixel.
///
/// The first `draw` lazily creates the atlas texture (and draws nothing
/// that turn); `send_and_await` settles that `create_texture` round trip,
/// so the texture id is live before the capture's pre-mail `draw`
/// rasterizes glyphs and emits the quad batch.
#[test]
#[allow(clippy::cast_precision_loss)]
fn text_draws_a_screen_space_string() {
    // The crate's vendored Roboto Mono (SIL OFL 1.1) — copied into the
    // sandbox so the `assets` namespace can read it.
    const TTF: &[u8] = include_bytes!("../assets/fonts/RobotoMono.ttf");
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("test-bench-text");
    fs::write(sandbox.join("font.ttf"), TTF).expect("stage font asset");

    let (frame_width, frame_height) = (128u32, 64u32);
    let mut bench = TestBench::builder()
        .size(frame_width, frame_height)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    // Load the font; the reply carries the session-scoped font_id.
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.text",
                &LoadFont {
                    namespace: "assets".to_owned(),
                    path: "font.ttf".to_owned(),
                },
            ),
        )])
        .expect("load_font sequence");
    let font_id = match loaded
        .reply::<LoadFontResult>("load")
        .expect("decode LoadFontResult")
    {
        LoadFontResult::Ok { font_id, .. } => font_id,
        LoadFontResult::Err { error, .. } => panic!("load_font failed: {error}"),
    };

    let draw = DrawText {
        font_id,
        text: "Hi".to_owned(),
        size_pixels: 32.0,
        color: [1.0, 1.0, 1.0, 1.0],
        origin: [0.0, 0.0],
        space: QuadSpace::Screen,
    };

    // First draw: lazily creates the atlas texture (fire-and-forget — a
    // `draw` has no reply). The advance pumps the `create_texture` reply
    // back into the text cap so its texture id is live; nothing is drawn
    // this turn.
    bench
        .execute(vec![
            (
                "prime",
                BenchOp::send_mail::<DrawText>("aether.text", &draw),
            ),
            ("settle", BenchOp::advance(2)),
        ])
        .expect("prime draw");

    // Now the glyphs rasterize and the quad batch reaches the renderer the
    // same tick the capture records.
    let pre = vec![envelope("aether.text", &draw)];
    let captured = bench
        .execute(vec![("snap", BenchOp::capture_with_mails(pre, vec![]))])
        .expect("capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    let bg = background_top_left(&img);
    let tolerance = 5;

    // Sparse but present — rules out an empty frame and a full-bleed one.
    let drawn = coverage(&img, bg, tolerance);
    assert!(
        (0.005..0.40).contains(&drawn),
        "text coverage {drawn} fell outside the expected band (0.005, 0.40); \
         the captured frame is effectively empty or entirely filled",
    );

    // Top-left-anchored text lands in the upper-left: the lit centroid
    // sits in the top half and left portion of the frame.
    let (center_x, center_y) = centroid(&img, bg, tolerance).expect("a lit frame has a centroid");
    assert!(
        center_y < frame_height as f32 / 2.0,
        "text centroid y={center_y} should sit in the top half (anchored at the top edge)",
    );
    assert!(
        center_x < frame_width as f32 * 0.75,
        "text centroid x={center_x} should sit toward the left (anchored at the left edge)",
    );

    // The lit box must not bleed to the far-right / bottom edges — the
    // short string occupies only the upper-left.
    let silhouette = bounding_box(&img, bg, tolerance).expect("a lit frame has a bounding box");
    assert!(
        silhouette.min_x < frame_width / 2 && silhouette.max_y < frame_height,
        "text silhouette {silhouette:?} should bound the upper-left of the \
         {frame_width}x{frame_height} frame",
    );
}

/// ADR-0105 font-metrics grab end to end (issue 1854): grab a real
/// font's size-independent metric table over the mail path — by path, so
/// the cap loads it on the miss — cache it guest-side, and assert the
/// local measurement of a run reproduces the cap's draw-path advance sum
/// bit-for-bit. That equality is the synchronous-local-layout invariant:
/// a consumer measures text without a per-measurement mail round trip and
/// still matches what the cap would draw.
///
/// CPU-only (no capture), but the bench still boots a full chassis, so it
/// skips on driverless runners like the other scenarios.
#[test]
fn font_metrics_grab_measures_like_the_draw_path() {
    const TTF: &[u8] = include_bytes!("../assets/fonts/RobotoMono.ttf");
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("test-bench-font-metrics");
    fs::write(sandbox.join("font.ttf"), TTF).expect("stage font asset");

    let mut bench = TestBench::builder()
        .size(64, 32)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    // Grab by path with no prior load — exercises load-on-miss.
    let grabbed = bench
        .execute(vec![(
            "grab",
            BenchOp::send_and_await(
                "aether.text",
                &FontMetricsRequest {
                    font: FontRef::Path {
                        namespace: "assets".to_owned(),
                        path: "font.ttf".to_owned(),
                    },
                },
            ),
        )])
        .expect("font_metrics grab sequence");
    let metrics = match grabbed
        .reply::<FontMetricsResult>("grab")
        .expect("decode FontMetricsResult")
    {
        FontMetricsResult::Ok { metrics } => metrics,
        FontMetricsResult::Err { error } => panic!("font_metrics failed: {error}"),
    };

    // Cache the table guest-side and measure a run locally.
    let cache = CachedFontMetrics::new(&metrics);
    let text = "Hello aether";
    let size = 29.0;
    let local = cache.measure(text, size);

    // Ground truth: fontdue's draw-path pen walk over the same string.
    let font = fontdue::Font::from_bytes(TTF, fontdue::FontSettings::default())
        .expect("vendored Roboto Mono parses");
    let mut draw_pen = 0.0f32;
    for ch in text.chars() {
        draw_pen += font.metrics(ch, size).advance_width;
    }

    assert!(local > 0.0, "a non-empty run has positive extent");
    assert_eq!(
        local, draw_pen,
        "local measure must equal the draw-path advance sum exactly",
    );
}

/// ADR-0105 screen-space text origin (issue 1773): drawing `Screen` text
/// at a non-zero `origin` shifts the lit centroid by the offset, so the
/// string no longer sits at the window top-left.
///
/// Two captures back-to-back — one at `origin = [0, 0]` and one at
/// `origin = [ox, oy]` — are taken in the same bench session (font and
/// atlas are already live by the time the second capture fires). The
/// centroid of the offset capture must sit further right and further down
/// than the zero-origin centroid by at least half the applied offset,
/// ruling out a no-op implementation.
///
/// Skipped on driverless runners.
#[test]
#[allow(clippy::cast_precision_loss)]
fn text_screen_origin_shifts_centroid() {
    const TTF: &[u8] = include_bytes!("../assets/fonts/RobotoMono.ttf");
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("test-bench-text-origin");
    fs::write(sandbox.join("font.ttf"), TTF).expect("stage font asset");

    let (frame_width, frame_height) = (256u32, 128u32);
    let mut bench = TestBench::builder()
        .size(frame_width, frame_height)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    // Load the font.
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.text",
                &LoadFont {
                    namespace: "assets".to_owned(),
                    path: "font.ttf".to_owned(),
                },
            ),
        )])
        .expect("load_font sequence");
    let font_id = match loaded
        .reply::<LoadFontResult>("load")
        .expect("decode LoadFontResult")
    {
        LoadFontResult::Ok { font_id, .. } => font_id,
        LoadFontResult::Err { error, .. } => panic!("load_font failed: {error}"),
    };

    let draw_zero = DrawText {
        font_id,
        text: "Hi".to_owned(),
        size_pixels: 24.0,
        color: [1.0, 1.0, 1.0, 1.0],
        origin: [0.0, 0.0],
        space: QuadSpace::Screen,
    };

    // Prime pass: lazily creates the atlas texture; nothing draws yet.
    bench
        .execute(vec![
            (
                "prime",
                BenchOp::send_mail::<DrawText>("aether.text", &draw_zero),
            ),
            ("settle", BenchOp::advance(2)),
        ])
        .expect("prime draw");

    // Capture at origin [0, 0].
    let pre_zero = vec![envelope("aether.text", &draw_zero)];
    let snap_zero = bench
        .execute(vec![(
            "snap0",
            BenchOp::capture_with_mails(pre_zero, vec![]),
        )])
        .expect("capture zero-origin");
    let img_zero = decode_png(snap_zero.captured("snap0").expect("snap0 ran"))
        .expect("decode zero-origin png");
    let bg = background_top_left(&img_zero);
    let tolerance = 5;
    let base_center = centroid(&img_zero, bg, tolerance).expect("zero-origin frame has lit pixels");

    // Capture at a shifted origin — well inside the frame so glyphs render.
    let ox = (frame_width / 2) as f32;
    let oy = (frame_height / 2) as f32;
    let draw_offset = DrawText {
        origin: [ox, oy],
        ..draw_zero
    };
    let pre_offset = vec![envelope("aether.text", &draw_offset)];
    let snap_offset = bench
        .execute(vec![(
            "snap1",
            BenchOp::capture_with_mails(pre_offset, vec![]),
        )])
        .expect("capture offset-origin");
    let img_offset = decode_png(snap_offset.captured("snap1").expect("snap1 ran"))
        .expect("decode offset-origin png");
    let shifted_center =
        centroid(&img_offset, bg, tolerance).expect("offset-origin frame has lit pixels");

    // The shifted centroid must sit at least half the applied offset further
    // right and down — a strict half-delta guard that would catch a no-op.
    assert!(
        shifted_center.0 > base_center.0 + ox / 2.0,
        "offset centroid x={} should be right of zero centroid x={} \
         by at least {} (applied offset {ox})",
        shifted_center.0,
        base_center.0,
        ox / 2.0,
    );
    assert!(
        shifted_center.1 > base_center.1 + oy / 2.0,
        "offset centroid y={} should be below zero centroid y={} \
         by at least {} (applied offset {oy})",
        shifted_center.1,
        base_center.1,
        oy / 2.0,
    );
}

/// ADR-0105 World-space text (issue 1699): draws `World { anchor,
/// scale }` text under a perspective camera and asserts:
///
/// 1. `Distance { reference_distance: 10 }` labels shrink proportionally
///    as the camera dollies from d=10 to d=20 — bbox width ratio ≈ 0.5.
/// 2. `Pixels` labels hold their screen size across the same dolly —
///    bbox width ratio ≈ 1.0.
/// 3. The Pixels label stays axis-aligned at a 45-degree orbit angle
///    (bbox width within ±30% of the front-facing width), confirming the
///    clip-space approach never skews the label with the camera.
///
/// Skipped when no wgpu adapter is available (driverless CI runner) or
/// the font asset hasn't been staged.
#[test]
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
fn text_draws_world_space_label() {
    use std::f32::consts::PI;

    const TTF: &[u8] = include_bytes!("../assets/fonts/RobotoMono.ttf");
    if !require_wgpu_only() {
        return;
    }
    let sandbox = init_save_sandbox("test-bench-world-text");
    fs::write(sandbox.join("font.ttf"), TTF).expect("stage font asset");

    let (frame_width, frame_height) = (128u32, 96u32);
    let mut bench = TestBench::builder()
        .size(frame_width, frame_height)
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.text",
                &LoadFont {
                    namespace: "assets".to_owned(),
                    path: "font.ttf".to_owned(),
                },
            ),
        )])
        .expect("load_font sequence");
    let font_id = match loaded
        .reply::<LoadFontResult>("load")
        .expect("decode LoadFontResult")
    {
        LoadFontResult::Ok { font_id, .. } => font_id,
        LoadFontResult::Err { error, .. } => panic!("load_font failed: {error}"),
    };

    // Build view-projection matrices for three camera positions.
    let fov_y = PI / 3.0;
    let aspect = frame_width as f32 / frame_height as f32;
    let proj = Mat4::perspective_rh(fov_y, aspect, 0.1, 100.0);
    let up = Vec3::new(0.0, 1.0, 0.0);

    let view_near = Mat4::look_at_rh(Vec3::new(0.0, 0.0, 10.0), Vec3::ZERO, up);
    let view_far = Mat4::look_at_rh(Vec3::new(0.0, 0.0, 20.0), Vec3::ZERO, up);
    let orbit_x = 10.0_f32 * (PI / 4.0).sin();
    let orbit_z = 10.0_f32 * (PI / 4.0).cos();
    let view_orbit = Mat4::look_at_rh(Vec3::new(orbit_x, 0.0, orbit_z), Vec3::ZERO, up);

    let vp_near = (proj * view_near).to_cols_array();
    let vp_far = (proj * view_far).to_cols_array();
    let vp_orbit = (proj * view_orbit).to_cols_array();

    let anchor = [0.0_f32, 0.0, 0.0];
    let draw_dist = DrawText {
        font_id,
        text: "Hy".to_owned(),
        size_pixels: 24.0,
        color: [1.0, 1.0, 1.0, 1.0],
        origin: [0.0, 0.0],
        space: QuadSpace::World {
            anchor,
            scale: QuadScale::Distance {
                reference_distance: 10.0,
            },
        },
    };
    let draw_px = DrawText {
        font_id,
        text: "Hy".to_owned(),
        size_pixels: 24.0,
        color: [1.0, 1.0, 1.0, 1.0],
        origin: [0.0, 0.0],
        space: QuadSpace::World {
            anchor,
            scale: QuadScale::Pixels,
        },
    };

    // Prime: the first draw lazily creates the atlas texture and draws
    // nothing until the create_texture reply lands. Advance twice to
    // settle it so subsequent captures can render immediately.
    bench
        .execute(vec![
            (
                "cam",
                BenchOp::send_mail::<Camera>("aether.render", &Camera { view_proj: vp_near }),
            ),
            (
                "prime",
                BenchOp::send_mail::<DrawText>("aether.text", &draw_dist),
            ),
            ("settle", BenchOp::advance(2)),
        ])
        .expect("prime draw");

    let tol = 5u8;

    // Capture Distance label at near (d=10) and far (d=20).
    let snap_near = bench
        .execute(vec![(
            "s",
            BenchOp::capture_with_mails(
                vec![
                    envelope("aether.render", &Camera { view_proj: vp_near }),
                    envelope("aether.text", &draw_dist),
                ],
                vec![],
            ),
        )])
        .expect("near capture");
    let img_near = decode_png(snap_near.captured("s").expect("s ran")).expect("decode near");
    let bb_near = bounding_box(&img_near, background_top_left(&img_near), tol)
        .expect("near frame has content");

    let snap_far = bench
        .execute(vec![(
            "s",
            BenchOp::capture_with_mails(
                vec![
                    envelope("aether.render", &Camera { view_proj: vp_far }),
                    envelope("aether.text", &draw_dist),
                ],
                vec![],
            ),
        )])
        .expect("far capture");
    let img_far = decode_png(snap_far.captured("s").expect("s ran")).expect("decode far");
    let bb_far =
        bounding_box(&img_far, background_top_left(&img_far), tol).expect("far frame has content");

    // Distance label at d=20 should be ~0.5x the width at d=10 because
    // k/clip.w = reference_distance/depth shrinks by half. Allow ±25%
    // slop for pixel-grid rounding.
    let near_w = (bb_near.max_x - bb_near.min_x + 1) as f32;
    let far_w = (bb_far.max_x - bb_far.min_x + 1) as f32;
    let dist_ratio = far_w / near_w;
    assert!(
        (0.25..0.75).contains(&dist_ratio),
        "Distance label width at d=20 / d=10 = {dist_ratio:.3} should be near 0.5 \
         (near={near_w}px, far={far_w}px); Distance scaling is broken",
    );

    // Capture Pixels label at near and far: width should hold constant.
    let snap_px_near = bench
        .execute(vec![(
            "s",
            BenchOp::capture_with_mails(
                vec![
                    envelope("aether.render", &Camera { view_proj: vp_near }),
                    envelope("aether.text", &draw_px),
                ],
                vec![],
            ),
        )])
        .expect("pixels-near capture");
    let img_px_near =
        decode_png(snap_px_near.captured("s").expect("s ran")).expect("decode px-near");
    let bb_px_near = bounding_box(&img_px_near, background_top_left(&img_px_near), tol)
        .expect("px-near frame has content");

    let snap_px_far = bench
        .execute(vec![(
            "s",
            BenchOp::capture_with_mails(
                vec![
                    envelope("aether.render", &Camera { view_proj: vp_far }),
                    envelope("aether.text", &draw_px),
                ],
                vec![],
            ),
        )])
        .expect("pixels-far capture");
    let img_px_far = decode_png(snap_px_far.captured("s").expect("s ran")).expect("decode px-far");
    let bb_px_far = bounding_box(&img_px_far, background_top_left(&img_px_far), tol)
        .expect("px-far frame has content");

    let px_near_w = (bb_px_near.max_x - bb_px_near.min_x + 1) as f32;
    let px_far_w = (bb_px_far.max_x - bb_px_far.min_x + 1) as f32;
    let px_ratio = px_far_w / px_near_w;
    assert!(
        (0.80..1.25).contains(&px_ratio),
        "Pixels label width at d=20 / d=10 = {px_ratio:.3} should be near 1.0 \
         (near={px_near_w}px, far={px_far_w}px); Pixels constant-size is broken",
    );

    // Orbit: a 45-degree horizontal orbit should not skew the label.
    // The Pixels-mode width at the orbit angle should be within ±30% of
    // the front-facing width — a true in-world quad would skew and widen
    // significantly.
    let snap_orbit = bench
        .execute(vec![(
            "s",
            BenchOp::capture_with_mails(
                vec![
                    envelope(
                        "aether.render",
                        &Camera {
                            view_proj: vp_orbit,
                        },
                    ),
                    envelope("aether.text", &draw_px),
                ],
                vec![],
            ),
        )])
        .expect("orbit capture");
    let img_orbit = decode_png(snap_orbit.captured("s").expect("s ran")).expect("decode orbit");
    let bb_orbit = bounding_box(&img_orbit, background_top_left(&img_orbit), tol)
        .expect("orbit frame has content");

    let orbit_w = (bb_orbit.max_x - bb_orbit.min_x + 1) as f32;
    let orbit_ratio = orbit_w / px_near_w;
    assert!(
        (0.70..1.43).contains(&orbit_ratio),
        "Pixels label width at 45-degree orbit / front-facing = {orbit_ratio:.3} should be \
         near 1.0 (orbit={orbit_w}px, front={px_near_w}px); label may be skewing with camera",
    );
}

/// ADR-0114 §5: an inline child carries its `type State` across a
/// `replace_component` swap. Loads `InlineStatefulParent` from the
/// `inline_child` bundle (issue 1994, ADR-0096) via
/// `export: Some("test.inline.stateful_parent")`, bumps the **child's**
/// counter to 2 through the child's first-class lineage address, replaces
/// the wasm at the same mailbox id with the same binary, then re-queries
/// the child's alias. The old instance's `on_dehydrate` packs the child's
/// state into the composite migration bundle; the new instance's
/// `on_rehydrate` reconstructs the child by type and restores its count —
/// so the post-replace query reads 2, not the fresh-`init` 0. Reload is
/// engine-internal correctness (dehydrate → composite → rehydrate
/// reconstruct), which is `TestBench`'s lane; #1916's `FleetBench` already
/// proved the over-the-wire child addressing, so this doesn't re-prove it.
#[test]
fn replace_preserves_inline_child_state_via_reconstruct() {
    use aether_actor::Addressable;

    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_NAME: &str = "inline_child_stateful";

    let Some(wasm_path) = require_runtime(BUNDLE_STEM) else {
        return;
    };
    let parent_addr = format!(
        "aether.component/{}:{FIXTURE_NAME}",
        aether_capabilities::WasmTrampoline::NAMESPACE,
    );
    // The child's first-class lineage address: the parent's rendered name
    // plus the inline-child node (ADR-0114). The parent spawns it under
    // the `Named("widget")` subname in `wire`.
    let child_addr = format!("{parent_addr}/aether.embedded:widget");

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load `InlineStatefulParent` from the `inline_child` bundle, capturing
    // its mailbox id for the replace. The name override keeps the registered
    // lineage address stable so the existing `parent_addr` / `child_addr`
    // strings remain valid.
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(FIXTURE_NAME.to_owned()),
                    config: Vec::new(),
                    export: Some("test.inline.stateful_parent".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    let mailbox_id = match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("inline_child_stateful load failed: {error}"),
    };

    // Bump the *child's* counter to 2 (mail demuxed to the child's alias),
    // then read it back. `send_mail` is fire-and-settle, so the bumps land
    // before the query.
    let pre = bench
        .execute(vec![
            (
                "bump_a",
                BenchOp::send_mail::<Bump>(child_addr.as_str(), &Bump),
            ),
            (
                "bump_b",
                BenchOp::send_mail::<Bump>(child_addr.as_str(), &Bump),
            ),
            (
                "query",
                BenchOp::send_and_await(child_addr.as_str(), &CountQuery),
            ),
        ])
        .expect("bump + query sequence");
    assert_eq!(
        pre.reply::<CountReport>("query")
            .expect("decode pre-replace CountReport"),
        CountReport { count: 2 },
        "two bumps should leave the inline child's counter at 2 before the replace",
    );

    // Replace the wasm at the parent's mailbox id with the same binary.
    // The old instance's `on_dehydrate` composites the child's state; the
    // new instance's `on_rehydrate` reconstructs the child and restores it.
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
                    export: None,
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

    // Query the reconstructed child's alias: the count must still be 2.
    // A 0 here means the child vanished across the reload (its state lost,
    // or it booted fresh) — the regression ADR-0114 §5 closes.
    let post = bench
        .execute(vec![(
            "query",
            BenchOp::send_and_await(child_addr.as_str(), &CountQuery),
        )])
        .expect("post-replace query sequence");
    let post_count = post
        .reply::<CountReport>("query")
        .expect("decode post-replace CountReport");
    assert_eq!(
        post_count,
        CountReport { count: 2 },
        "the inline child's state must survive replace_component via the composite bundle + \
         rehydrate reconstruct; got {post_count:?} (0 means the child was not reconstructed)",
    );
}

/// ADR-0114 teardown (#1939): an inline child torn down mid-life still
/// settles mail to its now-dead alias through the parent. Loads
/// `InlineDespawnParent` from the `inline_child` bundle (issue 1994,
/// ADR-0096) via `export: Some("test.inline.despawn_parent")`, probes
/// the child's first-class alias and asserts the *child* answers + the
/// chain settles, sends a `DespawnChild` trigger to the parent (which
/// calls `ctx.despawn_inline_child` on the stored alias), then probes the
/// **same** alias again. The substrate alias route is kept on teardown, so
/// the orphaned probe lands in the parent's inbox, the membrane finds no
/// resident child and falls through to the parent's dispatch tail — the
/// *parent* answers and the chain **settles**. A `SettlementTimeout` on
/// the post-teardown probe would be the leak this verb exists to prevent.
/// Teardown settlement is engine-internal (membrane fallthrough → parent
/// dispatch tail → `record_finished`), `TestBench`'s lane; #1916's
/// `FleetBench` already proved over-the-wire inline addressing.
#[test]
fn despawn_inline_child_settles_orphan_mail_via_parent() {
    use aether_actor::Addressable;

    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_NAME: &str = "inline_child_despawn";

    let Some(wasm_path) = require_runtime(BUNDLE_STEM) else {
        return;
    };
    let parent_addr = format!(
        "aether.component/{}:{FIXTURE_NAME}",
        aether_capabilities::WasmTrampoline::NAMESPACE,
    );
    // The child's first-class lineage address: the parent's rendered name
    // plus the inline-child node (ADR-0114). The parent spawns it under the
    // `Named("widget")` subname in `wire`.
    let child_addr = format!("{parent_addr}/aether.embedded:widget");

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load `InlineDespawnParent` from the `inline_child` bundle, then probe
    // the *live* child's alias: the membrane demuxes to the child, which
    // answers with the child marker, and the chain settles. The name override
    // keeps the registered lineage address stable so `parent_addr` / `child_addr`
    // remain valid.
    let live = bench
        .execute(vec![
            (
                "load",
                BenchOp::send_and_await(
                    "aether.component",
                    &LoadComponent {
                        wasm,
                        name: Some(FIXTURE_NAME.to_owned()),
                        config: Vec::new(),
                        export: Some("test.inline.despawn_parent".to_owned()),
                    },
                ),
            ),
            (
                "probe",
                BenchOp::send_and_await(child_addr.as_str(), &InlineProbe),
            ),
        ])
        .expect("load + live-probe sequence");
    match live.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("inline_child_despawn load failed: {error}"),
    }
    assert_eq!(
        live.reply::<InlineEcho>("probe")
            .expect("decode live-probe InlineEcho"),
        InlineEcho {
            who: INLINE_WHO_CHILD,
        },
        "a probe to the live child's alias is demuxed to and answered by the child",
    );

    // Tear the child down via the parent (`ctx.despawn_inline_child(self.child)`),
    // then probe the *same* alias again. The kept alias routes the orphaned
    // probe to the parent's dispatch tail, so it settles (a SettlementTimeout
    // here would be the leak this verb prevents) and the *parent* answers.
    let post = bench
        .execute(vec![
            (
                "despawn",
                BenchOp::send_mail::<DespawnChild>(parent_addr.as_str(), &DespawnChild),
            ),
            (
                "probe",
                BenchOp::send_and_await(child_addr.as_str(), &InlineProbe),
            ),
        ])
        .expect("despawn + post-teardown probe must settle, not SettlementTimeout");
    assert_eq!(
        post.reply::<InlineEcho>("probe")
            .expect("decode post-teardown InlineEcho"),
        InlineEcho {
            who: INLINE_WHO_PARENT,
        },
        "after teardown, a probe to the same alias falls through to the parent \
         (kept alias → membrane no resident child → parent dispatch tail)",
    );
}

/// ADR-0114 §5 no-regression: a childless component still hot-reloads
/// unchanged. The `stateful_replace` fixture spawns no inline children, so
/// its composite is byte-identical to its own `on_dehydrate` blob and the
/// reload behaves exactly as before the inline-child compose landed. This
/// guards the byte-identity invariant from the integration side; the
/// `aether-actor` unit `zero_children_compose_is_byte_identical_to_raw_parent`
/// guards it at the bundle layer.
#[test]
fn childless_component_hot_reloads_unchanged() {
    use aether_actor::Addressable;

    const FIXTURE_NAME: &str = "stateful_replace";

    let Some(wasm_path) = require_runtime("aether_test_fixtures_bundle") else {
        return;
    };
    let addr = format!(
        "aether.component/{}:{FIXTURE_NAME}",
        aether_capabilities::WasmTrampoline::NAMESPACE,
    );

    let mut bench = TestBench::start_with_size(64, 48).expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(FIXTURE_NAME.to_owned()),
                    config: Vec::new(),
                    // `Counter` is a non-entry actor in the bundle.
                    export: Some("stateful.counter".to_owned()),
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

    let pre = bench
        .execute(vec![
            ("bump_a", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("bump_b", BenchOp::send_mail::<Bump>(addr.as_str(), &Bump)),
            ("query", BenchOp::send_and_await(addr.as_str(), &CountQuery)),
        ])
        .expect("bump + query sequence");
    assert_eq!(
        pre.reply::<CountReport>("query")
            .expect("decode pre-replace CountReport"),
        CountReport { count: 2 },
        "two bumps leave the childless counter at 2 before the replace",
    );

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
                    export: None,
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

    let post = bench
        .execute(vec![(
            "query",
            BenchOp::send_and_await(addr.as_str(), &CountQuery),
        )])
        .expect("post-replace query sequence");
    assert_eq!(
        post.reply::<CountReport>("query")
            .expect("decode post-replace CountReport"),
        CountReport { count: 2 },
        "a childless component's state survives the reload unchanged (byte-identical composite)",
    );
}

// Pre-#775 the bench emitted `aether.observation.frame_stats` every
// 120 frames and a test verified one broadcast arrived after
// `advance(120)`. Issue 775 retired the broadcast cap, the frame_stats
// kind, and the helper that emitted it; this test went with them.
