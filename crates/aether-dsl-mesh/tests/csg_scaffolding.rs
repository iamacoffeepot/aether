//! Parser, serializer, and mesher-stub tests for the CSG operators
//! added by ADR-0054. The mesher itself is a stub — these tests cover
//! AST shape, arity rules, round-tripping, and that mesher dispatch
//! does not error on the new variants (it just emits no triangles).

use aether_dsl_mesh::parse::ParseError;
use aether_dsl_mesh::{Node, mesh, parse, serialize};
use pretty_assertions::assert_eq;

fn assert_round_trip(text: &str) {
    let ast = parse(text).expect("parse should succeed");
    let reserialized = serialize(&ast);
    let ast2 = parse(&reserialized).expect("re-parse should succeed");
    assert_eq!(
        ast, ast2,
        "round-trip not equal — reserialized text:\n{reserialized}"
    );
}

#[test]
fn parse_union_two_children() {
    let ast = parse("(union (box 1 1 1 :color 0) (box 1 1 1 :color 1))")
        .expect("union with two children should parse");
    let Node::Union { children } = ast else {
        panic!("expected Node::Union");
    };
    assert_eq!(children.len(), 2);
}

#[test]
fn parse_intersection_three_children() {
    let ast = parse(
        "(intersection (sphere 0.5 2 :color 0) (box 1 1 1 :color 1) (cylinder 0.4 1 8 :color 2))",
    )
    .expect("intersection with three children should parse");
    let Node::Intersection { children } = ast else {
        panic!("expected Node::Intersection");
    };
    assert_eq!(children.len(), 3);
}

#[test]
fn parse_difference_base_plus_one() {
    let ast = parse("(difference (cylinder 0.5 1 12 :color 0) (box 0.2 0.2 1.5 :color 1))")
        .expect("difference with base + one subtractor should parse");
    let Node::Difference { subtract, .. } = ast else {
        panic!("expected Node::Difference");
    };
    assert_eq!(subtract.len(), 1);
}

#[test]
fn parse_difference_base_plus_many() {
    let ast = parse(
        "(difference
            (cylinder 0.5 1 12 :color 0)
            (box 0.2 0.2 1.5 :color 1)
            (box 1.5 0.2 0.2 :color 1)
            (box 0.2 1.5 0.2 :color 1))",
    )
    .expect("difference with multiple subtractors should parse");
    let Node::Difference { subtract, .. } = ast else {
        panic!("expected Node::Difference");
    };
    assert_eq!(subtract.len(), 3);
}

#[test]
fn union_requires_at_least_two_children() {
    let err = parse("(union (box 1 1 1 :color 0))").unwrap_err();
    assert!(
        matches!(
            err,
            ParseError::CsgArityTooFew {
                node: "union",
                min: 2,
                got: 1
            }
        ),
        "got: {err:?}"
    );
}

#[test]
fn intersection_requires_at_least_two_children() {
    let err = parse("(intersection (box 1 1 1 :color 0))").unwrap_err();
    assert!(
        matches!(
            err,
            ParseError::CsgArityTooFew {
                node: "intersection",
                min: 2,
                got: 1
            }
        ),
        "got: {err:?}"
    );
}

#[test]
fn difference_requires_base_plus_at_least_one_subtractor() {
    let err = parse("(difference (cylinder 0.5 1 12 :color 0))").unwrap_err();
    assert!(
        matches!(
            err,
            ParseError::CsgArityTooFew {
                node: "difference",
                min: 2,
                got: 1
            }
        ),
        "got: {err:?}"
    );
}

#[test]
fn empty_csg_node_fails() {
    assert!(parse("(union)").is_err());
    assert!(parse("(intersection)").is_err());
    assert!(parse("(difference)").is_err());
}

#[test]
fn csg_rejects_unknown_keyword() {
    let err = parse("(union (box 1 1 1 :color 0) (box 1 1 1 :color 1) :foo bar)").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnknownKeyword { node: "union", .. }
    ));
}

#[test]
fn round_trip_union() {
    assert_round_trip("(union (box 1 1 1 :color 0) (box 1 1 1 :color 1) (box 1 1 1 :color 2))");
}

#[test]
fn round_trip_intersection() {
    assert_round_trip("(intersection (sphere 0.5 2 :color 0) (box 0.8 0.8 0.8 :color 1))");
}

#[test]
fn round_trip_difference() {
    assert_round_trip(
        "(difference (cylinder 0.5 1 12 :color 0) (box 0.2 0.2 1.5 :color 1) (box 1.5 0.2 0.2 :color 1))",
    );
}

#[test]
fn round_trip_csg_inside_transforms() {
    assert_round_trip(
        "(translate (0 1 0)
           (rotate (0 1 0) 0.785
             (difference
               (cylinder 0.5 1 12 :color 0)
               (box 0.2 0.2 1.5 :color 1))))",
    );
}

#[test]
fn round_trip_csg_inside_csg() {
    assert_round_trip(
        "(difference
           (union (box 1 1 1 :color 0) (sphere 0.7 2 :color 0))
           (intersection (box 0.5 0.5 1.5 :color 1) (cylinder 0.4 1.5 8 :color 1)))",
    );
}

#[test]
fn mesher_stub_emits_empty_for_csg_nodes() {
    // Until PR 4 lands, the CSG arms in mesh.rs return an empty Vec.
    // This test pins that contract so PR 4 obviously breaks it (and the
    // test then becomes a real geometric check).
    let ast = parse("(union (box 1 1 1 :color 0) (box 1 1 1 :color 1))").unwrap();
    let tris = mesh(&ast).expect("CSG mesher stub should not error");
    assert!(
        tris.is_empty(),
        "CSG mesher stub must emit no triangles until PR 4; got {} tris",
        tris.len()
    );
}

#[test]
fn mesher_traverses_into_csg_children_for_other_nodes() {
    // A composition wrapping a CSG node still meshes its non-CSG
    // siblings — verifying the dispatch doesn't shortcut the whole tree.
    let ast = parse(
        "(composition
           (box 1 1 1 :color 0)
           (union (sphere 0.5 2 :color 0) (sphere 0.5 2 :color 1)))",
    )
    .unwrap();
    let tris = mesh(&ast).expect("composition with CSG inside should not error");
    assert!(
        !tris.is_empty(),
        "the box sibling should still emit triangles even though the union stub does not"
    );
}
