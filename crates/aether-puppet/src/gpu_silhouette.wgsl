struct SilhouetteParams {
    view_proj: mat4x4<f32>,
    eye: vec3<f32>,
    mode: u32,
    counts: vec4<u32>,
    bones: array<vec4<f32>, 24>,
}

@group(0) @binding(0) var<uniform> params: SilhouetteParams;

// Every compute entry point uses a prefix of the same raw-storage spelling.
// The pass graph maps that prefix to the resident resource each stage needs.
@group(2) @binding(0) var<storage, read_write> storage0: array<u32>;
@group(2) @binding(1) var<storage, read_write> storage1: array<u32>;
@group(2) @binding(2) var<storage, read_write> storage2: array<u32>;
@group(2) @binding(3) var<storage, read_write> storage3: array<u32>;

const VERTEX_WORDS: u32 = 16u;
const POSED_WORDS: u32 = 8u;
const SEGMENT_WORDS: u32 = 16u;
const POINT_WORDS: u32 = 4u;
const SCRATCH_HEADER_WORDS: u32 = 8u;
const TOPOLOGY_HEADER_WORDS: u32 = 8u;
const EDGE_WORDS: u32 = 4u;
const INCIDENT_WORDS: u32 = 2u;
const OUTPUT_VERTEX_WORDS: u32 = 4u;
const EXCEPTIONAL: u32 = 0x80000000u;
const REFERENCE_PIXELS_PER_RADIAN: f32 = 1410.0;
const MINIMUM_ANGULAR_LENGTH: f32 = 1.5 / REFERENCE_PIXELS_PER_RADIAN;
const ANGULAR_HALF_WIDTH: f32 = 0.0007;
const ANGULAR_WOBBLE: f32 = 0.8 / REFERENCE_PIXELS_PER_RADIAN;
const PRESSURE_RAMP: f32 = 0.0064;

fn load0(at: u32) -> f32 {
    return bitcast<f32>(storage0[at]);
}

fn load2(at: u32) -> f32 {
    return bitcast<f32>(storage2[at]);
}

fn store0(at: u32, value: f32) {
    storage0[at] = bitcast<u32>(value);
}

fn store1(at: u32, value: f32) {
    storage1[at] = bitcast<u32>(value);
}

fn store2(at: u32, value: f32) {
    storage2[at] = bitcast<u32>(value);
}

fn posed_offset(vertex: u32) -> u32 {
    return SCRATCH_HEADER_WORDS + vertex * POSED_WORDS;
}

fn segment_base() -> u32 {
    return SCRATCH_HEADER_WORDS + params.counts.x * POSED_WORDS;
}

fn segment_offset(face: u32) -> u32 {
    return segment_base() + face * SEGMENT_WORDS;
}

fn point_base() -> u32 {
    return segment_base() + params.counts.y * SEGMENT_WORDS;
}

fn point_offset(point: u32) -> u32 {
    return point_base() + point * POINT_WORDS;
}

fn topology_edge_offset(edge: u32) -> u32 {
    return storage0[4u] + edge * EDGE_WORDS;
}

fn topology_incident_offset(incident: u32) -> u32 {
    return storage0[5u] + incident * INCIDENT_WORDS;
}

fn topology_face_edge(face: u32, local_edge: u32) -> u32 {
    return storage1[6u] + face * 3u + local_edge;
}

fn bone_point(bone: u32, p: vec3<f32>) -> vec3<f32> {
    let h = vec4<f32>(p, 1.0);
    let row = bone * 3u;
    return vec3<f32>(dot(params.bones[row], h), dot(params.bones[row + 1u], h), dot(params.bones[row + 2u], h));
}

fn bone_direction(bone: u32, v: vec3<f32>) -> vec3<f32> {
    let h = vec4<f32>(v, 0.0);
    let row = bone * 3u;
    return vec3<f32>(dot(params.bones[row], h), dot(params.bones[row + 1u], h), dot(params.bones[row + 2u], h));
}

// The same resident source that compute poses for silhouette derivation is
// rasterized once into a private depth slot. Dense weights stay in bone-table
// order, so this accumulation is deliberately the same one
// `cs_pose_classify` performs below. The color transient records distance like
// the established stroke-visibility prepass, while the shared depth slot is
// what the final indirect draw consumes.
struct SubjectVertex {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
}

@vertex
fn vs_subject(
    @location(0) rest_position: vec3<f32>,
    @location(1) _position_pad: f32,
    @location(2) _rest_normal: vec3<f32>,
    @location(3) _normal_pad: f32,
    @location(4) weight0: f32,
    @location(5) weight1: f32,
    @location(6) weight2: f32,
    @location(7) weight3: f32,
    @location(8) weight4: f32,
    @location(9) weight5: f32,
    @location(10) weight6: f32,
    @location(11) weight7: f32,
) -> SubjectVertex {
    let weights = array<f32, 8>(weight0, weight1, weight2, weight3, weight4, weight5, weight6, weight7);
    var world = vec3<f32>(0.0);
    var total = 0.0;
    for (var bone = 0u; bone < 8u; bone += 1u) {
        let weight = weights[bone];
        if weight > 1.0e-4 {
            world += bone_point(bone, rest_position) * weight;
            total += weight;
        }
    }
    if total <= 0.0 {
        world = rest_position;
    }

    var out: SubjectVertex;
    out.clip = params.view_proj * vec4<f32>(world, 1.0);
    out.world = world;
    return out;
}

@fragment
fn fs_subject_depth(subject: SubjectVertex) -> @location(0) vec4<f32> {
    return vec4<f32>(length(subject.world - params.eye), 0.0, 0.0, 1.0);
}

@compute @workgroup_size(64)
fn cs_pose_classify(@builtin(global_invocation_id) invocation: vec3<u32>) {
    let vertex = invocation.x;
    if vertex >= params.counts.x {
        return;
    }

    let source = vertex * VERTEX_WORDS;
    let rest_position = vec3<f32>(load0(source), load0(source + 1u), load0(source + 2u));
    let rest_normal = vec3<f32>(load0(source + 4u), load0(source + 5u), load0(source + 6u));
    var position = vec3<f32>(0.0);
    var normal = vec3<f32>(0.0);
    var total = 0.0;
    for (var bone = 0u; bone < 8u; bone += 1u) {
        let weight = load0(source + 8u + bone);
        if weight > 1.0e-4 {
            position += bone_point(bone, rest_position) * weight;
            normal += bone_direction(bone, rest_normal) * weight;
            total += weight;
        }
    }
    if total <= 0.0 {
        position = rest_position;
        normal = rest_normal;
    } else if length(normal) < 1.0e-12 {
        normal = rest_normal;
    } else {
        normal = normalize(normal);
    }

    let output = SCRATCH_HEADER_WORDS + vertex * POSED_WORDS;
    store1(output, position.x);
    store1(output + 1u, position.y);
    store1(output + 2u, position.z);
    store1(output + 4u, normal.x);
    store1(output + 5u, normal.y);
    store1(output + 6u, normal.z);
    store1(output + 7u, dot(position - params.eye, normal));
}

fn posed_position(vertex: u32) -> vec3<f32> {
    let at = posed_offset(vertex);
    return vec3<f32>(load2(at), load2(at + 1u), load2(at + 2u));
}

fn facing(vertex: u32) -> f32 {
    return load2(posed_offset(vertex) + 7u);
}

fn local_edge(a: u32, b: u32) -> u32 {
    let low = min(a, b);
    let high = max(a, b);
    if low == 0u && high == 1u {
        return 0u;
    }
    if low == 1u && high == 2u {
        return 1u;
    }
    return 2u;
}

fn crossing(corners: vec3<u32>, i: u32, j: u32) -> vec3<f32> {
    var low_corner = i;
    var high_corner = j;
    if corners[i] > corners[j] {
        low_corner = j;
        high_corner = i;
    }
    let low = corners[low_corner];
    let high = corners[high_corner];
    let low_value = facing(low);
    let span = facing(high) - low_value;
    var t = 0.5;
    if abs(span) >= 1.0e-20 {
        t = -low_value / span;
    }
    return mix(posed_position(low), posed_position(high), t);
}

fn write_endpoint(base: u32, endpoint: u32, edge: u32, position: vec3<f32>) {
    storage2[base + 1u + endpoint] = edge;
    let at = base + 4u + endpoint * 4u;
    store2(at, position.x);
    store2(at + 1u, position.y);
    store2(at + 2u, position.z);
}

@compute @workgroup_size(64)
fn cs_march_faces(@builtin(global_invocation_id) invocation: vec3<u32>) {
    let face = invocation.x;
    if face >= params.counts.y {
        return;
    }

    let corners = vec3<u32>(storage0[face * 3u], storage0[face * 3u + 1u], storage0[face * 3u + 2u]);
    let above = vec3<bool>(facing(corners.x) >= 0.0, facing(corners.y) >= 0.0, facing(corners.z) >= 0.0);
    let output = segment_base() + face * SEGMENT_WORDS;
    storage2[output] = 0u;
    storage2[output + 12u] = 0u;
    storage2[output + 13u] = 0u;
    storage2[output + 14u] = 0u;
    if above.x == above.y && above.y == above.z {
        return;
    }

    var odd = 0u;
    if above.x == above.y {
        odd = 2u;
    } else if above.x == above.z {
        odd = 1u;
    }
    let a = (odd + 1u) % 3u;
    let b = (odd + 2u) % 3u;
    storage2[output] = 1u;
    write_endpoint(output, 0u, storage1[topology_face_edge(face, local_edge(odd, a))], crossing(corners, odd, a));
    write_endpoint(output, 1u, storage1[topology_face_edge(face, local_edge(odd, b))], crossing(corners, odd, b));
}

fn endpoint_on(face: u32, edge: u32) -> i32 {
    let segment = segment_offset(face);
    if storage1[segment] == 0u {
        return -1;
    }
    if storage1[segment + 1u] == edge {
        return 0;
    }
    if storage1[segment + 2u] == edge {
        return 1;
    }
    return -1;
}

fn set_link(face: u32, endpoint: u32, linked: u32, exceptional: bool) {
    storage1[segment_offset(face) + 12u + endpoint] = linked | select(0u, EXCEPTIONAL, exceptional);
}

@compute @workgroup_size(64)
fn cs_link_edges(@builtin(global_invocation_id) invocation: vec3<u32>) {
    let edge = invocation.x;
    if edge >= params.counts.z {
        return;
    }
    let record = topology_edge_offset(edge);
    let first = storage0[record + 2u];
    let count = storage0[record + 3u];
    var active_count = 0u;
    for (var at = 0u; at < count; at += 1u) {
        let incident = topology_incident_offset(first + at);
        if endpoint_on(storage0[incident], edge) >= 0 {
            active_count += 1u;
        }
    }
    let exceptional = active_count > 2u;

    for (var at = 0u; at < count; at += 1u) {
        let incident = topology_incident_offset(first + at);
        let face = storage0[incident];
        let endpoint = endpoint_on(face, edge);
        if endpoint < 0 {
            continue;
        }
        let link_at = segment_offset(face) + 12u + u32(endpoint);
        if (storage1[link_at] & ~EXCEPTIONAL) != 0u {
            continue;
        }
        set_link(face, u32(endpoint), 0u, exceptional);
        for (var next = at + 1u; next < count; next += 1u) {
            let next_incident = topology_incident_offset(first + next);
            let next_face = storage0[next_incident];
            let next_endpoint = endpoint_on(next_face, edge);
            if next_face == face || next_endpoint < 0 {
                continue;
            }
            let next_link_at = segment_offset(next_face) + 12u + u32(next_endpoint);
            if (storage1[next_link_at] & ~EXCEPTIONAL) != 0u {
                continue;
            }
            set_link(face, u32(endpoint), next_face * 2u + u32(next_endpoint) + 1u, exceptional);
            set_link(next_face, u32(next_endpoint), face * 2u + u32(endpoint) + 1u, exceptional);
            break;
        }
    }
}

fn endpoint_position(face: u32, endpoint: u32) -> vec3<f32> {
    let at = segment_offset(face) + 4u + endpoint * 4u;
    return vec3<f32>(load0(at), load0(at + 1u), load0(at + 2u));
}

fn write_point(point: u32, position: vec3<f32>) {
    let at = point_offset(point);
    store0(at, position.x);
    store0(at + 1u, position.y);
    store0(at + 2u, position.z);
}

fn point_position(point: u32) -> vec3<f32> {
    let at = point_offset(point);
    return vec3<f32>(load0(at), load0(at + 1u), load0(at + 2u));
}

fn swap_points(a: u32, b: u32) {
    let held = point_position(a);
    write_point(a, point_position(b));
    write_point(b, held);
}

fn wander(at: vec3<f32>) -> f32 {
    let a = vec3<f32>(0.71, 0.52, 0.47);
    let b = vec3<f32>(-0.44, 0.63, 0.64);
    return sin(dot(at, a) * 7.9001908 + 5.5500054) * 0.72
        + sin(dot(at, b) * 16.181538 + 2.3869493) * 0.28;
}

fn output_vertex(vertex: u32, position: vec3<f32>, exceptional: bool) {
    let at = vertex * OUTPUT_VERTEX_WORDS;
    store1(at, position.x);
    store1(at + 1u, position.y);
    store1(at + 2u, position.z);
    store1(at + 3u, select(0.0, 1.0, exceptional));
}

// One bounded invocation deliberately transcribes `compact`'s face-order
// traversal. Parallel posing, marching, and edge pairing leave this stage
// only the output-order decision; serialising that decision is what keeps
// curve direction, cycle starts, and exceptional pen lifts identical to the
// CPU oracle while the prototype establishes its timing baseline.
@compute @workgroup_size(1)
fn cs_compact() {
    storage3[0] = 0u;
    storage3[1] = 1u;
    storage3[2] = 0u;
    storage3[3] = 0u;
    storage3[4] = 0u;
    storage3[7] = 0u;
    if params.counts.y * 4u > storage3[5] || params.counts.y * 6u > storage3[6] {
        storage3[7] = 1u;
        return;
    }

    var points = 0u;
    var indices = 0u;
    for (var start = 0u; start < params.counts.y; start += 1u) {
        let start_segment = segment_offset(start);
        if storage0[start_segment] == 0u || storage0[start_segment + 14u] != 0u {
            continue;
        }
        storage0[start_segment + 14u] = 1u;
        let first_point = points;
        var exceptional = false;
        write_point(points, endpoint_position(start, 0u));
        points += 1u;
        write_point(points, endpoint_position(start, 1u));
        points += 1u;

        var tail_face = start;
        var tail_endpoint = 1u;
        loop {
            let raw = storage0[segment_offset(tail_face) + 12u + tail_endpoint];
            exceptional = exceptional || (raw & EXCEPTIONAL) != 0u;
            let linked = raw & ~EXCEPTIONAL;
            if linked == 0u {
                break;
            }
            let next = linked - 1u;
            let next_face = next / 2u;
            let next_endpoint = next % 2u;
            let next_segment = segment_offset(next_face);
            if storage0[next_segment + 14u] != 0u {
                break;
            }
            storage0[next_segment + 14u] = 1u;
            let far = next_endpoint ^ 1u;
            write_point(points, endpoint_position(next_face, far));
            points += 1u;
            tail_face = next_face;
            tail_endpoint = far;
        }

        var left = first_point;
        var right = points - 1u;
        while left < right {
            swap_points(left, right);
            left += 1u;
            right -= 1u;
        }

        tail_face = start;
        tail_endpoint = 0u;
        loop {
            let raw = storage0[segment_offset(tail_face) + 12u + tail_endpoint];
            exceptional = exceptional || (raw & EXCEPTIONAL) != 0u;
            let linked = raw & ~EXCEPTIONAL;
            if linked == 0u {
                break;
            }
            let next = linked - 1u;
            let next_face = next / 2u;
            let next_endpoint = next % 2u;
            let next_segment = segment_offset(next_face);
            if storage0[next_segment + 14u] != 0u {
                break;
            }
            storage0[next_segment + 14u] = 1u;
            let far = next_endpoint ^ 1u;
            write_point(points, endpoint_position(next_face, far));
            points += 1u;
            tail_face = next_face;
            tail_endpoint = far;
        }

        let curve_points = points - first_point;
        var reference = 0.0;
        var angular = 0.0;
        for (var at = 0u; at < curve_points; at += 1u) {
            let position = point_position(first_point + at);
            let depth = max(length(position - params.eye), 1.0e-4);
            reference += depth;
            if at + 1u < curve_points {
                angular += length(point_position(first_point + at + 1u) - position) / depth;
            }
        }
        if angular < MINIMUM_ANGULAR_LENGTH {
            points = first_point;
            continue;
        }
        reference /= f32(curve_points);

        var travelled = 0.0;
        for (var at = 0u; at < curve_points; at += 1u) {
            let point = first_point + at;
            let position = point_position(point);
            let before = point_position(first_point + select(0u, at - 1u, at > 0u));
            let after = point_position(first_point + min(at + 1u, curve_points - 1u));
            let to_eye = params.eye - position;
            let depth = max(length(to_eye), 1.0e-4);
            var across = cross(after - before, to_eye);
            if length(across) < 1.0e-9 {
                across = vec3<f32>(0.0);
            } else {
                across = normalize(across);
            }
            let centre = position + across * (wander(position) * ANGULAR_WOBBLE * depth);
            let depth_weight = clamp(reference / depth, 0.82, 1.22);
            let ramp = min(PRESSURE_RAMP, angular * 0.45);
            var pressure = 1.0;
            if ramp > 1.0e-6 {
                let ends = clamp(min(travelled / ramp, (angular - travelled) / ramp), 0.0, 1.0);
                pressure = 0.42 + 0.58 * sqrt(ends);
            }
            let offset = across * (ANGULAR_HALF_WIDTH * depth * depth_weight * pressure);
            output_vertex(point * 2u, centre - offset, exceptional);
            output_vertex(point * 2u + 1u, centre + offset, exceptional);

            if at + 1u < curve_points {
                let next_left = point * 2u + 2u;
                let next_right = point * 2u + 3u;
                storage2[indices] = point * 2u;
                storage2[indices + 1u] = next_left;
                storage2[indices + 2u] = next_right;
                storage2[indices + 3u] = point * 2u;
                storage2[indices + 4u] = next_right;
                storage2[indices + 5u] = point * 2u + 1u;
                indices += 6u;
                travelled += length(point_position(point + 1u) - position) / depth;
            }
        }
    }
    storage3[0] = indices;
}

struct SilhouetteVertex {
    @builtin(position) clip: vec4<f32>,
    @location(0) exceptional: f32,
}

@vertex
fn vs_silhouette(@location(0) position: vec3<f32>, @location(1) exceptional: f32) -> SilhouetteVertex {
    var out: SilhouetteVertex;
    out.clip = params.view_proj * vec4<f32>(position, 1.0);
    out.exceptional = exceptional;
    return out;
}

@fragment
fn fs_silhouette(vertex: SilhouetteVertex) -> @location(0) vec4<f32> {
    if params.mode != 0u {
        if vertex.exceptional < 0.5 {
            discard;
        }
        return vec4<f32>(0.92, 0.08, 0.12, 1.0);
    }
    return vec4<f32>(0.106, 0.106, 0.122, 1.0);
}
