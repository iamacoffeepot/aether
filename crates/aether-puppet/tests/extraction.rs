//! Tests for the two pieces of judgement this crate owns.
//!
//! Not the level sets, the welding or the relief band-pass: those came
//! across from a renderer that already proves them by producing a drawing,
//! and asserting a curve count here would pin a number with no independent
//! truth behind it. What is tested is the two places where this port added
//! a decision, and where getting it wrong is silent.

use aether_math::Vec3;
use aether_puppet::labels::Labels;
use aether_puppet::mesh::Mesh;

/// A unit cube as OBJ text — twelve triangles, consistently wound outward.
fn cube() -> String {
    let corners = ["-1 -1 -1", "1 -1 -1", "1 1 -1", "-1 1 -1", "-1 -1 1", "1 -1 1", "1 1 1", "-1 1 1"];
    let faces = [
        (1, 3, 2),
        (1, 4, 3),
        (5, 6, 7),
        (5, 7, 8),
        (1, 2, 6),
        (1, 6, 5),
        (2, 3, 7),
        (2, 7, 6),
        (3, 4, 8),
        (3, 8, 7),
        (4, 1, 5),
        (4, 5, 8),
    ];

    corners.iter().map(|c| format!("v {c}\n")).chain(faces.iter().map(|(a, b, c)| format!("f {a} {b} {c}\n"))).collect()
}

/// Tripwire: the OBJ reader keeps winding, so vertex normals come out
/// pointing away from the body.
///
/// `agreement` is the mean of `normalize(p - centre) . n` — near `+1`
/// outward, near `-1` inward, and near `0` when the winding is
/// inconsistent. Zero is the dangerous one: it is what a face list
/// deduplicated on a *sorted* index triple produces, and it is silent.
/// Nothing errors, nothing looks wrong at load, and the silhouette — the
/// zero set of `view . normal` — simply has no coherent answer to find, so
/// the drawing comes back empty. That failure cost a full debugging cycle
/// to attribute, which is exactly what a tripwire is for.
#[test]
fn obj_winding_survives_the_reader() {
    let mesh = Mesh::from_obj_bytes(cube().as_bytes(), 0).expect("a cube is a mesh");
    assert_eq!(mesh.faces.len(), 12);

    let centre = (mesh.min + mesh.max) * 0.5;
    let agreement: f32 =
        mesh.positions.iter().zip(&mesh.normals).map(|(&p, &n)| (p - centre).normalize_or(n).dot(n)).sum::<f32>()
            / mesh.positions.len() as f32;

    assert!(agreement > 0.5, "normals should point outward, got {agreement:+.3}");
}

/// The material field's lattice is reconstructed from the cell count, so a
/// buffer that is not a cube would place every sample somewhere wrong —
/// quietly, since sampling out of range reads as unlabelled and unlabelled
/// reads as "ink this crease". A wrong field is worse than no field.
#[test]
fn a_material_field_that_is_not_a_cube_is_refused() {
    // `\x93NUMPY`, version, then a two-byte header length.
    let header = |cells: usize| {
        let mut bytes = b"\x93NUMPY\x01\x00\x00\x00".to_vec();
        bytes.extend(std::iter::repeat_n(0u8, cells));
        bytes
    };
    let (lo, hi) = (Vec3::splat(-1.0), Vec3::splat(1.0));

    assert!(Labels::parse(&header(4 * 4 * 4), lo, hi, 0.12).is_some(), "a cube is accepted");
    assert!(Labels::parse(&header(4 * 4 * 4 + 1), lo, hi, 0.12).is_none(), "a non-cube is refused");
    assert!(Labels::parse(b"\x93NU", lo, hi, 0.12).is_none(), "a truncated buffer is refused, not indexed");
}
