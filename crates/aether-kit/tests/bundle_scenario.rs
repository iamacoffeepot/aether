//! Reference asset bundle scenario tests (ADR-0163 §4). Each boots a
//! `SubstrateHarness`, loads `aether-kit`'s wasm artifact (built
//! separately for `wasm32-unknown-unknown`) selecting the non-entry
//! `aether.kit.bundle` export (ADR-0096), and drives the residency
//! lifecycle the reference actor bakes in:
//!
//! - `wire` pulls the embedded tile through the load window and uploads
//!   it as a texture, and the tick handler draws the resident every frame
//!   — which it can only do after the `create_texture` reply landed and the
//!   `texture_id` was stored. A committed overlay batch over a non-white
//!   texture (`committed_overlay_snapshot`) proves the whole warm→hot chain
//!   closed;
//! - dropping the component runs `unwire`, which destroys exactly the
//!   texture `wire` created — the symmetric-teardown convention the
//!   reference actor enforces by example. Texture ids are never reused, so
//!   a probe draw naming the resident's id that no longer records proves
//!   that id's registry entry is gone.
//!
//! These assert THIS actor's lifecycle logic — the pull→upload→store→draw
//! chain and the create/destroy symmetry — not the render cap or the load
//! window, which their own crates cover.
//!
//! Skipped when no wgpu adapter is available or the component's wasm
//! hasn't been built (`require_runtime` locates
//! `target/wasm32-unknown-unknown/{debug,release}/aether_kit.wasm`
//! and returns `None` when both are absent). CI builds the wasm before
//! `cargo test`.

use aether_component::ComponentHostCapability;
use aether_data::ErasedActorPath;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::test_helpers::require_runtime;
use aether_harness_substrate_capture::visual::{decode_png, differs_from_background};
use aether_harness_substrate_capture::{RenderHarnessBuilderExt, RenderHarnessExt};
use aether_kinds::{DropComponent, DropResult, LoadComponent, LoadResult};
use aether_render::{RenderCapability, WHITE_TEXTURE_ID};

// Force linkage of `aether-kit`'s `inventory::submit!`
// `KindDescriptor` entries into this test binary — cargo links the test
// against the host rlib, but the linker strips inventory submits the test
// code doesn't statically reference.
#[allow(unused_imports)]
use aether_kit as _;
use std::fs;
use std::path::Path;

/// Load `aether-kit`'s pre-built wasm into the harness selecting
/// the `aether.kit.bundle` export (ADR-0096; the kit is defaultless per
/// ADR-0138, so the selector is required), await `LoadResult`, and return
/// the loaded component's mailbox id so a test can drop it. The bundle
/// takes no config. Panics on load failure so the test surfaces the
/// error message.
fn load_bundle(harness: &mut SubstrateHarness, wasm_path: &Path) -> ErasedActorPath {
    let wasm = fs::read(wasm_path).expect("read kit wasm");
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                &harness.actor_ref::<ComponentHostCapability>(),
                &LoadComponent { wasm, name: None, config: Vec::new(), export: Some("aether.kit.bundle".to_owned()) },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { path, .. } => path,
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

/// `wire` makes the tile resident and the tick handler draws it. Loading
/// the bundle and advancing a few ticks must commit a frame whose overlay
/// holds a batch over the uploaded tile — reachable only once the
/// `create_texture` upload (the load-window transform) replied `Ok` — and
/// the drawn resident must diverge from the clear color in the captured
/// frame.
#[test]
fn bundle_wire_uploads_and_draws_the_resident_tile() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };

    let mut harness =
        SubstrateHarness::builder().with_render().with_component_host().size(64, 48).build().expect("boot");
    load_bundle(&mut harness, &wasm_path);

    // `wire` fires at load and mails `create_texture`; its reply lands a
    // few pumps later, after which the tick handler starts drawing. A
    // handful of post-load ticks covers the round trip and emits several
    // draw batches before the capture.
    let result = harness
        .execute(vec![
            ("prime", HarnessOp::advance(1)),
            ("post", HarnessOp::advance(5)),
            ("snap", HarnessOp::capture()),
        ])
        .expect("advance + capture");

    let png = result.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    differs_from_background(&img, 5).expect("the drawn resident tile should diverge from the clear color");

    // The tick handler draws only when the `texture_id` is stored, which
    // happens only after `create_texture` replied `Ok`, and the record keeps
    // only a batch whose texture is realized — so a committed batch over a
    // non-white texture proves the full pull→upload→store→draw chain.
    let overlay = harness.committed_overlay_snapshot();
    assert!(
        overlay.iter().any(|batch| batch.texture_id != WHITE_TEXTURE_ID),
        "the committed frame must draw the uploaded tile; committed overlay: {overlay:?}",
    );
}

/// `unwire` symmetry (ADR-0163 §4): dropping the component destroys
/// exactly the texture `wire` created. Establishes residency (the tile's
/// batch is in the committed frame, so no teardown has happened yet), drops
/// the component, then probes with a draw naming the resident's texture id:
/// ids are never reused, so the probe records only if that id's registry
/// entry survived — the invariant that keeps the loaded-component census an
/// exact census of resident tiles.
#[test]
fn bundle_unwire_destroys_the_resident_tile() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };

    let mut harness =
        SubstrateHarness::builder().with_render().with_component_host().size(64, 48).build().expect("boot");
    let path = load_bundle(&mut harness, &wasm_path);

    harness.execute(vec![("establish", HarnessOp::advance(6))]).expect("advance to residency");
    let resident = harness
        .committed_overlay_snapshot()
        .into_iter()
        .find(|batch| batch.texture_id != WHITE_TEXTURE_ID)
        .expect("the tile is resident and drawn before the drop");

    let dropped = harness
        .execute(vec![
            (
                "drop",
                HarnessOp::send_and_await_reply(
                    &harness.actor_ref::<ComponentHostCapability>(),
                    &DropComponent { target: path },
                ),
            ),
            ("settle", HarnessOp::advance(1)),
        ])
        .expect("drop sequence");
    match dropped.reply::<DropResult>("drop").expect("decode DropResult") {
        DropResult::Ok => {}
        DropResult::Err { error } => panic!("drop_component: {error}"),
    }

    let render = harness.actor_ref::<RenderCapability>();
    harness
        .execute(vec![("probe", HarnessOp::send_and_settle(&render, &resident)), ("frame", HarnessOp::advance(1))])
        .expect("probe the dropped tile's texture id");
    assert!(
        !harness.committed_overlay_snapshot().iter().any(|batch| batch.texture_id == resident.texture_id),
        "unwire must destroy exactly the texture wire created",
    );
}
