// The pigment ops as authored passes (iamacoffeepot/aether#4367):
// granulation, sag, spatter and the flow smear as fragment entry points
// over R32Float density planes, each a texel-exact port of its CPU
// oracle in easel/field.rs and easel/image.rs. Every constant below
// mirrors the private field.rs / image.rs constant it is named after —
// the parity scenarios in tests/program_pigment_scenario.rs hold each
// pair of implementations together.
//
// Shared conventions (ADR-0170): a pass's uniform window binds at
// @group(0) @binding(0); its input planes bind in declaration order at
// group 1 as texture / sampler pairs. The samplers go undeclared — every
// op reads exact texels through textureLoad, never filtered samples,
// because the oracles index integer pixels. The fragment's texel is
// recovered from @builtin(position): the fragment centre sits at
// (x + 0.5, y + 0.5), so truncation yields the integer texel the CPU
// loop indexes. Duplicate binding numbers across ops are fine — naga
// checks collisions per entry point, and no entry point touches another
// op's globals.

// Granulation (field.rs `Sheet::granulate`). Density under the floor is
// not worth granulating; above it the pigment is modulated about the
// tooth pivot — lifted where the grain sits below it, settled where it
// rises above. Mirrors field.rs GRANULATION_FLOOR / _AUTHORITY / _PIVOT.
const GRANULATION_FLOOR: f32 = 0.003;
const GRANULATION_AUTHORITY: f32 = 0.85;
const GRANULATION_PIVOT: f32 = 0.18;

struct GranulateParams {
    // How strongly the pigment settles into the tooth — `WashParams::gran`.
    gran: f32,
}

@group(0) @binding(0) var<uniform> granulate_params: GranulateParams;
@group(1) @binding(0) var granulate_density: texture_2d<f32>;
@group(1) @binding(2) var granulate_tooth: texture_2d<f32>;

@fragment
fn fs_granulate(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let pixel = vec2<i32>(position.xy);
    var settled = textureLoad(granulate_density, pixel, 0).r;

    if settled > GRANULATION_FLOOR {
        let grain = textureLoad(granulate_tooth, pixel, 0).r;
        settled = settled * (1.0 - granulate_params.gran * GRANULATION_AUTHORITY * (grain - GRANULATION_PIVOT));
    }
    return vec4<f32>(settled, 0.0, 0.0, 1.0);
}

// Sag (field.rs `sagged`): two samples from above, each weaker than the
// last, taken at their strongest so the wash grows downward and never
// erases what it passes. The carry weights mirror field.rs SAG_FALLOFF;
// the step arrives pre-tuned in texels (`SagUniforms::for_canvas`).
const SAG_FALLOFF_NEAR: f32 = 0.8;
const SAG_FALLOFF_FAR: f32 = 0.55;

struct SagParams {
    // Spacing of the downhill samples in whole texels, at least one.
    step_texels: u32,
}

@group(0) @binding(0) var<uniform> sag_params: SagParams;
@group(1) @binding(0) var sag_soft: texture_2d<f32>;

@fragment
fn fs_sag(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let pixel = vec2<i32>(position.xy);
    var dragged = textureLoad(sag_soft, pixel, 0).r;

    // The oracle takes each sample only where a full step exists above —
    // rows shy of the border keep their own value rather than clamping
    // the read toward row zero.
    let spacing = i32(sag_params.step_texels);
    if pixel.y >= spacing {
        dragged = max(dragged, textureLoad(sag_soft, vec2<i32>(pixel.x, pixel.y - spacing), 0).r * SAG_FALLOFF_NEAR);
    }
    if pixel.y >= 2 * spacing {
        dragged = max(dragged, textureLoad(sag_soft, vec2<i32>(pixel.x, pixel.y - 2 * spacing), 0).r * SAG_FALLOFF_FAR);
    }
    return vec4<f32>(dragged, 0.0, 0.0, 1.0);
}

// Spatter (field.rs `Sheet::spatter`): the pre-rolled drops (#4372's
// `WashAccidents` list) stamped from the uniform blob, each a linear
// falloff disc around where the throw lands — further down the sheet
// than across it by the droop, mirroring field.rs SPATTER_DROOP. The
// bounded per-fragment loop over the drop list trades a handful of
// distance checks per texel for having no scatter pass at all; the list
// is a few dozen drops.
const SPATTER_DROOP: f32 = 1.25;

// Ceiling on one stamp's drop list; mirrored by the Rust
// MAX_SPATTER_DROPS the encoder asserts against.
const MAX_SPATTER_DROPS: u32 = 64u;

struct SpatterParams {
    // The region centroid the drops are thrown about, in texels.
    centre: vec2<f32>,
    // Live entries at the head of `drops`.
    drop_count: u32,
    padding: u32,
    // One drop per element: bearing (radians, wrapped into [-pi, pi]
    // where cos/sin carry their specified accuracy), throw, radius and
    // strength — `DropAccident` in field.rs order.
    drops: array<vec4<f32>, MAX_SPATTER_DROPS>,
}

@group(0) @binding(0) var<uniform> spatter_params: SpatterParams;
@group(1) @binding(0) var spatter_density: texture_2d<f32>;

@fragment
fn fs_spatter(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let pixel = vec2<i32>(position.xy);
    // The oracle measures reach from the integer pixel indices as f32s.
    let at = floor(position.xy);
    var stained = textureLoad(spatter_density, pixel, 0).r;

    for (var drop = 0u; drop < spatter_params.drop_count; drop = drop + 1u) {
        let rolled = spatter_params.drops[drop];
        let landed = spatter_params.centre
            + vec2<f32>(cos(rolled.x) * rolled.y, sin(rolled.x) * rolled.y * SPATTER_DROOP);
        let reach = length(at - landed);
        if reach < rolled.z {
            stained = stained + rolled.w * (1.0 - reach / rolled.z);
        }
    }
    return vec4<f32>(stained, 0.0, 0.0, 1.0);
}

// Flow smear (image.rs `smear_along_flow`, one advection pass): average
// the field over a segment of the local flow line and mix that back in
// proportion to coherence. The pass count lives in the graph — the Rust
// `smear_passes` builder emits field.rs SMEAR_PASSES entries ping-ponging
// through a transient. Gate and authority mirror image.rs SMEAR_GATE /
// SMEAR_AUTHORITY.
const SMEAR_GATE: f32 = 0.25;
const SMEAR_AUTHORITY: f32 = 0.85;

struct SmearParams {
    // Steps taken either way along the flow, in texels — the oracle call
    // site's `image::tuned(SMEAR_REACH, height)` rounded to an integer.
    reach: i32,
}

@group(0) @binding(0) var<uniform> smear_params: SmearParams;
@group(1) @binding(0) var smear_field: texture_2d<f32>;
@group(1) @binding(2) var smear_flow_x: texture_2d<f32>;
@group(1) @binding(4) var smear_flow_y: texture_2d<f32>;
@group(1) @binding(6) var smear_coherence: texture_2d<f32>;

@fragment
fn fs_smear(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let pixel = vec2<i32>(position.xy);
    let pooled = textureLoad(smear_field, pixel, 0).r;
    let gate = textureLoad(smear_coherence, pixel, 0).r;

    if gate < SMEAR_GATE {
        return vec4<f32>(pooled, 0.0, 0.0, 1.0);
    }

    let along = vec2<f32>(
        textureLoad(smear_flow_x, pixel, 0).r,
        textureLoad(smear_flow_y, pixel, 0).r,
    );
    let extent = vec2<i32>(textureDimensions(smear_field));
    var sum = 0.0;
    var count = 0.0;

    for (var tap = -smear_params.reach; tap <= smear_params.reach; tap = tap + 1) {
        // sign * floor(abs + 0.5) is the oracle's f32::round — half away
        // from zero — at every position including the negative halves
        // just past the top and left edges; WGSL's own round() ties to
        // even and would part ways with it at exact halves. The bounds
        // test then runs on the ROUNDED texel — a sample position up to
        // half a texel past an edge still lands in the plane and counts
        // toward the average. Testing the unrounded position instead
        // deflates `count` along every coherent edge, and that half-texel
        // slack is exactly what the oracle's round-then-index carries.
        let offset = vec2<f32>(pixel) + along * f32(tap);
        let sampled = vec2<i32>(sign(offset) * floor(abs(offset) + vec2<f32>(0.5, 0.5)));
        if sampled.x >= 0 && sampled.x < extent.x && sampled.y >= 0 && sampled.y < extent.y {
            sum = sum + textureLoad(smear_field, sampled, 0).r;
            count = count + 1.0;
        }
    }

    // `count` is never zero: tap 0 lands on the fragment's own texel.
    let taken = gate * SMEAR_AUTHORITY;
    return vec4<f32>(pooled * (1.0 - taken) + (sum / count) * taken, 0.0, 0.0, 1.0);
}
