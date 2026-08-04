//! Per-pass GPU timing scenarios (iamacoffeepot/aether#4423): the
//! `aether.render.program.timings` reply driven end-to-end through an
//! in-process `SubstrateHarness` against a real device.
//!
//! What these pin is the instrument's contract rather than any
//! particular duration. A measured number is the device's, not this
//! crate's, and asserting on one would be asserting on the GPU; what
//! this crate owns is that the table describes the registered graph, and
//! that a device which cannot measure says so instead of reporting a
//! graph of free passes.
//!
//! The `Ok` arm is only reachable when the instrument is running, so
//! reaching it is itself the evidence the whole chain — bracket
//! placement, query resolve, buffer copy, asynchronous map, EWMA fold —
//! ran: a break anywhere in it leaves every row at the neutral seed, and
//! that is what the sample assertion catches.
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
use aether_harness_substrate_capture::test_helpers::has_wgpu_adapter;
use aether_render::{
    CreateTexture, CreateTextureResult, InputSlot, OutputSlot, PassStage, PassStageKind, ProgramDispatch, ProgramPass,
    ProgramRegister, ProgramRegisterResult, ProgramTimings, ProgramTimingsResult, SlotExtent, SlotSpec, TextureFormat,
    TextureSampling, TextureUsage,
};

/// Canvas the timed graph develops at. Small — the assertions are on
/// the table's shape, not on how long a pass takes.
const CANVAS: (u32, u32) = (64, 64);

/// The divisor the reduced pass declares, so the reply's `divisor` and
/// resolved extent have something other than the reference to report.
const REDUCED_DIVISOR: u32 = 4;

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
fn fs_reduce(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    let level = textureSample(source_texture, source_sampler, uv).r * window_params.value;
    return vec4<f32>(level, level, level, 1.0);
}

@fragment
fn fs_expand(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    let level = textureSample(source_texture, source_sampler, uv).r + window_params.value;
    return vec4<f32>(level, level, level, 1.0);
}
";

/// Binding 0 (source) reduced into a quarter-extent transient, then
/// expanded back into binding 1 (the writable output). Two passes, two
/// entry points, two different declared extents — so the reply has a
/// label, a stage, an extent and a divisor to get right on each row.
fn register() -> ProgramRegister {
    ProgramRegister {
        wgsl: MODULE.to_owned(),
        bindings: vec![
            SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full },
            SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full },
        ],
        transients: vec![SlotSpec {
            format: TextureFormat::Rgba8,
            extent: SlotExtent::Divided { divisor: REDUCED_DIVISOR },
        }],
        geometries: Vec::new(),
        depth_transients: Vec::new(),
        passes: vec![
            ProgramPass {
                stage: PassStage::Fragment,
                entry_point: "fs_reduce".to_owned(),
                inputs: vec![InputSlot::Binding { index: 0 }],
                output: OutputSlot::Transient { index: 0 },
                uniform_offset: 0,
                uniform_length: 4,
                repeat: None,
            },
            ProgramPass {
                stage: PassStage::Fragment,
                entry_point: "fs_expand".to_owned(),
                inputs: vec![InputSlot::Transient { index: 0 }],
                output: OutputSlot::Binding { index: 1 },
                uniform_offset: 4,
                uniform_length: 4,
                repeat: None,
            },
        ],
    }
}

fn texture(harness: &mut SubstrateHarness, label: &'static str, usage: TextureUsage) -> u32 {
    let pixels = match usage {
        TextureUsage::Writable => Vec::new(),
        TextureUsage::Sampled => vec![128u8; (CANVAS.0 * CANVAS.1 * 4) as usize],
    };
    let mail = CreateTexture {
        width: CANVAS.0,
        height: CANVAS.1,
        format: TextureFormat::Rgba8,
        sampling: TextureSampling::Nearest,
        usage,
        pixels,
    };
    let created = harness
        .execute(vec![(label, HarnessOp::send_and_await_reply("aether.render", &mail))])
        .expect("create_texture sequence");
    match created.reply::<CreateTextureResult>(label).expect("decode CreateTextureResult") {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture ({label}) failed: {error}"),
    }
}

/// Register the graph, dispatch it over several frames, and read the
/// timing table back.
fn timings_after_dispatches(harness: &mut SubstrateHarness) -> (u32, ProgramTimingsResult) {
    let registered = harness
        .execute(vec![("register", HarnessOp::send_and_await_reply("aether.render", &register()))])
        .expect("register sequence");
    let program_id = match registered.reply::<ProgramRegisterResult>("register").expect("decode ProgramRegisterResult")
    {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { reason } => panic!("register failed: {reason}"),
    };

    let dispatch = ProgramDispatch {
        program_id,
        bindings: vec![
            texture(harness, "source", TextureUsage::Sampled),
            texture(harness, "sink", TextureUsage::Writable),
        ],
        geometries: Vec::new(),
        uniforms: 0.5f32.to_le_bytes().into_iter().chain(0.25f32.to_le_bytes()).collect(),
    };

    // Several dispatch-bearing frames: the first frame's timestamps
    // resolve on a later frame's poll, so one dispatch would leave the
    // fold with nothing to have folded yet.
    for _ in 0..6 {
        harness
            .execute(vec![
                ("dispatch", HarnessOp::send_and_settle("aether.render", &dispatch)),
                ("settle", HarnessOp::advance(2)),
            ])
            .expect("dispatch frame");
    }

    let read = harness
        .execute(vec![("timings", HarnessOp::send_and_await_reply("aether.render", &ProgramTimings { program_id }))])
        .expect("timings sequence");
    let reply = read.reply::<ProgramTimingsResult>("timings").expect("decode ProgramTimingsResult");
    (program_id, reply)
}

/// With the instrument on, the reply either describes the whole graph or
/// says why it cannot — and the `Ok` arm is only reachable when the
/// instrument is running, so it must carry real samples.
#[test]
fn the_timing_table_describes_the_registered_graph_or_says_why_it_cannot() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness =
        SubstrateHarness::builder().size(CANVAS.0, CANVAS.1).with_render_pass_timings().build().expect("boot");
    let (_, reply) = timings_after_dispatches(&mut harness);

    let rows = match reply {
        // Never zeros: a device that cannot measure carries a reason,
        // and a caller can tell it from a graph of free passes.
        ProgramTimingsResult::Absent { reason } => {
            assert!(!reason.trim().is_empty(), "an absent timing surface must say why");
            eprintln!("per-pass gpu timings absent on this adapter: {reason}");
            return;
        }
        ProgramTimingsResult::Err { reason } => panic!("timings for a registered program failed: {reason}"),
        ProgramTimingsResult::Ok { rows, .. } => rows,
    };

    let declared = register().passes;
    assert_eq!(rows.len(), declared.len(), "the table must describe every declared pass");
    for (row, pass) in rows.iter().zip(&declared) {
        assert_eq!(row.label, pass.entry_point, "row {} names the wrong pass", row.pass);
        assert_eq!(row.stage, PassStageKind::Fragment, "row {} is not a draw pass", row.pass);
        assert_eq!(row.iterations, 1, "row {} declares no repeat", row.pass);
    }

    // The extents the next slice's merge-or-divide decision keys on: the
    // reduced pass must report its quarter extent and its divisor, not
    // the reference it was declared against.
    assert_eq!((rows[0].width, rows[0].height), (CANVAS.0 / REDUCED_DIVISOR, CANVAS.1 / REDUCED_DIVISOR));
    assert_eq!(rows[0].divisor, REDUCED_DIVISOR);
    assert_eq!((rows[1].width, rows[1].height), CANVAS);
    assert_eq!(rows[1].divisor, 1);

    // Tripwire: `Ok` is returned only while the instrument is running,
    // so every stage of the asynchronous chain must have completed for
    // this reply to exist at all. A break in any of them — an unplaced
    // timestamp, an unresolved query, a readback never mapped, a fold
    // that never finds its program — leaves every row at the neutral
    // seed, which is what this catches.
    assert!(
        rows.iter().any(|row| row.samples > 0 && row.mean_nanos > 0),
        "a running instrument must have folded a nonzero duration for some pass: {rows:?}",
    );
}

/// The `Err` arm is for a request that was wrong, not for a device that
/// cannot answer — so an unknown program id must not come back `Absent`
/// and be read as "unmeasurable".
#[test]
fn an_unknown_program_id_is_an_error_rather_than_an_absent_measurement() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness =
        SubstrateHarness::builder().size(CANVAS.0, CANVAS.1).with_render_pass_timings().build().expect("boot");
    // A frame has to have recorded for the instrument to have met the
    // device; before that the reply is legitimately `Absent`.
    let (program_id, reply) = timings_after_dispatches(&mut harness);
    if matches!(reply, ProgramTimingsResult::Absent { .. }) {
        eprintln!("skipping the error-arm assertion: the instrument is absent on this adapter");
        return;
    }

    let read = harness
        .execute(vec![(
            "unknown",
            HarnessOp::send_and_await_reply("aether.render", &ProgramTimings { program_id: program_id + 1000 }),
        )])
        .expect("timings sequence");
    match read.reply::<ProgramTimingsResult>("unknown").expect("decode ProgramTimingsResult") {
        ProgramTimingsResult::Err { reason } => {
            assert!(reason.contains("unknown program id"), "the error must name its class: {reason}");
        }
        other => panic!("an unregistered program id must be an error, got {other:?}"),
    }
}

/// A dispatch whose passes were never bracketed still reports the whole
/// graph: the instrument off is `Absent` with a reason, never a table.
#[test]
fn the_instrument_turned_off_is_absent_with_a_reason() {
    if !require_wgpu_only() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(CANVAS.0, CANVAS.1).with_render().build().expect("boot");
    let (_, reply) = timings_after_dispatches(&mut harness);
    match reply {
        ProgramTimingsResult::Absent { reason } => {
            assert!(
                reason.contains("disabled by configuration"),
                "the disabled instrument must say so rather than blaming the adapter: {reason}",
            );
        }
        other => panic!("the instrument off must report absent-with-reason, got {other:?}"),
    }
}
