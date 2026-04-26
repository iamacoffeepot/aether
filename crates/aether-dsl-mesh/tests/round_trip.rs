//! Round-trip property: `parse(serialize(parse(text))) == parse(text)`.
//!
//! Whitespace and exact formatting are not preserved across a round trip,
//! but the AST must be structurally identical.

use aether_dsl_mesh::{parse, serialize};
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
fn round_trip_box_from_examples() {
    let text = include_str!("../examples/box.dsl");
    assert_round_trip(text);
}

#[test]
fn round_trip_lamp_post_from_examples() {
    let text = include_str!("../examples/lamp_post.dsl");
    assert_round_trip(text);
}

#[test]
fn round_trip_every_primitive() {
    let text = "(composition
        (box 1 2 3 :color 0)
        (cylinder 0.5 1 8 :color 1)
        (cone 0.5 1 8 :color 2)
        (wedge 1 1 1 :color 3)
        (sphere 0.5 2 :color 4)
        (lathe ((0 0) (1 0) (1 2) (0 2)) 12 :color 5)
        (extrude ((0 0) (1 0) (1 1) (0 1)) 0.5 :color 6))";
    assert_round_trip(text);
}

#[test]
fn round_trip_every_structural_op() {
    let text = "(composition
        (translate (1 2 3) (box 1 1 1 :color 0))
        (rotate (0 1 0) 1.5708 (box 1 1 1 :color 0))
        (scale (2 1 0.5) (box 1 1 1 :color 0))
        (mirror x (box 1 1 1 :color 0))
        (mirror y (box 1 1 1 :color 0))
        (mirror z (box 1 1 1 :color 0))
        (array 4 (1 0 0) (sphere 0.2 1 :color 0)))";
    assert_round_trip(text);
}

#[test]
fn round_trip_nested_transforms() {
    let text = "(translate (1 0 0)
        (rotate (0 0 1) 0.785
          (mirror x
            (array 3 (0 0.5 0)
              (cylinder 0.1 0.4 6 :color 7)))))";
    assert_round_trip(text);
}

#[test]
fn parse_errors_on_unknown_node() {
    let result = parse("(teapot 1 2 3 :color 0)");
    assert!(result.is_err(), "unknown node should fail to parse");
}

#[test]
fn parse_errors_on_missing_color() {
    let result = parse("(box 1 1 1)");
    assert!(result.is_err(), "missing :color should fail");
}

#[test]
fn parse_errors_on_unknown_keyword() {
    let result = parse("(box 1 1 1 :color 0 :material shiny)");
    assert!(result.is_err(), "unknown keyword should fail");
}

#[test]
fn parse_errors_on_invalid_axis() {
    let result = parse("(mirror w (box 1 1 1 :color 0))");
    assert!(result.is_err(), "invalid axis should fail");
}
