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

use core::iter;

use aether_math::Vec3;
use aether_puppet::easel::palette::Palette;
use aether_puppet::labels::{CLASS_VOCABULARY, Labels, MaterialField};
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

fn npy(descr: &str, fortran_order: bool, shape: &[usize], payload: &[u8]) -> Vec<u8> {
    let shape = if shape.len() == 1 {
        format!("({},)", shape[0])
    } else {
        format!("({})", shape.iter().map(usize::to_string).collect::<Vec<_>>().join(", "))
    };
    let dictionary = format!(
        "{{'descr': '{descr}', 'fortran_order': {}, 'shape': {shape}, }}",
        if fortran_order {
            "True"
        } else {
            "False"
        },
    );
    let padding = (16 - ((10 + dictionary.len() + 1) % 16)) % 16;
    let mut header = dictionary;
    header.extend(iter::repeat_n(' ', padding));
    header.push('\n');

    let mut bytes = b"\x93NUMPY\x01\x00".to_vec();
    bytes.extend(u16::try_from(header.len()).expect("a short test header").to_le_bytes());
    bytes.extend(header.as_bytes());
    bytes.extend(payload);
    bytes
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
    let (lo, hi) = (Vec3::splat(-1.0), Vec3::splat(1.0));
    let her = Palette::canonical();
    let cube = npy("|u1", false, &[4, 4, 4], &[0; 64]);
    let rectangular = npy("|u1", false, &[4, 4, 5], &[0; 80]);

    assert!(Labels::decode(&cube, her.classes(), lo, hi, 0.12).is_ok(), "a cube is accepted");
    assert_eq!(
        Labels::decode(&rectangular, her.classes(), lo, hi, 0.12).err().as_deref(),
        Some("material field shape is [4, 4, 5], expected (n, n, n) with n >= 2"),
    );
}

#[test]
fn material_field_decoder_declares_dimensions_cells_placement_and_vocabulary() {
    let payload: Vec<u8> = (0_u8..64).map(|cell| cell % 9).collect();
    let bytes = npy("|u1", false, &[4, 4, 4], &payload);
    let field = MaterialField::decode(
        &bytes,
        Palette::canonical().classes(),
        Vec3::new(-1.0, 0.0, 1.0),
        Vec3::new(3.0, 2.0, 3.0),
        0.25,
    )
    .expect("the declared field");

    assert_eq!(field.dimensions, [4, 4, 4]);
    assert_eq!(field.cells, payload);
    assert_eq!(field.origin, [-2.0, -2.0, -1.0]);
    assert_eq!(field.spacing, [2.0, 2.0, 2.0]);
    assert_eq!(field.classes, CLASS_VOCABULARY.map(str::to_owned));
}

#[test]
fn a_field_that_arrives_before_its_mesh_is_replaced_against_the_real_bounds() {
    let bytes = npy("|u1", false, &[4, 4, 4], &[0; 64]);
    let mut field =
        MaterialField::decode(&bytes, Palette::canonical().classes(), Vec3::splat(-1.0), Vec3::splat(1.0), 0.12)
            .expect("the field placed against stand-in bounds");

    field.place_against(Vec3::new(-1.0, 0.0, 1.0), Vec3::new(3.0, 2.0, 3.0), 0.25);

    assert_eq!(field.origin, [-2.0, -2.0, -1.0]);
    assert_eq!(field.spacing, [2.0, 2.0, 2.0]);
}

#[test]
fn material_field_cells_must_index_the_declared_class_vocabulary() {
    let bytes = npy("|u1", false, &[2, 2, 2], &[0, 1, 2, 3, 4, 5, 6, 9]);

    assert_eq!(
        MaterialField::decode(&bytes, Palette::canonical().classes(), Vec3::splat(-1.0), Vec3::splat(1.0), 0.12)
            .err()
            .as_deref(),
        Some("material field cell 7 names class 9, but the vocabulary has 8 classes"),
    );
}

/// The bound a cell is checked against is the box's own vocabulary, not a
/// constant — so a field baked for one subject cannot be painted by
/// another subject's box without being refused, and the diagnostic names
/// the offending cell and the count it overran.
///
/// The failure this guards is silent: a cell past the box's range matches
/// no material's coverage, so the region simply never develops, and the
/// sheet comes back a plausible painting with a hole in it. Pinning the
/// two-class count keeps the check reading the vocabulary it was handed —
/// against the eight-class constant this cell would pass.
#[test]
fn a_field_is_checked_against_the_box_that_will_paint_it() {
    let hillside = Palette::decode_text(
        "classes rock grass\nmaterial rock 0x7a6f63 0.4 0.6 0.35\nmaterial grass 0x5c7a4a 0.3 0.4 0.2\n",
    )
    .expect("the hillside box is well formed");
    let bytes = npy("|u1", false, &[2, 2, 2], &[0, 1, 2, 1, 2, 1, 2, 3]);

    assert_eq!(
        MaterialField::decode(&bytes, hillside.classes(), Vec3::splat(-1.0), Vec3::splat(1.0), 0.12).err().as_deref(),
        Some("material field cell 7 names class 3, but the vocabulary has 2 classes"),
    );
}

#[test]
fn material_field_metadata_and_truncation_are_diagnostic() {
    let (lo, hi) = (Vec3::splat(-1.0), Vec3::splat(1.0));
    let her = Palette::canonical();
    let wrong_dtype = npy("<f4", false, &[2, 2, 2], &[0; 32]);
    let wrong_order = npy("|u1", true, &[2, 2, 2], &[0; 8]);
    let mut truncated = npy("|u1", false, &[2, 2, 2], &[0; 8]);
    truncated.pop();

    assert_eq!(
        Labels::decode(&wrong_dtype, her.classes(), lo, hi, 0.12).err().as_deref(),
        Some("material field dtype is '<f4', expected '|u1'"),
    );
    assert_eq!(
        Labels::decode(&wrong_order, her.classes(), lo, hi, 0.12).err().as_deref(),
        Some("material field is Fortran-order, expected C-order"),
    );
    assert_eq!(
        Labels::decode(&truncated, her.classes(), lo, hi, 0.12).err().as_deref(),
        Some("material field refused: NumPy payload is 7 bytes, expected 8 from shape and dtype"),
    );
}
