//! The end-to-end gate: a loaded mesh puts strokes on the frame, and it
//! still does from a second camera angle (iamacoffeepot/aether#4342).
//!
//! Every failure this crate has had was silent rather than loud. A keep
//! predicate passed as `true` disabled shading; a subscription on the wrong
//! capability compiled, registered and delivered nothing. Neither errored,
//! and the unit tests — which cover the two decisions the port added —
//! could not see either, because neither is a decision. They are the
//! pipeline failing to run, and nothing asserted that it does.
//!
//! So the assertion is coverage rather than a curve count: a total is a
//! number with no independent truth behind it, needs editing at every
//! threshold change, and that is the shape of a test that gets deleted
//! rather than fixed. A fraction of lit pixels either says "something was
//! drawn" or it does not.
//!
//! The second angle is not redundant. The silhouette is the one
//! view-dependent pass, re-solved per eye against a cache keyed on it, so
//! a regression that leaves the first frame intact and breaks every
//! subsequent one lands exactly there.
//!
//! What this deliberately does not cover is mesh winding, even though a
//! scrambled winding was one of the arc's silent failures. Inconsistent
//! normals do not empty the drawing — the level set finds spurious
//! crossings across the whole surface and coverage roughly doubles, well
//! inside any band a stroke drawing can honestly claim. Winding is gated
//! where it can be measured rather than inferred, by
//! `obj_winding_survives_the_reader`.
//!
//! `SubstrateHarness` rather than `FleetHarness` per the harness decision
//! rule: the assertion is about rendered output, and the fleet harness is
//! headless.
//!
//! Skipped without a wgpu adapter or a pre-built component wasm;
//! `AETHER_REQUIRE_RUNTIME=1` (which CI sets) turns both skips into
//! panics so a missing pre-build is loud rather than a vacuous pass.

use std::fs;
use std::path::Path;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::visual::{background_top_left, coverage, decode_png};
use aether_harness_substrate_capture::{
    RenderHarnessBuilderExt,
    test_helpers::{init_save_sandbox, require_runtime, test_namespace_roots, write_fixture},
};
use aether_kinds::{LoadComponent, LoadResult};
use aether_puppet::{Load, Look};

/// A closed, consistently outward-wound solid. Committed rather than
/// generated so the subject under test is a reviewable artifact rather
/// than a literal assembled a line at a time in the runner.
const CUBE_OBJ: &[u8] = include_bytes!("fixtures/cube.obj");

/// The address a loaded component registers at (ADR-0099), for the mail
/// that drives it after the load.
const PUPPET: &str = "aether.component/aether.embedded:aether.puppet";

/// Lit-versus-background tolerance. Strokes are anti-aliased ribbons, so
/// their edge pixels sit close to the clear color; a tolerance this tight
/// counts only pixels the pen actually reached.
const TOLERANCE: u8 = 5;

/// Load the puppet wasm, blocking on `LoadResult` so the subject-load
/// mail that follows reaches a live component.
fn load_puppet(harness: &mut SubstrateHarness, wasm_path: &Path) {
    let wasm = fs::read(wasm_path).expect("read the puppet wasm");
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent { wasm, name: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load_component(puppet): {error}"),
    }
}

/// Advance a frame, capture, and return the fraction of the frame the pen
/// reached.
fn drawn_fraction(harness: &mut SubstrateHarness, label: &'static str) -> f32 {
    let captured = harness
        // Several frames, not one: since ADR-0172 the ink is rendered by
        // a program rather than streamed as triangles, so it reaches the
        // frame only once the register, the texture creates and the
        // geometry creates have each answered — a handful of round trips
        // rather than the same tick. The wash layer has always warmed up
        // this way; the pen does now too.
        .execute(vec![("prime", HarnessOp::advance(12)), (label, HarnessOp::capture())])
        .expect("prime + capture");
    let png = captured.captured(label).expect("the capture step ran");
    let img = decode_png(png).expect("decode the captured png");
    let background = background_top_left(&img);

    coverage(&img, background, TOLERANCE)
}

/// Load a cube and assert the pen reached the frame — then orbit and
/// assert it still does.
///
/// The band is wide on purpose. Its lower edge rules out the failure this
/// exists to catch (an empty frame: nothing extracted, nothing survived
/// visibility, nothing emitted) and its upper edge rules out the opposite
/// one (a frame filled edge to edge, which is what a clear-color mismatch
/// or runaway geometry looks like). Between those two it says nothing, and
/// should not: how much of a subject an illustrator inks is a judgement
/// that belongs to a person looking at a window.
///
/// Measured, so the band is known to bracket rather than merely contain:
/// the cube draws 2.2% of the frame face-on and 4.8% turned, and with the
/// subject load removed it draws exactly 0.0% and this fails.
#[test]
fn a_loaded_mesh_draws_from_two_angles() {
    let Some(wasm_path) = require_runtime("aether_puppet") else {
        return;
    };
    let save_dir = init_save_sandbox("puppet");
    let path = write_fixture("cube.obj", CUBE_OBJ);

    let mut harness = SubstrateHarness::builder()
        .size(256, 192)
        .namespace_roots(test_namespace_roots(save_dir))
        .with_render()
        .with_component_host()
        .build()
        .expect("boot a rendering harness with a component host");
    load_puppet(&mut harness, &wasm_path);

    harness
        .execute(vec![(
            "subject",
            HarnessOp::send_and_settle(PUPPET, &Load { namespace: "assets".to_owned(), path, labels: String::new() }),
        )])
        .expect("the subject load settles");

    let face_on = drawn_fraction(&mut harness, "face_on");
    assert!(
        (0.001..0.60).contains(&face_on),
        "a loaded mesh should put strokes on the frame; coverage {face_on} is either an empty frame or a filled one",
    );

    // Orbit far enough that every silhouette edge is a different one.
    harness
        .execute(vec![(
            "orbit",
            HarnessOp::send_and_settle(PUPPET, &Look { azimuth: 55.0, elevation: 20.0, distance: 5.4, height: 0.0 }),
        )])
        .expect("the look change settles");

    let turned = drawn_fraction(&mut harness, "turned");
    assert!(
        (0.001..0.60).contains(&turned),
        "the silhouette is re-solved per eye, so a turned camera should still draw; coverage {turned}",
    );
}
