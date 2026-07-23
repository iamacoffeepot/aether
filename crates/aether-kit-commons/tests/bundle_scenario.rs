//! Reference asset bundle scenario tests (ADR-0163 §4). Each boots a
//! `SubstrateHarness`, loads `aether-kit-commons`'s wasm artifact (built
//! separately for `wasm32-unknown-unknown`) selecting the non-entry
//! `aether.kit.bundle` export (ADR-0096), and drives the residency
//! lifecycle the reference actor bakes in:
//!
//! - `wire` pulls the embedded tile through the load window and uploads
//!   it as a texture (`aether.render.create_texture`);
//! - the tick handler draws the resident every frame
//!   (`aether.render.draw_textured_quads`) — which fires only after the
//!   `create_texture` reply landed and the `texture_id` was stored, so
//!   observing it proves the whole warm→hot chain closed;
//! - dropping the component runs `unwire`, which destroys exactly the
//!   texture `wire` created (`aether.render.destroy_texture`) — the
//!   symmetric-teardown convention the reference actor enforces by
//!   example.
//!
//! These assert THIS actor's lifecycle logic — the pull→upload→store→draw
//! chain and the create/destroy symmetry — not the render cap or the load
//! window, which their own crates cover.
//!
//! Skipped when no wgpu adapter is available or the component's wasm
//! hasn't been built (`require_runtime` locates
//! `target/wasm32-unknown-unknown/{debug,release}/aether_kit_commons.wasm`
//! and returns `None` when both are absent). CI builds the wasm before
//! `cargo test`.

use aether_data::MailboxId;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::require_runtime;
use aether_harness_substrate_capture::visual::{decode_png, differs_from_background};
use aether_kinds::{DropComponent, DropResult, LoadComponent, LoadResult};

// Force linkage of `aether-kit-commons`'s `inventory::submit!`
// `KindDescriptor` entries into this test binary — cargo links the test
// against the host rlib, but the linker strips inventory submits the test
// code doesn't statically reference.
#[allow(unused_imports)]
use aether_kit_commons as _;
use std::fs;
use std::path::Path;

/// User-facing component name passed to `LoadComponent`.
const COMPONENT_NAME: &str = "tile";

/// Load `aether-kit-commons`'s pre-built wasm into the harness selecting
/// the `aether.kit.bundle` export (ADR-0096; the kit is defaultless per
/// ADR-0138, so the selector is required), await `LoadResult`, and return
/// the loaded component's mailbox id so a test can drop it. The bundle
/// takes no config. Panics on load failure so the test surfaces the
/// error message.
fn load_bundle(harness: &mut SubstrateHarness, wasm_path: &Path) -> MailboxId {
    let wasm = fs::read(wasm_path).expect("read kit wasm");
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(COMPONENT_NAME.to_owned()),
                    config: Vec::new(),
                    export: Some("aether.kit.bundle".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

/// `wire` makes the tile resident and the tick handler draws it. Loading
/// the bundle and advancing a few ticks must produce the
/// `create_texture` upload (the load-window transform) and the
/// `draw_textured_quads` batch (steady state) — and the drawn resident
/// must diverge from the clear color in the captured frame.
#[test]
fn bundle_wire_uploads_and_draws_the_resident_tile() {
    let Some(wasm_path) = require_runtime("aether_kit_commons") else {
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

    let created = harness.count_observed("aether.render.create_texture");
    assert!(
        created >= 1,
        "wire must upload the embedded tile via create_texture; got {created}; observed: {:?}",
        harness.observed_kinds(),
    );
    // The tick handler draws only when the `texture_id` is stored, which
    // happens only after `create_texture` replied `Ok` — so an observed
    // draw batch proves the full pull→upload→store→draw residency chain.
    let drawn = harness.count_observed("aether.render.draw_textured_quads");
    assert!(
        drawn >= 1,
        "the resident tile must be drawn every tick once uploaded; got {drawn}; observed: {:?}",
        harness.observed_kinds(),
    );

    let png = result.captured("snap").expect("snap step ran");
    let img = decode_png(png).expect("decode capture png");
    differs_from_background(&img, 5).expect("the drawn resident tile should diverge from the clear color");
}

/// `unwire` symmetry (ADR-0163 §4): dropping the component destroys
/// exactly the texture `wire` created. Establishes residency, asserts no
/// teardown has happened yet, drops the component, and verifies the
/// `destroy_texture` counterpart went out — the invariant that keeps the
/// loaded-component census an exact census of resident tiles.
#[test]
fn bundle_unwire_destroys_the_resident_tile() {
    let Some(wasm_path) = require_runtime("aether_kit_commons") else {
        return;
    };

    let mut harness =
        SubstrateHarness::builder().with_render().with_component_host().size(64, 48).build().expect("boot");
    let mailbox_id = load_bundle(&mut harness, &wasm_path);

    harness.execute(vec![("establish", HarnessOp::advance(6))]).expect("advance to residency");
    assert!(
        harness.count_observed("aether.render.create_texture") >= 1,
        "the tile must be resident before the drop; observed: {:?}",
        harness.observed_kinds(),
    );
    assert_eq!(
        harness.count_observed("aether.render.destroy_texture"),
        0,
        "no teardown should have happened before the component is dropped",
    );

    let dropped = harness
        .execute(vec![
            ("drop", HarnessOp::send_and_await("aether.component", &DropComponent { mailbox_id })),
            ("settle", HarnessOp::advance(1)),
        ])
        .expect("drop sequence");
    match dropped.reply::<DropResult>("drop").expect("decode DropResult") {
        DropResult::Ok => {}
        DropResult::Err { error } => panic!("drop_component: {error}"),
    }

    let destroyed = harness.count_observed("aether.render.destroy_texture");
    assert!(
        destroyed >= 1,
        "unwire must destroy the resident tile it created; got {destroyed}; observed: {:?}",
        harness.observed_kinds(),
    );
}
