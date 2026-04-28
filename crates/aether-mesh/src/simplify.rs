//! AST simplification: pure `Node → Node` rewrites that preserve the
//! mesh result while reducing work the mesher has to do.
//!
//! Active rewrites are all identity-preserving collapses:
//! - No-op transforms (`translate (0 0 0)`, `rotate _ 0`, `scale (1 1 1)`,
//!   `array 1`).
//! - Adjacent-transform folds (`translate a (translate b X)` →
//!   `translate (a+b) X`; same shape for `rotate` with parallel axes
//!   and `scale` with component-wise multiply).
//! - Single-child `composition` unwrap.
//!
//! Heavier CSG-specific rewrites (disjoint-AABB partitioning, lathe
//! wedge decomposition, distribute-difference-over-union) were retired
//! along with CSG itself in ADR-0062.

use crate::ast::Node;
use aether_math::Vec3;

/// Recursively simplify `node`. Pure transformation: input + output
/// always describe the same mesh.
pub fn simplify(node: &Node) -> Node {
    match node {
        Node::Box { .. }
        | Node::Cylinder { .. }
        | Node::Cone { .. }
        | Node::Wedge { .. }
        | Node::Sphere { .. }
        | Node::Lathe { .. }
        | Node::Extrude { .. }
        | Node::Torus { .. }
        | Node::Sweep { .. } => node.clone(),

        Node::Composition(children) => {
            let simplified: Vec<Node> = children.iter().map(simplify).collect();
            if simplified.len() == 1 {
                return simplified.into_iter().next().unwrap();
            }
            Node::Composition(simplified)
        }

        Node::Translate { offset, child } => {
            let child = simplify(child);
            let (offset, child) = if let Node::Translate {
                offset: inner_offset,
                child: inner_child,
            } = child
            {
                (*offset + inner_offset, *inner_child)
            } else {
                (*offset, child)
            };
            if offset == Vec3::ZERO {
                return child;
            }
            Node::Translate {
                offset,
                child: Box::new(child),
            }
        }

        Node::Rotate { axis, angle, child } => {
            let child = simplify(child);
            let (angle, child) = if let Node::Rotate {
                axis: inner_axis,
                angle: inner_angle,
                child: inner_child,
            } = &child
            {
                match parallel_sign(*axis, *inner_axis) {
                    Some(sign) => (*angle + sign * *inner_angle, (**inner_child).clone()),
                    None => (*angle, child),
                }
            } else {
                (*angle, child)
            };
            if angle == 0.0 {
                return child;
            }
            Node::Rotate {
                axis: *axis,
                angle,
                child: Box::new(child),
            }
        }

        Node::Scale { factor, child } => {
            let child = simplify(child);
            let (factor, child) = if let Node::Scale {
                factor: inner_factor,
                child: inner_child,
            } = child
            {
                (
                    Vec3::new(
                        factor.x * inner_factor.x,
                        factor.y * inner_factor.y,
                        factor.z * inner_factor.z,
                    ),
                    *inner_child,
                )
            } else {
                (*factor, child)
            };
            if factor == Vec3::ONE {
                return child;
            }
            Node::Scale {
                factor,
                child: Box::new(child),
            }
        }

        Node::Mirror { axis, child } => Node::Mirror {
            axis: *axis,
            child: Box::new(simplify(child)),
        },

        Node::Array {
            count,
            spacing,
            child,
        } => {
            let child = simplify(child);
            if *count == 1 {
                return child;
            }
            Node::Array {
                count: *count,
                spacing: *spacing,
                child: Box::new(child),
            }
        }
    }
}

/// `Some(+1.0)` if `a` and `b` point in the same direction (within
/// tolerance), `Some(-1.0)` if antiparallel, `None` otherwise. Used by
/// the rotate-fold rewrite to decide whether two adjacent rotations
/// can collapse.
fn parallel_sign(a: Vec3, b: Vec3) -> Option<f32> {
    let na = a.normalize_or(Vec3::Y);
    let nb = b.normalize_or(Vec3::Y);
    let dot = na.dot(nb);
    const TOL: f32 = 1e-4;
    if dot > 1.0 - TOL {
        Some(1.0)
    } else if dot < -1.0 + TOL {
        Some(-1.0)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_node() -> Node {
        Node::Box {
            x: 1.0,
            y: 1.0,
            z: 1.0,
            color: 0,
        }
    }

    #[test]
    fn translate_zero_collapses() {
        let n = Node::Translate {
            offset: Vec3::ZERO,
            child: Box::new(box_node()),
        };
        assert_eq!(simplify(&n), box_node());
    }

    #[test]
    fn nested_translate_folds() {
        let n = Node::Translate {
            offset: Vec3::new(1.0, 0.0, 0.0),
            child: Box::new(Node::Translate {
                offset: Vec3::new(0.0, 2.0, 0.0),
                child: Box::new(box_node()),
            }),
        };
        let simplified = simplify(&n);
        match simplified {
            Node::Translate { offset, child } => {
                assert_eq!(offset, Vec3::new(1.0, 2.0, 0.0));
                assert_eq!(*child, box_node());
            }
            other => panic!("expected fused Translate, got {other:?}"),
        }
    }

    #[test]
    fn rotate_zero_collapses() {
        let n = Node::Rotate {
            axis: Vec3::Y,
            angle: 0.0,
            child: Box::new(box_node()),
        };
        assert_eq!(simplify(&n), box_node());
    }

    #[test]
    fn parallel_rotates_fold() {
        let n = Node::Rotate {
            axis: Vec3::Y,
            angle: 0.5,
            child: Box::new(Node::Rotate {
                axis: Vec3::Y,
                angle: 0.25,
                child: Box::new(box_node()),
            }),
        };
        match simplify(&n) {
            Node::Rotate { angle, .. } => assert!((angle - 0.75).abs() < 1e-6),
            other => panic!("expected fused Rotate, got {other:?}"),
        }
    }

    #[test]
    fn scale_one_collapses() {
        let n = Node::Scale {
            factor: Vec3::ONE,
            child: Box::new(box_node()),
        };
        assert_eq!(simplify(&n), box_node());
    }

    #[test]
    fn array_one_collapses() {
        let n = Node::Array {
            count: 1,
            spacing: Vec3::new(1.0, 0.0, 0.0),
            child: Box::new(box_node()),
        };
        assert_eq!(simplify(&n), box_node());
    }

    #[test]
    fn single_child_composition_unwraps() {
        let n = Node::Composition(vec![box_node()]);
        assert_eq!(simplify(&n), box_node());
    }
}
