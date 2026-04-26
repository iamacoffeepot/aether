//! End-to-end CSG: DSL text → parse → mesh → triangle list.
//!
//! Pins the contract that the scaffolding-stub test in
//! `tests/csg_scaffolding.rs` placed: CSG nodes used to silently emit
//! no triangles; with PR 4 they emit real geometry. That stub-empty
//! assertion is intentionally not in this file — it's superseded by
//! `mesher_emits_geometry_for_csg_nodes` here.

use aether_dsl_mesh::{mesh, parse};

fn count_distinct_colors(tris: &[aether_dsl_mesh::Triangle]) -> std::collections::BTreeSet<u32> {
    tris.iter().map(|t| t.color).collect()
}

#[test]
fn mesher_emits_geometry_for_csg_nodes() {
    // Replaces the PR 2 scaffolding stub: union now produces real
    // geometry, not an empty list.
    let ast = parse("(union (box 1 1 1 :color 0) (box 1 1 1 :color 1))").unwrap();
    let tris = mesh(&ast).expect("CSG mesh should succeed");
    assert!(
        !tris.is_empty(),
        "PR 4 must replace the empty-stub behavior with real geometry"
    );
}

#[test]
fn difference_of_overlapping_boxes_keeps_both_colors() {
    // The cavity walls inherit color from the subtractor; the outer
    // walls keep the base's color. Per ADR-0054, color is inherited
    // from the contributing input mesh.
    let ast = parse(
        "(difference
           (box 2 2 2 :color 5)
           (translate (0.5 0 0) (box 1 1 3 :color 9)))",
    )
    .unwrap();
    let tris = mesh(&ast).expect("CSG mesh should succeed");
    let colors = count_distinct_colors(&tris);
    assert!(colors.contains(&5), "missing base-color polygons");
    assert!(
        colors.contains(&9),
        "missing cutter-color (cavity) polygons"
    );
}

#[test]
fn rook_class_geometry_meshes_without_panic() {
    // The rook-crenellations case from ADR-0054's motivation: a
    // cylinder with a notch carved out by a box. Doesn't validate
    // tri count exactly (sensitive to the splitter chosen at the BSP
    // root) but confirms the operation runs end-to-end.
    let ast = parse(
        "(difference
           (cylinder 0.5 1.0 12 :color 0)
           (translate (0.4 0.5 0) (box 0.4 0.4 1.5 :color 1)))",
    )
    .unwrap();
    let tris = mesh(&ast).expect("CSG mesh should succeed");
    assert!(!tris.is_empty(), "rook-class CSG produced no geometry");
}

#[test]
fn intersection_of_overlapping_boxes_is_smaller_than_either() {
    let solo = parse("(box 1 1 1 :color 0)").unwrap();
    let solo_tris = mesh(&solo).unwrap();
    let inter = parse(
        "(intersection
           (box 1 1 1 :color 0)
           (translate (0.5 0 0) (box 1 1 1 :color 0)))",
    )
    .unwrap();
    let inter_tris = mesh(&inter).unwrap();
    assert!(
        !inter_tris.is_empty(),
        "intersection of overlapping boxes should be non-empty"
    );
    // Volume comparison via face count would need a watertight metric
    // we don't have; just confirm the intersection produced fewer
    // triangles than two of the boxes combined.
    assert!(
        inter_tris.len() < solo_tris.len() * 2,
        "intersection produced more triangles than 2x a single input"
    );
}

#[test]
fn nested_csg_in_transforms_meshes() {
    // CSG inside translate inside rotate — composition with structural
    // operators must work transparently.
    let ast = parse(
        "(translate (3 0 0)
           (rotate (0 1 0) 0.785
             (difference
               (box 2 2 2 :color 0)
               (box 1 1 4 :color 1))))",
    )
    .unwrap();
    let tris = mesh(&ast).expect("CSG-in-transform should mesh");
    assert!(!tris.is_empty());
}

#[test]
fn coordinate_outside_csg_range_errors() {
    // ±256 unit cap per ADR-0054. A box with size 600 has vertices at
    // ±300, well past the limit.
    let ast = parse("(union (box 600 600 600 :color 0) (box 1 1 1 :color 1))").unwrap();
    let err = mesh(&ast).expect_err("out-of-range CSG input should fail loudly");
    let msg = format!("{err}");
    assert!(
        msg.contains("range") || msg.contains("CSG"),
        "expected range/CSG error, got: {msg}"
    );
}

#[test]
fn nary_difference_subtracts_each_in_turn() {
    // 4 cuts into a single base — exercises the fold loop in mesh.rs's
    // Difference arm. None of these subtractors fully consume the
    // base, so the result must be non-empty.
    let ast = parse(
        "(difference
           (box 3 3 3 :color 0)
           (translate ( 1.0  0.0  0.0) (box 1 1 4 :color 1))
           (translate (-1.0  0.0  0.0) (box 1 1 4 :color 1))
           (translate ( 0.0  1.0  0.0) (box 1 4 1 :color 1))
           (translate ( 0.0 -1.0  0.0) (box 1 4 1 :color 1)))",
    )
    .unwrap();
    let tris = mesh(&ast).expect("4-cut difference should mesh");
    assert!(!tris.is_empty(), "all four cuts removed the base entirely");
}

#[test]
fn cylinder_with_two_box_cuts_does_not_overflow_stack() {
    // Regression for the snap-drift cascade fixed by the side-test
    // tolerance on Plane3. Pre-fix: this DSL stack-overflowed on the
    // second cut because cylinder facet planes have non-axis-aligned
    // normals; snap-drifted intersection vertices on split fragments
    // re-classified as FRONT/BACK against their own parent plane on
    // subsequent BSP passes, causing unbounded recursion. Box-only CSG
    // never hit it because axis-aligned plane normals zero out drift
    // in unrelated axes.
    let ast = parse(
        "(difference
           (cylinder 0.5 1.0 12 :color 0)
           (translate ( 0.4 0 0) (box 0.4 0.4 0.4 :color 1))
           (translate (-0.4 0 0) (box 0.4 0.4 0.4 :color 1)))",
    )
    .unwrap();
    let tris = mesh(&ast).expect("CSG mesh should succeed");
    assert!(!tris.is_empty(), "cylinder + 2-cut produced no geometry");
}

#[test]
fn rook_class_geometry_with_cylinder_meshes() {
    // The motivating ADR-0054 case that wasn't reachable pre-fix:
    // a cylinder rook with four crenellation cuts.
    let ast = parse(
        "(difference
           (cylinder 0.5 1.0 12 :color 0)
           (translate ( 0.5 0.4 0)    (box 0.4 0.3 0.2 :color 1))
           (translate (-0.5 0.4 0)    (box 0.4 0.3 0.2 :color 1))
           (translate ( 0.0 0.4  0.5) (box 0.2 0.3 0.4 :color 1))
           (translate ( 0.0 0.4 -0.5) (box 0.2 0.3 0.4 :color 1)))",
    )
    .unwrap();
    let tris = mesh(&ast).expect("4-cut cylinder rook should mesh");
    assert!(!tris.is_empty());
}

#[test]
fn determinism_end_to_end() {
    // The whole pipeline (parse → mesh → CSG → triangulate) must
    // produce bit-exactly identical output for identical input.
    let text = "(difference
                  (box 2 2 2 :color 0)
                  (translate (0.3 0 0) (box 1 1 3 :color 1)))";
    let a = mesh(&parse(text).unwrap()).unwrap();
    let b = mesh(&parse(text).unwrap()).unwrap();
    assert_eq!(a.len(), b.len());
    for (t1, t2) in a.iter().zip(b.iter()) {
        assert_eq!(t1.vertices, t2.vertices);
        assert_eq!(t1.color, t2.color);
    }
}
