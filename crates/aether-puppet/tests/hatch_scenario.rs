//! The hatching has to read as shading from wherever the eye stands.
//!
//! One subject, three azimuths a third of a turn apart, and one number
//! per view: how much of the subject's own silhouette carries ink. The
//! subject is a sphere, which is what makes the number an oracle — its
//! visible half is the same visible half from every azimuth, so the three
//! views differ in nothing except where the drawing thinks the light is
//! and how the hatch planes fall. Any spread between them is the shading
//! model's own azimuth dependence.
//!
//! What this catches is a key light fixed in the world while the camera
//! orbits: the lit side faces the viewer at one azimuth and away at the
//! opposite one, so a sweep flips between a bare crescent and a solid
//! mesh with nothing in between reading as a tone ramp. Against a
//! world-fixed light the three views measure 0.14, 0.50 and 0.38 — the
//! solid end breaks the band's ceiling and the 3.7x spread between them
//! names the flip itself.
//!
//! `SubstrateHarness` rather than `FleetHarness` per the harness decision
//! rule: the assertion is about rendered output. The window size is
//! mailed to the puppet directly because the harness has no window to
//! announce one.

use std::f32::consts::{PI, TAU};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use aether_harness_substrate::{HarnessActor, HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::visual::{Image, decode_png};
use aether_harness_substrate_capture::{
    RenderHarnessBuilderExt,
    test_helpers::{init_save_sandbox, require_runtime, rgba_at, test_namespace_roots, write_fixture},
};
use aether_kinds::{LoadComponent, LoadResult, WindowId, WindowSize};
use aether_puppet::{Load, Look, Puppet};

/// ADR-0138: the merged multi-actor module is defaultless, so every load
/// names the actor it wants.
const PUPPET_EXPORT: &str = "aether.puppet";

const WIDTH: u32 = 512;
const HEIGHT: u32 = 384;

/// Where the eye stands for each view. A third of a turn apart, so the
/// three between them cover the sweep rather than one side of it.
const AZIMUTHS: [f32; 3] = [0.0, 120.0, 240.0];

/// Far enough back that the sphere clears the frame edges under the
/// puppet's own field of view, so every view frames the identical disc
/// and the silhouette denominator is the same number three times.
const DISTANCE: f32 = 5.0;

/// A pixel counts as ink when a colour channel is this far off the
/// background. The strokes are hairlines resolved by 4x MSAA, so a
/// partly covered pixel lands well short of the stroke's own colour;
/// this sits low enough to count those and high enough to ignore the
/// clear colour's own dither.
const INK_MARGIN: u8 = 20;

fn puppet() -> HarnessActor<Puppet> {
    HarnessOp::loaded_default::<Puppet>()
}

/// A UV sphere of radius one at the origin, as OBJ text.
///
/// Smooth and closed, so the tone term sweeps the whole range from fully
/// lit to the ambient floor across the visible half — which is what a
/// hatch ramp needs to have anything to ramp through. Written out here
/// rather than checked in as a fixture because the whole of it is these
/// twenty lines, and a reader who wants a coarser or finer sphere
/// changes two numbers.
fn sphere_obj(meridians: u16, rings: u16) -> String {
    let (across, around) = (f32::from(meridians), f32::from(rings));
    let mut out = String::from("v 0 1 0\n");
    for ring in 1..rings {
        let polar = PI * f32::from(ring) / around;
        let (radius, height) = (polar.sin(), polar.cos());
        for meridian in 0..meridians {
            let azimuth = TAU * f32::from(meridian) / across;
            writeln!(out, "v {} {height} {}", radius * azimuth.sin(), radius * azimuth.cos())
                .expect("writing to a String cannot fail");
        }
    }
    out.push_str("v 0 -1 0\n");

    // One-based, north pole first, then the rings in order, then the
    // south pole. Every triangle is wound counter-clockwise seen from
    // outside, which the OBJ reader keeps and the silhouette needs.
    let (meridians, rings) = (usize::from(meridians), usize::from(rings));
    let at = |ring: usize, meridian: usize| 2 + (ring - 1) * meridians + meridian % meridians;
    let south = 2 + (rings - 1) * meridians;
    for meridian in 0..meridians {
        writeln!(out, "f 1 {} {}", at(1, meridian), at(1, meridian + 1)).expect("infallible");
        writeln!(out, "f {south} {} {}", at(rings - 1, meridian + 1), at(rings - 1, meridian)).expect("infallible");
    }
    for ring in 1..rings - 1 {
        for meridian in 0..meridians {
            let (a, b) = (at(ring, meridian), at(ring, meridian + 1));
            let (c, d) = (at(ring + 1, meridian + 1), at(ring + 1, meridian));
            writeln!(out, "f {a} {d} {c}\nf {a} {c} {b}").expect("infallible");
        }
    }

    out
}

fn load_puppet(harness: &mut SubstrateHarness, wasm_path: &Path) {
    let wasm = fs::read(wasm_path).expect("read the puppet wasm");
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent { wasm, name: None, config: Vec::new(), export: Some(PUPPET_EXPORT.to_owned()) },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load_component(puppet): {error}"),
    }
}

/// Ink as a fraction of the subject's own silhouette, not of the frame.
///
/// The denominator is the span between the first and last off-background
/// pixel on each row, summed — the filled silhouette of a convex subject.
/// Measuring against the frame instead would fold the framing into the
/// number, and the framing is not what is being asserted.
fn ink_coverage(img: &Image) -> f64 {
    let background = rgba_at(img, 0, 0);
    let inked =
        |x, y| rgba_at(img, x, y).iter().take(3).zip(background).any(|(&at, paper)| at.abs_diff(paper) > INK_MARGIN);

    let (mut ink, mut inside) = (0u32, 0u32);
    for y in 0..img.height {
        let row: Vec<u32> = (0..img.width).filter(|&x| inked(x, y)).collect();
        let (Some(&first), Some(&last)) = (row.first(), row.last()) else {
            continue;
        };
        ink += u32::try_from(row.len()).expect("a row holds at most the image's width");
        inside += last - first + 1;
    }
    assert!(inside > 0, "the capture carries no subject at all");

    f64::from(ink) / f64::from(inside)
}

/// Ink coverage a smooth subject's shading may sit between.
///
/// The estimate this brackets: at this framing the disc is about 355
/// pixels across, so the authored spacing lands the three families 7, 9
/// and 12 pixels apart, and a resolved hairline covers a little over one
/// pixel of that — a fifth, a seventh and a ninth of whatever region each
/// family draws in. A ramp that leaves the lit part bare and climbs
/// through one, two and three families over the rest averages near a
/// fifth of the silhouette. The band is that halved and doubled, which
/// leaves room for another rasterizer's idea of a hairline without
/// leaving room for a drawing that stopped ramping: below the floor the
/// subject is an outline with a few strokes on it, above the ceiling it
/// is a solid mesh, and those are the two ends of the flip.
const COVERAGE: (f64, f64) = (0.09, 0.38);

/// How far apart the three views' coverage may sit, as a ratio.
///
/// Not one: the hatch planes are held in world space, so each view sees
/// them at its own angle and a family turned toward the viewer packs more
/// ink into the same area than one seen edge-on. That is a projection
/// term and it is bounded. A key light that stays put while the camera
/// orbits is not bounded — it takes the ramp from every family to none —
/// and this catches it with room to spare.
const SPREAD: f64 = 2.0;

#[test]
fn hatching_reads_as_shading_from_every_azimuth() {
    let Some(wasm_path) = require_runtime("aether_puppet") else {
        return;
    };
    let save_dir = init_save_sandbox("puppet-hatch");
    let subject = write_fixture("sphere.obj", sphere_obj(64, 32).as_bytes());

    let mut harness = SubstrateHarness::builder()
        .size(WIDTH, HEIGHT)
        .namespace_roots(test_namespace_roots(save_dir))
        .with_render()
        .with_component_host()
        .build()
        .expect("boot a rendering harness with a component host");
    load_puppet(&mut harness, &wasm_path);

    harness
        .execute(vec![
            (
                "size",
                puppet().send(&WindowSize { window: WindowId(1), width: WIDTH, height: HEIGHT, scale_factor: 1.0 }),
            ),
            (
                "subject",
                puppet().send(&Load {
                    namespace: "assets".to_owned(),
                    path: subject,
                    labels: String::new(),
                    material_field_padding: 0.12,
                    rig: String::new(),
                    palette: String::new(),
                }),
            ),
        ])
        .expect("the size and subject load settle");

    // The drawing is re-solved on the render that follows a new eye, and
    // the ink's own textures settle a frame behind their creates, so each
    // view is primed before it is read.
    let views = [("look-a", "prime-a", "front"), ("look-b", "prime-b", "left"), ("look-c", "prime-c", "right")];
    let steps = views
        .iter()
        .zip(AZIMUTHS)
        .flat_map(|(&(look, prime, view), azimuth)| {
            [
                (look, puppet().send(&Look { azimuth, elevation: 12.0, distance: DISTANCE, height: 0.0 })),
                (prime, HarnessOp::advance(5)),
                (view, HarnessOp::capture()),
            ]
        })
        .collect::<Vec<_>>();
    let swept = harness.execute(steps).expect("the sweep runs");

    let measured: Vec<(&str, f64)> = views
        .iter()
        .map(|&(_, _, view)| {
            let png = swept.captured(view).expect("the capture step ran");
            (view, ink_coverage(&decode_png(png).expect("decode the captured png")))
        })
        .collect();
    let report = measured.iter().map(|(view, ink)| format!("{view} {ink:.4}")).collect::<Vec<_>>().join(", ");

    for (view, ink) in &measured {
        assert!(
            (COVERAGE.0..=COVERAGE.1).contains(ink),
            "the {view} view's ink coverage {ink:.4} is outside {COVERAGE:?}; all three were {report}",
        );
    }

    let low = measured.iter().map(|(_, ink)| *ink).fold(f64::MAX, f64::min);
    let high = measured.iter().map(|(_, ink)| *ink).fold(f64::MIN, f64::max);
    assert!(
        high <= low * SPREAD,
        "the three views' ink coverage spreads by {:.2}x, over the {SPREAD:.1}x a projection term can account for; \
         they were {report}",
        high / low,
    );
}
