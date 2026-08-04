//! What one frame of the shipped drawing costs, and what it looks like
//! at the framing every visual A/B is pinned to.
//!
//! Both are instruments rather than gates — they need a 434k-face
//! subject and its material field, neither of which lives in the
//! repository — so both are `#[ignore]`d and read the same
//! `AETHER_CROSSFEED_DIR` the field's own cross-feed does.
//!
//! Two rules the numbers here depend on, both learned the hard way:
//!
//! **Nothing is timed through a capture.** The harness capture encodes
//! a PNG synchronously and its cost is priced by the image's entropy,
//! which is nothing to do with the frame and swamps it
//! (iamacoffeepot/aether#4422). The timed loops advance frames and
//! capture nothing.
//!
//! **The wasm has to be a release build.** `cargo xtask dist` produces
//! a debug component by default and a debug guest paces about seven
//! times slow, which is a number about `rustc -O` rather than about the
//! drawing. Build with `cargo xtask dist --no-bins --profile release`
//! before running these.

// Instrument output goes to stderr so `cargo test -- --nocapture`
// surfaces it next to the test name.
#![allow(clippy::print_stderr)]
// Reads the instrument's own directory knob and the AETHER_REQUIRE_RUNTIME
// CI skip toggle — test-harness knobs, not cap config.
#![allow(clippy::disallowed_methods)]
#![allow(clippy::cast_precision_loss)]
// `mul_add` on a sample counter and a millisecond accumulator is
// arithmetic nobody reads better for the rewrite.
#![allow(clippy::suboptimal_flops)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{init_save_sandbox, require_runtime, test_namespace_roots};
use aether_kinds::{LoadComponent, LoadResult};
use aether_math::Vec3;
use aether_puppet::easel::program::{sight, stroke};
use aether_puppet::extract::{self, Settings};
use aether_puppet::feature::{Curve3, Drawing, Half};
use aether_puppet::labels::Labels;
use aether_puppet::mesh::Mesh;
use aether_puppet::{Load, Look, anchor, chart};

/// The address a loaded component registers at (ADR-0099).
const PUPPET: &str = "aether.component/aether.embedded:aether.puppet";

/// The canvas every visual A/B and every pacing number is taken at.
const WIDTH: u32 = 900;
const HEIGHT: u32 = 1200;

/// The pinned framing: facing her, slightly above, her whole height in
/// frame. `Puppet::init`'s own default.
const AZIMUTH: f32 = 0.0;
const ELEVATION: f32 = 3.0;
const DISTANCE: f32 = 5.4;

/// Mesh relaxation the component loads at, so the census below draws the
/// same curves the component does.
const RELAXATION: usize = 2;

/// Padding the material field was baked with — `Puppet::LABEL_PAD`.
const LABEL_PAD: f32 = 0.12;

/// Frames each timed condition averages over, and how many lead frames
/// are thrown away first. The warm-up pays for the first realization of
/// every buffer on the GPU and for the render runtime's one frame in
/// flight, neither of which is a per-frame cost.
const SAMPLES: u32 = 40;
const WARMUP: u32 = 12;

/// Azimuths the orbit condition sweeps, one per timed frame — far enough
/// apart that every frame is a genuine re-split rather than a repeat of
/// the eye already solved for.
const ORBIT_STEP: f32 = 0.7;

/// The subject and its material field, or `None` when the instrument has
/// nothing to measure.
fn subject_dir() -> Option<PathBuf> {
    let Ok(dir) = env::var("AETHER_CROSSFEED_DIR") else {
        eprintln!("skipping: AETHER_CROSSFEED_DIR unset, so there is no subject to measure");
        return None;
    };

    Some(PathBuf::from(dir))
}

/// A harness sized to the pinned canvas, with the subject copied into
/// its sandbox. Copied rather than read in place because the sandbox is
/// the one root the harness serves every namespace from, and the copy is
/// paid once against a subject load that takes seconds anyway.
fn mounted(dir: &Path, wasm: &Path) -> SubstrateHarness {
    let save = init_save_sandbox("puppet-pace");
    for name in ["subject.obj", "labels.npy"] {
        fs::copy(dir.join(name), save.join(name)).expect("stage the subject");
    }
    let mut harness = SubstrateHarness::builder()
        .size(WIDTH, HEIGHT)
        .namespace_roots(test_namespace_roots(save))
        .with_render()
        .with_component_host()
        .build()
        .expect("boot a rendering harness with a component host");

    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm: fs::read(wasm).expect("read the puppet wasm"),
                    name: None,
                    config: Vec::new(),
                    export: None,
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load_component(puppet): {error}"),
    }

    harness
        .execute(vec![(
            "subject",
            HarnessOp::send_and_settle(
                PUPPET,
                &Load {
                    namespace: "assets".to_owned(),
                    path: "subject.obj".to_owned(),
                    labels: "labels.npy".to_owned(),
                },
            ),
        )])
        .expect("the subject load settles");

    harness
}

fn look(azimuth: f32) -> Look {
    Look { azimuth, elevation: ELEVATION, distance: DISTANCE, height: 0.0 }
}

/// The instrument proper: the frame at the pinned framing written out
/// for a visual A/B, the static and orbit frames timed, then a second
/// frame written from a turned camera the orbit arrived at.
///
/// One test rather than three because they share a several-second
/// subject load, and because a capture may not sit inside a timed loop.
///
/// The turned frame is the one that catches a stale resident buffer.
/// It is photographed at the end of an orbit sweep, after the volatile
/// half has been re-solved and re-uploaded fifty times over the same
/// resident half — so a resident span that drifted, or a resident
/// buffer that stopped being addressed correctly, shows up as ghosting
/// or as missing hatch rather than as an assertion nobody wrote.
///
/// ```text
/// AETHER_CROSSFEED_DIR=/path/to/dir AETHER_PUPPET_GATE_PNG=/path/pinned.png \
///     AETHER_PUPPET_TURNED_PNG=/path/turned.png \
///     cargo test -p aether-puppet --release --test pace_instrument \
///     -- --ignored --nocapture
/// ```
#[test]
#[ignore = "instrument; needs the shipped subject in AETHER_CROSSFEED_DIR and a release-profile component wasm"]
fn the_frame_paces_at_the_pinned_framing() {
    let (Some(wasm), Some(dir)) = (require_runtime("aether_puppet"), subject_dir()) else {
        return;
    };
    let mut harness = mounted(&dir, &wasm);

    // Program-rendered ink reaches the frame only once the register, the
    // texture creates and the geometry creates have each answered, so
    // the drawing needs a handful of round trips before it is there to
    // photograph.
    harness
        .execute(vec![("frame", HarnessOp::send_and_settle(PUPPET, &look(AZIMUTH))), ("prime", HarnessOp::advance(24))])
        .expect("prime the pinned framing");

    photograph(&mut harness, "AETHER_PUPPET_GATE_PNG", "the pinned framing");

    let still = timed(&mut harness, None);
    let orbit = timed(&mut harness, Some(AZIMUTH));
    let after = timed(&mut harness, None);
    eprintln!(
        "pace: static frame {still:.2} ms (and {after:.2} ms after the orbit), orbit frame {orbit:.2} ms, at \
         {WIDTH}x{HEIGHT} over {SAMPLES} frames each",
    );

    let turned = AZIMUTH + (SAMPLES + WARMUP - 1) as f32 * ORBIT_STEP;
    harness
        .execute(vec![("turn", HarnessOp::send_and_settle(PUPPET, &look(turned))), ("settle", HarnessOp::advance(8))])
        .expect("settle the turned framing");
    photograph(&mut harness, "AETHER_PUPPET_TURNED_PNG", &format!("azimuth {turned:.1} after the orbit"));
}

/// Write one frame out under the path `variable` names, or do nothing
/// when it is unset. Outside every timed loop, because a capture encodes
/// a PNG synchronously and its cost is the image's entropy (#4422).
fn photograph(harness: &mut SubstrateHarness, variable: &str, what: &str) {
    let Ok(path) = env::var(variable) else {
        return;
    };
    let captured = harness.execute(vec![("gate", HarnessOp::capture())]).expect("capture the frame");
    let png = captured.captured("gate").expect("the capture step ran");
    fs::write(&path, png).expect("write the gate png");
    eprintln!("pace: wrote {what} to {path} ({} bytes)", png.len());
}

/// One condition's mean frame, in milliseconds. `orbit` turns the camera
/// by [`ORBIT_STEP`] before each frame, so every frame re-solves; `None`
/// leaves the eye alone, so every frame serves the drawing already
/// solved.
fn timed(harness: &mut SubstrateHarness, orbit: Option<f32>) -> f64 {
    let mut millis = 0.0;
    for sample in 0..SAMPLES + WARMUP {
        let started = Instant::now();
        let mut ops = Vec::new();
        if let Some(from) = orbit {
            ops.push(("turn", HarnessOp::send_and_settle(PUPPET, &look(from + sample as f32 * ORBIT_STEP))));
        }
        ops.push(("frame", HarnessOp::advance(1)));
        harness.execute(ops).expect("timed frame");

        if sample >= WARMUP {
            millis += started.elapsed().as_secs_f64() * 1000.0;
        }
    }

    millis / f64::from(SAMPLES)
}

/// The census the pacing is explained by: how the drawing divides into
/// its resident and volatile halves, and what each costs in packed
/// bytes at the pinned canvas.
///
/// No GPU and no component — this is the CPU-side pack alone, which is
/// exactly the mail the frame ships.
#[test]
#[ignore = "instrument; needs the shipped subject in AETHER_CROSSFEED_DIR"]
fn the_drawing_divides_by_volatility() {
    let Some(dir) = subject_dir() else {
        return;
    };
    let mesh = Mesh::from_obj_bytes(&fs::read(dir.join("subject.obj")).expect("read subject.obj"), RELAXATION)
        .expect("parse the subject");
    let labels =
        Labels::parse(&fs::read(dir.join("labels.npy")).expect("read labels.npy"), mesh.min, mesh.max, LABEL_PAD)
            .expect("parse the material field");
    let settings = Settings::default();
    let anchors = anchor::Anchors::measure(&mesh, &labels);
    let resident = extract::surface(&mesh, Some(&labels), anchors.as_ref(), &settings);

    for azimuth in [0.0f32, 30.0, 55.0, 90.0] {
        let eye = eye_at(azimuth);
        let volatile: Vec<Curve3> = anchors
            .as_ref()
            .zip(settings.face)
            .map(|(anchors, face)| chart::marks(&mesh, anchors, face, &settings, eye))
            .unwrap_or_default()
            .into_iter()
            .chain(extract::suggestive(&mesh, Some(&labels), eye, &settings))
            .chain(extract::silhouettes(&mesh, eye))
            .collect();

        let drawing = Drawing { resident: &resident, volatile: &volatile };
        let layout = sight::layout(drawing, (WIDTH, HEIGHT)).expect("the drawing fits");
        let both = |packed: (Vec<u8>, Vec<u8>)| packed.0.len() + packed.1.len();
        let points = |spans: &[sight::Span]| both((sight::point_vertices(drawing, spans), sight::point_indices(spans)));
        let ribbons = |half| both(stroke::ribbon_geometry(drawing, &layout, half));
        let curves = both((sight::curve_vertices(drawing, &layout, eye), sight::curve_indices(&layout)));

        // The drawing's two halves, each carrying its points and its
        // ribbons, and the per-curve references that belong to neither
        // — a reference depth is the eye's for a resident curve as
        // much as for a volatile one (#4440).
        let staying = points(layout.resident()) + ribbons(Half::Resident);
        let travelling = points(layout.volatile()) + ribbons(Half::Volatile) + curves;
        let megabytes = |bytes: usize| bytes as f64 / 1.0e6;

        eprintln!(
            "census azimuth {azimuth:>4}: {} resident curves + {} volatile | resident {:.2} MB | volatile {:.2} MB \
             (points {:.2} + ribbons {:.2}) | references {:.3} MB | whole drawing {:.2} MB, per re-split {:.2} MB",
            resident.len(),
            volatile.len(),
            megabytes(staying),
            megabytes(travelling),
            megabytes(points(layout.volatile())),
            megabytes(ribbons(Half::Volatile)),
            megabytes(curves),
            megabytes(staying + travelling),
            megabytes(travelling),
        );
    }
}

fn eye_at(azimuth: f32) -> Vec3 {
    let (azimuth, elevation) = (azimuth.to_radians(), ELEVATION.to_radians());
    let (sin_a, cos_a) = azimuth.sin_cos();
    let (sin_e, cos_e) = elevation.sin_cos();

    Vec3::new(sin_a * cos_e, sin_e, cos_a * cos_e) * DISTANCE
}
