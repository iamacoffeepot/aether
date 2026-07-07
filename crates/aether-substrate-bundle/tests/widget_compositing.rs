//! ADR-0117 widget-compositing end-to-end scenarios (issue 2659).
//!
//! A cluster of `aether-kit` `Widget` inline-child actors draws local and
//! composites up so the whole subtree reaches `aether.render` as **one**
//! ordered `DrawSolidQuads`. These are the gate that the protocol's own
//! logic — the filled-slot completion counter, `source_mailbox`
//! attribution, and the depth-first flatten — holds end-to-end through the
//! real inline-cluster FIFO drain, not just in the unit tests over the
//! `Composite` helper.
//!
//! Two properties are pinned per frame:
//!
//! - **One render sender.** `count_observed("aether.render.draw_solid_quads")`
//!   is exactly 1 after one frame, for a flat panel and for a two-level
//!   tree alike — the #1852 fan-in fix (the whole cluster is one sender
//!   regardless of widget count).
//! - **Structural draw order.** A background drawn as the root's own chrome
//!   sits *under* its children, and a nested subtree draws its own chrome
//!   under its own children — the depth-first order the tree structure
//!   encodes, read straight off the captured pixels by hue dominance.
//!
//! Skipped when no wgpu adapter is available or the `aether_kit` wasm has
//! not been pre-built (the shared `require_runtime` gate). CI sets
//! `AETHER_REQUIRE_RUNTIME=1` to turn either skip into a hard failure.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Pixel-rect layout constants read clearest as float literals inline.
#![allow(clippy::cast_precision_loss)]

use std::fs;

use aether_data::Kind;
use aether_kinds::{LoadComponent, LoadResult, NamedMail};
use aether_kit::{WidgetChildSpec, WidgetConfig, WidgetDrawItem};
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::require_runtime};
use aether_substrate_bundle::visual::{Image, background_top_left, decode_png};

/// Linear RGBA primaries chosen so each survives the sRGB encode as a
/// single dominant channel — the compositing order is then read off the
/// captured pixels by which channel wins, gamma-invariant.
const BLUE: [f32; 4] = [0.05, 0.05, 0.90, 1.0];
const RED: [f32; 4] = [0.90, 0.05, 0.05, 1.0];
const GREEN: [f32; 4] = [0.05, 0.90, 0.05, 1.0];
const WHITE: [f32; 4] = [0.95, 0.95, 0.95, 1.0];

/// The full trampoline address a loaded component registers at (ADR-0099
/// §4) — `aether.component` `/`-joined to the trampoline node named
/// `panel`, matching what `LoadResult.name` reports.
fn panel_address() -> String {
    use aether_actor::Addressable;
    format!(
        "aether.component/{}:panel",
        aether_capabilities::WasmTrampoline::NAMESPACE,
    )
}

/// A flat-colored quad draw item in the widget's own local coordinates.
fn quad(x: f32, y: f32, width: f32, height: f32, color: [f32; 4]) -> WidgetDrawItem {
    WidgetDrawItem::Quad {
        x,
        y,
        width,
        height,
        color,
    }
}

/// A leaf `WidgetConfig` (no children) whose only draw is one local quad,
/// pre-encoded to the bytes a parent's `WidgetChildSpec` carries.
fn leaf_config(width: f32, height: f32, color: [f32; 4]) -> Vec<u8> {
    WidgetConfig {
        root: false,
        chrome: vec![quad(0.0, 0.0, width, height, color)],
        intrinsic: None,
        children: Vec::new(),
    }
    .encode_into_bytes()
}

/// Load the `Widget` root export from the kit wasm under the name `panel`,
/// carrying `config`, and block on `LoadResult` so the root is
/// instantiated before the capture frame runs.
fn load_panel(bench: &mut TestBench, wasm: &[u8], config: &WidgetConfig) {
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some("panel".to_owned()),
                    config: config.encode_into_bytes(),
                    export: Some("aether.kit.widget".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { name, .. } => assert!(
            name.ends_with(":panel"),
            "the Widget root should register under :panel; got {name}",
        ),
        LoadResult::Err { error } => panic!("load Widget root: {error}"),
    }
}

/// One synthesized frame tick addressed straight to the root's mailbox, so
/// its `on_tick` runs exactly once during the captured frame — the same
/// way the probe scenarios synthesize `aether.lifecycle.tick` to drive a
/// draw right before readback.
fn tick_to_root() -> NamedMail {
    NamedMail {
        recipient_name: panel_address(),
        kind_name: "aether.lifecycle.tick".to_owned(),
        payload: Vec::new(),
        count: 1,
    }
}

/// The RGB of the captured pixel at `(x, y)`. The frame is 8-bit RGBA,
/// row-major top-down.
fn rgb_at(image: &Image, x: u32, y: u32) -> [u8; 3] {
    let idx = ((y * image.width + x) * 4) as usize;
    [image.rgba[idx], image.rgba[idx + 1], image.rgba[idx + 2]]
}

/// Whether channel `c` (0 = R, 1 = G, 2 = B) is the strict maximum at the
/// pixel — the hue-dominance test the color primaries are chosen for.
fn dominant(pixel: [u8; 3], channel: usize) -> bool {
    (0..3).all(|c| c == channel || pixel[channel] > pixel[c])
}

/// A flat panel: a blue root-chrome background under a red and a green
/// leaf child. Exactly one `DrawSolidQuads` reaches the render sink for
/// the whole cluster, and each child's fill sits over the background where
/// it overlaps — chrome-first structural order.
#[test]
fn flat_panel_is_one_sender_with_chrome_under_children() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");

    // Root chrome fills the middle (8,8)-(56,40); two leaves sit inside it
    // — child a red at (12,12)-(24,24), child b green at (36,20)-(48,32).
    let config = WidgetConfig {
        root: true,
        chrome: vec![quad(8.0, 8.0, 48.0, 32.0, BLUE)],
        intrinsic: None,
        children: vec![
            WidgetChildSpec {
                subname: "a".to_owned(),
                origin: [12.0, 12.0],
                config: leaf_config(12.0, 12.0, RED),
            },
            WidgetChildSpec {
                subname: "b".to_owned(),
                origin: [36.0, 20.0],
                config: leaf_config(12.0, 12.0, GREEN),
            },
        ],
    };
    load_panel(&mut bench, &wasm, &config);

    let captured = bench
        .execute(vec![(
            "snap",
            BenchOp::capture_with_mails(vec![tick_to_root()], vec![]),
        )])
        .expect("capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");

    assert_eq!(
        bench.count_observed("aether.render.draw_solid_quads"),
        1,
        "the whole two-widget cluster must reach the render sink as exactly one \
         DrawSolidQuads; observed: {:?}",
        bench.observed_kinds(),
    );

    // The corner is outside the root chrome, so it stays the clear color —
    // the panel did not paint the whole frame.
    let clear = background_top_left(&img);
    assert_eq!(rgb_at(&img, 1, 1), clear, "corner is untouched clear color");

    // Child fills sit over the background where they overlap it (chrome
    // drawn first, under): the child rects read as their own hue.
    assert!(
        dominant(rgb_at(&img, 18, 18), 0),
        "child a's rect should read red (its fill over the blue background), got {:?}",
        rgb_at(&img, 18, 18),
    );
    assert!(
        dominant(rgb_at(&img, 42, 26), 1),
        "child b's rect should read green, got {:?}",
        rgb_at(&img, 42, 26),
    );
    // A background-only pixel (inside the chrome rect, outside both
    // children) reads blue — the chrome is visible where nothing overdraws.
    assert!(
        dominant(rgb_at(&img, 30, 12), 2),
        "the background-only region should read blue, got {:?}",
        rgb_at(&img, 30, 12),
    );
}

/// A two-level tree: a blue root background, a red leaf, and a green
/// interior node that itself carries a white leaf. Still one render sender
/// for the whole tree, and the captured pixels show the depth-first order
/// blue (root chrome) under green (interior chrome) under white (the
/// interior's child) — a nested subtree drawing its own chrome under its
/// own children.
#[test]
fn nested_tree_draws_in_depth_first_order() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = TestBench::start_with_size(64, 48).expect("boot");

    // Interior node b: green chrome (0,0,20,20), one white leaf b1 inset at
    // local (2,2), sized 6×6.
    let b1 = WidgetConfig {
        root: false,
        chrome: vec![quad(0.0, 0.0, 6.0, 6.0, WHITE)],
        intrinsic: None,
        children: Vec::new(),
    }
    .encode_into_bytes();
    let interior_b = WidgetConfig {
        root: false,
        chrome: vec![quad(0.0, 0.0, 20.0, 20.0, GREEN)],
        intrinsic: None,
        children: vec![WidgetChildSpec {
            subname: "b1".to_owned(),
            origin: [2.0, 2.0],
            config: b1,
        }],
    }
    .encode_into_bytes();

    // Root: blue background (8,8,48,32); red leaf a at (12,14)-(20,22);
    // interior b anchored at (30,12) → its green chrome is (30,12)-(50,32)
    // and its white leaf b1 is (32,14)-(38,20).
    let config = WidgetConfig {
        root: true,
        chrome: vec![quad(8.0, 8.0, 48.0, 32.0, BLUE)],
        intrinsic: None,
        children: vec![
            WidgetChildSpec {
                subname: "a".to_owned(),
                origin: [12.0, 14.0],
                config: leaf_config(8.0, 8.0, RED),
            },
            WidgetChildSpec {
                subname: "b".to_owned(),
                origin: [30.0, 12.0],
                config: interior_b,
            },
        ],
    };
    load_panel(&mut bench, &wasm, &config);

    let captured = bench
        .execute(vec![(
            "snap",
            BenchOp::capture_with_mails(vec![tick_to_root()], vec![]),
        )])
        .expect("capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");

    assert_eq!(
        bench.count_observed("aether.render.draw_solid_quads"),
        1,
        "the whole two-level tree must reach the render sink as exactly one \
         DrawSolidQuads; observed: {:?}",
        bench.observed_kinds(),
    );

    // Depth-first order, read by hue: root chrome (blue) under the interior
    // node's chrome (green) under the interior's leaf (white).
    assert!(
        dominant(rgb_at(&img, 16, 18), 0),
        "leaf a reads red, got {:?}",
        rgb_at(&img, 16, 18),
    );
    // Inside b's green chrome but outside its white leaf: green wins,
    // proving b's chrome drew over the root's blue background.
    assert!(
        dominant(rgb_at(&img, 46, 28), 1),
        "the interior node's chrome should read green over the root background, got {:?}",
        rgb_at(&img, 46, 28),
    );
    // The interior's leaf b1: white draws over b's green chrome — every
    // channel is high, and the red channel in particular is far above the
    // green-chrome region's, proving b1 drew last (after its parent chrome).
    let b1_pixel = rgb_at(&img, 35, 17);
    assert!(
        b1_pixel[0] > 150 && b1_pixel[1] > 150 && b1_pixel[2] > 150,
        "the interior's white leaf should read bright on every channel (drawn over \
         its green parent chrome), got {b1_pixel:?}",
    );
}
