// A test binary is its own compilation unit, so the crate-level allows do
// not reach it. The vertex-count cast is bounded by a twelve-face cube.
#![allow(clippy::cast_precision_loss)]

//! Tests for the two pieces of judgement this crate owns.
//!
//! Not the level sets, the welding or the relief band-pass: those came
//! across from a renderer that already proves them by producing a drawing,
//! and asserting a curve count here would pin a number with no independent
//! truth behind it. What is tested is the two places where this port added
//! a decision, and where getting it wrong is silent.

use core::f32::consts::{PI, TAU};
use core::fmt::Write as _;
use core::iter;

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
        bytes.extend(iter::repeat_n(0u8, cells));
        bytes
    };
    let (lo, hi) = (Vec3::splat(-1.0), Vec3::splat(1.0));

    assert!(Labels::parse(&header(4 * 4 * 4), lo, hi, 0.12).is_some(), "a cube is accepted");
    assert!(Labels::parse(&header(4 * 4 * 4 + 1), lo, hi, 0.12).is_none(), "a non-cube is refused");
    assert!(Labels::parse(b"\x93NU", lo, hi, 0.12).is_none(), "a truncated buffer is refused, not indexed");
}

/// A closed, consistently outward-wound sphere as OBJ text, dense enough
/// that clustering has something to remove. A cube cannot serve — eight
/// vertices survive any lattice, so the decimation is a no-op and the test
/// asserts nothing.
fn sphere(rings: usize, segments: usize) -> String {
    let mut text = String::new();
    for ring in 0..=rings {
        let phi = PI * ring as f32 / rings as f32;
        for segment in 0..segments {
            let theta = TAU * segment as f32 / segments as f32;
            let (x, y, z) = (phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin());
            let _ = writeln!(text, "v {x} {y} {z}");
        }
    }
    // Outward winding, matching the cube fixture's convention.
    for ring in 0..rings {
        for segment in 0..segments {
            let next = (segment + 1) % segments;
            let (a, b) = (ring * segments + segment + 1, ring * segments + next + 1);
            let (c, d) = ((ring + 1) * segments + segment + 1, (ring + 1) * segments + next + 1);
            let _ = writeln!(text, "f {a} {b} {c}\nf {b} {d} {c}");
        }
    }
    text
}

/// Orientation agreement: the mean of `normalize(p - centre) . n`. Near
/// `+1` outward, near `-1` inward, near `0` inconsistent.
fn agreement(mesh: &Mesh) -> f32 {
    let centre = (mesh.min + mesh.max) * 0.5;

    mesh.positions.iter().zip(&mesh.normals).map(|(&p, &n)| (p - centre).normalize_or(n).dot(n)).sum::<f32>()
        / mesh.positions.len() as f32
}

/// Tripwire: clustering removes faces and keeps the winding.
///
/// The silhouette is the zero set of `view . normal`, so a decimation that
/// scrambles the normal sign leaves it with no coherent zero set and the
/// outline simply vanishes — with nothing having errored, which is what
/// makes it worth a tripwire rather than a comment. The failure has a name:
/// deduplicating faces on a *sorted* index triple maps a face and its
/// mirror onto one key, so whichever arrived first survives and the sign
/// becomes input order. Agreement is the quantity that sees it, because
/// inconsistency drives it to zero rather than to `-1`.
///
/// Both halves matter. Reduction alone passes for a decimation that
/// destroyed the surface; agreement alone passes for one that removed
/// nothing.
#[test]
fn clustering_reduces_the_face_count_without_losing_the_winding() {
    let fine = Mesh::from_obj_bytes(sphere(48, 96).as_bytes(), 0).expect("a sphere is a mesh");
    let coarse = fine.coarsened(16, 0).expect("a lattice this coarse still leaves a surface");

    assert!(
        coarse.faces.len() * 4 < fine.faces.len(),
        "clustering should remove most of the faces: {} of {}",
        coarse.faces.len(),
        fine.faces.len(),
    );
    assert!(
        agreement(&coarse) > 0.5,
        "the coarse mesh keeps its outward winding; got {:+.3} against the source's {:+.3}",
        agreement(&coarse),
        agreement(&fine),
    );
}
