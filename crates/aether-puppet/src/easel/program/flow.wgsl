// The drawing's own flow as authored passes (iamacoffeepot/aether#4387,
// ADR-0171): `image::structure_tensor_flow`, re-spoken over the ink
// coverage plane the ink pass rasterizes.
//
// The CPU oracle softens the coverage, takes a central-difference
// gradient, forms the three distinct components of the symmetric 2x2
// structure tensor, pools each over a wide radius, and reads the minor
// eigenvector out. Every step but the eigen solve is already an authored
// op — the blur chain does the softening and all three poolings — so
// what this module adds is the two pointwise ends: the tensor components
// and the resolve.
//
// Both ends are a selector over one entry point rather than three entry
// points, because the three components differ only in which product of
// the gradient they take and the three outputs differ only in which
// number they read off the same solve. One transcription of the formula
// is one thing to keep in step with the oracle; three copies are three.

// Below this trace the tensor is noise, not orientation
// (`image::TENSOR_FLOOR`).
const TENSOR_FLOOR: f32 = 1e-7;

// Which product of the gradient a tensor pass takes.
const TENSOR_XX: u32 = 0u;
const TENSOR_XY: u32 = 1u;

// Which of the resolve's three answers a pass carries out.
const FLOW_X: u32 = 0u;
const FLOW_Y: u32 = 1u;

struct FlowSelectParams {
    // Which component or channel this pass writes.
    channel: u32,
}

@group(0) @binding(0) var<uniform> flow_select: FlowSelectParams;
@group(1) @binding(0) var flow_soft: texture_2d<f32>;

// One component of the structure tensor at this texel.
//
// The oracle walks `1..height - 1` and `1..width - 1`, leaving the
// one-texel border at the zero it allocated, so the border is answered
// here rather than clamped into — a clamped gradient at the edge is a
// different field, and the difference would sit under every parity
// measurement.
@fragment
fn fs_flow_tensor(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let size = vec2<i32>(textureDimensions(flow_soft));
    let at = vec2<i32>(position.xy);
    if at.x < 1 || at.y < 1 || at.x >= size.x - 1 || at.y >= size.y - 1 {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    let across = vec2<i32>(1, 0);
    let down = vec2<i32>(0, 1);
    let slope_x = (textureLoad(flow_soft, at + across, 0).r - textureLoad(flow_soft, at - across, 0).r) * 0.5;
    let slope_y = (textureLoad(flow_soft, at + down, 0).r - textureLoad(flow_soft, at - down, 0).r) * 0.5;

    var component = slope_y * slope_y;
    if flow_select.channel == TENSOR_XX {
        component = slope_x * slope_x;
    } else if flow_select.channel == TENSOR_XY {
        component = slope_x * slope_y;
    }

    return vec4<f32>(component, 0.0, 0.0, 1.0);
}

@group(1) @binding(0) var flow_xx: texture_2d<f32>;
@group(1) @binding(2) var flow_xy: texture_2d<f32>;
@group(1) @binding(4) var flow_yy: texture_2d<f32>;

// Which way the drawing runs here, and how sure it is.
//
// The minor eigenvector of the pooled tensor — along the strokes rather
// than across them — and the eigenvalue split, which is near zero
// wherever the ink has no preferred direction, so blank paper gates
// itself out of anything riding this field.
@fragment
fn fs_flow_resolve(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let at = vec2<i32>(position.xy);
    let xx = textureLoad(flow_xx, at, 0).r;
    let xy = textureLoad(flow_xy, at, 0).r;
    let yy = textureLoad(flow_yy, at, 0).r;

    let difference = xx - yy;
    let angle = 0.5 * atan2(2.0 * xy, difference);

    var answer = 0.0;
    if flow_select.channel == FLOW_X {
        answer = -sin(angle);
    } else if flow_select.channel == FLOW_Y {
        answer = cos(angle);
    } else {
        let trace = xx + yy;
        if trace > TENSOR_FLOOR {
            answer = sqrt(difference * difference + 4.0 * xy * xy) / trace;
        }
    }

    return vec4<f32>(answer, 0.0, 0.0, 1.0);
}
