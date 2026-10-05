//! Measurement spike: what does it cost to draw a few thousand textured,
//! vertex-coloured model instances per frame through `aether.render`'s
//! authored render programs as they exist today?
//!
//! `mesh-draw-path [--frames N] [--warmup N] [--only <approach>] [--models a,b] [--instances a,b] [--capture]`
//!
//! Drives the real render capability in-process through `SubstrateHarness`
//! (offscreen, no vsync), one fresh harness per configuration, and prints a
//! markdown table. `--capture` writes one PNG per configuration under
//! `captures/` and reports each picture's difference from the static batch.

mod model;
mod scene;
mod shaders;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aether_data::Kind;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::envelope;
use aether_harness_substrate_capture::visual::{Image, decode_png};
use aether_kinds::QuadSpace;
use aether_math::Rgba;
use aether_render::{DrawTexturedQuads, QuadBlend, RenderCapability, TexturedQuad, spike_probe};

use scene::{Approach, HEIGHT, Scene, WIDTH};

struct Options {
    frames: usize,
    warmup: usize,
    only: Option<String>,
    models: Vec<usize>,
    instances: Vec<usize>,
    capture: bool,
}

fn parse_list(value: &str) -> Vec<usize> {
    value.split(',').map(|item| item.parse().expect("a comma-separated list of counts")).collect()
}

fn options() -> Options {
    let mut options = Options {
        frames: 200,
        warmup: 30,
        only: None,
        models: vec![1, 50, 300],
        instances: vec![100, 1000, 5000, 20000],
        capture: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().expect("flag takes a value");
        match flag.as_str() {
            "--frames" => options.frames = value().parse().expect("frame count"),
            "--warmup" => options.warmup = value().parse().expect("warm-up frame count"),
            "--only" => options.only = Some(value()),
            "--models" => options.models = parse_list(&value()),
            "--instances" => options.instances = parse_list(&value()),
            "--capture" => options.capture = true,
            "--no-metal-guard" => {}
            other => panic!("unknown flag {other}"),
        }
    }
    options
}

fn composite(output: u32) -> DrawTexturedQuads {
    DrawTexturedQuads {
        texture_id: output,
        space: QuadSpace::Screen,
        clip: None,
        blend: QuadBlend::Premultiplied,
        quads: vec![TexturedQuad {
            x: 0.0,
            y: 0.0,
            width: WIDTH as f32,
            height: HEIGHT as f32,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    }
}

struct Probe {
    record: u64,
    wait: u64,
    submit: u64,
    frame: u64,
    dispatches: u64,
}

fn probe() -> Probe {
    let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    Probe {
        record: read(&spike_probe::PROGRAM_RECORD_NANOS),
        wait: read(&spike_probe::GPU_WAIT_NANOS),
        submit: read(&spike_probe::SUBMIT_NANOS),
        frame: read(&spike_probe::FRAME_NANOS),
        dispatches: read(&spike_probe::DISPATCHES),
    }
}

struct Row {
    approach: Approach,
    models: usize,
    instances: usize,
    scene_counts: scene::Counts,
    register_millis: f64,
    frames: usize,
    median_millis: f64,
    worst_millis: f64,
    build_millis: f64,
    handler_millis: f64,
    record_millis: f64,
    submit_millis: f64,
    wait_millis: f64,
    mails: usize,
    dispatches: f64,
    mailed_bytes: usize,
}

/// Send one frame's mail and run one frame. Returns the wall-clock millis
/// from the first mail's encode to the frame handler returning, the
/// blob-building millis that preceded it, the mail count, and the bytes.
fn run_frame(
    harness: &mut SubstrateHarness,
    scene: &Scene,
    instances: usize,
    frame: usize,
) -> (f64, f64, usize, usize) {
    let render = harness.actor_ref::<RenderCapability>();
    let build_started = Instant::now();
    let view_proj = model::view_projection(instances, frame, WIDTH as f32 / HEIGHT as f32);
    let dispatches = (scene.frame)(&view_proj);
    let build_millis = build_started.elapsed().as_secs_f64() * 1e3;

    let labels: Vec<String> = (0..dispatches.len()).map(|index| format!("dispatch-{index}")).collect();
    let quad = composite(scene.output);
    let mut bytes = quad.encode_into_bytes().len();
    let started = Instant::now();
    let mut steps: Vec<(&str, HarnessOp)> = Vec::with_capacity(dispatches.len() + 2);
    for (label, dispatch) in labels.iter().zip(&dispatches) {
        steps.push((label, HarnessOp::send_and_settle(&render, dispatch)));
    }
    steps.push(("composite", HarnessOp::send_and_settle(&render, &quad)));
    steps.push(("frame", HarnessOp::advance(1)));
    harness.execute(steps).expect("frame");
    let millis = started.elapsed().as_secs_f64() * 1e3;

    bytes += dispatches.iter().map(|dispatch| dispatch.encode_into_bytes().len()).sum::<usize>();
    (millis, build_millis, dispatches.len() + 1, bytes)
}

fn capture(harness: &mut SubstrateHarness, scene: &Scene, instances: usize, name: &str) -> Image {
    let view_proj = model::view_projection(instances, 0, WIDTH as f32 / HEIGHT as f32);
    let mut mails: Vec<_> =
        (scene.frame)(&view_proj).iter().map(|dispatch| envelope("aether.render", dispatch)).collect();
    mails.push(envelope("aether.render", &composite(scene.output)));
    let result = harness.execute(vec![("capture", HarnessOp::capture_with_mails(mails, Vec::new()))]).expect("capture");
    let png = result.captured("capture").expect("capture step ran");
    std::fs::create_dir_all("captures").expect("create captures dir");
    std::fs::write(format!("captures/{name}.png"), png).expect("write capture");
    decode_png(png).expect("decode capture")
}

/// Mean absolute per-channel difference and the share of pixels that are
/// not the background, so a blank frame cannot pass as a match.
fn compare(image: &Image, reference: &Image) -> (f64, f64) {
    let background = &image.rgba[..4];
    let mut difference = 0u64;
    let mut covered = 0u64;
    for (pixel, other) in image.rgba.chunks_exact(4).zip(reference.rgba.chunks_exact(4)) {
        difference += pixel.iter().zip(other).map(|(a, b)| u64::from(a.abs_diff(*b))).sum::<u64>();
        covered += u64::from(pixel != background);
    }
    let pixels = (image.rgba.len() / 4) as f64;
    (difference as f64 / (pixels * 4.0), covered as f64 / pixels * 100.0)
}

fn measure(options: &Options, approach: Approach, models: usize, instances: usize) -> Row {
    let mut harness =
        SubstrateHarness::builder().size(WIDTH, HEIGHT).with_render().build().expect("boot render harness");
    let placed = model::instances(models, instances);
    let scene = scene::build(&mut harness, approach, models, &placed);

    for frame in 0..options.warmup {
        run_frame(&mut harness, &scene, instances, frame);
    }
    let before = probe();
    let mut times = Vec::with_capacity(options.frames);
    let (mut build, mut mails, mut bytes) = (0.0, 0, 0);
    let mut frames = 0;
    let budget_started = Instant::now();
    while frames < options.frames {
        let (millis, build_millis, frame_mails, frame_bytes) =
            run_frame(&mut harness, &scene, instances, options.warmup + frames);
        times.push(millis);
        build += build_millis;
        mails = frame_mails;
        bytes = frame_bytes;
        frames += 1;
        let slow = budget_started.elapsed().as_secs_f64() > 60.0;
        let enough = frames >= 30;
        if slow && enough {
            break;
        }
    }
    let after = probe();
    times.sort_by(f64::total_cmp);
    let per_frame = |end: u64, start: u64| (end - start) as f64 / 1e6 / frames as f64;
    Row {
        approach,
        models,
        instances,
        scene_counts: scene.counts,
        register_millis: scene.register_millis,
        frames,
        median_millis: times[times.len() / 2],
        worst_millis: times[times.len() - 1],
        build_millis: build / frames as f64,
        handler_millis: per_frame(after.frame, before.frame),
        record_millis: per_frame(after.record, before.record),
        submit_millis: per_frame(after.submit, before.submit),
        wait_millis: per_frame(after.wait, before.wait),
        mails,
        dispatches: (after.dispatches - before.dispatches) as f64 / frames as f64,
        mailed_bytes: bytes,
    }
}

fn print_header() {
    println!(
        "| approach | models | instances | pass entries | geometries | geometry MB | register ms | frames | frame ms median | frame ms worst | on_frame ms | program record ms | finish+submit ms | wait prev GPU ms | blob build ms | mails | dispatches | render passes | compute passes | draw calls | bytes mailed |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
}

fn print_row(row: &Row) {
    let counts = &row.scene_counts;
    println!(
        "| {} | {} | {} | {} | {} | {:.1} | {:.0} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {} | {:.0} | {} | {} | {} | {} |",
        row.approach.name(),
        row.models,
        row.instances,
        counts.pass_entries,
        counts.geometries,
        counts.geometry_bytes as f64 / 1e6,
        row.register_millis,
        row.frames,
        row.median_millis,
        row.worst_millis,
        row.handler_millis,
        row.record_millis,
        row.submit_millis,
        row.wait_millis,
        row.build_millis,
        row.mails,
        row.dispatches,
        counts.render_passes,
        counts.compute_passes,
        counts.draw_calls,
        row.mailed_bytes,
    );
}

/// Why a configuration cannot be drawn at all, if it cannot.
fn refusal(approach: Approach, models: usize, instances: usize) -> Option<String> {
    if models > instances {
        return Some("fewer instances than distinct models".to_owned());
    }
    let iterations = match approach {
        Approach::RepeatPerTexture => instances * model::PARTS,
        Approach::RepeatAtlas => instances,
        _ => 0,
    };
    if iterations > 65_536 {
        return Some(format!("{iterations} pass iterations exceed MAX_PASS_ITERATIONS 65536"));
    }
    // Measured on Metal (wgpu-hal 29.0.1): every program pass costs two
    // Metal command buffers and the queue allows 4096 outstanding, so a
    // frame that encodes more than about 2040 passes loses the device.
    let passes = match approach {
        Approach::RepeatPerTexture => instances * model::PARTS,
        Approach::RepeatAtlas | Approach::DispatchPerInstance => instances,
        Approach::InstancedPerTexture => models * model::PARTS * 2,
        Approach::InstancedAtlas => models * 2,
        Approach::StaticBatch | Approach::Baseline => 0,
    };
    let unchecked = std::env::args().any(|flag| flag == "--no-metal-guard");
    if passes > 2040 && !unchecked {
        return Some(format!(
            "{passes} passes in one frame lose the Metal device (2 command buffers per pass, limit 4096)"
        ));
    }
    None
}

fn main() {
    let options = options();
    let selected: Vec<Approach> = Approach::ALL
        .into_iter()
        .filter(|approach| {
            options.only.as_deref().is_none_or(|only| only.split(',').any(|name| name == approach.name()))
        })
        .collect();

    if options.capture {
        for &models in &options.models {
            for &instances in &options.instances {
                let mut reference: Option<Image> = None;
                let mut order = vec![Approach::StaticBatch];
                order.extend(selected.iter().copied().filter(|approach| *approach != Approach::StaticBatch));
                for approach in order {
                    if let Some(reason) = refusal(approach, models, instances) {
                        println!("{} {models}x{instances}: skipped ({reason})", approach.name());
                        continue;
                    }
                    let mut harness = SubstrateHarness::builder()
                        .size(WIDTH, HEIGHT)
                        .with_render()
                        .build()
                        .expect("boot render harness");
                    let placed = model::instances(models, instances);
                    let scene = scene::build(&mut harness, approach, models, &placed);
                    let name = format!("{}-{models}x{instances}", approach.name());
                    let image = capture(&mut harness, &scene, instances, &name);
                    let (difference, covered) = compare(&image, reference.as_ref().unwrap_or(&image));
                    println!(
                        "{name}: {covered:.1}% of pixels drawn, mean abs channel difference vs static-batch {difference:.3}"
                    );
                    if approach == Approach::StaticBatch {
                        reference = Some(image);
                    }
                }
            }
        }
        return;
    }

    print_header();
    for approach in selected {
        for &models in &options.models {
            for &instances in &options.instances {
                if let Some(reason) = refusal(approach, models, instances) {
                    println!("| {} | {models} | {instances} | skipped: {reason} |", approach.name());
                    continue;
                }
                print_row(&measure(&options, approach, models, instances));
            }
        }
    }
}
