//! ADR-0117 widget-compositing end-to-end scenarios (issue 2659).
//!
//! A cluster of `aether-kit` `Widget` inline-child actors draws local and
//! composites up so the whole subtree reaches `aether.render` through **one
//! root sender**. Unclipped baselines remain one ordered `DrawSolidQuads`;
//! distinct effective clips may require multiple ordered batches. These are
//! the gate that the protocol's own
//! logic — the filled-slot completion counter, `source_mailbox`
//! attribution, and the depth-first flatten — holds end-to-end through the
//! real inline-cluster FIFO drain, not just in the unit tests over the
//! `Composite` helper.
//!
//! Two properties are pinned per frame:
//!
//! - **Unclipped one-batch baseline.**
//!   `count_observed("aether.render.draw_solid_quads")` is exactly 1 after one
//!   frame for an unclipped flat panel and two-level tree alike. Clipped runs
//!   may emit multiple mails, but every batch still comes from the one root
//!   sender — the #1852 fan-in fix regardless of widget count.
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

use aether_harness_substrate_capture::{RenderHarnessBuilderExt, RenderHarnessExt};
use std::fs;

use aether_data::Kind;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::test_helpers::require_runtime;
use aether_harness_substrate_capture::visual::{Image, Rect, background_top_left, decode_png, target_color_stats};
use aether_kinds::{ClipRect, LoadComponent, LoadResult, NamedMail, QuadSpace};
use aether_kit::{
    PanelConfig, ScrollConfig, ScrollExtent, ScrollOffset, Theme, WidgetChildSpec, WidgetClipRect, WidgetConfig,
    WidgetDrawItem, WidgetKind,
};
use aether_math::Rgba;
use aether_render::{
    CreateTexture, CreateTextureResult, TextureFormat, TexturedQuad as RenderTexturedQuad, WHITE_TEXTURE_ID,
};

/// Linear RGBA primaries chosen so each survives the sRGB encode as a
/// single dominant channel — the compositing order is then read off the
/// captured pixels by which channel wins, gamma-invariant.
const BLUE: Rgba = Rgba::new(0.05, 0.05, 0.90, 1.0);
const RED: Rgba = Rgba::new(0.90, 0.05, 0.05, 1.0);
const GREEN: Rgba = Rgba::new(0.05, 0.90, 0.05, 1.0);
const WHITE: Rgba = Rgba::new(0.95, 0.95, 0.95, 1.0);
const YELLOW: Rgba = Rgba::new(1.0, 1.0, 0.0, 1.0);
const TEXTURE_RED: [u8; 3] = [255, 0, 0];
const TEXTURE_GREEN: [u8; 3] = [0, 255, 0];
const TEXTURE_BLUE: [u8; 3] = [0, 0, 255];
const TEXTURE_YELLOW: [u8; 3] = [255, 255, 0];

/// The full trampoline address a loaded component registers at (ADR-0099
/// §4) — `aether.component` `/`-joined to the trampoline node named
/// `panel`, matching what `LoadResult.name` reports.
fn panel_address() -> String {
    use aether_actor::Addressable;
    format!("aether.component/{}:panel", aether_component::WasmTrampoline::NAMESPACE)
}

/// A flat-colored quad draw item in the widget's own local coordinates.
fn quad(x: f32, y: f32, width: f32, height: f32, color: Rgba) -> WidgetDrawItem {
    WidgetDrawItem::Quad { x, y, width, height, color, clip: None }
}

fn clipped_quad(x: f32, y: f32, width: f32, height: f32, color: Rgba, clip: WidgetClipRect) -> WidgetDrawItem {
    WidgetDrawItem::Quad { x, y, width, height, color, clip: Some(clip) }
}

#[allow(clippy::too_many_arguments)]
fn textured_quad(
    texture_id: u32,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
    tint: Rgba,
) -> WidgetDrawItem {
    WidgetDrawItem::TexturedQuad { texture_id, x, y, width, height, u0, v0, u1, v1, tint, clip: None }
}

fn four_color_texture_pixels(size: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let color = match (x < size / 2, y < size / 2) {
                (true, true) => TEXTURE_RED,
                (false, true) => TEXTURE_GREEN,
                (true, false) => TEXTURE_BLUE,
                (false, false) => TEXTURE_YELLOW,
            };
            pixels.extend_from_slice(&[color[0], color[1], color[2], 255]);
        }
    }
    pixels
}

fn create_four_color_texture(harness: &mut SubstrateHarness) -> u32 {
    let size = 8;
    let created = harness
        .execute(vec![(
            "create",
            HarnessOp::send_and_await(
                "aether.render",
                &CreateTexture {
                    width: size,
                    height: size,
                    format: TextureFormat::Rgba8,
                    pixels: four_color_texture_pixels(size),
                },
            ),
        )])
        .expect("create four-color texture");
    match created.reply::<CreateTextureResult>("create").expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create texture: {error}"),
    }
}

/// A leaf `WidgetConfig` (no children) whose only draw is one local quad,
/// pre-encoded to the bytes a parent's `WidgetChildSpec` carries.
fn leaf_config(width: f32, height: f32, color: Rgba) -> Vec<u8> {
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
fn load_panel(harness: &mut SubstrateHarness, wasm: &[u8], config: &WidgetConfig) {
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await(
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
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name, .. } => {
            assert!(name.ends_with(":panel"), "the Widget root should register under :panel; got {name}");
        }
        LoadResult::Err { error } => panic!("load Widget root: {error}"),
    }
}

fn load_scroll_panel(harness: &mut SubstrateHarness, wasm: &[u8], child: WidgetChildSpec) {
    let config = PanelConfig {
        x: 12.0,
        y: 8.0,
        width: 64.0,
        font_namespace: String::new(),
        font_path: String::new(),
        theme: Theme::DEFAULT,
        children: vec![child],
        owns_input: true,
    };
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some("panel".to_owned()),
                    config: config.encode_into_bytes(),
                    export: Some("aether.kit.widget.panel".to_owned()),
                },
            ),
        )])
        .expect("load scroll panel sequence");
    match loaded.reply::<LoadResult>("load").expect("decode scroll-panel LoadResult") {
        LoadResult::Ok { name, .. } => assert!(name.ends_with(":panel")),
        LoadResult::Err { error } => panic!("load scroll WidgetPanel: {error}"),
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
    let mut harness =
        SubstrateHarness::builder().size(64, 48).with_render().with_component_host().build().expect("boot");

    // Root chrome fills the middle (8,8)-(56,40); two leaves sit inside it
    // — child a red at (12,12)-(24,24), child b green at (36,20)-(48,32).
    let config = WidgetConfig {
        root: true,
        chrome: vec![quad(8.0, 8.0, 48.0, 32.0, BLUE)],
        intrinsic: None,
        children: vec![
            WidgetChildSpec {
                subname: "a".to_owned(),
                kind: WidgetKind::Composite,
                origin: [12.0, 12.0],
                clip: None,
                config: leaf_config(12.0, 12.0, RED),
            },
            WidgetChildSpec {
                subname: "b".to_owned(),
                kind: WidgetKind::Composite,
                origin: [36.0, 20.0],
                clip: None,
                config: leaf_config(12.0, 12.0, GREEN),
            },
        ],
    };
    load_panel(&mut harness, &wasm, &config);

    let captured = harness
        .execute(vec![("snap", HarnessOp::capture_with_mails(vec![tick_to_root()], vec![]))])
        .expect("capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");

    assert_eq!(
        harness.count_observed("aether.render.draw_solid_quads"),
        1,
        "the whole two-widget cluster must reach the render sink as exactly one \
         DrawSolidQuads; observed: {:?}",
        harness.observed_kinds(),
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
    assert!(dominant(rgb_at(&img, 42, 26), 1), "child b's rect should read green, got {:?}", rgb_at(&img, 42, 26));
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
    let mut harness =
        SubstrateHarness::builder().size(64, 48).with_render().with_component_host().build().expect("boot");

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
            kind: WidgetKind::Composite,
            origin: [2.0, 2.0],
            clip: None,
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
                kind: WidgetKind::Composite,
                origin: [12.0, 14.0],
                clip: None,
                config: leaf_config(8.0, 8.0, RED),
            },
            WidgetChildSpec {
                subname: "b".to_owned(),
                kind: WidgetKind::Composite,
                origin: [30.0, 12.0],
                clip: None,
                config: interior_b,
            },
        ],
    };
    load_panel(&mut harness, &wasm, &config);

    let captured = harness
        .execute(vec![("snap", HarnessOp::capture_with_mails(vec![tick_to_root()], vec![]))])
        .expect("capture-with-mails");
    let png = captured.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");

    assert_eq!(
        harness.count_observed("aether.render.draw_solid_quads"),
        1,
        "the whole two-level tree must reach the render sink as exactly one \
         DrawSolidQuads; observed: {:?}",
        harness.observed_kinds(),
    );

    // Depth-first order, read by hue: root chrome (blue) under the interior
    // node's chrome (green) under the interior's leaf (white).
    assert!(dominant(rgb_at(&img, 16, 18), 0), "leaf a reads red, got {:?}", rgb_at(&img, 16, 18));
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

#[test]
#[allow(clippy::too_many_lines)] // one cohesive nested clip structural + pixel acceptance run
fn nested_local_clips_forward_exact_runs_and_contain_oversized_pixels() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");

    let leaf_clip = WidgetClipRect { x: 6.0, y: 5.0, width: 10.0, height: 8.0 };
    let root_clip = WidgetClipRect { x: 12.0, y: 10.0, width: 20.0, height: 16.0 };
    let green_local_clip = WidgetClipRect { x: 2.0, y: 1.0, width: 20.0, height: 20.0 };
    let disjoint_local_clip = WidgetClipRect { x: 30.0, y: 30.0, width: 4.0, height: 4.0 };
    let red_local_clip = WidgetClipRect { x: 0.0, y: 0.0, width: 30.0, height: 20.0 };

    let leaf_items = vec![
        clipped_quad(0.0, 0.0, 30.0, 20.0, GREEN, green_local_clip),
        clipped_quad(0.0, 0.0, 30.0, 20.0, WHITE, disjoint_local_clip),
    ];

    let leaf =
        WidgetConfig { root: false, chrome: leaf_items, intrinsic: None, children: Vec::new() }.encode_into_bytes();
    let interior = WidgetConfig {
        root: false,
        chrome: vec![clipped_quad(0.0, 0.0, 30.0, 20.0, RED, red_local_clip)],
        intrinsic: None,
        children: vec![WidgetChildSpec {
            subname: "leaf".to_owned(),
            kind: WidgetKind::Composite,
            origin: [4.0, 3.0],
            clip: Some(leaf_clip),
            config: leaf,
        }],
    }
    .encode_into_bytes();
    let config = WidgetConfig {
        root: true,
        chrome: vec![quad(0.0, 0.0, 64.0, 48.0, BLUE)],
        intrinsic: None,
        children: vec![WidgetChildSpec {
            subname: "interior".to_owned(),
            kind: WidgetKind::Composite,
            origin: [10.0, 8.0],
            clip: Some(root_clip),
            config: interior,
        }],
    };

    let mut harness =
        SubstrateHarness::builder().size(64, 48).with_render().with_component_host().build().expect("boot");
    load_panel(&mut harness, &wasm, &config);
    let captured = harness
        .execute(vec![("snap", HarnessOp::capture_with_mails(vec![tick_to_root()], vec![]))])
        .expect("capture clipped tree");
    let img = decode_png(captured.captured("snap").expect("snap bytes")).expect("decode clipped capture");

    let solids: Vec<_> =
        harness.committed_overlay_snapshot().into_iter().filter(|batch| batch.texture_id == WHITE_TEXTURE_ID).collect();
    assert_eq!(solids.len(), 3, "None, root clip, and nested clip are three runs");
    assert_eq!(solids[0].clip, None);
    assert_eq!(solids[1].clip, Some(ClipRect { x: 12.0, y: 10.0, width: 20.0, height: 16.0 }),);
    assert_eq!(solids[2].clip, Some(ClipRect { x: 16.0, y: 13.0, width: 10.0, height: 8.0 }),);
    assert_eq!(solids[0].quads.len(), 1);
    assert_eq!(solids[1].quads.len(), 1);
    assert_eq!(solids[2].quads.len(), 1);
    assert_eq!(solids[0].quads[0].tint, BLUE);
    assert_eq!(solids[1].quads[0].tint, RED);
    assert_eq!(solids[2].quads[0].tint, GREEN);
    assert_eq!(
        (solids[1].quads[0].x, solids[1].quads[0].y, solids[1].quads[0].width, solids[1].quads[0].height,),
        (10.0, 8.0, 30.0, 20.0),
    );
    assert_eq!(
        (solids[2].quads[0].x, solids[2].quads[0].y, solids[2].quads[0].width, solids[2].quads[0].height,),
        (14.0, 11.0, 30.0, 20.0),
    );

    assert!(dominant(rgb_at(&img, 11, 11), 2), "red is clipped before x=12");
    assert!(dominant(rgb_at(&img, 13, 11), 0), "red appears inside its clip");
    assert!(dominant(rgb_at(&img, 15, 15), 0), "green is clipped before x=16");
    assert!(dominant(rgb_at(&img, 18, 16), 1), "green appears inside the nested clip");
    assert!(dominant(rgb_at(&img, 33, 12), 2), "red is clipped after x=32");
}

/// Mixed root/child solids and textured items keep structural painter order,
/// nested clips, UVs, and texture identity through the guest wire and the real
/// render accumulator. The same capture proves the four-color texture is
/// sampled, clipped, and overdrawn where the final solid says it should be.
#[test]
#[allow(clippy::too_many_lines)] // one cohesive typed snapshot + raster acceptance scenario
fn textured_items_preserve_nested_order_clips_uvs_and_pixels() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut harness =
        SubstrateHarness::builder().size(64, 48).with_render().with_component_host().build().expect("boot");
    let texture_id = create_four_color_texture(&mut harness);

    let root_texture_clip = WidgetClipRect { x: 6.0, y: 6.0, width: 12.0, height: 12.0 };
    let child_clip = WidgetClipRect { x: 26.0, y: 10.0, width: 26.0, height: 24.0 };
    let child = WidgetConfig {
        root: false,
        chrome: vec![
            quad(0.0, 0.0, 28.0, 26.0, GREEN),
            textured_quad(texture_id, 0.0, 0.0, 16.0, 16.0, 0.0, 0.0, 0.5, 0.5, Rgba::WHITE),
            textured_quad(texture_id, 12.0, 8.0, 16.0, 16.0, 0.0, 0.5, 0.5, 1.0, Rgba::new(0.75, 1.0, 1.0, 1.0)),
            quad(20.0, 14.0, 8.0, 8.0, YELLOW),
        ],
        intrinsic: None,
        children: Vec::new(),
    }
    .encode_into_bytes();
    let config = WidgetConfig {
        root: true,
        chrome: vec![
            quad(0.0, 0.0, 64.0, 48.0, BLUE),
            WidgetDrawItem::TexturedQuad {
                texture_id,
                x: 4.0,
                y: 4.0,
                width: 16.0,
                height: 16.0,
                u0: 0.0,
                v0: 0.0,
                u1: 0.5,
                v1: 0.5,
                tint: Rgba::WHITE,
                clip: Some(root_texture_clip),
            },
        ],
        intrinsic: None,
        children: vec![WidgetChildSpec {
            subname: "child".to_owned(),
            kind: WidgetKind::Composite,
            origin: [24.0, 8.0],
            clip: Some(child_clip),
            config: child,
        }],
    };
    load_panel(&mut harness, &wasm, &config);

    let captured = harness
        .execute(vec![("snap", HarnessOp::capture_with_mails(vec![tick_to_root()], vec![]))])
        .expect("capture textured widget tree");
    let img = decode_png(captured.captured("snap").expect("snap bytes")).expect("decode textured capture");

    let child_framebuffer_clip =
        ClipRect { x: child_clip.x, y: child_clip.y, width: child_clip.width, height: child_clip.height };
    let snapshot = harness.committed_overlay_snapshot();
    assert_eq!(snapshot.len(), 5, "solid/textured transitions form five runs");

    assert_eq!(snapshot[0].texture_id, WHITE_TEXTURE_ID);
    assert_eq!(snapshot[0].space, QuadSpace::Screen);
    assert_eq!(snapshot[0].clip, None);
    assert_eq!(
        snapshot[0].quads,
        vec![RenderTexturedQuad {
            x: 0.0,
            y: 0.0,
            width: 64.0,
            height: 48.0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: BLUE,
        }],
    );

    assert_eq!(snapshot[1].texture_id, texture_id);
    assert_eq!(snapshot[1].space, QuadSpace::Screen);
    assert_eq!(
        snapshot[1].clip,
        Some(ClipRect {
            x: root_texture_clip.x,
            y: root_texture_clip.y,
            width: root_texture_clip.width,
            height: root_texture_clip.height,
        }),
    );
    assert_eq!(
        snapshot[1].quads,
        vec![RenderTexturedQuad {
            x: 4.0,
            y: 4.0,
            width: 16.0,
            height: 16.0,
            u0: 0.0,
            v0: 0.0,
            u1: 0.5,
            v1: 0.5,
            tint: Rgba::WHITE,
        }],
    );

    assert_eq!(snapshot[2].texture_id, WHITE_TEXTURE_ID);
    assert_eq!(snapshot[2].space, QuadSpace::Screen);
    assert_eq!(snapshot[2].clip, Some(child_framebuffer_clip.clone()));
    assert_eq!(
        snapshot[2].quads,
        vec![RenderTexturedQuad {
            x: 24.0,
            y: 8.0,
            width: 28.0,
            height: 26.0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: GREEN,
        }],
    );

    assert_eq!(snapshot[3].texture_id, texture_id);
    assert_eq!(snapshot[3].space, QuadSpace::Screen);
    assert_eq!(snapshot[3].clip, Some(child_framebuffer_clip.clone()));
    assert_eq!(
        snapshot[3].quads,
        vec![
            RenderTexturedQuad {
                x: 24.0,
                y: 8.0,
                width: 16.0,
                height: 16.0,
                u0: 0.0,
                v0: 0.0,
                u1: 0.5,
                v1: 0.5,
                tint: Rgba::WHITE,
            },
            RenderTexturedQuad {
                x: 36.0,
                y: 16.0,
                width: 16.0,
                height: 16.0,
                u0: 0.0,
                v0: 0.5,
                u1: 0.5,
                v1: 1.0,
                tint: Rgba::new(0.75, 1.0, 1.0, 1.0),
            },
        ],
    );

    assert_eq!(snapshot[4].texture_id, WHITE_TEXTURE_ID);
    assert_eq!(snapshot[4].space, QuadSpace::Screen);
    assert_eq!(snapshot[4].clip, Some(child_framebuffer_clip));
    assert_eq!(
        snapshot[4].quads,
        vec![RenderTexturedQuad {
            x: 44.0,
            y: 22.0,
            width: 8.0,
            height: 8.0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: YELLOW,
        }],
    );

    let tolerance = 20;
    let intended_region = Rect { min_x: 29, min_y: 13, max_x: 33, max_y: 17 };
    let intended = target_color_stats(&img, TEXTURE_RED, tolerance, Some(intended_region));
    assert!(intended.fraction > 0.8, "the child's red UV crop should own its intended region: {intended:?}");
    let wrong_color = target_color_stats(&img, TEXTURE_GREEN, tolerance, Some(intended_region));
    assert!(wrong_color.fraction < 0.1, "the red crop must not silently sample the green quadrant: {wrong_color:?}");
    let clipped_out =
        target_color_stats(&img, TEXTURE_RED, tolerance, Some(Rect { min_x: 24, min_y: 12, max_x: 25, max_y: 17 }));
    assert!(clipped_out.fraction < 0.1, "the textured child must not escape its slot clip: {clipped_out:?}");
    let overlap_blue =
        target_color_stats(&img, TEXTURE_BLUE, tolerance, Some(Rect { min_x: 38, min_y: 18, max_x: 42, max_y: 20 }));
    assert!(
        overlap_blue.fraction > 0.8,
        "the later blue UV crop should overdraw the earlier red crop: {overlap_blue:?}",
    );
    let final_region = Rect { min_x: 46, min_y: 24, max_x: 49, max_y: 27 };
    let final_yellow = target_color_stats(&img, TEXTURE_YELLOW, tolerance, Some(final_region));
    assert!(final_yellow.fraction > 0.8, "the final solid should overdraw the blue textured item: {final_yellow:?}");
    let covered_blue = target_color_stats(&img, TEXTURE_BLUE, tolerance, Some(final_region));
    assert!(covered_blue.fraction < 0.1, "the blue crop should be hidden beneath the final solid: {covered_blue:?}");
}

#[test]
#[allow(clippy::too_many_lines)] // one cohesive exact-layout + four-edge pixel proof
fn scroll_composition_offsets_content_and_contains_pixels_on_every_viewport_edge() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let content = WidgetConfig {
        root: false,
        chrome: vec![quad(0.0, 0.0, 40.0, 32.0, RED), quad(12.0, 12.0, 8.0, 8.0, GREEN)],
        intrinsic: Some([40.0, 32.0]),
        children: Vec::new(),
    };
    let scroll = WidgetChildSpec {
        subname: "scroll".to_owned(),
        kind: WidgetKind::Scroll,
        origin: [0.0, 0.0],
        clip: None,
        config: ScrollConfig {
            viewport_extent: ScrollExtent { width_pixels: 24.0, height_pixels: 16.0 },
            content_extent: ScrollExtent { width_pixels: 40.0, height_pixels: 32.0 },
            initial_offset: ScrollOffset { x_pixels: 8.0, y_pixels: 10.0 },
            content: WidgetChildSpec {
                subname: "content".to_owned(),
                kind: WidgetKind::Composite,
                origin: [4.0, 3.0],
                clip: None,
                config: content.encode_into_bytes(),
            },
        }
        .encode_into_bytes(),
    };

    let mut harness =
        SubstrateHarness::builder().size(80, 48).with_render().with_component_host().build().expect("boot");
    load_scroll_panel(&mut harness, &wasm, scroll);
    let captured = harness
        .execute(vec![("snap", HarnessOp::capture_with_mails(vec![tick_to_root()], Vec::new()))])
        .expect("capture scrolled composite");
    let image = decode_png(captured.captured("snap").expect("scroll capture bytes")).expect("decode scroll capture");

    let clip = ClipRect { x: 12.0, y: 8.0, width: 24.0, height: 16.0 };
    let snapshot = harness.committed_overlay_snapshot();
    let content_batch = snapshot
        .iter()
        .find(|batch| batch.clip.as_ref() == Some(&clip))
        .unwrap_or_else(|| panic!("missing scroll viewport batch {clip:?}: {snapshot:?}"));
    assert_eq!(content_batch.texture_id, WHITE_TEXTURE_ID);
    assert_eq!(
        content_batch.quads,
        vec![
            RenderTexturedQuad {
                x: 8.0,
                y: 1.0,
                width: 40.0,
                height: 32.0,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                tint: RED,
            },
            RenderTexturedQuad {
                x: 20.0,
                y: 13.0,
                width: 8.0,
                height: 8.0,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                tint: GREEN,
            },
        ],
        "content_origin - initial_offset and panel placement agree exactly",
    );
    assert_eq!(
        harness.count_observed("aether.render.draw_solid_quads"),
        2,
        "the panel background and one equal-clip content run are the only solid batches",
    );

    let strong_primary = |pixel: [u8; 3], channel: usize| {
        (0..3).all(|other| other == channel || i16::from(pixel[channel]) > i16::from(pixel[other]) + 80)
    };
    assert!(strong_primary(rgb_at(&image, 14, 10), 0));
    assert!(strong_primary(rgb_at(&image, 22, 15), 1));
    for (x, y, side) in [(11, 12, "left"), (36, 12, "right"), (20, 7, "top"), (20, 24, "bottom")] {
        let pixel = rgb_at(&image, x, y);
        assert!(
            !strong_primary(pixel, 0) && !strong_primary(pixel, 1),
            "scroll content escaped the {side} clip edge at ({x}, {y}): {pixel:?}",
        );
    }
}
