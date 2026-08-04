//! GPU error-isolation scenarios (issue #4424): an actor-authored
//! request that exceeds a device limit or the executor's per-dispatch
//! budget must be refused where it is declared, with the renderer still
//! drawing afterwards. Each scenario here reproduced a real panic before
//! the guards existed — an oversized `create_texture` died inside
//! `Device::create_texture` at realization, and an over-budget program
//! died on the `u32` staging-offset cast in `encode_passes` — so what
//! they pin is that the rejection happens at mail time and the frame
//! after it still renders.
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
use aether_harness_substrate_capture::test_helpers::{envelope, has_wgpu_adapter, rgba_at};
use aether_harness_substrate_capture::visual::decode_png;
use aether_kinds::QuadSpace;
use aether_math::Rgba;
use aether_render::QuadBlend;
use aether_render::{
    CreateTexture, CreateTextureResult, DestroyTexture, DrawSolidQuads, DrawTexturedQuads, InputSlot, OutputSlot,
    PassRepeat, PassStage, ProgramPass, ProgramRegister, ProgramRegisterResult, SlotExtent, SlotSpec, SolidQuad,
    TextureFormat, TextureSampling, TextureUsage, TexturedQuad,
};

/// Skip (or panic under `AETHER_REQUIRE_RUNTIME`) when no wgpu adapter
/// is available — every scenario here boots a real device.
fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

const MODULE: &str = r"
struct WindowParams { value: f32 }
@group(0) @binding(0) var<uniform> window_params: WindowParams;
@group(1) @binding(0) var source_texture: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

@fragment
fn fs_copy(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, uv) * window_params.value;
}
";

fn full(format: TextureFormat) -> SlotSpec {
    SlotSpec { format, extent: SlotExtent::Full }
}

fn create_reply(harness: &mut SubstrateHarness, label: &'static str, mail: &CreateTexture) -> CreateTextureResult {
    harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", mail))])
        .expect("create_texture sequence")
        .reply::<CreateTextureResult>(label)
        .expect("decode CreateTextureResult")
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

/// A magenta solid quad covering (8, 8)-(40, 40): the control draw every
/// scenario captures to prove the renderer is still alive after the
/// rejected mail.
fn control_quad() -> DrawSolidQuads {
    DrawSolidQuads {
        space: QuadSpace::Screen,
        clip: None,
        quads: vec![SolidQuad { x: 8.0, y: 8.0, width: 32.0, height: 32.0, color: Rgba::new(1.0, 0.0, 1.0, 1.0) }],
    }
}

/// Capture a frame containing the control quad and assert it drew — the
/// renderer survived whatever the scenario just sent it.
fn assert_renderer_alive(harness: &mut SubstrateHarness, label: &'static str) {
    let captured = harness
        .execute(vec![(label, HarnessOp::capture_with_mails(vec![envelope("aether.render", &control_quad())], vec![]))])
        .expect("the frame after a rejected mail must still record");
    let img = decode_png(captured.captured(label).expect("capture step ran")).expect("decode control capture png");
    let pixel = rgba_at(&img, 24, 24);
    assert!(
        pixel[0] >= 200 && pixel[1] <= 60 && pixel[2] >= 200,
        "the control quad must still draw after the rejected mail; got {pixel:?}",
    );
}

/// A texture past `max_texture_dimension_2d` must be refused by
/// `create_texture` rather than accepted and realized. Before the limit
/// check this replied `Ok`, and the renderer then panicked inside
/// `Device::create_texture` ("Dimension X value 16384 exceeds the limit
/// of 8192") the first time anything drew with the id — one actor mail
/// taking down every actor's picture. The writable case matters
/// separately because it carries no pixels, so the mail that kills the
/// renderer is a few dozen bytes.
#[test]
fn oversized_texture_create_rejects_and_the_renderer_survives() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let sampled = create_reply(
        &mut harness,
        "oversized_sampled",
        &CreateTexture {
            width: 16_384,
            height: 1,
            format: TextureFormat::R8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
            pixels: vec![255u8; 16_384],
        },
    );
    assert!(matches!(sampled, CreateTextureResult::Err { .. }), "an oversized sampled create must reject: {sampled:?}");

    let writable = create_reply(
        &mut harness,
        "oversized_writable",
        &CreateTexture {
            width: 20_000,
            height: 20_000,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        },
    );
    assert!(
        matches!(writable, CreateTextureResult::Err { .. }),
        "an oversized writable create must reject: {writable:?}",
    );

    let accepted = create_reply(
        &mut harness,
        "accepted",
        &CreateTexture {
            width: 2,
            height: 2,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
            pixels: vec![255u8; 16],
        },
    );
    assert!(
        matches!(accepted, CreateTextureResult::Ok { texture_id: 0 }),
        "a rejected create must consume no id, so the first accepted texture is id 0: {accepted:?}",
    );

    assert_renderer_alive(&mut harness, "after_oversized_texture");
}

/// A graph whose declared cost exceeds the executor's per-dispatch
/// budget must be refused at register. `MAX_REPEAT_COUNT` bounds one
/// pass entry, which left the product across entries unbounded: 24
/// passes at 4096 repeats over a 64 KiB window registered fine, and the
/// dispatch — a 64 KiB mail — then staged gigabytes and panicked the
/// driver thread on the `u32` staging-offset cast in `encode_passes`.
/// Rejecting at register is what keeps the cost refusable at all, since
/// by record time the encode is already underway.
#[test]
fn oversized_encode_budget_rejects_and_the_renderer_survives() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let over_budget_passes: Vec<ProgramPass> = (0..24)
        .map(|index| ProgramPass {
            stage: PassStage::Fragment,
            entry_point: "fs_copy".to_owned(),
            inputs: vec![InputSlot::Binding { index: 0 }],
            output: if index == 23 {
                OutputSlot::Binding { index: 1 }
            } else {
                OutputSlot::Transient { index: 0 }
            },
            uniform_offset: 0,
            uniform_length: 65_536,
            repeat: Some(PassRepeat { count: 4096, uniform_stride: 0 }),
        })
        .collect();

    let rejected = register_reply(
        &mut harness,
        "over_budget",
        &ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![full(TextureFormat::Rgba8), full(TextureFormat::Rgba8)],
            transients: vec![full(TextureFormat::Rgba8)],
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: over_budget_passes,
        },
    );
    assert!(
        matches!(rejected, ProgramRegisterResult::Err { .. }),
        "an over-budget graph must be refused at register: {rejected:?}",
    );

    let accepted = register_reply(
        &mut harness,
        "in_budget",
        &ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![full(TextureFormat::Rgba8), full(TextureFormat::Rgba8)],
            transients: Vec::new(),
            depth_transients: Vec::new(),
            geometries: Vec::new(),
            passes: vec![ProgramPass {
                stage: PassStage::Fragment,
                entry_point: "fs_copy".to_owned(),
                inputs: vec![InputSlot::Binding { index: 0 }],
                output: OutputSlot::Binding { index: 1 },
                uniform_offset: 0,
                uniform_length: 4,
                // The named consumer's shape: one maximally-repeated
                // pass over a small window must still register.
                repeat: Some(PassRepeat { count: 4096, uniform_stride: 0 }),
            }],
        },
    );
    assert!(
        matches!(accepted, ProgramRegisterResult::Ok { program_id: 0 }),
        "a rejected register must consume no id, and a wash-shaped chain must still register: {accepted:?}",
    );

    assert_renderer_alive(&mut harness, "after_over_budget_register");
}

/// The session-scoped id schemes hand out ids monotonically and never
/// recycle a destroyed one, which is what makes a stale id
/// distinguishable from a live one without generational tagging: a
/// destroyed id matches no entry, so it warn-drops, and the id a later
/// create returns is a fresh one that no held reference can collide
/// with. The bugs pinned: a registry that refills a destroyed id's slot
/// (a stale reference would silently address a different actor's
/// texture), and a stale id reaching the record path instead of dropping
/// its batch.
#[test]
fn destroyed_texture_ids_drop_cleanly_and_are_never_reissued() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let opaque = CreateTexture {
        width: 2,
        height: 2,
        format: TextureFormat::Rgba8,
        sampling: TextureSampling::Linear,
        usage: TextureUsage::Sampled,
        pixels: vec![255u8; 16],
    };
    let CreateTextureResult::Ok { texture_id: first } = create_reply(&mut harness, "first", &opaque) else {
        panic!("the first create must be accepted");
    };

    harness
        .execute(vec![("destroy", HarnessOp::send_and_settle("aether.render", &DestroyTexture { texture_id: first }))])
        .expect("destroy sequence");

    // Drawing with the destroyed id must drop the batch, not fault the
    // frame: the control quad in the same capture still draws.
    let stale_draw = DrawTexturedQuads {
        texture_id: first,
        blend: QuadBlend::Straight,
        space: QuadSpace::Screen,
        clip: None,
        quads: vec![TexturedQuad {
            x: 0.0,
            y: 0.0,
            width: 8.0,
            height: 8.0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    };
    let captured = harness
        .execute(vec![(
            "stale",
            HarnessOp::capture_with_mails(
                vec![envelope("aether.render", &stale_draw), envelope("aether.render", &control_quad())],
                vec![],
            ),
        )])
        .expect("a stale texture id must not fault the frame");
    let img = decode_png(captured.captured("stale").expect("capture step ran")).expect("decode stale capture png");
    let pixel = rgba_at(&img, 24, 24);
    assert!(
        pixel[0] >= 200 && pixel[1] <= 60 && pixel[2] >= 200,
        "the control quad must draw in the frame that referenced a destroyed texture; got {pixel:?}",
    );

    let CreateTextureResult::Ok { texture_id: second } = create_reply(&mut harness, "second", &opaque) else {
        panic!("the create after a destroy must be accepted");
    };
    assert_ne!(second, first, "a destroyed id must never be reissued — that reuse is what generational ids would fix");
}
