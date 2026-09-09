// The direction oracle below is pixel arithmetic: a probe offset is a
// rounded distance, an index is a pixel count, and a mean is a sum over a
// count of windows — none of which has a lossless conversion. The crate
// under test carries the same note at its own root for the same reason.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

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
//! The second gate here asks the other half of the same question. Ink
//! measures how much the drawing draws; it cannot see *which way* the
//! strokes run, and stroke direction has its own way of failing on a
//! turntable — hatch families whose axes share a plane of the model
//! project to one screen angle from the two azimuths whose view direction
//! lies in that plane, and the cross-hatch collapses into contour bands
//! there. So a twelve-view sweep measures how widely the families cross
//! where the drawing is darkest, and holds every view above a floor
//! (iamacoffeepot/aether#5878).
//!
//! `SubstrateHarness` rather than `FleetHarness` per the harness decision
//! rule: the assertion is about rendered output. The window size is
//! mailed to the puppet directly because the harness has no window to
//! announce one.

use std::cmp::Reverse;
use std::env;
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

/// The subject the direction sweep turns, and the framing the demo turns
/// it at (`demo/turntable.json`).
///
/// A teapot rather than the sphere the ink test uses, and measured rather
/// than assumed: a sphere's level sets curve out from under the plane they
/// were cut on fast enough that three families sharing one screen
/// direction still meet at an angle over most of it, so the sphere reads a
/// collapse as a mild dip. Swept on the sphere, the head this replaces
/// measures 60.6, 26.2, 16.8, 29.7, 45.2, 66.9, 70.0, 47.1, 24.7, 23.0,
/// 34.4 and 52.3 degrees — no floor separates the azimuths that collapse
/// from the ones that do not. The teapot's body is a surface of revolution
/// about the axis the turntable sweeps, so its normals stay near
/// horizontal over a wide band and the collapse shows there as what the
/// eye sees: contour banding.
const TEAPOT_OBJ: &[u8] = include_bytes!("../../aether-mesh/examples/utah_teapot.obj");
const TEAPOT_DISTANCE: f32 = 4.0;
const TEAPOT_HEIGHT: f32 = 0.6;

/// The frame the direction sweep is read at — the demo's own, so the
/// number is measured on the picture the collapse was seen in.
const SWEEP_WIDTH: u32 = 960;
const SWEEP_HEIGHT: u32 = 600;

/// Azimuths the direction sweep walks: a whole turn in twelve steps, so
/// the two views a world-fixed axis set collapses at land on samples
/// rather than between them. All three of the old axes lay in the model's
/// XY plane, and at 90 and 270 the view direction lies in that plane too
/// — every family then projects to the same screen angle and the
/// cross-hatch rules one set of near-horizontal bands.
const DIRECTION_SWEEP: [f32; 12] = [0.0, 30.0, 60.0, 90.0, 120.0, 150.0, 180.0, 210.0, 240.0, 270.0, 300.0, 330.0];

/// Orientations the oracle bins strokes into: one every 7.5 degrees over
/// the half turn an undirected line spans.
const DIRECTIONS: usize = 24;

/// How far along a candidate direction the line probe reaches, in pixels,
/// and how many of its `2 * REACH` samples have to be ink before the
/// pixel counts as a point on a line running that way.
///
/// A probe rather than an image gradient because a gradient answers the
/// wrong question here: the hatch is dithered into dashes and drawn with
/// a wobble, so a stroke's ends and kinks throw gradients along the
/// stroke as readily as across it, and a whole-image gradient histogram
/// measured the same spread on a collapsed view as on a crossed one. A
/// probe asks what a reader's eye asks — does a line continue this way.
const REACH: i32 = 5;
const ON_A_LINE: u32 = 8;

/// How far inside the silhouette the oracle looks, in pixels. The outline
/// is a closed curve carrying every orientation there is, so leaving it in
/// hands the histogram a background of directions the hatching never drew.
const INSET: u32 = 8;

/// The neighbourhood one verdict is reached over, and how far apart those
/// neighbourhoods sit.
///
/// Crossing is local: a view whose families all rule the same direction
/// still spreads orientations across the whole figure, because the level
/// sets curve over a rounded subject. What separates a cross-hatch from
/// contour banding is whether two directions meet inside one patch, so
/// the window is a few hatch spacings across and the verdict is per
/// window.
const WINDOW: u32 = 48;
const STEP: u32 = 16;

/// How much of the leading orientation's weight the second one has to
/// carry before the window counts as crossing rather than as one family
/// with a little noise beside it.
const SECOND_SHARE: f64 = 0.35;

/// Least mean crossing angle a view may draw at, in degrees.
///
/// Measured, not derived. Over the twelve views this sweep walks, the
/// resident axis set draws at 36.4, 30.2, 23.5, 23.2, 24.8, 28.7, 46.8,
/// 28.8, 31.3, 43.3, 22.9 and 25.5 degrees; the three world-fixed axes it
/// replaces drew at 48.0, 33.7, 18.8, 35.6, 37.9, 58.3, 47.8, 22.7, 23.2,
/// 26.9, 29.2 and 40.5. The old set is not uniformly worse — it is
/// *uneven*, running from 18.8 to 58.3 over one turn, and the low end is
/// the collapse. The new set runs from 22.9 to 46.8, which is the same
/// drawing seen from any side.
///
/// The floor sits under the new set's worst view and over the old set's,
/// so the sweep fails on the head this replaces and passes on this one.
/// It is a floor rather than a band deliberately: crossing more widely
/// than this is never a fault.
const CROSSING_FLOOR: f64 = 20.0;

/// Which pixels are ink, and which of those sit far enough inside the
/// subject for the outline not to speak for them.
fn ink_and_inside(img: &Image) -> (Vec<bool>, Vec<bool>) {
    let background = rgba_at(img, 0, 0);
    let (width, height) = (img.width as usize, img.height as usize);
    let mut ink = vec![false; width * height];
    for y in 0..img.height {
        for x in 0..img.width {
            let at = rgba_at(img, x, y);
            ink[y as usize * width + x as usize] =
                at.iter().take(3).zip(background).any(|(&here, paper)| here.abs_diff(paper) > INK_MARGIN);
        }
    }

    // The silhouette is the span between the first and last ink on each
    // row, as in `ink_coverage`; inside it is that span held clear of its
    // own edge by `INSET` in every direction.
    let mut span = vec![false; width * height];
    for y in 0..height {
        let row: Vec<usize> = (0..width).filter(|&x| ink[y * width + x]).collect();
        let (Some(&first), Some(&last)) = (row.first(), row.last()) else {
            continue;
        };
        span[y * width + first..=y * width + last].fill(true);
    }

    let inset = INSET as usize;
    let mut inside = vec![false; width * height];
    for y in inset..height - inset {
        for x in inset..width - inset {
            inside[y * width + x] = [y - inset, y, y + inset]
                .into_iter()
                .flat_map(|row| [x - inset, x, x + inset].map(move |column| row * width + column))
                .all(|at| span[at]);
        }
    }

    (ink, inside)
}

/// The pixel offsets each candidate direction probes along.
fn probes() -> Vec<Vec<(i32, i32)>> {
    (0..DIRECTIONS)
        .map(|direction| {
            let (sin, cos) = (PI * direction as f32 / DIRECTIONS as f32).sin_cos();
            (-REACH..=REACH)
                .filter(|step| *step != 0)
                .map(|step| ((cos * step as f32).round() as i32, (sin * step as f32).round() as i32))
                .collect()
        })
        .collect()
}

/// Which direction each ink pixel's line runs in, or `None` where no
/// direction carries enough ink for the pixel to be on a line at all.
fn stroke_directions(img: &Image) -> Vec<Option<usize>> {
    let (ink, inside) = ink_and_inside(img);
    let probes = probes();
    let (width, height) = (img.width as i32, img.height as i32);
    let mut running = vec![None; ink.len()];

    for y in REACH..height - REACH {
        for x in REACH..width - REACH {
            let at = (y * width + x) as usize;
            if !(ink[at] && inside[at]) {
                continue;
            }

            let along = |offsets: &Vec<(i32, i32)>| {
                offsets.iter().filter(|(dx, dy)| ink[((y + dy) * width + x + dx) as usize]).count() as u32
            };
            let (best, hits) = probes.iter().enumerate().map(|(direction, offsets)| (direction, along(offsets))).fold(
                (0, 0),
                |held, candidate| {
                    if candidate.1 > held.1 {
                        candidate
                    } else {
                        held
                    }
                },
            );
            if hits >= ON_A_LINE {
                running[at] = Some(best);
            }
        }
    }

    running
}

/// The angle between one window's two leading stroke directions, in
/// degrees, and zero where it has only one.
fn crossing_angle(window: &[f64; DIRECTIONS]) -> f64 {
    let apart = |a: usize, b: usize| a.abs_diff(b).min(DIRECTIONS - a.abs_diff(b));
    let heaviest = |over: &dyn Fn(usize) -> bool| {
        (0..DIRECTIONS).filter(|&d| over(d)).fold(None, |held: Option<usize>, d| match held {
            Some(best) if window[best] >= window[d] => Some(best),
            _ => Some(d),
        })
    };

    let Some(first) = heaviest(&|_| true).filter(|&d| window[d] > 0.0) else {
        return 0.0;
    };
    let Some(second) = heaviest(&|d| apart(first, d) > 1) else {
        return 0.0;
    };
    if window[second] < SECOND_SHARE * window[first] {
        return 0.0;
    }

    apart(first, second) as f64 * 180.0 / DIRECTIONS as f64
}

/// How widely the strokes cross in this view, in degrees.
///
/// The mean crossing angle over the third of the windows carrying the
/// most stroke ink. The densest third rather than all of them because
/// that is where the ramp has three families running at once — a lightly
/// hatched window legitimately carries one, and averaging those in would
/// measure the tone ramp instead of the crossing.
fn crossing_spread(img: &Image) -> f64 {
    let running = stroke_directions(img);
    let (width, height) = (img.width, img.height);
    let mut windows: Vec<(usize, [f64; DIRECTIONS])> = Vec::new();

    for top in (0..height.saturating_sub(WINDOW)).step_by(STEP as usize) {
        for left in (0..width.saturating_sub(WINDOW)).step_by(STEP as usize) {
            let mut counts = [0.0; DIRECTIONS];
            let mut ink = 0;
            for y in top..top + WINDOW {
                for x in left..left + WINDOW {
                    if let Some(direction) = running[(y * width + x) as usize] {
                        counts[direction] += 1.0;
                        ink += 1;
                    }
                }
            }
            if ink > 0 {
                windows.push((ink, counts));
            }
        }
    }
    assert!(!windows.is_empty(), "the capture carries no strokes to measure a direction on");

    windows.sort_by_key(|(ink, _)| Reverse(*ink));
    let densest = &windows[..windows.len().div_ceil(3)];

    densest.iter().map(|(_, counts)| crossing_angle(counts)).sum::<f64>() / densest.len() as f64
}

/// The strokes have to keep crossing from wherever the eye stands.
///
/// The demo's own subject at the demo's own framing, swept twelve ways,
/// and one number per view: how widely the hatch families cross where the
/// drawing is darkest. A teapot rather than the ink test's sphere for the
/// reason [`TEAPOT_OBJ`] gives.
///
/// What this catches is a stroke direction that belongs to the world
/// rather than to the view. Three plane families fixed in one plane of
/// the model project to one screen angle from the two azimuths whose view
/// direction lies in that plane, and the cross-hatch collapses into
/// contour bands there once per half turn (iamacoffeepot/aether#5878).
#[test]
fn hatch_directions_keep_crossing_through_a_turn() {
    let Some(wasm_path) = require_runtime("aether_puppet") else {
        return;
    };
    let save_dir = init_save_sandbox("puppet-hatch-directions");
    let subject = write_fixture("utah_teapot.obj", TEAPOT_OBJ);

    let mut harness = SubstrateHarness::builder()
        .size(SWEEP_WIDTH, SWEEP_HEIGHT)
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
                puppet().send(&WindowSize {
                    window: WindowId(1),
                    width: SWEEP_WIDTH,
                    height: SWEEP_HEIGHT,
                    scale_factor: 1.0,
                }),
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

    // Three steps per view under three labels, because a label addresses
    // one step's result: the eye moves, the drawing and its textures
    // settle a frame behind, and only then is the view read.
    let staged: Vec<(String, String, String)> = DIRECTION_SWEEP
        .iter()
        .map(|azimuth| (format!("look-{azimuth:.0}"), format!("prime-{azimuth:.0}"), format!("view-{azimuth:.0}")))
        .collect();
    let steps = staged
        .iter()
        .zip(DIRECTION_SWEEP)
        .flat_map(|((look, prime, view), azimuth)| {
            [
                (
                    look.as_str(),
                    puppet().send(&Look { azimuth, elevation: 12.0, distance: TEAPOT_DISTANCE, height: TEAPOT_HEIGHT }),
                ),
                (prime.as_str(), HarnessOp::advance(5)),
                (view.as_str(), HarnessOp::capture()),
            ]
        })
        .collect::<Vec<_>>();
    let swept = harness.execute(steps).expect("the sweep runs");

    let measured: Vec<(f32, f64)> = DIRECTION_SWEEP
        .iter()
        .zip(&staged)
        .map(|(&azimuth, (_, _, view))| {
            let png = swept.captured(view).expect("the capture step ran");
            keep(view, png);

            (azimuth, crossing_spread(&decode_png(png).expect("decode the captured png")))
        })
        .collect();
    let report =
        measured.iter().map(|(azimuth, spread)| format!("{azimuth:.0} {spread:.1}")).collect::<Vec<_>>().join(", ");

    for (azimuth, spread) in &measured {
        assert!(
            *spread >= CROSSING_FLOOR,
            "the strokes at azimuth {azimuth:.0} cross at a mean {spread:.1} degrees, under the \
             {CROSSING_FLOOR:.0} a drawing whose families still cross holds; the turn measured {report}",
        );
    }
}

/// Write one view out under the directory `AETHER_PUPPET_SWEEP_DIR` names,
/// or do nothing when it is unset. The sweep is the argument a change to
/// the stroke directions is made with, and the argument is the pictures.
fn keep(view: &str, png: &[u8]) {
    // Test-only: the harness has no capability config to route a
    // developer's output directory through.
    #[allow(clippy::disallowed_methods, reason = "test-only output path, not capability configuration")]
    let Ok(directory) = env::var("AETHER_PUPPET_SWEEP_DIR") else {
        return;
    };

    fs::write(Path::new(&directory).join(format!("{view}.png")), png).expect("write the swept view");
}
