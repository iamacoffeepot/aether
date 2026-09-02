//! Authored-render-program harness scenarios (ADR-0170, issue #4365):
//! the `aether.render.program` family driven end-to-end through an
//! in-process `SubstrateHarness` — register-time validation classes, a
//! two-pass ping-pong program observed in pixels through the overlay
//! draw path, and a mismatched-binding dispatch that warn-drops while
//! the frame survives.
//!
//! Skipped when no wgpu adapter is available (driverless runners);
//! `AETHER_REQUIRE_RUNTIME=1` (CI) flips the skip into a hard panic.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]

use std::env;

use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{envelope, has_wgpu_adapter, pixel_is_lit, rgba_at};
use aether_harness_substrate_capture::visual::{background_top_left, decode_png};
use aether_kinds::QuadSpace;
use aether_math::Rgba;
use aether_render::QuadBlend;
use aether_render::{
    CreateTexture, CreateTextureResult, DrawSolidQuads, DrawTexturedQuads, InputSlot, OutputSlot, PassStage,
    ProgramDispatch, ProgramPass, ProgramRegister, ProgramRegisterResult, SlotExtent, SlotSpec, SolidQuad,
    TextureFormat, TextureSampling, TextureUsage, TexturedQuad,
};

/// Skip (or panic under `AETHER_REQUIRE_RUNTIME`) when no wgpu adapter
/// is available — every scenario here executes programs for real.
fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

/// The shared test module: both entry points window the same one-float
/// uniform block, so the two passes reading different windows out of one
/// blob is exactly what the scenarios observe.
const MODULE: &str = r"
struct WindowParams { value: f32 }
@group(0) @binding(0) var<uniform> window_params: WindowParams;
@group(1) @binding(0) var source_texture: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

@fragment
fn fs_threshold(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    let lit = select(0.0, 1.0, textureSample(source_texture, source_sampler, uv).r >= window_params.value);
    return vec4<f32>(lit, lit, lit, 1.0);
}

@fragment
fn fs_invert(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    let level = window_params.value - textureSample(source_texture, source_sampler, uv).r;
    return vec4<f32>(level, level, level, 1.0);
}
";

fn full(format: TextureFormat) -> SlotSpec {
    SlotSpec { format, extent: SlotExtent::Full }
}

fn pass(entry: &str, inputs: Vec<InputSlot>, output: OutputSlot, offset: u32, length: u32) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Fragment,
        entry_point: entry.to_owned(),
        inputs,
        output,
        uniform_offset: offset,
        uniform_length: length,
        repeat: None,
    }
}

/// The threshold-then-invert ping-pong graph both pixel scenarios use:
/// binding 0 (source) -> transient 0 -> binding 1 (writable output),
/// pass 0 windowing bytes 0..4 of the blob and pass 1 bytes 4..8.
fn ping_pong_register() -> ProgramRegister {
    ProgramRegister {
        wgsl: MODULE.to_owned(),
        bindings: vec![full(TextureFormat::Rgba8), full(TextureFormat::Rgba8)],
        transients: vec![full(TextureFormat::Rgba8)],
        geometries: Vec::new(),
        depth_transients: Vec::new(),
        passes: vec![
            pass("fs_threshold", vec![InputSlot::Binding { index: 0 }], OutputSlot::Transient { index: 0 }, 0, 4),
            pass("fs_invert", vec![InputSlot::PassOutput { pass: 0 }], OutputSlot::Binding { index: 1 }, 4, 4),
        ],
    }
}

fn create_texture(harness: &mut SubstrateHarness, label: &'static str, mail: &CreateTexture) -> u32 {
    let created = harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", mail))])
        .expect("create_texture sequence");
    match created.reply::<CreateTextureResult>(label).expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture ({label}) failed: {error}"),
    }
}

/// A 2x2 linear-sampled `Rgba8` texture staged from `pixels`, or the
/// writable output the graph draws into when `pixels` is empty — the two
/// shapes every binding in `ping_pong_register` takes.
fn create_2x2(harness: &mut SubstrateHarness, label: &'static str, pixels: Vec<u8>) -> u32 {
    let usage = if pixels.is_empty() {
        TextureUsage::Writable
    } else {
        TextureUsage::Sampled
    };
    create_texture(
        harness,
        label,
        &CreateTexture {
            width: 2,
            height: 2,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage,
            pixels,
        },
    )
}

fn register_reply(
    harness: &mut SubstrateHarness,
    label: &'static str,
    mail: &ProgramRegister,
) -> ProgramRegisterResult {
    harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", mail))])
        .expect("register sequence")
        .reply::<ProgramRegisterResult>(label)
        .expect("decode ProgramRegisterResult")
}

fn register_err(harness: &mut SubstrateHarness, label: &'static str, mail: &ProgramRegister) -> String {
    match register_reply(harness, label, mail) {
        ProgramRegisterResult::Err { reason } => reason,
        ProgramRegisterResult::Ok { program_id } => panic!("register ({label}) must reject; got program {program_id}"),
    }
}

/// An overlay draw of `texture_id` as a 32x32 screen rect at (16, 8) —
/// how the pixel scenarios observe a program's output texture through
/// the existing sampling path.
fn output_overlay(texture_id: u32) -> DrawTexturedQuads {
    DrawTexturedQuads {
        texture_id,
        blend: QuadBlend::Straight,
        space: QuadSpace::Screen,
        clip: None,
        layer: 0,
        quads: vec![TexturedQuad {
            x: 16.0,
            y: 8.0,
            width: 32.0,
            height: 32.0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    }
}

/// ADR-0170 register validation: bad WGSL, a slot read before any pass
/// writes it, and a uniform window shorter than the shader's declared
/// block each reply their own distinguishable `Err` reason — and a
/// rejected register consumes no id, so the first accepted program is
/// id 0. The named bugs: the validation classes collapsing into one
/// opaque reason (callers triage a rejected program by class), an
/// invalid graph slipping through to panic the executor at record, and
/// a rejected register burning an id so accepted ids stop being dense.
#[test]
fn register_validation_classes_reply_distinguishable_errors() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let bad_wgsl = register_err(
        &mut harness,
        "bad_wgsl",
        &ProgramRegister { wgsl: "not wgsl at all".to_owned(), ..ping_pong_register() },
    );
    assert!(bad_wgsl.starts_with("invalid wgsl:"), "naga class must be named; got: {bad_wgsl}");

    let read_before_write = register_err(
        &mut harness,
        "read_before_write",
        &ProgramRegister {
            passes: vec![pass(
                "fs_invert",
                vec![InputSlot::Transient { index: 0 }],
                OutputSlot::Binding { index: 1 },
                0,
                4,
            )],
            ..ping_pong_register()
        },
    );
    assert!(
        read_before_write.contains("before any earlier pass writes it"),
        "sequence class must be named; got: {read_before_write}",
    );

    let short_window = register_err(
        &mut harness,
        "short_window",
        &ProgramRegister {
            passes: vec![pass(
                "fs_invert",
                vec![InputSlot::Binding { index: 0 }],
                OutputSlot::Binding { index: 1 },
                0,
                2,
            )],
            ..ping_pong_register()
        },
    );
    assert!(short_window.contains("uniform window"), "window class must be named; got: {short_window}");

    match register_reply(&mut harness, "accepted", &ping_pong_register()) {
        ProgramRegisterResult::Ok { program_id } => {
            assert_eq!(program_id, 0, "rejected registers must not consume ids");
        }
        ProgramRegisterResult::Err { reason } => panic!("the valid ping-pong program must register: {reason}"),
    }
}

/// ADR-0170 execution: a two-pass threshold-then-invert program over an
/// uploaded 2x2 source produces exactly the expected pixels in its
/// writable output texture, observed by sampling that output through
/// the overlay draw path in the same captured frame. Source red values
/// [0.2, 0.8 / 0.4, 0.9] under threshold 0.5 then invert-from-1.0 leave
/// the left texel column white and the right column black. The named
/// bugs: program passes recorded after the sampling passes (the capture
/// would show the cleared transparent output, both probes at
/// background), the ping-pong transient misrouted (pass 1 sampling the
/// source instead of pass 0's output), the two uniform windows
/// mis-sliced or unaligned (swapped windows threshold at 1.0 and invert
/// from 0.5, turning both columns mid-gray; an offset-4 window bound
/// raw would trip wgpu's 256-byte dynamic-offset alignment and lose the
/// dispatch), and a `PassOutput` alias resolving to the wrong slot.
#[test]
fn ping_pong_program_writes_expected_pixels_into_output() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    // Red channel per texel: (0,0)=0.2, (1,0)=0.8, (0,1)=0.4, (1,1)=0.9.
    let source_pixels: Vec<u8> = [51u8, 204, 102, 230].iter().flat_map(|&red| [red, 0, 0, 255]).collect();
    let source_id = create_texture(
        &mut harness,
        "create_source",
        &CreateTexture {
            width: 2,
            height: 2,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
            pixels: source_pixels,
        },
    );
    let output_id = create_texture(
        &mut harness,
        "create_output",
        &CreateTexture {
            width: 2,
            height: 2,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        },
    );
    let program_id = match register_reply(&mut harness, "register", &ping_pong_register()) {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register failed: {reason}"),
    };

    // Blob bytes 0..4: threshold 0.5 (pass 0's window). Bytes 4..8:
    // invert base 1.0 (pass 1's window, deliberately not aligned to any
    // uniform-offset boundary).
    let uniforms: Vec<u8> = [0.5f32, 1.0].iter().flat_map(|value| value.to_le_bytes()).collect();
    let pre = vec![
        envelope(
            "aether.render",
            &ProgramDispatch { program_id, bindings: vec![source_id, output_id], geometries: Vec::new(), uniforms },
        ),
        envelope("aether.render", &output_overlay(output_id)),
    ];
    let captured =
        harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture program output");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode program capture png");

    // The 2x2 output stretches over the 32x32 quad at (16, 8): texel
    // column centers land at x=24 and x=40; rows are identical per
    // column, so y=24 probes both.
    let white = rgba_at(&img, 24, 24);
    let black = rgba_at(&img, 40, 24);
    assert!(
        white[0] >= 200 && white[1] >= 200 && white[2] >= 200,
        "threshold(0.2|0.4 < 0.5) -> 0, invert -> 1: the left texel column must be white; got {white:?}",
    );
    assert!(
        black[0] <= 30 && black[1] <= 30 && black[2] <= 30,
        "threshold(0.8|0.9 >= 0.5) -> 1, invert -> 0: the right texel column must be black; got {black:?}",
    );
}

/// ADR-0170 runtime mismatch: a dispatch whose binding disagrees with
/// the registered graph (an R8 texture where the graph declares Rgba8)
/// warn-drops the dispatch and the frame survives — the capture still
/// succeeds, the control quad still draws, and the program's output
/// texture keeps its cleared transparent content. The named bug: the
/// mismatch reaching wgpu instead of the record-time guard — binding an
/// R8 view to an Rgba8-declared pipeline is a device validation error
/// that would poison the whole frame (no capture, no control quad)
/// rather than dropping one dispatch.
#[test]
fn mismatched_binding_dispatch_drops_and_frame_survives() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let wrong_format_id = create_texture(
        &mut harness,
        "create_r8",
        &CreateTexture {
            width: 2,
            height: 2,
            format: TextureFormat::R8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
            pixels: vec![255u8; 4],
        },
    );
    let output_id = create_texture(
        &mut harness,
        "create_output",
        &CreateTexture {
            width: 2,
            height: 2,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        },
    );
    let program_id = match register_reply(&mut harness, "register", &ping_pong_register()) {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register failed: {reason}"),
    };

    let uniforms: Vec<u8> = [0.5f32, 1.0].iter().flat_map(|value| value.to_le_bytes()).collect();
    let pre = vec![
        envelope(
            "aether.render",
            &ProgramDispatch {
                program_id,
                bindings: vec![wrong_format_id, output_id],
                geometries: Vec::new(),
                uniforms,
            },
        ),
        envelope("aether.render", &output_overlay(output_id)),
        envelope(
            "aether.render",
            &DrawSolidQuads {
                space: QuadSpace::Screen,
                clip: None,
                layer: 0,
                quads: vec![SolidQuad {
                    x: 2.0,
                    y: 2.0,
                    width: 5.0,
                    height: 5.0,
                    color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                }],
            },
        ),
    ];
    let captured = harness
        .execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))])
        .expect("capture must survive the dropped dispatch");
    let img = decode_png(captured.captured("snap").expect("snap step ran")).expect("decode surviving capture png");
    let bg = background_top_left(&img);
    let tolerance = 5;

    assert!(pixel_is_lit(&img, 4, 4, bg, tolerance), "the control quad must draw — the frame's passes ran");
    for (x, y) in [(24u32, 24u32), (40, 24)] {
        let probe = rgba_at(&img, x, y);
        assert!(
            probe[0].abs_diff(bg[0]) <= tolerance
                && probe[1].abs_diff(bg[1]) <= tolerance
                && probe[2].abs_diff(bg[2]) <= tolerance,
            "the dropped dispatch must leave the output texture cleared, so the probe at ({x}, {y}) stays \
             background; bg={bg:?} probe={probe:?}",
        );
    }
}

/// Cache invalidation for the executor's per-pass setup (issue #4431):
/// a program's bind groups and texture views are built once and reused
/// across dispatches, so the two ways a binding's content can change
/// underneath them both have to reach the pixels.
///
/// The named bugs, both of which fail silently as a frame that renders
/// the *previous* dispatch's content: an `update_texture` whose fresh
/// bytes never reach the frame because the cached entry is treated as
/// the content rather than as a handle to it (the re-upload lands in
/// the same GPU texture the cached view names, so a cache that skips
/// realization on a hit shows the stale pixels); and a rebind of a
/// different texture id into the same binding slot that reuses the
/// bind group built for the old id, so the dispatch samples a texture
/// it was never handed.
#[test]
fn cached_pass_setup_follows_an_updated_texture_and_a_rebind() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    // Red per texel, left column dark and right column bright: under
    // threshold 0.5 then invert-from-1.0 the left column reads white.
    let dark_left: Vec<u8> = [51u8, 204, 102, 230].iter().flat_map(|&red| [red, 0, 0, 255]).collect();
    let bright_left: Vec<u8> = [204u8, 51, 230, 102].iter().flat_map(|&red| [red, 0, 0, 255]).collect();

    let source_id = create_2x2(&mut harness, "create_source", dark_left.clone());
    let output_id = create_2x2(&mut harness, "create_output", Vec::new());
    let program_id = match register_reply(&mut harness, "register", &ping_pong_register()) {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register failed: {reason}"),
    };

    let uniforms: Vec<u8> = [0.5f32, 1.0].iter().flat_map(|value| value.to_le_bytes()).collect();
    let dispatch = |source: u32| ProgramDispatch {
        program_id,
        bindings: vec![source, output_id],
        geometries: Vec::new(),
        uniforms: uniforms.clone(),
    };

    // The 2x2 output stretches over the 32x32 quad at (16, 8), so the
    // texel column centers land at x=24 and x=40 and y=24 probes both.
    // Which column is the brighter one is the whole signal here, so the
    // probes are compared against each other rather than against
    // absolute levels the sampling path would have to keep promising.
    let columns = |harness: &mut SubstrateHarness, label: &'static str, pre: Vec<_>| {
        let captured =
            harness.execute(vec![(label, HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture program output");
        let img = decode_png(captured.captured(label).expect("capture step ran")).expect("decode program capture png");
        (i32::from(rgba_at(&img, 24, 24)[0]), i32::from(rgba_at(&img, 40, 24)[0]))
    };
    // Well past any filtering or transfer wobble, well inside a real flip.
    let separation = 100;

    // First dispatch: nothing is cached yet, so this is the reference —
    // the dark source column thresholds low and inverts to a bright one.
    let (left, right) = columns(
        &mut harness,
        "first",
        vec![envelope("aether.render", &dispatch(source_id)), envelope("aether.render", &output_overlay(output_id))],
    );
    assert!(
        left - right > separation,
        "the reference dispatch must read a bright left column over a dark right one; got {left}|{right}",
    );

    // Second dispatch: same bindings, so every cached bind group is
    // reused — but `update_texture` re-uploaded into the texture the
    // cached view names, so the columns must swap.
    let (updated_left, updated_right) = columns(
        &mut harness,
        "updated",
        vec![
            envelope(
                "aether.render",
                &aether_render::UpdateTexture {
                    texture_id: source_id,
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                    pixels: bright_left,
                },
            ),
            envelope("aether.render", &dispatch(source_id)),
            envelope("aether.render", &output_overlay(output_id)),
        ],
    );
    assert!(
        updated_right - updated_left > separation,
        "an update_texture under a cached bind group must reach the frame, flipping which column is bright; \
         got {updated_left}|{updated_right}",
    );

    // Third dispatch: a different texture id in binding 0, which is the
    // cache key's own business — the input bind group must be rebuilt
    // against the new id rather than reused from the old one.
    let rebound_id = create_2x2(&mut harness, "create_rebound", dark_left);
    let (rebound_left, rebound_right) = columns(
        &mut harness,
        "rebound",
        vec![envelope("aether.render", &dispatch(rebound_id)), envelope("aether.render", &output_overlay(output_id))],
    );
    assert!(
        rebound_left - rebound_right > separation,
        "rebinding a new texture id must rebuild the pass's input bind group, restoring the reference \
         columns; got {rebound_left}|{rebound_right}",
    );
}
