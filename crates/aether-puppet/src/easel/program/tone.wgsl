// `extract::Settings::tone` and the noise that dithers its thresholds,
// in the vertex stage's own language.
//
// Appended after a program's own source, like `skin.wgsl`, and reading
// the same `params` block — `light`, `ambient` and `face_lift`, which
// are the three authored numbers the lighting term is made of.
//
// It has to live here because tone reads the *posed* normal. The gate it
// feeds was a load-time CPU pass while the subject stood still and
// became a per-pose pass once the subject could turn; move the skin to
// the vertex stage and the normal it reads exists nowhere else, so the
// gate moves with it or reads a normal from the pose before last.
//
// This is a second transcription of a Rust function and is held to it by
// `program_bake_scenario`'s tone channel, which measures the two against
// each other pixel by pixel. The two must move together.

// How much of the face-lift applies at a point: full across the front of
// the face, falling off before it reaches the jaw or the hair —
// `extract::face_weight`, constant for constant.
fn face_weight(p: vec3<f32>) -> f32 {
    let horizontal = 1.0 - min(abs(p.x) / 0.26, 1.0);
    let vertical = 1.0 - min(abs(p.y - 0.30) / 0.24, 1.0);
    let frontal = clamp((p.z - 0.16) / 0.12, 0.0, 1.0);

    return pow(horizontal * vertical * frontal, 0.40);
}

// The lighting term at a point: 0 in shadow, 1 fully lit, and past one
// where the face lift carries it — unclamped by contract, exactly as the
// Rust side leaves it.
fn tone_at(p: vec3<f32>, n: vec3<f32>) -> f32 {
    let lambert = max(dot(n, normalize(params.light)), 0.0);

    return params.ambient + (1.0 - params.ambient) * lambert + params.face_lift * face_weight(p);
}

// `math3::noise`: smooth banded noise in world space, roughly [-1, 1],
// which breaks a hatch family's boundary into dashes instead of ruling
// a level curve of the lighting across the figure.
fn tone_noise(p: vec3<f32>) -> f32 {
    let a = dot(p, vec3<f32>(37.3, 24.1, 15.9));
    let b = dot(p, vec3<f32>(-13.7, 48.2, 32.6));
    let c = dot(p, vec3<f32>(25.1, -12.9, -47.4));

    return sin(a) * 0.5 + sin(b) * 0.3 + sin(c) * 0.2;
}
