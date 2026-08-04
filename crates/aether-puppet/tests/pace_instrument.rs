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

use std::cmp::Reverse;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use aether_data::Kind;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{init_save_sandbox, require_runtime, test_namespace_roots};
use aether_harness_substrate_capture::visual::{decode_png, encode_png};
use aether_kinds::{CostRow, CostTail, CostTailResult, LoadComponent, LoadResult, Render, WindowId, WindowSize};
use aether_math::{Mat4, Vec2, Vec3};
use aether_puppet::easel::program::wash::{Canvas, Faces, Frame, Placement, Presence};
use aether_puppet::easel::program::{bake, face, ink, sight, stroke, wash};
use aether_puppet::easel::survey::{self, Survey};
use aether_puppet::easel::{View, accent};
use aether_puppet::extract::{self, Settings};
use aether_puppet::feature::{Curve3, Drawing, Half};
use aether_puppet::labels::{CLASSES, Labels};
use aether_puppet::mesh::Mesh;
use aether_puppet::{Load, Look, anchor, chart, ribbon, visibility};
use aether_render::{DrawTriangle, PassTimingRow, ProgramTimings, ProgramTimingsResult};

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

/// How many program ids the per-pass table asks after. The puppet
/// registers a handful — the visibility field's, the ink's, the bake's
/// and the wash's — and the render cap assigns them from zero in
/// register order, so asking past the last one costs one `Err` reply
/// apiece and needs no agreement with the component about the count.
const PROGRAM_IDS: u32 = 8;

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
    mounted_with(dir, wasm, false)
}

/// The same, with the per-pass GPU timing instrument on or off. It is off
/// for every timed loop: the queries cost a few percent of the tick, so a
/// whole-frame millisecond measured with them on is not the frame this
/// branch ships.
fn mounted_with(dir: &Path, wasm: &Path, pass_timings: bool) -> SubstrateHarness {
    let save = init_save_sandbox("puppet-pace");
    for name in ["subject.obj", "labels.npy"] {
        fs::copy(dir.join(name), save.join(name)).expect("stage the subject");
    }
    let builder = SubstrateHarness::builder().size(WIDTH, HEIGHT).namespace_roots(test_namespace_roots(save));
    let builder = if pass_timings {
        builder.with_render_pass_timings()
    } else {
        builder.with_render()
    };
    let mut harness = builder.with_component_host().build().expect("boot a rendering harness with a component host");

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

    // How big the surface is. On a desktop chassis the window cap
    // announces this; the harness has no window, so nothing publishes it
    // and the easel would sit at no canvas at all — developing nothing,
    // dispatching nothing, and pacing a frame with no paint in it. Sent
    // as ordinary mail to the component, which is the same handler the
    // broadcast would reach.
    harness
        .execute(vec![(
            "canvas",
            HarnessOp::send_and_settle(PUPPET, &WindowSize { window: WindowId(0), width: WIDTH, height: HEIGHT }),
        )])
        .expect("the canvas announcement settles");

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

    if let Some(pinned) = photograph(&mut harness, "AETHER_PUPPET_GATE_PNG", "the pinned framing") {
        compare_against_baseline(&pinned);
    }
    assert_the_develop_repeats(&mut harness);

    let still = timed(&mut harness, None);
    let still_cpu = handler_cost(&mut harness, Render::ID);
    let orbit = timed(&mut harness, Some(AZIMUTH));
    let orbit_cpu = handler_cost(&mut harness, Render::ID);
    let after = timed(&mut harness, None);
    eprintln!(
        "pace: static frame {still:.2} ms (and {after:.2} ms after the orbit), orbit frame {orbit:.2} ms, at \
         {WIDTH}x{HEIGHT} over {SAMPLES} frames each",
    );
    // The component's own share of each, read straight after its
    // condition so the EWMA is that condition's. What the frame carries
    // beyond it is the GPU, the host's encode, and the present.
    eprintln!("pace: of which the component's render handler — static {still_cpu:.2} ms, orbit {orbit_cpu:.2} ms");

    let turned = AZIMUTH + (SAMPLES + WARMUP - 1) as f32 * ORBIT_STEP;
    harness
        .execute(vec![("turn", HarnessOp::send_and_settle(PUPPET, &look(turned))), ("settle", HarnessOp::advance(8))])
        .expect("settle the turned framing");
    photograph(&mut harness, "AETHER_PUPPET_TURNED_PNG", &format!("azimuth {turned:.1} after the orbit"));
}

/// Tripwire: a held view must develop the same sheet every frame.
///
/// The develop runs per frame now rather than once a view settles, so
/// the whole picture is re-derived from scratch on every tick. Anything
/// in it that carried over between frames — an accident stream re-rolled
/// on a different count, a centroid taken off a plane the previous frame
/// wrote, a transient the pool handed out in a different order — would
/// show as the paint quietly crawling under a still camera. It reads as
/// texture rather than as a fault, so nothing but a byte comparison
/// catches it.
///
/// Two captures rather than a develop run twice in a scenario, because
/// the claim that matters here is about the shipped frame path, and the
/// per-frame develop is what makes the frame the unit to compare.
fn assert_the_develop_repeats(harness: &mut SubstrateHarness) {
    let mut sheets = Vec::new();
    for label in ["first", "second"] {
        let captured = harness
            .execute(vec![("hold", HarnessOp::advance(4)), (label, HarnessOp::capture())])
            .expect("capture a held frame");
        sheets.push(captured.captured(label).expect("the capture step ran").to_vec());
    }

    assert_eq!(
        sheets[0],
        sheets[1],
        "a held view developed two different sheets ({} bytes against {})",
        sheets[0].len(),
        sheets[1].len(),
    );
    eprintln!("pace: two develops of the held view are byte-identical ({} bytes)", sheets[0].len());
}

/// Write one frame out under the path `variable` names, or do nothing
/// when it is unset. Outside every timed loop, because a capture encodes
/// a PNG synchronously and its cost is the image's entropy (#4422).
fn photograph(harness: &mut SubstrateHarness, variable: &str, what: &str) -> Option<PathBuf> {
    let path = PathBuf::from(env::var(variable).ok()?);
    let captured = harness.execute(vec![("gate", HarnessOp::capture())]).expect("capture the frame");
    let png = captured.captured("gate").expect("the capture step ran");
    fs::write(&path, png).expect("write the gate png");
    eprintln!("pace: wrote {what} to {} ({} bytes)", path.display(), png.len());

    Some(path)
}

/// One handler's current mean, in milliseconds — the same per-handler
/// EWMA `actor_cost` reads. Zero when the handler has never run.
fn handler_cost(harness: &mut SubstrateHarness, kind: aether_data::KindId) -> f64 {
    let read = harness
        .execute(vec![("cost", HarnessOp::send_and_await_reply(PUPPET, &CostTail { kind: Some(kind) }))])
        .expect("query one handler's cost");
    match read.reply::<CostTailResult>("cost").expect("decode CostTailResult") {
        CostTailResult::Ok { rows } => rows.first().map_or(0.0, |row| row.mean_nanos as f64 / 1.0e6),
        CostTailResult::Err { .. } => 0.0,
    }
}

/// Score the frame just photographed against a baseline written by an
/// earlier run, and write the difference out. Both paths come from the
/// environment and both are optional, so an ordinary run does none of
/// this — it is the A/B a change to the look is argued with.
///
/// Differences are reported in 8-bit steps of the worst channel, which
/// is the same currency `program_wash_scenario` states its budgets in.
/// The difference image is amplified so a change the eye would otherwise
/// have to hunt for is visible at a glance; the numbers beside it are
/// what the amplification must not be read instead of.
fn compare_against_baseline(after: &Path) {
    let (Ok(baseline), Ok(diff)) = (env::var("AETHER_PUPPET_BASELINE_PNG"), env::var("AETHER_PUPPET_DIFF_PNG")) else {
        return;
    };
    let read = |path: &Path| {
        decode_png(&fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())))
            .unwrap_or_else(|error| panic!("decode {}: {error}", path.display()))
    };
    let (before, after) = (read(Path::new(&baseline)), read(after));
    assert_eq!(
        (before.width, before.height),
        (after.width, after.height),
        "a difference between two framings of different sizes is unmeasured, not a result",
    );

    let mut amplified = vec![0u8; after.rgba.len()];
    let (mut sum, mut worst, mut past_eight) = (0u64, 0u8, 0u64);
    for ((out, was), now) in
        amplified.chunks_exact_mut(4).zip(before.rgba.chunks_exact(4)).zip(after.rgba.chunks_exact(4))
    {
        let step = (0..3).map(|channel| was[channel].abs_diff(now[channel])).max().unwrap_or(0);
        sum += u64::from(step);
        worst = worst.max(step);
        past_eight += u64::from(step > 8);
        out.copy_from_slice(&[step.saturating_mul(8), step.saturating_mul(8), step.saturating_mul(8), 255]);
    }

    let texels = (after.rgba.len() / 4) as f64;
    fs::write(&diff, encode_png(&amplified, after.width, after.height).expect("encode the difference png"))
        .expect("write the difference png");
    eprintln!(
        "pace: against {baseline} — mean {:.3} steps, worst {worst}, {:.3}% of texels past 8; difference (8x) at {diff}",
        sum as f64 / texels,
        100.0 * past_eight as f64 / texels,
    );
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

/// Vertical field of view — `Puppet::FIELD_OF_VIEW`, so the phases below
/// project through the matrix the component's own frame does.
const FIELD_OF_VIEW: f32 = 0.454;

/// Occlusion sampling stride — `Puppet::VISIBILITY_STRIDE`, so the split
/// timed below casts the rays the shipped one casts.
const VISIBILITY_STRIDE: usize = 3;

/// The studio's seed. Any seed rolls the same amount of work; this is the
/// one the easel develops with, so the blob timed here is the shipped
/// blob's size.
const SHEET_SEED: u64 = 0x5375_6d69_7265;

/// Repetitions each timed phase averages over.
///
/// The phases span three orders of magnitude — a matrix encode against a
/// whole-drawing occlusion split — and the dear ones are the ones the
/// mean is wanted for, so one count serves both rather than a schedule
/// nobody can read a table against.
const PHASE_REPEATS: u32 = 8;

/// Where the render handler's CPU milliseconds go, phase by phase
/// (iamacoffeepot/aether#4447).
///
/// Native rather than through the component, and it has to be: the guest
/// is `wasm32-unknown-unknown`, which has no clock at all, so a phase
/// timed inside the component would need a host call per phase and would
/// price the call rather than the phase. What this measures instead is
/// the same functions the guest runs, on the same subject, at the same
/// framing — so the *proportions* are the handler's even though the
/// absolute milliseconds are a native build's rather than a wasm one's.
/// Read it against the whole-handler EWMA the pacing instrument prints.
///
/// ```text
/// AETHER_CROSSFEED_DIR=/path/to/dir \
///     cargo test -p aether-puppet --release --test pace_instrument \
///     -- --ignored --nocapture the_render_handler_divides_by_phase
/// ```
#[test]
#[ignore = "instrument; needs the shipped subject in AETHER_CROSSFEED_DIR"]
fn the_render_handler_divides_by_phase() {
    let Some(dir) = subject_dir() else {
        return;
    };
    let at = Subject::load(&dir);
    let view = view_at(AZIMUTH);
    let volatile = at.volatile_at(view.eye);
    let drawing = Drawing { resident: &at.resident, volatile: &volatile };
    let canvas = Canvas { width: WIDTH as usize, height: HEIGHT as usize };

    eprintln!(
        "phase: at {WIDTH}x{HEIGHT}, {} resident curves + {} volatile, over {PHASE_REPEATS} repeats",
        at.resident.len(),
        volatile.len(),
    );
    time_the_develop(&at, drawing, &view, canvas);
    time_the_eye_moved_half(&at, drawing, &view);
}

/// The phases `Easel::develop` runs, in the order it runs them — the ones
/// a held view now skips entirely.
fn time_the_develop(at: &Subject, drawing: Drawing<'_>, view: &View, canvas: Canvas) {
    let (body_width, body_height) = canvas.body();
    let drawn =
        phase("split — the visible runs the ink plane rasterizes", PHASE_REPEATS, || at.split(drawing, view.eye));
    let survey = phase("survey::measure — per subject", 1, || Survey::measure(&at.mesh, &at.scores));
    let centroids = phase("survey::centroids", PHASE_REPEATS, || {
        survey.centroids(&at.mesh, view.eye, &view.view_proj, body_width, body_height)
    });
    let frames = phase("chart::eye_frames", PHASE_REPEATS, || {
        chart::eye_frames(&at.mesh, &at.anchors, at.chart_face(), at.settings.eye_style)
    });
    let (fine_eyes, body_eyes) = phase("accent::project — both extents", PHASE_REPEATS, || {
        (
            accent::project(&frames, &view.view_proj, canvas.width, canvas.height),
            accent::project(&frames, &view.view_proj, body_width, body_height),
        )
    });
    let presence: Vec<f32> = phase("survey::presence — the aperture cast", PHASE_REPEATS, || {
        fine_eyes.iter().map(|eye| survey::presence(&at.mesh, view.eye, &frames[eye.frame()].aperture)).collect()
    });

    let program = phase("wash::program — the graph lay, per canvas height", 1, || wash::program(canvas.height));
    // The stain poles are a few dozen float operations and are stood in
    // for rather than reconstructed; what the blob write is priced
    // against is the placement's shape.
    let placement = Placement { centroids: &centroids, stains: &centroids, iris: iris_of(&fine_eyes) };
    let wanted = Presence::of(&placement);
    let slice =
        phase("seed_uniforms — per canvas and visible set", 1, || program.seed_uniforms(SHEET_SEED, canvas, wanted));
    let frame = Frame {
        view_proj: view.view_proj,
        placement,
        faces: Some(Faces { fine: &fine_eyes, body: &body_eyes, presence: &presence }),
    };
    let wash_uniforms =
        phase("frame_uniforms — the wash blob", PHASE_REPEATS, || program.frame_uniforms(&slice, &frame));
    phase("bake uniforms", PHASE_REPEATS, || bake::BakeUniforms { view_proj: view.view_proj, eye: view.eye }.encode());

    let ink_bytes = phase("ink pack — the drawing's vertex and index buffers", PHASE_REPEATS, || {
        (ink::vertices(&drawn), ink::indices(&drawn))
    });
    let aperture_bytes = phase("aperture pack", PHASE_REPEATS, || {
        (
            face::vertices(&fine_eyes, canvas.width, canvas.height),
            face::indices(&fine_eyes, canvas.width, canvas.height),
        )
    });
    phase("bake pack — per subject", 1, || {
        (bake::vertices(&at.mesh, &at.scores, &at.settings), bake::indices(&at.mesh))
    });

    let megabytes = |bytes: usize| bytes as f64 / 1.0e6;
    eprintln!(
        "phase: a develop that re-derives ships {:.2} MB of geometry — ink {:.2} MB over {} triangles, aperture \
         {:.3} MB — and {} bytes of wash uniforms",
        megabytes(ink_bytes.0.len() + ink_bytes.1.len() + aperture_bytes.0.len() + aperture_bytes.1.len()),
        megabytes(ink_bytes.0.len() + ink_bytes.1.len()),
        drawn.len(),
        megabytes(aperture_bytes.0.len() + aperture_bytes.1.len()),
        wash_uniforms.len(),
    );
}

/// The half only an eye that moved pays: the volatile curves and the
/// field's own re-pack of them. Neither is the easel's, and neither is
/// skippable — an orbit frame genuinely re-solves both.
fn time_the_eye_moved_half(at: &Subject, drawing: Drawing<'_>, view: &View) {
    phase("volatile extraction — the eye-moved half", PHASE_REPEATS, || at.volatile_at(view.eye));
    phase("strokes pack — the volatile field and ribbon buffers", PHASE_REPEATS, || {
        let layout = sight::layout(drawing, (WIDTH, HEIGHT)).expect("the drawing fits");
        (
            sight::point_vertices(drawing, layout.volatile()),
            sight::point_indices(layout.volatile()),
            sight::curve_vertices(drawing, &layout, view.eye),
            stroke::ribbon_geometry(drawing, &layout, Half::Volatile),
        )
    });
}

/// The subject the phases are measured against, assembled once — the
/// mesh, its material field, and everything the component solves at load.
struct Subject {
    mesh: Mesh,
    labels: Labels,
    settings: Settings,
    anchors: anchor::Anchors,
    scores: Vec<[f32; CLASSES]>,
    resident: Vec<Curve3>,
}

impl Subject {
    fn load(dir: &Path) -> Self {
        let mesh = Mesh::from_obj_bytes(&fs::read(dir.join("subject.obj")).expect("read subject.obj"), RELAXATION)
            .expect("parse the subject");
        let labels =
            Labels::parse(&fs::read(dir.join("labels.npy")).expect("read labels.npy"), mesh.min, mesh.max, LABEL_PAD)
                .expect("parse the material field");
        let settings = Settings::default();
        let anchors = anchor::Anchors::measure(&mesh, &labels).expect("the subject carries a charted face");
        let scores = labels.vertex_scores(&mesh.positions);
        let resident = extract::surface(&mesh, Some(&labels), Some(&anchors), &settings);

        Self { mesh, labels, settings, anchors, scores, resident }
    }

    fn chart_face(&self) -> chart::Face {
        self.settings.face.expect("the default settings chart a face")
    }

    /// The drawing's eye-dependent half, exactly as `Puppet::on_render`
    /// assembles it.
    fn volatile_at(&self, eye: Vec3) -> Vec<Curve3> {
        chart::marks(&self.mesh, &self.anchors, self.chart_face(), &self.settings, eye)
            .into_iter()
            .chain(extract::suggestive(&self.mesh, Some(&self.labels), eye, &self.settings))
            .chain(extract::silhouettes(&self.mesh, eye))
            .collect()
    }

    /// The drawing split into visible runs and ribboned — `Puppet::split`,
    /// which the easel asks for once per develop.
    fn split(&self, drawing: Drawing<'_>, eye: Vec3) -> Vec<DrawTriangle> {
        let mut triangles = Vec::new();
        for curve in drawing.curves() {
            let mode = visibility::Mode::Opaque;
            let bias = self.mesh.surface_bias();
            for run in visibility::runs(&self.mesh, eye, curve, &|_| true, mode, VISIBILITY_STRIDE, bias) {
                ribbon::ribbon(&run, eye, 0, &mut triangles);
            }
        }

        triangles
    }
}

/// One phase's mean, in milliseconds, and its result.
///
/// The last repetition's value is the one returned, so a phase feeds the
/// phase that consumes it without being run an extra time for the value.
fn phase<T>(label: &str, repeats: u32, mut work: impl FnMut() -> T) -> T {
    let started = Instant::now();
    let mut last = work();
    for _ in 1..repeats {
        last = work();
    }
    eprintln!("phase: {label:<58} {:>8.3} ms", started.elapsed().as_secs_f64() * 1000.0 / f64::from(repeats.max(1)));

    last
}

/// The camera the component holds at `azimuth`, the whole `View` the
/// easel develops through.
fn view_at(azimuth: f32) -> View {
    let (eye, aspect) = (eye_at(azimuth), WIDTH as f32 / HEIGHT as f32);
    let view_proj =
        Mat4::perspective_rh(FIELD_OF_VIEW, aspect, 0.05, 40.0) * Mat4::look_at_rh(eye, Vec3::ZERO, Vec3::Y);

    View { eye, target: Vec3::ZERO, view_proj, aspect, field_of_view: FIELD_OF_VIEW }
}

/// The iris pole, area-weighted over the projected eyes — the easel's own
/// `iris_centre`, which is private to it.
fn iris_of(eyes: &[accent::Eye]) -> Option<Vec2> {
    let (mut sum, mut weight) = (Vec2::new(0.0, 0.0), 0.0);
    for eye in eyes {
        let area = eye.size() * eye.size();
        sum += eye.centre() * area;
        weight += area;
    }

    (weight > 0.0).then(|| sum / weight)
}

/// Where the frame's GPU time goes, pass by pass, across every program
/// the puppet registers (iamacoffeepot/aether#4423).
///
/// A separate run from the pacing above, and it has to be: the timestamp
/// instrument places two queries per pass per frame and costs a few
/// percent of the tick, so a whole-frame millisecond measured with it on
/// is not the frame this branch ships. Read the two together — the
/// pacing says how long the frame is, this says what it is made of.
///
/// Program ids are not visible to a test driving the component, so the
/// table asks every id the puppet could have been assigned and keeps the
/// ones that answer. Each program identifies itself by its own pass
/// labels, which is what the reader wants named anyway.
///
/// ```text
/// AETHER_CROSSFEED_DIR=/path/to/dir \
///     cargo test -p aether-puppet --release --test pace_instrument \
///     -- --ignored --nocapture the_frame_divides_by_pass
/// ```
#[test]
#[ignore = "instrument; needs the shipped subject in AETHER_CROSSFEED_DIR and a release-profile component wasm"]
fn the_frame_divides_by_pass() {
    let (Some(wasm), Some(dir)) = (require_runtime("aether_puppet"), subject_dir()) else {
        return;
    };
    let mut harness = mounted_with(&dir, &wasm, true);

    harness
        .execute(vec![("frame", HarnessOp::send_and_settle(PUPPET, &look(AZIMUTH))), ("prime", HarnessOp::advance(24))])
        .expect("prime the pinned framing");
    // The EWMA needs samples, and the readback lands a frame or two
    // behind the encode that placed the queries.
    harness.execute(vec![("measure", HarnessOp::advance(SAMPLES))]).expect("accumulate timing samples");

    for program_id in 0..PROGRAM_IDS {
        let read = harness
            .execute(vec![(
                "timings",
                HarnessOp::send_and_await_reply("aether.render", &ProgramTimings { program_id }),
            )])
            .expect("query the per-pass timings");
        let rows = match read.reply::<ProgramTimingsResult>("timings").expect("decode ProgramTimingsResult") {
            ProgramTimingsResult::Ok { rows, .. } => rows,
            ProgramTimingsResult::Absent { reason } => {
                eprintln!("passes: instrument absent — {reason}");
                return;
            }
            // Every id past the last the puppet registered.
            ProgramTimingsResult::Err { .. } => continue,
        };

        report_program(program_id, &rows);
    }

    report_handler_cost(&mut harness);
}

/// The other half of the frame: what the component's own handlers cost
/// on the CPU, from the same per-handler EWMA `actor_cost` reads.
///
/// The GPU table above and this one do not sum to the wall-clock frame
/// and are not meant to — the two run concurrently, and the wall clock
/// also carries the host's encode and the harness' own present. What the
/// pair answers is which side a frame is spent on, and which handler or
/// which pass to look at first.
fn report_handler_cost(harness: &mut SubstrateHarness) {
    let read = harness
        .execute(vec![("cost", HarnessOp::send_and_await_reply(PUPPET, &CostTail { kind: None }))])
        .expect("query the puppet's handler costs");
    let rows = match read.reply::<CostTailResult>("cost").expect("decode CostTailResult") {
        CostTailResult::Ok { rows } => rows,
        CostTailResult::Err { error } => {
            eprintln!("cost: unavailable — {error}");
            return;
        }
    };

    // A wasm trampoline answers with ids alone, so the kinds this
    // instrument reads are named from their own `Kind::ID` consts. The
    // rest print as tagged ids, which is enough to tell them apart.
    let named = |row: &CostRow| {
        [(Render::ID, Render::NAME), (Look::ID, Look::NAME), (Load::ID, Load::NAME), (WindowSize::ID, WindowSize::NAME)]
            .into_iter()
            .find_map(|(id, name)| (id == row.kind_id).then_some(name.to_owned()))
            .or_else(|| row.kind_name.clone())
            .unwrap_or_else(|| row.kind_id.to_string())
    };

    let mut rows: Vec<_> = rows.into_iter().filter(|row| row.samples > 0).collect();
    rows.sort_unstable_by_key(|row| Reverse(row.mean_nanos));
    for row in rows.iter().take(8) {
        eprintln!(
            "cost: {:<32} {:>8.3} ms  (mad {:>6.3} ms over {} samples)",
            named(row),
            row.mean_nanos as f64 / 1.0e6,
            row.mad_nanos as f64 / 1.0e6,
            row.samples,
        );
    }
}

/// One program's table: its whole GPU time, then the entry points that
/// carry it, widest first, and the ten most expensive individual passes.
fn report_program(program_id: u32, rows: &[PassTimingRow]) {
    let millis = |nanos: u64| nanos as f64 / 1.0e6;
    let total: u64 = rows.iter().map(|row| row.mean_nanos).sum();
    let measured = rows.iter().filter(|row| row.samples > 0).count();
    if total == 0 {
        eprintln!("passes: program {program_id} — {} passes, none measured", rows.len());
        return;
    }

    let mut by_entry: Vec<(&str, u32, u64)> = Vec::new();
    for row in rows {
        match by_entry.iter_mut().find(|(label, ..)| *label == row.label) {
            Some((_, passes, nanos)) => {
                *passes += 1;
                *nanos += row.mean_nanos;
            }
            None => by_entry.push((&row.label, 1, row.mean_nanos)),
        }
    }
    by_entry.sort_unstable_by_key(|&(.., nanos)| Reverse(nanos));

    eprintln!(
        "passes: program {program_id} — {:.2} ms over {} passes ({measured} measured)",
        millis(total),
        rows.len(),
    );
    for (label, passes, nanos) in by_entry.iter().take(12) {
        eprintln!(
            "  {label:<24} {passes:>4} passes  {:>7.2} ms  {:>5.1}%",
            millis(*nanos),
            100.0 * *nanos as f64 / total as f64,
        );
    }

    let mut hottest: Vec<&PassTimingRow> = rows.iter().collect();
    hottest.sort_unstable_by_key(|row| Reverse(row.mean_nanos));
    for row in hottest.iter().take(10) {
        eprintln!(
            "  pass {:>4} {:<24} {}x{} /{} x{}  {:>7.3} ms",
            row.pass,
            row.label,
            row.width,
            row.height,
            row.divisor,
            row.iterations,
            millis(row.mean_nanos),
        );
    }
}

fn eye_at(azimuth: f32) -> Vec3 {
    let (azimuth, elevation) = (azimuth.to_radians(), ELEVATION.to_radians());
    let (sin_a, cos_a) = azimuth.sin_cos();
    let (sin_e, cos_e) = elevation.sin_cos();

    Vec3::new(sin_a * cos_e, sin_e, cos_a * cos_e) * DISTANCE
}
