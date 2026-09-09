//! The demo's gate: a puppet instantiated with the checked-in demo config
//! draws the checked-in demo subject, with nobody mailing it anything.
//!
//! Three things have to hold at once for the one-command demo to show
//! something, and each of them fails silently on its own:
//!
//! 1. `wire` issues the load `PuppetConfig::subject` names. A config that
//!    is parsed and then never acted on leaves a live, correct, blank
//!    puppet — the exact condition the config exists to remove.
//! 2. The subject reader accepts `.dsl`. The demo's subject is mesh-DSL
//!    text rather than an exported sculpt, so a reader that dispatches only
//!    on `.obj` refuses it into the actor log and the window stays empty.
//! 3. `demo/puppet.json` says what the puppet's `Config` kind can hear.
//!    The file is read here rather than restated, so a renamed or dropped
//!    field fails this test instead of failing the first stranger who runs
//!    the demo. `demo/turntable.json` is read the same way for the framing
//!    it puts the camera at.
//!
//! Nothing here re-tests the drawing itself — `draws_scenario` owns that.
//! The assertion is coverage for the same reason it is there: a fraction of
//! lit pixels either says something was drawn or it does not, and a curve
//! count is a number with no independent truth behind it.
//!
//! `SubstrateHarness` rather than `FleetHarness` per the harness decision
//! rule: the assertion is about rendered output.
//!
//! Skipped without a wgpu adapter or a pre-built component wasm;
//! `AETHER_REQUIRE_RUNTIME=1` (which CI sets) turns both skips into panics.

use std::fs;
use std::path::Path;

use aether_data::Kind;
use aether_harness_substrate::{HarnessActor, HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::visual::{background_top_left, coverage, decode_png};
use aether_harness_substrate_capture::{
    RenderHarnessBuilderExt,
    test_helpers::{init_save_sandbox, require_runtime, test_namespace_roots, write_fixture},
};
use aether_kinds::{LoadComponent, LoadResult};
use aether_puppet::{Look, Puppet, PuppetConfig, TurntableConfig};

/// The demo's subject, and the demo's two configs — the files themselves,
/// so this scenario and `demo/README.md` cannot drift apart.
const TEAPOT_DSL: &[u8] = include_bytes!("../../aether-mesh/examples/teapot.dsl");
const PUPPET_CONFIG_JSON: &str = include_str!("../../../demo/puppet.json");
const TURNTABLE_CONFIG_JSON: &str = include_str!("../../../demo/turntable.json");

/// ADR-0138: the merged three-actor module is defaultless, so every load
/// names the actor it wants.
const PUPPET_EXPORT: &str = "aether.puppet";

/// Lit-versus-background tolerance, as in `draws_scenario`: strokes are
/// anti-aliased ribbons whose edge pixels sit close to the clear colour, so
/// a tight tolerance counts only pixels the pen actually reached.
const TOLERANCE: u8 = 5;

/// The band the demo frame has to land in.
///
/// The floor rules out the failure this exists to catch — an empty frame,
/// which is what an unacted config, a refused `.dsl`, or a drifted config
/// file each produce — and the ceiling rules out the opposite one, a frame
/// filled edge to edge, which is what a clear-colour mismatch or runaway
/// geometry looks like. Between them it says nothing about how much of a
/// teapot an illustrator inks, because that is a judgement for a person
/// looking at a window.
///
/// Measured, so the band is known to bracket rather than merely contain:
/// the demo frame draws 2.7% of it, and every one of the three failures
/// above draws exactly 0.0% and lands under the floor. The floor is the
/// same 0.1% `draws_scenario` uses, an order of magnitude under the
/// measurement, so it answers "did the pen reach the frame" and nothing
/// about the drawing's density.
const FLOOR: f32 = 0.001;
const CEILING: f32 = 0.60;

fn puppet() -> HarnessActor<Puppet> {
    HarnessOp::loaded_default::<Puppet>()
}

/// Load the puppet wasm carrying `config`, blocking on `LoadResult` so the
/// `wire`-issued subject read is in flight before the frames are advanced.
fn load_puppet(harness: &mut SubstrateHarness, wasm_path: &Path, config: &PuppetConfig) {
    let wasm = fs::read(wasm_path).expect("read the puppet wasm");
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: None,
                    config: config.encode_into_bytes(),
                    export: Some(PUPPET_EXPORT.to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load_component(puppet): {error}"),
    }
}

#[test]
fn the_demo_config_draws_the_demo_subject_with_no_mail() {
    let Some(wasm_path) = require_runtime("aether_puppet") else {
        return;
    };
    let config: PuppetConfig = serde_json::from_str(PUPPET_CONFIG_JSON).expect("demo/puppet.json is a PuppetConfig");
    let framing: TurntableConfig =
        serde_json::from_str(TURNTABLE_CONFIG_JSON).expect("demo/turntable.json is a TurntableConfig");
    let subject = config.subject.as_ref().expect("demo/puppet.json names a subject");

    let save_dir = init_save_sandbox("puppet-config");
    let staged = write_fixture("teapot.dsl", TEAPOT_DSL);
    assert_eq!(staged, subject.path, "the fixture has to land at the path the demo config names");

    let mut harness = SubstrateHarness::builder()
        .size(256, 192)
        .namespace_roots(test_namespace_roots(save_dir))
        .with_render()
        .with_component_host()
        .build()
        .expect("boot a rendering harness with a component host");
    load_puppet(&mut harness, &wasm_path, &config);

    // The framing the demo's turntable holds while it sweeps. Sent rather
    // than driven, because what is under test is the subject reaching the
    // frame, not the motor that turns it — the turntable's own sweep is
    // covered by its unit tests.
    let look = Look {
        azimuth: framing.azimuth,
        elevation: framing.elevation,
        distance: framing.distance,
        height: framing.height,
    };
    let captured = harness
        // Several frames, not one: since ADR-0172 the ink reaches the frame
        // only once the program register, the texture creates and the
        // geometry creates have each answered — a handful of round trips
        // rather than the same tick. The `wire`-issued subject read settles
        // inside the same window.
        .execute(vec![
            ("frame", puppet().send(&look)),
            ("prime", HarnessOp::advance(12)),
            ("demo", HarnessOp::capture()),
        ])
        .expect("frame + prime + capture");
    let image = decode_png(captured.captured("demo").expect("the capture step ran")).expect("decode the capture");
    let drawn = coverage(&image, background_top_left(&image), TOLERANCE);

    assert!(
        (FLOOR..CEILING).contains(&drawn),
        "a config-named .dsl subject should put strokes on the frame; coverage {drawn} is either an empty frame \
         or a filled one",
    );
}
