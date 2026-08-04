// Posing the subject in the vertex stage, from bone maps that ride the
// uniform blob (ADR-0171, iamacoffeepot/aether#4462).
//
// Appended to every program whose vertex stage stands on a posed
// surface, after that program's own source — so `params` is already
// declared, and each program supplies a `bones` member of its own block
// rather than this text carrying a binding it could not agree with the
// rest of the block about.
//
// # What a vertex carries
//
// A subject vertex carries its own binding: four joint indices and four
// shares, the pair the closed vertex format set names for skinning.
// Four is the whole row on this rig rather than a truncation of it —
// `deform::Skin::influences` says why.
//
// A *drawing* vertex carries two of them. A curve point is not a vertex
// but an address inside a face, and posing it is the barycentric blend
// of the posed corners — apply, then interpolate. Blending the corners'
// bone maps and applying the blend once is the same arithmetic and the
// same answer at a vertex, and a different function everywhere else: at
// a soft weight boundary the two disagree by a visible fraction of the
// deformation, which reads as the drawing sliding off the skin it was
// cut into. So `anchored_point` poses each corner and interpolates the
// results, never the other way round.
//
// Two corners rather than three because every point the drawing carries
// onto the GPU is a level-set crossing on an *edge*, and an edge
// crossing's third barycentric share is exactly zero. A point that is
// not — a planted mark — arrives already posed, with both corners set to
// its own position and no binding at all, which is what an empty share
// row means below.

// One row of a bone's affine map: the linear part's row, then that
// axis' translation.
fn bone_row(bone: u32, row: u32) -> vec4<f32> {
    return params.bones[bone * 3u + row];
}

fn bone_point(bone: u32, p: vec3<f32>) -> vec3<f32> {
    let h = vec4<f32>(p, 1.0);

    return vec3<f32>(dot(bone_row(bone, 0u), h), dot(bone_row(bone, 1u), h), dot(bone_row(bone, 2u), h));
}

// The same map on a direction — a normal, or the chord across a stroke
// — which the translation must not reach.
fn bone_dir(bone: u32, v: vec3<f32>) -> vec3<f32> {
    let h = vec4<f32>(v, 0.0);

    return vec3<f32>(dot(bone_row(bone, 0u), h), dot(bone_row(bone, 1u), h), dot(bone_row(bone, 2u), h));
}

// Linear blend skinning at one vertex.
//
// The shares are renormalised here rather than trusted, because they
// arrive quantised to eight bits: four lanes each off by up to a part in
// 255 sum to something that is not one, and an affine blend whose
// weights miss one scales the vertex toward the origin by the miss.
//
// A row that sums to zero is not a vertex with no bones; it is a point
// that was posed before it was packed, and its position is already the
// answer.
fn skin_point(joints: vec4<u32>, shares: vec4<f32>, p: vec3<f32>) -> vec3<f32> {
    let total = shares.x + shares.y + shares.z + shares.w;
    if total <= 0.0 {
        return p;
    }
    let posed = bone_point(joints.x, p) * shares.x
        + bone_point(joints.y, p) * shares.y
        + bone_point(joints.z, p) * shares.z
        + bone_point(joints.w, p) * shares.w;

    return posed / total;
}

fn skin_dir(joints: vec4<u32>, shares: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let total = shares.x + shares.y + shares.z + shares.w;
    if total <= 0.0 {
        return v;
    }
    let turned = bone_dir(joints.x, v) * shares.x
        + bone_dir(joints.y, v) * shares.y
        + bone_dir(joints.z, v) * shares.z
        + bone_dir(joints.w, v) * shares.w;

    return turned / total;
}

// One point's two corner bindings and where between them it sits.
struct Anchorage {
    a_joints: vec4<u32>,
    a_shares: vec4<f32>,
    b_joints: vec4<u32>,
    b_shares: vec4<f32>,
    // The second corner's barycentric share; the first takes the rest.
    between: f32,
}

// Pose the two corners, then interpolate — the order the whole design
// turns on.
fn anchored_point(at: Anchorage, a: vec3<f32>, b: vec3<f32>) -> vec3<f32> {
    return skin_point(at.a_joints, at.a_shares, a) * (1.0 - at.between)
        + skin_point(at.b_joints, at.b_shares, b) * at.between;
}

// A direction anchored at the same address: the chord across a stroke,
// which the rail solve takes a cross product with.
//
// Interpolated between the corners' *maps* rather than posed at each
// end, because a direction has no ends — it is one vector, and the two
// corners disagree about how to turn it only by the weight gradient
// across a single triangle edge.
fn anchored_dir(at: Anchorage, v: vec3<f32>) -> vec3<f32> {
    return skin_dir(at.a_joints, at.a_shares, v) * (1.0 - at.between)
        + skin_dir(at.b_joints, at.b_shares, v) * at.between;
}

// A normal at the same address: each corner's rest normal turned by its
// own blend, then interpolated and renormalised.
fn anchored_normal(at: Anchorage, a: vec3<f32>, b: vec3<f32>) -> vec3<f32> {
    let blended = skin_dir(at.a_joints, at.a_shares, a) * (1.0 - at.between)
        + skin_dir(at.b_joints, at.b_shares, b) * at.between;
    if length(blended) < 1e-12 {
        return a;
    }

    return normalize(blended);
}
