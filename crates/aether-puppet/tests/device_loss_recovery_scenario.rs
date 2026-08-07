//! Consumer-view offscreen device-loss gate (ADR-0173, issue #4538).
//!
//! One loaded puppet and its session-scoped render ids remain live across a
//! forced host-device loss. The ordinary render lifecycle redispatches on the
//! replacement generation; no actor callback or resource recreation is used.
//!
//! Set `AETHER_DEVICE_LOSS_EVIDENCE_DIR` to an explicit directory to persist
//! `before.png`, `control.png`, and `recovery.png` for owner inspection. The
//! workflow derives the contact sheet and amplified difference beside them.

#![allow(clippy::disallowed_methods)]
#![allow(clippy::print_stderr)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::test_helpers::{
    init_save_sandbox, require_runtime, test_namespace_roots, write_fixture,
};
use aether_harness_substrate_capture::visual::{background_top_left, coverage, decode_png, mean_absolute_error};
use aether_harness_substrate_capture::{RenderHarnessBuilderExt, RenderHarnessExt};
use aether_kinds::{LoadComponent, LoadResult};
use aether_puppet::{Load, Look};

const CUBE_OBJ: &[u8] = include_bytes!("fixtures/cube.obj");
const PUPPET: &str = "aether.component/aether.embedded:aether.puppet";
const PUPPET_EXPORT: &str = "aether.puppet";
const TOLERANCE: u8 = 5;

fn control_look() -> Look {
    Look { azimuth: 55.0, elevation: 20.0, distance: 5.4, height: 0.0 }
}

fn alternate_look() -> Look {
    Look { azimuth: 15.0, elevation: 10.0, distance: 5.4, height: 0.0 }
}

fn load_puppet(harness: &mut SubstrateHarness, wasm_path: &Path) {
    let wasm = fs::read(wasm_path).expect("read puppet wasm");
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent { wasm, name: None, config: Vec::new(), export: Some(PUPPET_EXPORT.to_owned()) },
            ),
        )])
        .expect("load puppet sequence");
    match loaded.reply::<LoadResult>("load").expect("decode puppet load result") {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load_component(puppet): {error}"),
    }
}

fn advance_and_capture(harness: &mut SubstrateHarness, label: &'static str, frames: u32) -> Vec<u8> {
    harness
        .execute(vec![("advance", HarnessOp::advance(frames)), (label, HarnessOp::capture())])
        .expect("advance and capture puppet")
        .captured(label)
        .expect("capture step ran")
        .to_vec()
}

fn assert_drawn(label: &str, png: &[u8]) {
    let image = decode_png(png).expect("decode puppet capture");
    let background = background_top_left(&image);
    let drawn = coverage(&image, background, TOLERANCE);
    assert!(
        (0.001..0.60).contains(&drawn),
        "{label} must contain the loaded cube's strokes; coverage {drawn} is empty or frame-filling",
    );
}

fn persist_evidence(before: &[u8], control: &[u8], recovery: &[u8]) -> Option<PathBuf> {
    let dir = env::var_os("AETHER_DEVICE_LOSS_EVIDENCE_DIR").map(PathBuf::from)?;
    fs::create_dir_all(&dir).expect("create explicitly supplied device-loss evidence directory");
    for (name, png) in [("before.png", before), ("control.png", control), ("recovery.png", recovery)] {
        fs::write(dir.join(name), png).expect("write device-loss evidence png");
    }
    Some(dir)
}

#[test]
fn loaded_cube_recovers_without_recreating_the_actor_or_public_ids() {
    let Some(wasm_path) = require_runtime("aether_puppet") else {
        return;
    };
    let save_dir = init_save_sandbox("puppet-device-loss");
    let path = write_fixture("device-loss-cube.obj", CUBE_OBJ);

    let mut harness = SubstrateHarness::builder()
        .size(256, 192)
        .namespace_roots(test_namespace_roots(save_dir))
        .with_render()
        .with_component_host()
        .build()
        .expect("boot rendering harness with component host");
    load_puppet(&mut harness, &wasm_path);
    harness
        .execute(vec![(
            "subject",
            HarnessOp::send_and_settle(
                PUPPET,
                &Load {
                    namespace: "assets".to_owned(),
                    path,
                    labels: String::new(),
                    material_field_padding: 0.12,
                    rig: String::new(),
                    palette: String::new(),
                },
            ),
        )])
        .expect("load the committed cube");

    let before = advance_and_capture(&mut harness, "before", 12);
    harness
        .execute(vec![("control_look", HarnessOp::send_and_settle(PUPPET, &control_look()))])
        .expect("set the control camera");
    let control = advance_and_capture(&mut harness, "control", 12);
    assert_drawn("before", &before);
    assert_drawn("control", &control);

    // Leave the actor at a distinct camera so the post-loss control look is
    // a real state change and necessarily requests a fresh repaint.
    harness
        .execute(vec![("alternate_look", HarnessOp::send_and_settle(PUPPET, &alternate_look()))])
        .expect("stage an alternate camera before loss");
    harness
        .execute(vec![("alternate_frame", HarnessOp::advance(12))])
        .expect("commit the alternate camera before loss");

    assert_eq!(harness.force_render_device_loss().expect("force generation zero loss"), 0);
    // Re-send ordinary actor state to request an ordinary repaint. The actor,
    // component instance, and all session-scoped render ids remain untouched.
    harness
        .execute(vec![("repaint", HarnessOp::send_and_settle(PUPPET, &control_look()))])
        .expect("request the ordinary post-loss repaint");
    let recovery = advance_and_capture(&mut harness, "recovery", 12);

    let evidence_dir = persist_evidence(&before, &control, &recovery);
    assert_drawn("recovery", &recovery);

    let control_image = decode_png(&control).expect("decode control capture");
    let recovery_image = decode_png(&recovery).expect("decode recovery capture");
    let score = mean_absolute_error(&control_image, &recovery_image).expect("matching capture dimensions");
    assert!(
        score <= 0.03,
        "replacement rendering should remain visually equivalent to the healthy control; MAE={score}",
    );

    if let Some(dir) = evidence_dir {
        eprintln!("device-loss evidence: {}", dir.display());
    }
}
