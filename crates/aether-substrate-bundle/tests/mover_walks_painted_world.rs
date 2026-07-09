//! Live-gameplay acceptance: an input-driven body walks the painted world
//! (issue 2651).
//!
//! Loads the two `aether-kit` actors the first live-gameplay rung composes —
//! `WorldView` (`aether.world`, which paints the cell lattice) and
//! `WorldMover` (`aether.kit.mover`, the controllable body) — paints a
//! high-contrast chunk, drives the mover with a held `W`, and captures before
//! and after the walk. The mover owns the follow-camera, so the marker stays
//! centered while the world-anchored ground scrolls beneath it: the honest
//! rendered signal that the body moved across the painted world is that the
//! two frames differ. The cell-committed cadence itself (commit-lands-on-
//! center, velocity-normalized diagonals) is pinned by `WorldMover`'s own unit
//! tests; this is the composition-and-motion proof the harness split routes to
//! `TestBench` (rendered output can only be asserted on the GPU path).
//!
//! Skipped when no wgpu adapter is available or the `aether_kit` wasm has not
//! been pre-built (the shared `require_runtime` gate). CI sets
//! `AETHER_REQUIRE_RUNTIME=1` to turn either skip into a hard failure.

// Integration-test skip diagnostic: emit via stderr so `cargo test` surfaces
// "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]

use std::fs;

use aether_actor::Addressable;
use aether_capabilities::render::{CreateTexture, DrawMaterialCoverage, UpdateTexture};
use aether_data::Kind;
use aether_kinds::keycode::KEY_W;
use aether_kinds::{Key, LoadComponent, LoadResult, NamedMail, Render, WindowSize};
use aether_kit::world::Material;
use aether_kit::{MoverTeleport, SetChunk};
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::require_runtime};
use aether_substrate_bundle::visual::{
    background_top_left, coverage, decode_png, mean_absolute_error,
};

/// Capture surface — a 4:3 frame the mover's follow-camera aspect matches once
/// the `WindowSize` below lands.
const WINDOW_WIDTH: u32 = 128;
const WINDOW_HEIGHT: u32 = 96;

/// Chunk edge in cells (`CELLS_PER_CHUNK`), mirrored here so the split-paint
/// plane below is the right length.
const CHUNK_EDGE: usize = 16;
const SUBCELL_EDGE: usize = 16;
const SUBCELLS_PER_CELL: usize = SUBCELL_EDGE * SUBCELL_EDGE;
const OVERLAY_MASK_BYTES: usize = CHUNK_EDGE * CHUNK_EDGE * SUBCELLS_PER_CELL;

/// The full trampoline address a loaded component registers at (ADR-0099 §4):
/// the component host `/`-joined to the trampoline node under `name`.
fn component_address(name: &str) -> String {
    format!(
        "aether.component/{}:{name}",
        aether_capabilities::WasmTrampoline::NAMESPACE,
    )
}

/// A `NamedMail` carrying `mail`'s wire encoding to `recipient` — the capture
/// bundle / input-injection envelope.
fn envelope<K: Kind>(recipient: &str, mail: &K) -> NamedMail {
    NamedMail {
        recipient_name: recipient.to_owned(),
        kind_name: K::NAME.to_owned(),
        payload: mail.encode_into_bytes(),
        count: 1,
    }
}

/// Load one `aether_kit` export under `name`, blocking on `LoadResult` so the
/// component is instantiated and subscribed before the next op.
fn load_kit_export(bench: &mut TestBench, wasm: &[u8], export: &str, name: &str) {
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some(name.to_owned()),
                    config: Vec::new(),
                    export: Some(export.to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded
        .reply::<LoadResult>("load")
        .expect("decode LoadResult")
    {
        LoadResult::Ok { name: addr, .. } => assert!(
            addr.ends_with(&format!(":{name}")),
            "export {export} should register under :{name}; got {addr}",
        ),
        LoadResult::Err { error } => panic!("load {export}: {error}"),
    }
}

/// A single chunk 0 underlay plane split down the middle — grass on the west
/// half, stone on the east — so a sharp, world-anchored material boundary sits
/// in the scene. That boundary is the high-contrast feature the follow-camera
/// scrolls over as the body walks, guaranteeing the two captures differ.
fn split_chunk() -> SetChunk {
    let mut underlay = vec![0u8; CHUNK_EDGE * CHUNK_EDGE];
    let mut overlay = vec![0u8; CHUNK_EDGE * CHUNK_EDGE];
    let mut overlay_mask = vec![0u8; OVERLAY_MASK_BYTES];
    for z in 0..CHUNK_EDGE {
        for x in 0..CHUNK_EDGE {
            let material = if x < CHUNK_EDGE / 2 {
                Material::Grass
            } else {
                Material::Stone
            };
            underlay[z * CHUNK_EDGE + x] = material.to_u8();
            if (5..11).contains(&x) && (4..12).contains(&z) {
                overlay[z * CHUNK_EDGE + x] = Material::Sand.to_u8();
                let base = (z * CHUNK_EDGE + x) * SUBCELLS_PER_CELL;
                overlay_mask[base..base + SUBCELLS_PER_CELL].fill(u8::MAX);
            }
        }
    }
    SetChunk {
        chunk_x: 0,
        chunk_z: 0,
        underlay,
        underlay_points: Vec::new(),
        height_points: Vec::new(),
        overlay,
        overlay_mask,
        height: Vec::new(),
        region: Vec::new(),
        water_plane: Vec::new(),
        smoothing: Vec::new(),
    }
}

/// Capture one frame that draws both actors: the mover's `Render` publishes its
/// follow-camera and marker, the world-view's `Render` replays the painted
/// ground under that same camera, both into the accumulator right before the
/// GPU readback.
fn capture_scene(bench: &mut TestBench, mover: &str, world: &str, label: &'static str) -> Vec<u8> {
    let pre = vec![envelope(mover, &Render), envelope(world, &Render)];
    let captured = bench
        .execute(vec![(label, BenchOp::capture_with_mails(pre, Vec::new()))])
        .expect("capture-with-mails");
    captured.captured(label).expect("capture step ran").to_vec()
}

/// **The first live-gameplay rung, end to end.** A held `W` walks the body
/// north across the painted meadow-and-stone chunk under camera follow; the
/// world-anchored ground scrolls beneath the centered marker, so the frame
/// after the walk differs from the frame before it. Proves the two actors
/// co-load (the `WorldMover` export wiring this change adds), compose over the
/// shared render sink + latest-wins camera, and that input drives motion across
/// the real world data — not the tile arena.
#[test]
#[allow(clippy::cast_precision_loss)]
fn held_key_walks_the_body_across_the_painted_world() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = TestBench::start_with_size(WINDOW_WIDTH, WINDOW_HEIGHT).expect("boot");

    let world = component_address("world");
    let mover = component_address("mover");
    load_kit_export(&mut bench, &wasm, "aether.kit.world", "world");
    load_kit_export(&mut bench, &wasm, "aether.kit.mover", "mover");

    // Paint the split chunk, feed the mover a real window aspect, and place the
    // body on the grass/stone boundary column with room to walk north. Then
    // settle the subscriptions before the first capture.
    bench
        .execute(vec![
            ("paint", BenchOp::send_mail(world.as_str(), &split_chunk())),
            (
                "aspect",
                BenchOp::send_mail(
                    mover.as_str(),
                    &WindowSize {
                        width: WINDOW_WIDTH,
                        height: WINDOW_HEIGHT,
                    },
                ),
            ),
            (
                "place",
                BenchOp::send_mail(
                    mover.as_str(),
                    &MoverTeleport {
                        cell_x: 8,
                        cell_z: 12,
                    },
                ),
            ),
            ("settle", BenchOp::advance(2)),
        ])
        .expect("paint + place + settle");

    let before = capture_scene(&mut bench, &mover, &world, "before");
    assert!(
        bench.count_observed(CreateTexture::NAME) > 0,
        "painted overlay should upload an R8 coverage texture; observed kinds: {:?}",
        bench.observed_kinds(),
    );
    assert!(
        bench.count_observed(DrawMaterialCoverage::NAME) > 0,
        "painted overlay should render through a coverage material, not overlay triangles; \
         observed kinds: {:?}",
        bench.observed_kinds(),
    );

    // Hold W (no release) and advance four cells' worth of travel: at 8
    // octimeters/tick a 256-octimeter cell takes 32 ticks, so 128 ticks walks
    // the body from cell z=12 to z=8.
    bench
        .execute(vec![
            (
                "press_w",
                BenchOp::send_mail(mover.as_str(), &Key { code: KEY_W }),
            ),
            ("walk", BenchOp::advance(128)),
        ])
        .expect("hold W + walk");

    let after = capture_scene(&mut bench, &mover, &world, "after");

    let before_img = decode_png(&before).expect("decode before png");
    let after_img = decode_png(&after).expect("decode after png");

    // Both frames rendered a real scene: the painted ground plus the marker
    // fill a healthy fraction of the frame, far from an empty (clear-color)
    // capture.
    let before_cov = coverage(&before_img, background_top_left(&before_img), 5);
    let after_cov = coverage(&after_img, background_top_left(&after_img), 5);
    eprintln!("scene coverage: before={before_cov:.3} after={after_cov:.3}");
    assert!(
        before_cov > 0.2 && after_cov > 0.2,
        "both captures should render the painted ground + marker (coverage > 0.2); \
         before={before_cov:.3} after={after_cov:.3} — the actors did not compose a scene",
    );

    // The body moved: walking north scrolled the world-anchored ground under
    // the follow-camera, so the two frames diverge. The tolerance band absorbs
    // GPU nondeterminism at the low end and rules out a garbage/all-different
    // frame at the high end.
    let mae = mean_absolute_error(&after_img, &before_img).expect("same-size frames");
    eprintln!("frame mean-absolute-error across the walk: {mae:.3}");
    assert!(
        (0.02..0.9).contains(&mae),
        "walking the body should scroll the painted world under the camera; the frame \
         mean-absolute-error was {mae:.3} (expected 0.02..0.9) — the body did not move \
         across the painted ground",
    );
}

#[test]
fn painted_overlay_repaint_updates_the_coverage_texture() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read kit wasm");
    let mut bench = TestBench::start_with_size(WINDOW_WIDTH, WINDOW_HEIGHT).expect("boot");

    let world = component_address("world");
    load_kit_export(&mut bench, &wasm, "aether.kit.world", "world");

    let mut repainted = split_chunk();
    let cell = 7 * CHUNK_EDGE + 7;
    repainted.overlay[cell] = Material::Void.to_u8();
    let base = cell * SUBCELLS_PER_CELL;
    repainted.overlay_mask[base..base + SUBCELLS_PER_CELL].fill(0);

    bench
        .execute(vec![
            ("paint", BenchOp::send_mail(world.as_str(), &split_chunk())),
            ("settle_create", BenchOp::advance(2)),
            ("repaint", BenchOp::send_mail(world.as_str(), &repainted)),
            ("settle_update", BenchOp::advance(1)),
            (
                "draw",
                BenchOp::capture_with_mails(vec![envelope(&world, &Render)], Vec::new()),
            ),
        ])
        .expect("paint + repaint + capture");

    assert!(
        bench.count_observed(CreateTexture::NAME) > 0,
        "initial overlay paint should create an R8 texture; observed kinds: {:?}",
        bench.observed_kinds(),
    );
    assert!(
        bench.count_observed(UpdateTexture::NAME) > 0,
        "overlay repaint should update the existing coverage texture; observed kinds: {:?}",
        bench.observed_kinds(),
    );
    assert!(
        bench.count_observed(DrawMaterialCoverage::NAME) > 0,
        "updated overlay should still draw through coverage material; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}
