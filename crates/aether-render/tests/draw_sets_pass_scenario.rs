//! Draw-sets pass scenarios (ADR-0246 decision 4): a program whose
//! `PassStage::DrawSets` pass draws the draw sets a dispatch lists,
//! driven end-to-end through an in-process `SubstrateHarness`.
//!
//! Every scenario draws one half-width quad placed by instance records:
//! a record carries an offset and a colour, so where a draw lands and
//! what colour it has say which instance buffer the pass read. Each
//! observes the program's writable output by drawing it through the
//! overlay path in the same captured frame; an output texel nothing drew
//! stays transparent and shows the frame's background.
//!
//! Skipped when no wgpu adapter is available (driverless runners);
//! `AETHER_REQUIRE_RUNTIME=1` (CI) flips the skip into a hard panic.

use aether_data::{Blob, Kind};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{
    envelope, pixel_is_lit, require_wgpu_adapter, rgb_close, rgba_at,
};
use aether_harness_substrate_capture::visual::{Image, background_top_left, decode_png};
use aether_kinds::QuadSpace;
use aether_math::Rgba;
use aether_render::{
    Blend, CreateDrawSet, CreateDrawSetResult, CreateGeometry, CreateGeometryResult, CreateInstances,
    CreateInstancesResult, CreateTexture, CreateTextureResult, Cull, DepthExtent, DepthSpec, DepthUse, DepthWrite,
    DestroyGeometry, DrawSetsPass, DrawSpec, DrawTexturedQuads, IndexRange, InstanceRange, Mips, OutputSlot, PassLoad,
    PassStage, ProgramDispatch, ProgramPass, ProgramRegister, ProgramRegisterResult, QuadBlend, RenderCapability,
    Samples, Sampling, SlotExtent, SlotShape, SlotSpec, TextureFormat, TextureSampling, TextureUsage, TexturedQuad,
    VertexAttribute, VertexFormat, Wrap,
};

/// Side of every program output texture and of the overlay quad that
/// reads it back, so one output texel is one frame pixel.
const OUTPUT_SIDE: u16 = 32;

/// Top edge of every readback quad in the 64x48 frame.
const QUAD_TOP: u16 = 8;

/// Output texels at the middle of the left and the right half.
const LEFT: (u32, u32) = (8, 16);
const RIGHT: (u32, u32) = (24, 16);

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

/// How far a captured channel may sit from the colour it shows.
const TOLERANCE: u8 = 30;

/// Counter-clockwise and clockwise index orders over the quad's four
/// corners.
const COUNTER_CLOCKWISE: [u32; 6] = [0, 1, 2, 0, 2, 3];
const CLOCKWISE: [u32; 6] = [0, 2, 1, 0, 3, 2];

/// The vertex stage adds a record's offset to each corner and hands its
/// colour on: location 0 comes from vertex buffer 0, locations 4 and 5
/// from the instance buffer at vertex buffer 1. `fs_nothing` returns no
/// colour: the fragment entry point of a depth-only pass.
const MODULE: &str = r"
struct Placed {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_placed(@location(0) corner: vec3<f32>, @location(4) offset: vec3<f32>, @location(5) color: vec4<f32>) -> Placed {
    return Placed(vec4<f32>(corner + offset, 1.0), color);
}

@fragment
fn fs_color(in: Placed) -> @location(0) vec4<f32> {
    return in.color;
}

@fragment
fn fs_nothing() {}
";

fn vertex_layout() -> Vec<VertexAttribute> {
    vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }]
}

/// An offset and a colour per instance: stride 16.
fn instance_layout() -> Vec<VertexAttribute> {
    vec![
        VertexAttribute { location: 4, format: VertexFormat::Float32x3 },
        VertexAttribute { location: 5, format: VertexFormat::Unorm8x4 },
    ]
}

/// One instance record: where the quad goes and what colour it is. An
/// `x` of 0 leaves the quad on the left half and 1 moves it to the
/// right half; `depth` is the clip-space depth it is drawn at.
#[derive(Copy, Clone)]
struct Record {
    x: f32,
    depth: f32,
    color: [u8; 4],
}

const fn left(color: [u8; 4]) -> Record {
    Record { x: 0.0, depth: 0.0, color }
}

const fn right(color: [u8; 4]) -> Record {
    Record { x: 1.0, depth: 0.0, color }
}

fn record_bytes(records: &[Record]) -> Vec<u8> {
    records
        .iter()
        .flat_map(|record| {
            let offset = [record.x, 0.0, record.depth].into_iter().flat_map(f32::to_le_bytes);
            offset.chain(record.color)
        })
        .collect()
}

/// Run one request and decode its reply as `R`. The request comes first
/// so a call site can build it from the harness it then lends.
fn reply<R: Kind>(request: HarnessOp, label: &'static str, harness: &mut SubstrateHarness) -> R {
    harness.execute(vec![(label, request)]).expect("request sequence").reply::<R>(label).expect("decode the reply")
}

/// A quad over the left half of clip space at depth 0, its triangles
/// wound by `indices`.
fn create_quad(harness: &mut SubstrateHarness, label: &'static str, indices: [u32; 6]) -> u32 {
    let corners: [[f32; 3]; 4] = [[-1.0, -1.0, 0.0], [0.0, -1.0, 0.0], [0.0, 1.0, 0.0], [-1.0, 1.0, 0.0]];
    let mail = CreateGeometry {
        layout: vertex_layout(),
        vertices: Blob::from(corners.iter().flatten().flat_map(|value| value.to_le_bytes()).collect::<Vec<u8>>()),
        indices: Blob::from(indices.iter().flat_map(|index| index.to_le_bytes()).collect::<Vec<u8>>()),
    };
    let created: CreateGeometryResult =
        reply(HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), &mail), label, harness);
    match created {
        CreateGeometryResult::Ok { geometry_id } => geometry_id,
        CreateGeometryResult::Err { error } => panic!("create_geometry ({label}) failed: {error}"),
    }
}

fn create_records(harness: &mut SubstrateHarness, label: &'static str, records: &[Record]) -> u32 {
    let mail = CreateInstances {
        layout: instance_layout(),
        capacity: u32::try_from(records.len()).expect("record count fits u32"),
        records: Blob::from(record_bytes(records)),
    };
    let created: CreateInstancesResult =
        reply(HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), &mail), label, harness);
    match created {
        CreateInstancesResult::Ok { instances_id } => instances_id,
        CreateInstancesResult::Err { error } => panic!("create_instances ({label}) failed: {error}"),
    }
}

/// A draw of the whole quad once per record of `records`, a
/// `(first, count)` run of `instances_id`.
fn draw(geometry_id: u32, instances_id: u32, records: (u32, u32)) -> DrawSpec {
    DrawSpec {
        geometry_id,
        indices: IndexRange { first: 0, count: 6 },
        instances_id,
        instances: InstanceRange { first: records.0, count: records.1 },
    }
}

fn create_set(harness: &mut SubstrateHarness, label: &'static str, draws: Vec<DrawSpec>) -> u32 {
    let mail = CreateDrawSet { vertex_layout: vertex_layout(), instance_layout: instance_layout(), draws };
    let created: CreateDrawSetResult =
        reply(HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), &mail), label, harness);
    match created {
        CreateDrawSetResult::Ok { draw_set_id } => draw_set_id,
        CreateDrawSetResult::Err { error } => panic!("create_draw_set ({label}) failed: {error}"),
    }
}

/// The writable `Rgba8` texture a program here draws into.
fn create_output(harness: &mut SubstrateHarness, label: &'static str) -> u32 {
    let mail = CreateTexture {
        width: u32::from(OUTPUT_SIDE),
        height: u32::from(OUTPUT_SIDE),
        format: TextureFormat::Rgba8,
        sampling: TextureSampling::Nearest,
        usage: TextureUsage::Writable,
        pixels: Blob::from(Vec::new()),
    };
    let created: CreateTextureResult =
        reply(HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), &mail), label, harness);
    match created {
        CreateTextureResult::Ok { texture_id } => texture_id,
        CreateTextureResult::Err { error } => panic!("create_texture ({label}) failed: {error}"),
    }
}

/// What one `DrawSets` pass of a scenario program varies. A
/// `depth_only` pass declares no output, `Blend::Replace` and the
/// fragment entry point that returns nothing; any other pass writes
/// binding 0 through `fs_color`.
struct PassShape {
    list: u32,
    cull: Cull,
    depth: Option<DepthWrite>,
    load: PassLoad,
    depth_only: bool,
}

/// A pass drawing list 0 over a cleared output, with no depth.
fn plain_pass(cull: Cull) -> PassShape {
    PassShape { list: 0, cull, depth: None, load: PassLoad::Clear, depth_only: false }
}

/// A depth-only pass drawing `list` into the depth slot.
fn depth_only_pass(list: u32) -> PassShape {
    PassShape { list, cull: Cull::None, depth: Some(DepthWrite::Write), load: PassLoad::Load, depth_only: true }
}

/// Register a program of `DrawSets` passes, with one full-extent depth
/// slot for the passes that name it.
fn register(harness: &mut SubstrateHarness, label: &'static str, passes: &[PassShape]) -> u32 {
    register_over(harness, label, DepthExtent::Output(SlotExtent::Full), passes)
}

/// Register a program of `DrawSets` passes whose one depth slot is
/// sized by `depth_extent`.
fn register_over(
    harness: &mut SubstrateHarness,
    label: &'static str,
    depth_extent: DepthExtent,
    passes: &[PassShape],
) -> u32 {
    let passes = passes
        .iter()
        .map(|shape| {
            let (blend, entry_point, output) = if shape.depth_only {
                (Blend::Replace, "fs_nothing", OutputSlot::None)
            } else {
                (Blend::Alpha, "fs_color", OutputSlot::Binding { index: 0 })
            };
            ProgramPass {
                stage: PassStage::DrawSets(DrawSetsPass {
                    vertex_entry_point: "vs_placed".to_owned(),
                    vertex_layout: vertex_layout(),
                    instance_layout: instance_layout(),
                    draw_sets: shape.list,
                    cull: shape.cull,
                    depth: shape.depth.map(|write| DepthUse { slot: 0, write }),
                    load: shape.load,
                }),
                blend,
                entry_point: entry_point.to_owned(),
                inputs: Vec::new(),
                output,
                uniform_offset: 0,
                uniform_length: 0,
                repeat: None,
            }
        })
        .collect();
    let mail = ProgramRegister {
        wgsl: MODULE.to_owned(),
        bindings: vec![SlotSpec {
            format: TextureFormat::Rgba8,
            shape: SlotShape::Target(SlotExtent::Full),
            sampling: Sampling::Filtered { wrap: Wrap::Clamp, mips: Mips::Base },
        }],
        transients: Vec::new(),
        geometries: Vec::new(),
        depth_transients: vec![DepthSpec { extent: depth_extent, samples: Samples::One }],
        passes,
    };
    let registered: ProgramRegisterResult =
        reply(HarnessOp::send_and_await_reply(&harness.actor_ref::<RenderCapability>(), &mail), label, harness);
    match registered {
        ProgramRegisterResult::Ok { program_id } => program_id,
        ProgramRegisterResult::Err { error } => panic!("register ({label}) failed: {error}"),
    }
}

fn dispatch(program_id: u32, output_id: u32, draw_sets: Vec<Vec<u32>>) -> ProgramDispatch {
    ProgramDispatch { program_id, bindings: vec![output_id], geometries: Vec::new(), draw_sets, uniforms: Vec::new() }
}

/// An overlay draw of a program's output as an `OUTPUT_SIDE` square
/// whose left edge is at `left_edge` and whose top edge is at
/// `QUAD_TOP`.
fn output_overlay(texture_id: u32, left_edge: u16) -> DrawTexturedQuads {
    DrawTexturedQuads {
        texture_id,
        blend: QuadBlend::Straight,
        space: QuadSpace::Screen,
        clip: None,
        quads: vec![TexturedQuad {
            x: f32::from(left_edge),
            y: f32::from(QUAD_TOP),
            width: f32::from(OUTPUT_SIDE),
            height: f32::from(OUTPUT_SIDE),
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::new(1.0, 1.0, 1.0, 1.0),
        }],
    }
}

/// Run `dispatches` in order, then capture a frame showing each of
/// `outputs` through an overlay at its left edge.
fn capture(harness: &mut SubstrateHarness, dispatches: &[ProgramDispatch], outputs: &[(u32, u16)]) -> Image {
    let program_mails = dispatches.iter().map(|dispatch| envelope("aether.render", dispatch));
    let overlay_mails =
        outputs.iter().map(|&(output_id, left_edge)| envelope("aether.render", &output_overlay(output_id, left_edge)));
    let pre = program_mails.chain(overlay_mails).collect();
    let captured = harness.execute(vec![("snap", HarnessOp::capture_with_mails(pre, vec![]))]).expect("capture frame");
    decode_png(captured.captured("snap").expect("snap step ran")).expect("decode capture png")
}

/// The frame pixel showing output texel `texel` of a quad at
/// `left_edge`.
fn frame_pixel(left_edge: u16, texel: (u32, u32)) -> (u32, u32) {
    (u32::from(left_edge) + texel.0, u32::from(QUAD_TOP) + texel.1)
}

fn shows(img: &Image, left_edge: u16, texel: (u32, u32), color: [u8; 4]) -> bool {
    let (x, y) = frame_pixel(left_edge, texel);
    rgb_close(rgba_at(img, x, y), [color[0], color[1], color[2]], TOLERANCE)
}

fn unlit(img: &Image, left_edge: u16, texel: (u32, u32)) -> bool {
    let (x, y) = frame_pixel(left_edge, texel);
    !pixel_is_lit(img, x, y, background_top_left(img), TOLERANCE)
}

/// Two sets share one geometry and name different instance buffers, one
/// placing the quad left in red and one right in green; both halves
/// show their colour. The named bug: the walk keeps its last-bound rows
/// across sets, and since each set's only draw names instance row 0 the
/// second set is drawn from the first set's records, leaving the right
/// half unlit.
#[test]
fn two_sets_sharing_a_geometry_each_draw_their_own_records() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let quad = create_quad(&mut harness, "create_quad", COUNTER_CLOCKWISE);
    let left_records = create_records(&mut harness, "create_left_records", &[left(RED)]);
    let right_records = create_records(&mut harness, "create_right_records", &[right(GREEN)]);
    let left_set = create_set(&mut harness, "create_left_set", vec![draw(quad, left_records, (0, 1))]);
    let right_set = create_set(&mut harness, "create_right_set", vec![draw(quad, right_records, (0, 1))]);
    let output = create_output(&mut harness, "create_output");
    let program = register(&mut harness, "register", &[plain_pass(Cull::None)]);

    let img = capture(&mut harness, &[dispatch(program, output, vec![vec![left_set, right_set]])], &[(output, 16)]);

    assert!(shows(&img, 16, LEFT, RED), "the first set's records place the quad left in red");
    assert!(shows(&img, 16, RIGHT, GREEN), "the second set's records place the quad right in green");
}

/// After a dispatch that drew the left set, a dispatch listing the right
/// set and an unknown id is dropped whole: the left half stays red and
/// the right half stays unlit. The named bug: the unknown id is found
/// while encoding, after the pass has opened and cleared the output, so
/// the first dispatch's work is wiped.
#[test]
fn a_dispatch_listing_an_unknown_set_records_nothing() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let quad = create_quad(&mut harness, "create_quad", COUNTER_CLOCKWISE);
    let records = create_records(&mut harness, "create_records", &[left(RED), right(GREEN)]);
    let left_set = create_set(&mut harness, "create_left_set", vec![draw(quad, records, (0, 1))]);
    let right_set = create_set(&mut harness, "create_right_set", vec![draw(quad, records, (1, 1))]);
    let output = create_output(&mut harness, "create_output");
    let program = register(&mut harness, "register", &[plain_pass(Cull::None)]);

    let dispatches =
        [dispatch(program, output, vec![vec![left_set]]), dispatch(program, output, vec![vec![right_set, 999]])];
    let img = capture(&mut harness, &dispatches, &[(output, 16)]);

    assert!(shows(&img, 16, LEFT, RED), "the dropped dispatch must not clear what the first one drew");
    assert!(unlit(&img, 16, RIGHT), "the dropped dispatch must not draw its known set either");
}

/// A geometry destroyed after its set is made is still drawn: the set
/// holds it. The named bug: the encode looks the geometry up among the
/// live entries and misses the retired store, which panics the frame or
/// draws nothing.
#[test]
fn a_set_draws_a_geometry_destroyed_under_it() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let quad = create_quad(&mut harness, "create_quad", COUNTER_CLOCKWISE);
    let records = create_records(&mut harness, "create_records", &[left(RED)]);
    let set = create_set(&mut harness, "create_set", vec![draw(quad, records, (0, 1))]);
    let output = create_output(&mut harness, "create_output");
    let program = register(&mut harness, "register", &[plain_pass(Cull::None)]);
    harness
        .execute(vec![(
            "destroy_geometry",
            HarnessOp::send_and_settle(
                &harness.actor_ref::<RenderCapability>(),
                &DestroyGeometry { geometry_id: quad },
            ),
        )])
        .expect("destroy_geometry settles");

    let img = capture(&mut harness, &[dispatch(program, output, vec![vec![set]])], &[(output, 16)]);

    assert!(shows(&img, 16, LEFT, RED), "the set still draws the geometry it holds");
}

/// A depth-writing pass draws a near quad on the left; a `TestOnly` pass
/// then draws a middle quad and a far quad over the whole output. The
/// left shows the near colour and the right the far colour. The named
/// bugs: `TestOnly` writing depth, which leaves the middle quad's depth
/// on the right and hides the far quad behind it; and `TestOnly` not
/// testing, which paints the far quad over the near one on the left.
#[test]
fn a_test_only_pass_tests_depth_and_leaves_it_as_it_was() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let quad = create_quad(&mut harness, "create_quad", COUNTER_CLOCKWISE);
    let near = create_records(&mut harness, "create_near", &[Record { depth: 0.2, ..left(RED) }]);
    let behind = create_records(
        &mut harness,
        "create_behind",
        &[
            Record { depth: 0.5, ..left(GREEN) },
            Record { depth: 0.5, ..right(GREEN) },
            Record { depth: 0.8, ..left(BLUE) },
            Record { depth: 0.8, ..right(BLUE) },
        ],
    );
    let near_set = create_set(&mut harness, "create_near_set", vec![draw(quad, near, (0, 1))]);
    let behind_set =
        create_set(&mut harness, "create_behind_set", vec![draw(quad, behind, (0, 2)), draw(quad, behind, (2, 2))]);
    let output = create_output(&mut harness, "create_output");
    let program = register(
        &mut harness,
        "register",
        &[
            PassShape {
                list: 0,
                cull: Cull::None,
                depth: Some(DepthWrite::Write),
                load: PassLoad::Clear,
                depth_only: false,
            },
            PassShape {
                list: 1,
                cull: Cull::None,
                depth: Some(DepthWrite::TestOnly),
                load: PassLoad::Load,
                depth_only: false,
            },
        ],
    );

    let img =
        capture(&mut harness, &[dispatch(program, output, vec![vec![near_set], vec![behind_set]])], &[(output, 16)]);

    assert!(shows(&img, 16, LEFT, RED), "both later quads are behind the near one and fail the test");
    assert!(shows(&img, 16, RIGHT, BLUE), "the middle quad left no depth for the far one to fail against");
}

/// A depth-only pass draws a near quad on the left into the depth slot
/// and no colour anywhere; a colour pass attaching the same slot
/// `TestOnly` then draws a farther quad on both halves. The left is
/// unlit and the right shows the far colour (ADR-0246 decision 9). The
/// named bugs: the depth-only pass recording nothing or not writing
/// depth, which shows the far colour on the left; and the depth-only
/// pass raising a GPU error that drops the rest of the dispatch, which
/// leaves the right unlit.
#[test]
fn a_depth_only_pass_hides_what_a_later_pass_draws_behind_it() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let quad = create_quad(&mut harness, "create_quad", COUNTER_CLOCKWISE);
    let near = create_records(&mut harness, "create_near", &[Record { depth: 0.2, ..left(RED) }]);
    let far = create_records(
        &mut harness,
        "create_far",
        &[Record { depth: 0.8, ..left(BLUE) }, Record { depth: 0.8, ..right(BLUE) }],
    );
    let near_set = create_set(&mut harness, "create_near_set", vec![draw(quad, near, (0, 1))]);
    let far_set = create_set(&mut harness, "create_far_set", vec![draw(quad, far, (0, 2))]);
    let output = create_output(&mut harness, "create_output");
    let color_pass = PassShape {
        list: 1,
        cull: Cull::None,
        depth: Some(DepthWrite::TestOnly),
        load: PassLoad::Clear,
        depth_only: false,
    };
    let program = register(&mut harness, "register", &[depth_only_pass(0), color_pass]);

    let img = capture(&mut harness, &[dispatch(program, output, vec![vec![near_set], vec![far_set]])], &[(output, 16)]);

    assert!(unlit(&img, 16, LEFT), "the depth-only pass drew no colour, and its depth hides the far quad");
    assert!(shows(&img, 16, RIGHT, BLUE), "nothing was drawn in front of the far quad on the right");
}

/// A depth-only pass draws into a `Fixed` depth slot twice the output's
/// side, and a colour pass with no depth then draws a quad on the left;
/// the quad shows. The named bug: a pass with no colour attachment
/// whose one attachment is not the output's size failing when it is
/// encoded, which drops the rest of the dispatch and leaves the output
/// empty.
#[test]
fn a_depth_only_pass_draws_into_a_fixed_slot_larger_than_the_output() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let quad = create_quad(&mut harness, "create_quad", COUNTER_CLOCKWISE);
    let records = create_records(&mut harness, "create_records", &[left(RED)]);
    let set = create_set(&mut harness, "create_set", vec![draw(quad, records, (0, 1))]);
    let output = create_output(&mut harness, "create_output");
    let shadow_map = DepthExtent::Fixed { side: u32::from(OUTPUT_SIDE) * 2 };
    let color_pass = PassShape { list: 1, ..plain_pass(Cull::None) };
    let program = register_over(&mut harness, "register", shadow_map, &[depth_only_pass(0), color_pass]);

    let img = capture(&mut harness, &[dispatch(program, output, vec![vec![set], vec![set]])], &[(output, 16)]);

    assert!(shows(&img, 16, LEFT, RED), "the colour pass after the depth-only pass must still draw");
}

/// One set draws a clockwise quad on the left and a counter-clockwise
/// one on the right. Under `Cull::Back` the left is unlit and the right
/// drawn; under `Cull::None` both are drawn. The named bug: the cull
/// declaration never reaching the pipeline, so both programs draw both.
#[test]
fn back_culling_discards_clockwise_triangles() {
    if !require_wgpu_adapter() {
        return;
    }
    let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build().expect("boot");

    let clockwise = create_quad(&mut harness, "create_clockwise", CLOCKWISE);
    let counter_clockwise = create_quad(&mut harness, "create_counter_clockwise", COUNTER_CLOCKWISE);
    let records = create_records(&mut harness, "create_records", &[left(RED), right(GREEN)]);
    let set = create_set(
        &mut harness,
        "create_set",
        vec![draw(clockwise, records, (0, 1)), draw(counter_clockwise, records, (1, 1))],
    );
    let culled_output = create_output(&mut harness, "create_culled_output");
    let plain_output = create_output(&mut harness, "create_plain_output");
    let culled_program = register(&mut harness, "register_culled", &[plain_pass(Cull::Back)]);
    let plain_program = register(&mut harness, "register_plain", &[plain_pass(Cull::None)]);

    let (culled_left, plain_left) = (0, 32);
    let dispatches = [
        dispatch(culled_program, culled_output, vec![vec![set]]),
        dispatch(plain_program, plain_output, vec![vec![set]]),
    ];
    let img = capture(&mut harness, &dispatches, &[(culled_output, culled_left), (plain_output, plain_left)]);

    assert!(unlit(&img, culled_left, LEFT), "Cull::Back must discard the clockwise quad");
    assert!(shows(&img, culled_left, RIGHT, GREEN), "Cull::Back must keep the counter-clockwise quad");
    assert!(shows(&img, plain_left, LEFT, RED), "Cull::None must draw the clockwise quad");
    assert!(shows(&img, plain_left, RIGHT, GREEN), "Cull::None must draw the counter-clockwise quad");
}
