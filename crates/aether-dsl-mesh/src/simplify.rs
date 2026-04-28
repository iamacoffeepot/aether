//! AST simplification: pure `Node → Node` rewrites that preserve the
//! mesh result while reducing the work the mesher has to do.
//!
//! This module is the AST-level lever for issue 300's BSP fragmentation
//! cliff. It ships:
//!
//! - [`Aabb`]: an axis-aligned bounding box with the standard set ops.
//! - [`compute_aabb`]: per-`Node` conservative bound, used by AABB-
//!   pruning rewrites to decide when a CSG composition is trivial.
//! - [`simplify`]: the rewrite driver. [`crate::mesh::mesh`] runs it as
//!   a pre-pass, so every input the mesher ever sees has already been
//!   normalized.
//!
//! Active rewrites:
//! - Identity collapse (no-op transforms, single-child wrappers).
//! - **Disjoint-union → composition**: a `(union A B …)` whose children
//!   partition into disjoint-AABB groups becomes
//!   `(composition (union group_1) (union group_2) …)`. Single-element
//!   groups unwrap to the bare child, so a fully-disjoint union of N
//!   primitives becomes a flat composition of N nodes — and the
//!   mesher's [`Node::Composition`] arm just concatenates polygon
//!   streams, skipping BSP entirely. This is the big single-PR win for
//!   "scene = bunch of separated parts" inputs.

#[cfg(test)]
use crate::ast::Axis;
use crate::ast::Node;
use aether_math::{Aabb, Vec3};

/// Conservative AABB enclosing the polygon stream `node` would emit
/// when meshed at the origin (caller-supplied offsets aren't applied —
/// they're folded into surrounding `Translate` nodes by construction).
///
/// "Conservative" means: the returned AABB is guaranteed to contain
/// every emitted polygon, but may be larger than the tight bound — the
/// rotated-AABB-of-corners step in particular never under-estimates.
/// CSG ops use the pre-CSG bounds (union of inputs) since BSP can't
/// produce geometry outside the input solids.
pub fn compute_aabb(node: &Node) -> Aabb {
    match node {
        Node::Box { x, y, z, .. } => Aabb::from_half_extents(*x * 0.5, *y * 0.5, *z * 0.5),
        Node::Cylinder { radius, height, .. } => {
            Aabb::from_half_extents(*radius, *height * 0.5, *radius)
        }
        Node::Cone { radius, height, .. } => {
            Aabb::from_half_extents(*radius, *height * 0.5, *radius)
        }
        Node::Wedge { x, y, z, .. } => Aabb::from_half_extents(*x * 0.5, *y * 0.5, *z * 0.5),
        Node::Sphere { radius, .. } => Aabb::from_half_extents(*radius, *radius, *radius),
        Node::LatheSegment { profile, .. } => {
            // Conservative: same bound as the parent lathe. A tighter
            // per-wedge bound would let pairwise-disjoint segments
            // route through the disjoint-union fast path, but adjacent
            // segments share radial walls so they'd never partition
            // out anyway. The win that matters — distribute-difference-
            // over-union (PR 4) doing per-arm BSP on small inputs —
            // doesn't depend on per-segment AABB tightness.
            compute_aabb(&Node::Lathe {
                profile: profile.clone(),
                segments: 0,
                color: 0,
            })
        }
        Node::Lathe { profile, .. } => {
            // Lathe revolves around Y axis. Radial extent is max |x| of
            // the profile; Y extent spans the profile's y range.
            let mut max_r = 0.0f32;
            let mut min_y = f32::INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for &[r, y] in profile {
                max_r = max_r.max(r.abs());
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
            if !min_y.is_finite() {
                return Aabb::EMPTY;
            }
            Aabb::from_min_max(
                Vec3::new(-max_r, min_y, -max_r),
                Vec3::new(max_r, max_y, max_r),
            )
        }
        Node::Extrude { profile, depth, .. } => {
            let mut min_x = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for &[x, y] in profile {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
            if !min_x.is_finite() {
                return Aabb::EMPTY;
            }
            // Extrude pushes profile along +Z by `depth` from z=0.
            let (z0, z1) = if *depth >= 0.0 {
                (0.0, *depth)
            } else {
                (*depth, 0.0)
            };
            Aabb::from_min_max(Vec3::new(min_x, min_y, z0), Vec3::new(max_x, max_y, z1))
        }
        Node::Torus {
            major_radius,
            minor_radius,
            ..
        } => {
            // Torus is around Y axis. Radial extent = major + minor;
            // Y extent = ±minor.
            let r = major_radius + minor_radius;
            Aabb::from_half_extents(r, *minor_radius, r)
        }
        Node::Sweep {
            profile,
            path,
            scales,
            ..
        } => {
            // Conservative bound: at each waypoint, take the worst-case
            // ring radius (max |profile| × scale at that waypoint) and
            // produce a sphere-like box around the waypoint. Union them.
            let mut max_profile_r = 0.0f32;
            for &[x, y] in profile {
                max_profile_r = max_profile_r.max((x * x + y * y).sqrt());
            }
            let mut out = Aabb::EMPTY;
            for (k, p) in path.iter().enumerate() {
                let s = scales
                    .as_ref()
                    .and_then(|s| s.get(k))
                    .copied()
                    .unwrap_or(1.0);
                let r = max_profile_r * s.abs();
                out = out.union(&Aabb::from_min_max(
                    *p - Vec3::splat(r),
                    *p + Vec3::splat(r),
                ));
            }
            out
        }
        Node::Composition(children) => children
            .iter()
            .map(compute_aabb)
            .fold(Aabb::EMPTY, |acc, b| acc.union(&b)),
        Node::Translate { offset, child } => compute_aabb(child).translate(*offset),
        Node::Rotate { axis, angle, child } => compute_aabb(child).rotate(*axis, *angle),
        Node::Scale { factor, child } => compute_aabb(child).scale(*factor),
        Node::Mirror { axis, child } => compute_aabb(child).mirror(axis.index()),
        Node::Array {
            count,
            spacing,
            child,
        } => {
            if *count == 0 {
                return Aabb::EMPTY;
            }
            let base = compute_aabb(child);
            let mut out = Aabb::EMPTY;
            for i in 0..*count {
                let f = i as f32;
                out = out.union(&base.translate(*spacing * f));
            }
            out
        }
        // CSG result is contained by the union of its inputs (BSP can
        // only carve, never extrude). For union/intersection that's
        // exactly tight; for difference it's conservatively the base
        // bound (subtractors can't enlarge).
        Node::Union { children } => children
            .iter()
            .map(compute_aabb)
            .fold(Aabb::EMPTY, |acc, b| acc.union(&b)),
        Node::Intersection { children } => {
            let mut iter = children.iter();
            let first = match iter.next() {
                Some(n) => compute_aabb(n),
                None => return Aabb::EMPTY,
            };
            iter.fold(first, |acc, n| acc.intersection(&compute_aabb(n)))
        }
        Node::Difference { base, .. } => compute_aabb(base),
    }
}

/// Partition `children` into groups whose AABBs are pairwise disjoint
/// across groups (touching counts as overlapping — see
/// [`Aabb::intersects`]). Within a group, every member is connected to
/// every other through a chain of intersecting-AABB pairs.
///
/// Returns groups as lists of indices into `children`, in stable
/// first-appearance order so the rewrite output is deterministic.
///
/// O(n²) pairwise check + iterative union-find — fine for the handful
/// of children CSG ops typically have.
fn partition_disjoint_aabb(children: &[Node]) -> Vec<Vec<usize>> {
    let n = children.len();
    if n == 0 {
        return Vec::new();
    }
    let aabbs: Vec<Aabb> = children.iter().map(compute_aabb).collect();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut [usize], mut x: usize) -> usize {
        let mut root = x;
        while parent[root] != root {
            root = parent[root];
        }
        while parent[x] != root {
            let next = parent[x];
            parent[x] = root;
            x = next;
        }
        root
    }

    for i in 0..n {
        for j in (i + 1)..n {
            if aabbs[i].intersects(&aabbs[j]) {
                let ri = find(&mut parent, i);
                let rj = find(&mut parent, j);
                if ri != rj {
                    parent[ri] = rj;
                }
            }
        }
    }

    // Bucket by root, preserving first-appearance order of group roots
    // so the output is deterministic across runs.
    let mut order: Vec<usize> = Vec::new();
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        if !groups.contains_key(&r) {
            order.push(r);
        }
        groups.entry(r).or_default().push(i);
    }
    order
        .into_iter()
        .map(|r| groups.remove(&r).unwrap())
        .collect()
}

/// `true` when a lathe profile starts and ends at the axis (`r == 0`).
/// Such profiles produce a closed solid when revolved, which is the
/// precondition for wedge decomposition: each angular slice's radial
/// walls collapse to single axis points at top and bottom, so the
/// wall polygons close cleanly without an explicit cap.
fn profile_axis_closed(profile: &[[f32; 2]]) -> bool {
    profile.len() >= 2
        && profile.first().unwrap()[0].abs() < f32::EPSILON
        && profile.last().unwrap()[0].abs() < f32::EPSILON
}

/// Apply rewrites bottom-up. Each rewrite preserves the meshed output
/// exactly — a `mesh(simplify(n))` is `mesh(n)` for every input.
///
/// Identity collapse:
/// - `(translate (0 0 0) child)` → `child`
/// - `(rotate axis 0.0 child)` → `child`
/// - `(scale (1 1 1) child)` → `child`
/// - `(array 1 ... child)` → `child` (single-element array applies no
///   spacing; same as the bare child)
/// - `(composition (single))` → `single` (when the composition has
///   exactly one child)
///
/// Transform composition (fold adjacent same-kind transforms):
/// - `(translate a (translate b inner))` → `(translate a+b inner)`
/// - `(scale a (scale b inner))` → `(scale a*b inner)` (component-wise)
/// - `(rotate axis_a θa (rotate axis_b θb inner))` → `(rotate axis_a θa+θb inner)`
///   when `axis_a` and `axis_b` are parallel (same direction up to
///   sign — opposite-direction folds negate the inner angle). Skew
///   axes don't compose into a single axis-angle without a quaternion
///   intermediate; we leave those alone rather than introduce a
///   normalization-pass that could subtly drift floating-point.
///
/// After folding, the resulting transform may itself be an identity —
/// re-checking happens automatically via the rewrite's own identity
/// guard (so `(translate +x (translate -x leaf))` collapses to `leaf`
/// in one pass).
///
/// AABB pruning:
/// - `(union A B …)` partitioned into disjoint-AABB groups becomes
///   `(composition (union group_1) (union group_2) …)`. The mesher's
///   composition arm concatenates polygon streams instead of running
///   BSP, which is exact when the inputs share zero volume — the
///   resulting boundary loops are identical, and the root cleanup pass
///   handles welding the same way for either path.
/// - `(difference X [Y_1 … Y_n])` drops any subtractor `Y_i` whose
///   AABB doesn't touch the base's AABB — subtracting something
///   geometrically separate is a no-op. If every subtractor gets
///   pruned, the rewrite returns the bare base.
///
/// Algebraic distribution:
/// - `(difference (union A B …) Y_1 … Y_n)` distributes to
///   `(union (difference A Y_1 … Y_n) (difference B Y_1 … Y_n) …)`
///   when both the union's children and every subtractor are
///   CSG-leaves (per [`crate::mesh::is_csg_leaf`]). The set-algebra
///   identity is exact; we restrict to leaves to avoid amplifying
///   T-junctions/slivers from a composite operand. Distribution
///   unlocks per-arm AABB pruning of the inner differences and the
///   subsequent disjoint-union rewrite, which together collapse most
///   "bunch of parts minus one cutter" inputs to a flat composition
///   that skips BSP entirely.
///
/// Wedge decomposition:
/// - `(lathe profile segments color)` rewrites to
///   `(union seg_0 seg_1 … seg_{n-1})` of [`Node::LatheSegment`]
///   primitives when the profile is axis-closed (first and last
///   profile points at `r == 0`). The decomposed lathe meshes
///   identically to the original — adjacent segments share radial
///   walls that the BSP-union (and root cleanup) merge — but each
///   segment is a small convex-ish solid, so a CSG operation against
///   the lathe distributes (via the rule above) into per-segment BSP
///   work that's bounded per-pair instead of having one giant
///   operation over all `segments × profile-edges` facets.
///
/// Identity rules for `Mirror` are intentionally absent — every mirror
/// flips winding regardless of axis, so there's no zero-effect form.
pub fn simplify(node: &Node) -> Node {
    match node {
        // Primitives have no children to rewrite.
        Node::Box { .. }
        | Node::Cylinder { .. }
        | Node::Cone { .. }
        | Node::Wedge { .. }
        | Node::Sphere { .. }
        | Node::LatheSegment { .. }
        | Node::Extrude { .. }
        | Node::Torus { .. }
        | Node::Sweep { .. } => node.clone(),

        Node::Lathe {
            profile,
            segments,
            color,
        } => {
            // Wedge-decompose into a Union of LatheSegments when the
            // profile is axis-closed and there's at least 3 segments.
            // Non-axis-closed profiles can't form closed wedges (no
            // radial-wall closure at the axis), so we leave them as a
            // single Lathe and the caller's CSG ops bear the original
            // fragmentation cost.
            if *segments >= 3 && profile_axis_closed(profile) {
                let children: Vec<Node> = (0..*segments)
                    .map(|i| Node::LatheSegment {
                        profile: profile.clone(),
                        segments: *segments,
                        segment_index: i,
                        color: *color,
                    })
                    .collect();
                Node::Union { children }
            } else {
                node.clone()
            }
        }

        Node::Composition(children) => {
            let simplified: Vec<Node> = children.iter().map(simplify).collect();
            if simplified.len() == 1 {
                return simplified.into_iter().next().unwrap();
            }
            Node::Composition(simplified)
        }

        Node::Translate { offset, child } => {
            let child = simplify(child);
            // Fold (translate a (translate b X)) → (translate a+b X).
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
            // Fold (rotate ax θa (rotate ax_inner θb X)) → (rotate ax θa+θb X)
            // when the inner axis is parallel to the outer (same direction
            // up to sign). Opposite directions negate the inner angle.
            let (angle, child) = if let Node::Rotate {
                axis: inner_axis,
                angle: inner_angle,
                child: inner_child,
            } = &child
            {
                match axis.parallel_sign(*inner_axis) {
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
            // Fold (scale a (scale b X)) → (scale a*b X) component-wise.
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

        Node::Union { children } => {
            let simplified: Vec<Node> = children.iter().map(simplify).collect();
            let groups = partition_disjoint_aabb(&simplified);
            if groups.len() <= 1 {
                return Node::Union {
                    children: simplified,
                };
            }
            // Multiple disjoint groups: wrap each (single-child groups
            // unwrap to the bare child) and concat via Composition.
            let group_nodes: Vec<Node> = groups
                .into_iter()
                .map(|indices| {
                    let mut group_children: Vec<Node> =
                        indices.iter().map(|&i| simplified[i].clone()).collect();
                    if group_children.len() == 1 {
                        group_children.pop().unwrap()
                    } else {
                        Node::Union {
                            children: group_children,
                        }
                    }
                })
                .collect();
            Node::Composition(group_nodes)
        }
        Node::Intersection { children } => Node::Intersection {
            children: children.iter().map(simplify).collect(),
        },
        Node::Difference { base, subtract } => {
            let subtract: Vec<Node> = subtract.iter().map(simplify).collect();
            let base = simplify(base);

            // Try distribution on the simplified base. Three eligible
            // shapes — all preserve `(A ∪ B …) − Y = (A − Y) ∪ (B − Y) …`:
            //
            // 1. `Union { children }` with leaf children — the
            //    set-algebra identity, restricted to leaves to avoid
            //    amplifying composite-operand T-junctions/slivers.
            // 2. `Composition(children)` with pairwise-disjoint leaf
            //    children — Composition came from the disjoint-union
            //    rewrite, so its children represent set-disjoint regions
            //    and the same identity holds. Pairwise-disjoint check
            //    via `partition_disjoint_aabb` returning N singleton
            //    groups for N children.
            //
            // Subtractors must also be leaves (same safety guarantee).
            //
            // The Lathe wedge-decomposition rewrite makes this case
            // load-bearing for issue 300: `(difference lathe cutter)`
            // simplifies the Lathe into a Union of LatheSegments
            // *during base simplification*, and distribution then turns
            // it into N small per-segment differences instead of one
            // catastrophically-fragmenting BSP composition.
            let distributable_children: Option<&[Node]> = if subtract
                .iter()
                .all(crate::mesh::is_csg_leaf)
            {
                match &base {
                    Node::Union { children } if children.iter().all(crate::mesh::is_csg_leaf) => {
                        Some(children.as_slice())
                    }
                    Node::Composition(children)
                        if children.iter().all(crate::mesh::is_csg_leaf)
                            && partition_disjoint_aabb(children).len() == children.len() =>
                    {
                        Some(children.as_slice())
                    }
                    _ => None,
                }
            } else {
                None
            };

            if let Some(children) = distributable_children {
                let arms: Vec<Node> = children
                    .iter()
                    .map(|c| {
                        simplify(&Node::Difference {
                            base: Box::new(c.clone()),
                            subtract: subtract.clone(),
                        })
                    })
                    .collect();
                // Composition base → arms stay disjoint (each arm is a
                // Difference whose AABB is bounded by its leaf base,
                // and the original leaves were pairwise disjoint), so
                // wrap in Composition directly. Union base → wrap in
                // Union and re-simplify; the disjoint-union rewrite
                // may further collapse it if the arms turn out
                // pairwise-disjoint.
                return match &base {
                    Node::Composition(_) => Node::Composition(arms),
                    _ => simplify(&Node::Union { children: arms }),
                };
            }

            // No distribution — AABB-prune subtractors against the base.
            let base_aabb = compute_aabb(&base);
            let pruned: Vec<Node> = subtract
                .into_iter()
                .filter(|s| compute_aabb(s).intersects(&base_aabb))
                .collect();
            if pruned.is_empty() {
                return base;
            }
            Node::Difference {
                base: Box::new(base),
                subtract: pruned,
            }
        }
    }
}

#[cfg(test)]
mod aabb_tests {
    use super::*;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    fn aabb_approx_eq(a: &Aabb, b: &Aabb) -> bool {
        approx_eq(a.min.x, b.min.x)
            && approx_eq(a.min.y, b.min.y)
            && approx_eq(a.min.z, b.min.z)
            && approx_eq(a.max.x, b.max.x)
            && approx_eq(a.max.y, b.max.y)
            && approx_eq(a.max.z, b.max.z)
    }

    #[test]
    fn empty_is_empty() {
        assert!(Aabb::EMPTY.is_empty());
    }

    #[test]
    fn from_half_extents_is_centered() {
        let b = Aabb::from_half_extents(2.0, 3.0, 4.0);
        assert_eq!(b.min, Vec3::new(-2.0, -3.0, -4.0));
        assert_eq!(b.max, Vec3::new(2.0, 3.0, 4.0));
    }

    #[test]
    fn from_half_extents_takes_absolute_value() {
        // Negative extents would otherwise produce inverted (= empty)
        // bounds. Take abs so callers can't accidentally produce empty
        // boxes from primitives with negative size.
        let b = Aabb::from_half_extents(-2.0, 3.0, -4.0);
        assert_eq!(b.min, Vec3::new(-2.0, -3.0, -4.0));
        assert_eq!(b.max, Vec3::new(2.0, 3.0, 4.0));
    }

    #[test]
    fn union_with_empty_is_identity() {
        let b = Aabb::from_half_extents(1.0, 1.0, 1.0);
        assert_eq!(b.union(&Aabb::EMPTY), b);
        assert_eq!(Aabb::EMPTY.union(&b), b);
    }

    #[test]
    fn union_of_two_disjoint_boxes_spans_both() {
        let a = Aabb::from_min_max(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 1.0, 1.0));
        let b = Aabb::from_min_max(Vec3::new(10.0, 10.0, 10.0), Vec3::new(11.0, 11.0, 11.0));
        let u = a.union(&b);
        assert_eq!(u.min, Vec3::new(0.0, 0.0, 0.0));
        assert_eq!(u.max, Vec3::new(11.0, 11.0, 11.0));
    }

    #[test]
    fn intersection_of_overlapping_boxes_is_overlap() {
        let a = Aabb::from_min_max(Vec3::new(0.0, 0.0, 0.0), Vec3::new(2.0, 2.0, 2.0));
        let b = Aabb::from_min_max(Vec3::new(1.0, 1.0, 1.0), Vec3::new(3.0, 3.0, 3.0));
        let i = a.intersection(&b);
        assert_eq!(i.min, Vec3::new(1.0, 1.0, 1.0));
        assert_eq!(i.max, Vec3::new(2.0, 2.0, 2.0));
    }

    #[test]
    fn intersection_of_disjoint_boxes_is_empty() {
        let a = Aabb::from_min_max(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 1.0, 1.0));
        let b = Aabb::from_min_max(Vec3::new(10.0, 10.0, 10.0), Vec3::new(11.0, 11.0, 11.0));
        let i = a.intersection(&b);
        assert!(i.is_empty());
    }

    #[test]
    fn intersects_overlapping_returns_true() {
        let a = Aabb::from_min_max(Vec3::new(0.0, 0.0, 0.0), Vec3::new(2.0, 2.0, 2.0));
        let b = Aabb::from_min_max(Vec3::new(1.0, 1.0, 1.0), Vec3::new(3.0, 3.0, 3.0));
        assert!(a.intersects(&b));
        assert!(b.intersects(&a));
    }

    #[test]
    fn intersects_disjoint_returns_false() {
        let a = Aabb::from_min_max(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 1.0, 1.0));
        let b = Aabb::from_min_max(Vec3::new(10.0, 10.0, 10.0), Vec3::new(11.0, 11.0, 11.0));
        assert!(!a.intersects(&b));
    }

    #[test]
    fn intersects_touching_face_returns_true() {
        // Pin: AABBs that share a face (max == min along one axis) are
        // treated as intersecting. CSG callers can still optimize this
        // case if they want to — the intersects API just gives the
        // closed-set answer.
        let a = Aabb::from_min_max(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 1.0, 1.0));
        let b = Aabb::from_min_max(Vec3::new(1.0, 0.0, 0.0), Vec3::new(2.0, 1.0, 1.0));
        assert!(a.intersects(&b));
    }

    #[test]
    fn intersects_with_empty_is_false() {
        let a = Aabb::from_min_max(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 1.0, 1.0));
        assert!(!a.intersects(&Aabb::EMPTY));
        assert!(!Aabb::EMPTY.intersects(&a));
        assert!(!Aabb::EMPTY.intersects(&Aabb::EMPTY));
    }

    #[test]
    fn translate_shifts_bounds() {
        let b = Aabb::from_half_extents(1.0, 1.0, 1.0);
        let t = b.translate(Vec3::new(5.0, -3.0, 2.0));
        assert_eq!(t.min, Vec3::new(4.0, -4.0, 1.0));
        assert_eq!(t.max, Vec3::new(6.0, -2.0, 3.0));
    }

    #[test]
    fn translate_empty_is_empty() {
        let t = Aabb::EMPTY.translate(Vec3::new(5.0, 5.0, 5.0));
        assert!(t.is_empty());
    }

    #[test]
    fn scale_positive_factors_scales_in_place() {
        let b = Aabb::from_half_extents(1.0, 1.0, 1.0);
        let s = b.scale(Vec3::new(2.0, 3.0, 4.0));
        assert_eq!(s.min, Vec3::new(-2.0, -3.0, -4.0));
        assert_eq!(s.max, Vec3::new(2.0, 3.0, 4.0));
    }

    #[test]
    fn scale_negative_factor_swaps_min_max_along_axis() {
        // Pin: a -1 scale on x flips a [0, 5] range to [-5, 0]. Without
        // swap-on-negative the result would be the inverted (empty)
        // [0, -5].
        let b = Aabb::from_min_max(Vec3::new(0.0, 0.0, 0.0), Vec3::new(5.0, 1.0, 1.0));
        let s = b.scale(Vec3::new(-1.0, 1.0, 1.0));
        assert_eq!(s.min, Vec3::new(-5.0, 0.0, 0.0));
        assert_eq!(s.max, Vec3::new(0.0, 1.0, 1.0));
    }

    #[test]
    fn rotate_180_around_y_swaps_x_and_z_signs() {
        // Off-center box rotated 180° around Y: end up on the opposite
        // side along x and z, but still the same shape.
        let b = Aabb::from_min_max(Vec3::new(1.0, 0.0, 1.0), Vec3::new(3.0, 1.0, 3.0));
        let r = b.rotate(Vec3::new(0.0, 1.0, 0.0), std::f32::consts::PI);
        // Floating-point Rodrigues introduces small ULPs even for
        // exact 180° rotations; assert via approx.
        let expected = Aabb::from_min_max(Vec3::new(-3.0, 0.0, -3.0), Vec3::new(-1.0, 1.0, -1.0));
        assert!(
            aabb_approx_eq(&r, &expected),
            "rotated bounds {:?} ≠ expected {:?}",
            r,
            expected
        );
    }

    #[test]
    fn rotate_centered_box_45_degrees_around_z_grows_xy_extent() {
        // A 1x1 box rotated 45° around Z should fit in a 1.414x1.414
        // bound (the corner-to-corner diagonal). Pins the worst-case
        // conservative-bound behavior.
        let b = Aabb::from_half_extents(0.5, 0.5, 0.0);
        let r = b.rotate(Vec3::new(0.0, 0.0, 1.0), std::f32::consts::FRAC_PI_4);
        // sqrt(0.5) ≈ 0.7071
        let half_diag = std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            (r.max.x - half_diag).abs() < 1e-4,
            "expected ~{half_diag} along x, got {}",
            r.max.x
        );
        assert!(
            (r.max.y - half_diag).abs() < 1e-4,
            "expected ~{half_diag} along y, got {}",
            r.max.y
        );
    }

    #[test]
    fn mirror_x_flips_x_bounds() {
        let b = Aabb::from_min_max(Vec3::new(1.0, 2.0, 3.0), Vec3::new(4.0, 5.0, 6.0));
        let m = b.mirror(Axis::X.index());
        assert_eq!(m.min, Vec3::new(-4.0, 2.0, 3.0));
        assert_eq!(m.max, Vec3::new(-1.0, 5.0, 6.0));
    }

    #[test]
    fn mirror_y_flips_y_bounds() {
        let b = Aabb::from_min_max(Vec3::new(1.0, 2.0, 3.0), Vec3::new(4.0, 5.0, 6.0));
        let m = b.mirror(Axis::Y.index());
        assert_eq!(m.min, Vec3::new(1.0, -5.0, 3.0));
        assert_eq!(m.max, Vec3::new(4.0, -2.0, 6.0));
    }

    #[test]
    fn mirror_z_flips_z_bounds() {
        let b = Aabb::from_min_max(Vec3::new(1.0, 2.0, 3.0), Vec3::new(4.0, 5.0, 6.0));
        let m = b.mirror(Axis::Z.index());
        assert_eq!(m.min, Vec3::new(1.0, 2.0, -6.0));
        assert_eq!(m.max, Vec3::new(4.0, 5.0, -3.0));
    }
}

#[cfg(test)]
mod compute_aabb_tests {
    use super::*;

    #[test]
    fn box_is_centered_at_origin_with_half_extents() {
        let b = compute_aabb(&Node::Box {
            x: 2.0,
            y: 4.0,
            z: 6.0,
            color: 0,
        });
        assert_eq!(b.min, Vec3::new(-1.0, -2.0, -3.0));
        assert_eq!(b.max, Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn cylinder_radius_dominates_xz() {
        let b = compute_aabb(&Node::Cylinder {
            radius: 2.5,
            height: 3.0,
            segments: 16,
            color: 0,
        });
        assert_eq!(b.min, Vec3::new(-2.5, -1.5, -2.5));
        assert_eq!(b.max, Vec3::new(2.5, 1.5, 2.5));
    }

    #[test]
    fn cone_aabb_is_full_cylinder_bound() {
        // The bound is conservative: the cone sits inside a cylinder of
        // the same radius and height, so AABB matches a cylinder.
        let b = compute_aabb(&Node::Cone {
            radius: 1.0,
            height: 2.0,
            segments: 12,
            color: 0,
        });
        assert_eq!(b.min, Vec3::new(-1.0, -1.0, -1.0));
        assert_eq!(b.max, Vec3::new(1.0, 1.0, 1.0));
    }

    #[test]
    fn sphere_is_radius_in_all_axes() {
        let b = compute_aabb(&Node::Sphere {
            radius: 0.5,
            subdivisions: 12,
            color: 0,
        });
        assert_eq!(b.min, Vec3::new(-0.5, -0.5, -0.5));
        assert_eq!(b.max, Vec3::new(0.5, 0.5, 0.5));
    }

    #[test]
    fn lathe_radial_extent_is_max_abs_x() {
        let b = compute_aabb(&Node::Lathe {
            profile: vec![[0.0, -0.5], [0.7, -0.5], [0.7, 0.5], [0.0, 0.5]],
            segments: 16,
            color: 0,
        });
        assert_eq!(b.min, Vec3::new(-0.7, -0.5, -0.7));
        assert_eq!(b.max, Vec3::new(0.7, 0.5, 0.7));
    }

    #[test]
    fn lathe_with_empty_profile_is_empty() {
        let b = compute_aabb(&Node::Lathe {
            profile: vec![],
            segments: 16,
            color: 0,
        });
        assert!(b.is_empty());
    }

    #[test]
    fn torus_radial_is_major_plus_minor() {
        let b = compute_aabb(&Node::Torus {
            major_radius: 2.0,
            minor_radius: 0.3,
            major_segments: 16,
            minor_segments: 8,
            color: 0,
        });
        assert_eq!(b.min, Vec3::new(-2.3, -0.3, -2.3));
        assert_eq!(b.max, Vec3::new(2.3, 0.3, 2.3));
    }

    #[test]
    fn extrude_z_spans_zero_to_depth() {
        let b = compute_aabb(&Node::Extrude {
            profile: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 2.0], [0.0, 2.0]],
            depth: 5.0,
            color: 0,
        });
        assert_eq!(b.min, Vec3::new(0.0, 0.0, 0.0));
        assert_eq!(b.max, Vec3::new(1.0, 2.0, 5.0));
    }

    #[test]
    fn extrude_negative_depth_swaps_z_bounds() {
        // Degenerate but defined: negative depth extrudes backward.
        let b = compute_aabb(&Node::Extrude {
            profile: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]],
            depth: -3.0,
            color: 0,
        });
        assert_eq!(b.min.z, -3.0);
        assert_eq!(b.max.z, 0.0);
    }

    #[test]
    fn translate_offsets_child_bounds() {
        let b = compute_aabb(&Node::Translate {
            offset: Vec3::new(10.0, 20.0, 30.0),
            child: Box::new(Node::Box {
                x: 2.0,
                y: 2.0,
                z: 2.0,
                color: 0,
            }),
        });
        assert_eq!(b.min, Vec3::new(9.0, 19.0, 29.0));
        assert_eq!(b.max, Vec3::new(11.0, 21.0, 31.0));
    }

    #[test]
    fn rotate_grows_off_axis_box_bound() {
        // 4x1x1 box rotated 90° around Z: x-extent becomes 1, y-extent
        // becomes 4. Pin the conservative bound matches the rotated
        // shape.
        let b = compute_aabb(&Node::Rotate {
            axis: Vec3::new(0.0, 0.0, 1.0),
            angle: std::f32::consts::FRAC_PI_2,
            child: Box::new(Node::Box {
                x: 4.0,
                y: 1.0,
                z: 1.0,
                color: 0,
            }),
        });
        assert!((b.max.x - 0.5).abs() < 1e-4, "x_max {}", b.max.x);
        assert!((b.max.y - 2.0).abs() < 1e-4, "y_max {}", b.max.y);
    }

    #[test]
    fn scale_multiplies_child_bound() {
        let b = compute_aabb(&Node::Scale {
            factor: Vec3::new(2.0, 3.0, 4.0),
            child: Box::new(Node::Box {
                x: 1.0,
                y: 1.0,
                z: 1.0,
                color: 0,
            }),
        });
        assert_eq!(b.min, Vec3::new(-1.0, -1.5, -2.0));
        assert_eq!(b.max, Vec3::new(1.0, 1.5, 2.0));
    }

    #[test]
    fn mirror_flips_child_bound_along_axis() {
        let b = compute_aabb(&Node::Mirror {
            axis: Axis::X,
            child: Box::new(Node::Translate {
                offset: Vec3::new(5.0, 0.0, 0.0),
                child: Box::new(Node::Box {
                    x: 2.0,
                    y: 2.0,
                    z: 2.0,
                    color: 0,
                }),
            }),
        });
        // Box translated to (5,0,0), bounds [4..6]; mirror around x=0
        // gives [-6..-4].
        assert_eq!(b.min.x, -6.0);
        assert_eq!(b.max.x, -4.0);
    }

    #[test]
    fn array_unions_translated_child_bounds() {
        let b = compute_aabb(&Node::Array {
            count: 3,
            spacing: Vec3::new(10.0, 0.0, 0.0),
            child: Box::new(Node::Box {
                x: 2.0,
                y: 2.0,
                z: 2.0,
                color: 0,
            }),
        });
        // i=0: [-1..1], i=1: [9..11], i=2: [19..21] → union [-1..21].
        assert_eq!(b.min.x, -1.0);
        assert_eq!(b.max.x, 21.0);
    }

    #[test]
    fn array_count_zero_is_empty() {
        let b = compute_aabb(&Node::Array {
            count: 0,
            spacing: Vec3::new(1.0, 0.0, 0.0),
            child: Box::new(Node::Box {
                x: 1.0,
                y: 1.0,
                z: 1.0,
                color: 0,
            }),
        });
        assert!(b.is_empty());
    }

    #[test]
    fn composition_unions_children() {
        let b = compute_aabb(&Node::Composition(vec![
            Node::Box {
                x: 2.0,
                y: 2.0,
                z: 2.0,
                color: 0,
            },
            Node::Translate {
                offset: Vec3::new(10.0, 0.0, 0.0),
                child: Box::new(Node::Box {
                    x: 2.0,
                    y: 2.0,
                    z: 2.0,
                    color: 0,
                }),
            },
        ]));
        assert_eq!(b.min.x, -1.0);
        assert_eq!(b.max.x, 11.0);
    }

    #[test]
    fn empty_composition_is_empty() {
        assert!(compute_aabb(&Node::Composition(vec![])).is_empty());
    }

    #[test]
    fn union_takes_union_of_child_bounds() {
        let b = compute_aabb(&Node::Union {
            children: vec![
                Node::Box {
                    x: 2.0,
                    y: 2.0,
                    z: 2.0,
                    color: 0,
                },
                Node::Translate {
                    offset: Vec3::new(10.0, 0.0, 0.0),
                    child: Box::new(Node::Box {
                        x: 2.0,
                        y: 2.0,
                        z: 2.0,
                        color: 0,
                    }),
                },
            ],
        });
        assert_eq!(b.min.x, -1.0);
        assert_eq!(b.max.x, 11.0);
    }

    #[test]
    fn intersection_takes_intersection_of_child_bounds() {
        let b = compute_aabb(&Node::Intersection {
            children: vec![
                Node::Box {
                    x: 4.0,
                    y: 4.0,
                    z: 4.0,
                    color: 0,
                },
                Node::Translate {
                    offset: Vec3::new(1.0, 0.0, 0.0),
                    child: Box::new(Node::Box {
                        x: 4.0,
                        y: 4.0,
                        z: 4.0,
                        color: 0,
                    }),
                },
            ],
        });
        // Box[-2..2] ∩ Box[-1..3] = [-1..2]
        assert_eq!(b.min.x, -1.0);
        assert_eq!(b.max.x, 2.0);
    }

    #[test]
    fn intersection_of_disjoint_inputs_is_empty_bound() {
        let b = compute_aabb(&Node::Intersection {
            children: vec![
                Node::Box {
                    x: 1.0,
                    y: 1.0,
                    z: 1.0,
                    color: 0,
                },
                Node::Translate {
                    offset: Vec3::new(10.0, 0.0, 0.0),
                    child: Box::new(Node::Box {
                        x: 1.0,
                        y: 1.0,
                        z: 1.0,
                        color: 0,
                    }),
                },
            ],
        });
        assert!(b.is_empty());
    }

    #[test]
    fn difference_uses_base_bound_only() {
        // Pin: subtractor's bound doesn't enlarge the difference's
        // bound. Otherwise a tiny base minus a giant subtractor would
        // claim AABB(giant) — wrong, since BSP can't enlarge geometry.
        let b = compute_aabb(&Node::Difference {
            base: Box::new(Node::Box {
                x: 2.0,
                y: 2.0,
                z: 2.0,
                color: 0,
            }),
            subtract: vec![Node::Box {
                x: 100.0,
                y: 100.0,
                z: 100.0,
                color: 1,
            }],
        });
        assert_eq!(b.min, Vec3::new(-1.0, -1.0, -1.0));
        assert_eq!(b.max, Vec3::new(1.0, 1.0, 1.0));
    }

    #[test]
    fn sweep_bound_covers_every_waypoint_with_max_profile_radius() {
        let b = compute_aabb(&Node::Sweep {
            profile: vec![[1.0, 0.0], [0.0, 1.0], [-1.0, 0.0], [0.0, -1.0]],
            path: vec![Vec3::new(0.0, 0.0, 0.0), Vec3::new(10.0, 0.0, 0.0)],
            scales: None,
            open: false,
            color: 0,
        });
        // Profile worst-case radius = 1; bound at each waypoint extends
        // ±1 in every direction → union [(-1, -1, -1), (11, 1, 1)].
        assert_eq!(b.min, Vec3::new(-1.0, -1.0, -1.0));
        assert_eq!(b.max, Vec3::new(11.0, 1.0, 1.0));
    }

    #[test]
    fn sweep_with_scales_uses_per_waypoint_scale() {
        let b = compute_aabb(&Node::Sweep {
            profile: vec![[1.0, 0.0], [0.0, 1.0], [-1.0, 0.0], [0.0, -1.0]],
            path: vec![Vec3::new(0.0, 0.0, 0.0), Vec3::new(10.0, 0.0, 0.0)],
            scales: Some(vec![1.0, 3.0]),
            open: false,
            color: 0,
        });
        // Waypoint 0: ±1; waypoint 1 (scaled 3x): ±3. y/z bounds come
        // from waypoint 1 alone (largest).
        assert_eq!(b.min.y, -3.0);
        assert_eq!(b.max.y, 3.0);
    }
}

#[cfg(test)]
mod simplify_tests {
    use super::*;

    fn unit_box() -> Node {
        Node::Box {
            x: 1.0,
            y: 1.0,
            z: 1.0,
            color: 0,
        }
    }

    #[test]
    fn primitive_unchanged() {
        let n = unit_box();
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn zero_translate_is_stripped() {
        let n = Node::Translate {
            offset: Vec3::new(0.0, 0.0, 0.0),
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn nonzero_translate_is_preserved() {
        let n = Node::Translate {
            offset: Vec3::new(1.0, 0.0, 0.0),
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn zero_rotate_is_stripped_regardless_of_axis() {
        // Pin: even a non-normalized axis is fine — angle=0 means no
        // rotation no matter what axis is named.
        let n = Node::Rotate {
            axis: Vec3::new(1.0, 1.0, 1.0),
            angle: 0.0,
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn nonzero_rotate_is_preserved() {
        let n = Node::Rotate {
            axis: Vec3::new(0.0, 1.0, 0.0),
            angle: 0.5,
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn unit_scale_is_stripped() {
        let n = Node::Scale {
            factor: Vec3::new(1.0, 1.0, 1.0),
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn non_unit_scale_is_preserved() {
        let n = Node::Scale {
            factor: Vec3::new(2.0, 1.0, 1.0),
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn array_count_one_is_stripped() {
        let n = Node::Array {
            count: 1,
            spacing: Vec3::new(10.0, 0.0, 0.0),
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn array_count_two_is_preserved() {
        let n = Node::Array {
            count: 2,
            spacing: Vec3::new(10.0, 0.0, 0.0),
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn single_child_composition_collapses_to_child() {
        let n = Node::Composition(vec![unit_box()]);
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn multi_child_composition_is_preserved() {
        let n = Node::Composition(vec![unit_box(), unit_box()]);
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn nested_identities_collapse_through_recursion() {
        // (translate (0,0,0) (rotate any 0 (scale (1,1,1) box))) → box
        let n = Node::Translate {
            offset: Vec3::new(0.0, 0.0, 0.0),
            child: Box::new(Node::Rotate {
                axis: Vec3::new(1.0, 0.0, 0.0),
                angle: 0.0,
                child: Box::new(Node::Scale {
                    factor: Vec3::new(1.0, 1.0, 1.0),
                    child: Box::new(unit_box()),
                }),
            }),
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn mirror_is_never_an_identity() {
        // Pin: mirror always flips, so even a trivial-looking child
        // isn't stripped.
        let n = Node::Mirror {
            axis: Axis::X,
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn composition_recurses_into_children() {
        // Inner identity gets stripped even though the parent
        // composition stays.
        let n = Node::Composition(vec![
            unit_box(),
            Node::Scale {
                factor: Vec3::new(1.0, 1.0, 1.0),
                child: Box::new(unit_box()),
            },
        ]);
        let s = simplify(&n);
        match s {
            Node::Composition(ref children) => {
                assert_eq!(children.len(), 2);
                assert_eq!(children[0], unit_box());
                assert_eq!(children[1], unit_box());
            }
            _ => panic!("expected Composition, got {s:?}"),
        }
    }

    #[test]
    fn csg_recurses_into_operands() {
        let n = Node::Difference {
            base: Box::new(Node::Translate {
                offset: Vec3::new(0.0, 0.0, 0.0),
                child: Box::new(unit_box()),
            }),
            subtract: vec![Node::Scale {
                factor: Vec3::new(1.0, 1.0, 1.0),
                child: Box::new(unit_box()),
            }],
        };
        let s = simplify(&n);
        match s {
            Node::Difference { base, subtract } => {
                assert_eq!(*base, unit_box());
                assert_eq!(subtract, vec![unit_box()]);
            }
            _ => panic!("expected Difference, got {s:?}"),
        }
    }

    #[test]
    fn simplify_is_idempotent() {
        // Pin: applying simplify twice gives the same result as once.
        // Catches future rewrites that might leave the tree in a state
        // a second pass would further reduce — at which point the rule
        // should be reapplied internally instead of leaking.
        let n = Node::Translate {
            offset: Vec3::new(0.0, 0.0, 0.0),
            child: Box::new(Node::Rotate {
                axis: Vec3::new(0.0, 1.0, 0.0),
                angle: 0.0,
                child: Box::new(Node::Composition(vec![unit_box()])),
            }),
        };
        let once = simplify(&n);
        let twice = simplify(&once);
        assert_eq!(once, twice);
    }
}

#[cfg(test)]
mod partition_tests {
    use super::*;

    /// Returns a unit box at the given center, in its simplification-
    /// stable form: bare `Box` when the offset is zero, `Translate`
    /// wrapping `Box` otherwise. Tests can compare `simplify(...)`
    /// output directly without worrying about identity-rewrite noise.
    fn box_at(x: f32, y: f32, z: f32) -> Node {
        let unit = Node::Box {
            x: 1.0,
            y: 1.0,
            z: 1.0,
            color: 0,
        };
        if (x, y, z) == (0.0, 0.0, 0.0) {
            unit
        } else {
            Node::Translate {
                offset: Vec3::new(x, y, z),
                child: Box::new(unit),
            }
        }
    }

    #[test]
    fn empty_input_returns_empty() {
        let groups = partition_disjoint_aabb(&[]);
        assert!(groups.is_empty());
    }

    #[test]
    fn single_child_is_one_group() {
        let groups = partition_disjoint_aabb(&[box_at(0.0, 0.0, 0.0)]);
        assert_eq!(groups, vec![vec![0]]);
    }

    #[test]
    fn two_disjoint_children_yield_two_groups() {
        // Boxes at (0,0,0) and (10,0,0) — extents are ±0.5 around each
        // center, so AABBs are [-0.5..0.5] vs [9.5..10.5]: clearly
        // disjoint.
        let groups = partition_disjoint_aabb(&[box_at(0.0, 0.0, 0.0), box_at(10.0, 0.0, 0.0)]);
        assert_eq!(groups, vec![vec![0], vec![1]]);
    }

    #[test]
    fn two_overlapping_children_yield_one_group() {
        // Boxes at (0,0,0) and (0.5,0,0) — AABBs [-0.5..0.5] vs
        // [0..1] overlap.
        let groups = partition_disjoint_aabb(&[box_at(0.0, 0.0, 0.0), box_at(0.5, 0.0, 0.0)]);
        assert_eq!(groups, vec![vec![0, 1]]);
    }

    #[test]
    fn touching_face_counts_as_intersecting() {
        // AABBs [-0.5..0.5] and [0.5..1.5] share the x=0.5 face. Per
        // Aabb::intersects's closed-set semantics, that's intersecting.
        // Pin so a future "strict" change to intersects doesn't
        // silently re-route shared-face geometry through the concat
        // path, where any cleanup-pass divergence between BSP and
        // composition would surface.
        let groups = partition_disjoint_aabb(&[box_at(0.0, 0.0, 0.0), box_at(1.0, 0.0, 0.0)]);
        assert_eq!(groups, vec![vec![0, 1]]);
    }

    #[test]
    fn three_all_disjoint_yield_three_singleton_groups() {
        let groups = partition_disjoint_aabb(&[
            box_at(0.0, 0.0, 0.0),
            box_at(10.0, 0.0, 0.0),
            box_at(20.0, 0.0, 0.0),
        ]);
        assert_eq!(groups, vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn mixed_overlap_yields_grouped_partition() {
        // 0 and 1 overlap (close together), 2 is far away.
        let groups = partition_disjoint_aabb(&[
            box_at(0.0, 0.0, 0.0),
            box_at(0.5, 0.0, 0.0),
            box_at(20.0, 0.0, 0.0),
        ]);
        assert_eq!(groups, vec![vec![0, 1], vec![2]]);
    }

    #[test]
    fn transitive_chain_collapses_to_one_group() {
        // A overlaps B (close), B overlaps C (close), but A does NOT
        // touch C. Union-find should still merge all three through B.
        // Pin: a future "only direct-pair" implementation would split
        // this into [A, B] / [B, C] which is incoherent (B in two
        // groups). Verifies the transitive closure step.
        let groups = partition_disjoint_aabb(&[
            box_at(0.0, 0.0, 0.0),
            box_at(0.5, 0.0, 0.0),
            box_at(1.0, 0.0, 0.0),
        ]);
        assert_eq!(groups, vec![vec![0, 1, 2]]);
    }

    #[test]
    fn first_appearance_order_is_stable() {
        // Children at indices 0 (alone), 1+2 (overlap), 3 (alone).
        // First-appearance order of group roots: 0, 1, 3.
        let groups = partition_disjoint_aabb(&[
            box_at(0.0, 0.0, 0.0),
            box_at(10.0, 0.0, 0.0),
            box_at(10.5, 0.0, 0.0),
            box_at(20.0, 0.0, 0.0),
        ]);
        assert_eq!(groups, vec![vec![0], vec![1, 2], vec![3]]);
    }
}

#[cfg(test)]
mod disjoint_union_rewrite_tests {
    use super::*;

    /// Returns a unit box at the given center, in its simplification-
    /// stable form: bare `Box` when the offset is zero, `Translate`
    /// wrapping `Box` otherwise. Tests can compare `simplify(...)`
    /// output directly without worrying about identity-rewrite noise.
    fn box_at(x: f32, y: f32, z: f32) -> Node {
        let unit = Node::Box {
            x: 1.0,
            y: 1.0,
            z: 1.0,
            color: 0,
        };
        if (x, y, z) == (0.0, 0.0, 0.0) {
            unit
        } else {
            Node::Translate {
                offset: Vec3::new(x, y, z),
                child: Box::new(unit),
            }
        }
    }

    #[test]
    fn disjoint_two_box_union_becomes_composition() {
        let n = Node::Union {
            children: vec![box_at(0.0, 0.0, 0.0), box_at(10.0, 0.0, 0.0)],
        };
        let s = simplify(&n);
        assert_eq!(
            s,
            Node::Composition(vec![box_at(0.0, 0.0, 0.0), box_at(10.0, 0.0, 0.0)])
        );
    }

    #[test]
    fn overlapping_two_box_union_is_unchanged() {
        let n = Node::Union {
            children: vec![box_at(0.0, 0.0, 0.0), box_at(0.5, 0.0, 0.0)],
        };
        let s = simplify(&n);
        assert_eq!(s, n);
    }

    #[test]
    fn fully_disjoint_three_box_union_unwraps_singletons() {
        // Pin: each disjoint child ends up as a bare element of the
        // composition, not as a `Union` with one child.
        let n = Node::Union {
            children: vec![
                box_at(0.0, 0.0, 0.0),
                box_at(10.0, 0.0, 0.0),
                box_at(20.0, 0.0, 0.0),
            ],
        };
        let s = simplify(&n);
        match s {
            Node::Composition(items) => {
                assert_eq!(items.len(), 3);
                for item in &items {
                    // Each composition entry is a primitive (or its
                    // translate wrapper), never a Union.
                    assert!(
                        !matches!(item, Node::Union { .. }),
                        "singleton group should unwrap, got {item:?}"
                    );
                }
            }
            _ => panic!("expected Composition, got {s:?}"),
        }
    }

    #[test]
    fn mixed_overlap_yields_composition_of_union_and_singleton() {
        // 0 and 1 overlap → wrapped as a Union. 2 is alone → bare.
        let n = Node::Union {
            children: vec![
                box_at(0.0, 0.0, 0.0),
                box_at(0.5, 0.0, 0.0),
                box_at(20.0, 0.0, 0.0),
            ],
        };
        let s = simplify(&n);
        match s {
            Node::Composition(items) => {
                assert_eq!(items.len(), 2);
                match &items[0] {
                    Node::Union { children } => assert_eq!(children.len(), 2),
                    other => panic!("expected first item to be Union, got {other:?}"),
                }
                assert_eq!(items[1], box_at(20.0, 0.0, 0.0));
            }
            _ => panic!("expected Composition, got {s:?}"),
        }
    }

    #[test]
    fn rewrite_recurses_through_difference_subtractor() {
        // Inner Union inside a Difference's subtract list should get
        // rewritten too. With disjoint subtractors, the inner Union
        // becomes a Composition — meshes the same way (both produce
        // the union of polygon streams) but without the inner BSP
        // composition.
        let n = Node::Difference {
            base: Box::new(Node::Box {
                x: 5.0,
                y: 5.0,
                z: 5.0,
                color: 0,
            }),
            subtract: vec![Node::Union {
                children: vec![box_at(0.0, 0.0, 0.0), box_at(20.0, 0.0, 0.0)],
            }],
        };
        let s = simplify(&n);
        match &s {
            Node::Difference { subtract, .. } => {
                assert_eq!(subtract.len(), 1);
                assert!(
                    matches!(&subtract[0], Node::Composition(_)),
                    "subtractor should be rewritten to Composition, got {:?}",
                    subtract[0]
                );
            }
            _ => panic!("expected Difference, got {s:?}"),
        }
    }

    #[test]
    fn rewrite_is_idempotent_on_disjoint_union() {
        let n = Node::Union {
            children: vec![
                box_at(0.0, 0.0, 0.0),
                box_at(10.0, 0.0, 0.0),
                box_at(20.0, 0.0, 0.0),
            ],
        };
        let once = simplify(&n);
        let twice = simplify(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn no_rewrite_when_all_overlap_transitively() {
        // A-B-C chain where A overlaps B, B overlaps C; A does not
        // touch C directly. Transitive closure makes one group, so the
        // Union stays put.
        let n = Node::Union {
            children: vec![
                box_at(0.0, 0.0, 0.0),
                box_at(0.5, 0.0, 0.0),
                box_at(1.0, 0.0, 0.0),
            ],
        };
        let s = simplify(&n);
        assert!(matches!(s, Node::Union { .. }));
    }

    #[test]
    fn single_element_union_is_passed_through() {
        // Defensive: AST normally rejects 1-child unions at parse, but
        // if one shows up the rewrite mustn't drop it.
        let n = Node::Union {
            children: vec![box_at(0.0, 0.0, 0.0)],
        };
        let s = simplify(&n);
        assert_eq!(
            s,
            Node::Union {
                children: vec![box_at(0.0, 0.0, 0.0)]
            }
        );
    }
}

#[cfg(test)]
mod parallel_axis_tests {
    use super::*;

    #[test]
    fn same_direction_returns_plus_one() {
        assert_eq!(
            Vec3::new(0.0, 1.0, 0.0).parallel_sign(Vec3::new(0.0, 2.0, 0.0)),
            Some(1.0)
        );
    }

    #[test]
    fn opposite_direction_returns_minus_one() {
        assert_eq!(
            Vec3::new(1.0, 0.0, 0.0).parallel_sign(Vec3::new(-3.0, 0.0, 0.0)),
            Some(-1.0)
        );
    }

    #[test]
    fn skew_axes_return_none() {
        assert_eq!(
            Vec3::new(1.0, 0.0, 0.0).parallel_sign(Vec3::new(0.0, 1.0, 0.0)),
            None
        );
    }

    #[test]
    fn diagonal_vs_axis_returns_none() {
        // (1,1,0) is at 45° to (1,0,0) — definitely not parallel.
        assert_eq!(
            Vec3::new(1.0, 1.0, 0.0).parallel_sign(Vec3::new(1.0, 0.0, 0.0)),
            None
        );
    }

    #[test]
    fn zero_vector_returns_none() {
        assert_eq!(
            Vec3::new(0.0, 0.0, 0.0).parallel_sign(Vec3::new(1.0, 0.0, 0.0)),
            None
        );
        assert_eq!(
            Vec3::new(1.0, 0.0, 0.0).parallel_sign(Vec3::new(0.0, 0.0, 0.0)),
            None
        );
    }

    #[test]
    fn near_parallel_within_tolerance_returns_sign() {
        // Tiny perturbation from a clean (0,1,0) — well within
        // the 1e-10 relative-cross-magnitude tolerance.
        assert_eq!(
            Vec3::new(1e-7, 1.0, 0.0).parallel_sign(Vec3::new(0.0, 1.0, 0.0)),
            Some(1.0)
        );
    }

    #[test]
    fn axes_with_different_magnitude_still_parallel() {
        // Direction is what matters, not magnitude.
        assert_eq!(
            Vec3::new(0.0, 100.0, 0.0).parallel_sign(Vec3::new(0.0, 0.001, 0.0)),
            Some(1.0)
        );
    }
}

#[cfg(test)]
mod fold_transform_tests {
    use super::*;

    fn unit_box() -> Node {
        Node::Box {
            x: 1.0,
            y: 1.0,
            z: 1.0,
            color: 0,
        }
    }

    #[test]
    fn nested_translate_folds_to_one() {
        let n = Node::Translate {
            offset: Vec3::new(1.0, 2.0, 3.0),
            child: Box::new(Node::Translate {
                offset: Vec3::new(4.0, 5.0, 6.0),
                child: Box::new(unit_box()),
            }),
        };
        assert_eq!(
            simplify(&n),
            Node::Translate {
                offset: Vec3::new(5.0, 7.0, 9.0),
                child: Box::new(unit_box()),
            }
        );
    }

    #[test]
    fn translate_pair_summing_to_zero_collapses_completely() {
        // Pin: the fold must produce the identity-then-strip behavior in
        // a single pass, not leave a `(translate (0 0 0) leaf)` behind.
        let n = Node::Translate {
            offset: Vec3::new(1.0, 2.0, 3.0),
            child: Box::new(Node::Translate {
                offset: Vec3::new(-1.0, -2.0, -3.0),
                child: Box::new(unit_box()),
            }),
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn three_deep_translate_chain_folds_to_one() {
        let n = Node::Translate {
            offset: Vec3::new(1.0, 0.0, 0.0),
            child: Box::new(Node::Translate {
                offset: Vec3::new(2.0, 0.0, 0.0),
                child: Box::new(Node::Translate {
                    offset: Vec3::new(3.0, 0.0, 0.0),
                    child: Box::new(unit_box()),
                }),
            }),
        };
        // Bottom-up: inner two fold → (translate 5 leaf); then outer
        // folds against that → (translate 6 leaf).
        assert_eq!(
            simplify(&n),
            Node::Translate {
                offset: Vec3::new(6.0, 0.0, 0.0),
                child: Box::new(unit_box()),
            }
        );
    }

    #[test]
    fn translate_then_rotate_does_not_fold() {
        // Pin: translate doesn't commute with rotate-of-non-translated-
        // child, so we leave the structure alone.
        let n = Node::Translate {
            offset: Vec3::new(1.0, 0.0, 0.0),
            child: Box::new(Node::Rotate {
                axis: Vec3::new(0.0, 1.0, 0.0),
                angle: 0.5,
                child: Box::new(unit_box()),
            }),
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn nested_scale_folds_to_componentwise_product() {
        let n = Node::Scale {
            factor: Vec3::new(2.0, 3.0, 4.0),
            child: Box::new(Node::Scale {
                factor: Vec3::new(5.0, 6.0, 7.0),
                child: Box::new(unit_box()),
            }),
        };
        assert_eq!(
            simplify(&n),
            Node::Scale {
                factor: Vec3::new(10.0, 18.0, 28.0),
                child: Box::new(unit_box()),
            }
        );
    }

    #[test]
    fn scale_pair_with_exact_reciprocals_collapses_completely() {
        // Powers-of-two multiplied through give exact f32 products, so
        // the identity check fires and both Scale wrappers strip.
        let n = Node::Scale {
            factor: Vec3::new(2.0, 4.0, 8.0),
            child: Box::new(Node::Scale {
                factor: Vec3::new(0.5, 0.25, 0.125),
                child: Box::new(unit_box()),
            }),
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn nested_scale_with_non_unity_product_keeps_one_scale() {
        // Pin: a non-identity product collapses the two Scale wrappers
        // into one (no nesting), but the resulting Scale stays put.
        let n = Node::Scale {
            factor: Vec3::new(2.0, 3.0, 4.0),
            child: Box::new(Node::Scale {
                factor: Vec3::new(3.0, 5.0, 7.0),
                child: Box::new(unit_box()),
            }),
        };
        assert_eq!(
            simplify(&n),
            Node::Scale {
                factor: Vec3::new(6.0, 15.0, 28.0),
                child: Box::new(unit_box()),
            }
        );
    }

    #[test]
    fn nested_rotate_same_axis_sums_angles() {
        let n = Node::Rotate {
            axis: Vec3::new(0.0, 1.0, 0.0),
            angle: 0.3,
            child: Box::new(Node::Rotate {
                axis: Vec3::new(0.0, 1.0, 0.0),
                angle: 0.4,
                child: Box::new(unit_box()),
            }),
        };
        let s = simplify(&n);
        match s {
            Node::Rotate { axis, angle, child } => {
                assert_eq!(axis, Vec3::new(0.0, 1.0, 0.0));
                assert!((angle - 0.7).abs() < 1e-6, "expected ~0.7, got {angle}");
                assert_eq!(*child, unit_box());
            }
            other => panic!("expected Rotate, got {other:?}"),
        }
    }

    #[test]
    fn nested_rotate_opposite_axis_subtracts_angles() {
        // (rotate +Y θa (rotate -Y θb X)) ≡ (rotate +Y θa-θb X).
        let n = Node::Rotate {
            axis: Vec3::new(0.0, 1.0, 0.0),
            angle: 0.7,
            child: Box::new(Node::Rotate {
                axis: Vec3::new(0.0, -1.0, 0.0),
                angle: 0.3,
                child: Box::new(unit_box()),
            }),
        };
        let s = simplify(&n);
        match s {
            Node::Rotate { axis, angle, .. } => {
                assert_eq!(axis, Vec3::new(0.0, 1.0, 0.0));
                assert!((angle - 0.4).abs() < 1e-6, "expected ~0.4, got {angle}");
            }
            other => panic!("expected Rotate, got {other:?}"),
        }
    }

    #[test]
    fn nested_rotate_opposing_full_cancellation_strips_both() {
        let n = Node::Rotate {
            axis: Vec3::new(1.0, 0.0, 0.0),
            angle: 0.5,
            child: Box::new(Node::Rotate {
                axis: Vec3::new(-1.0, 0.0, 0.0),
                angle: 0.5,
                child: Box::new(unit_box()),
            }),
        };
        // (axis_a · angle_a) + (axis_b · angle_b) = +0.5 + (-1)*0.5 = 0
        // → identity → stripped.
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn nested_rotate_skew_axes_does_not_fold() {
        // Pin: rotation around X then around Y is NOT a single
        // axis-angle rotation in general. We don't compose via
        // quaternions, so the structure stays.
        let n = Node::Rotate {
            axis: Vec3::new(1.0, 0.0, 0.0),
            angle: 0.5,
            child: Box::new(Node::Rotate {
                axis: Vec3::new(0.0, 1.0, 0.0),
                angle: 0.3,
                child: Box::new(unit_box()),
            }),
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn rotate_with_unnormalized_parallel_axes_still_folds() {
        // Outer axis (0, 2, 0), inner axis (0, 5, 0) — both point
        // along +Y, just with different magnitudes. parallel_axis_sign
        // ignores magnitude, so the fold fires.
        let n = Node::Rotate {
            axis: Vec3::new(0.0, 2.0, 0.0),
            angle: 0.3,
            child: Box::new(Node::Rotate {
                axis: Vec3::new(0.0, 5.0, 0.0),
                angle: 0.4,
                child: Box::new(unit_box()),
            }),
        };
        let s = simplify(&n);
        match s {
            Node::Rotate { axis, angle, .. } => {
                assert_eq!(axis, Vec3::new(0.0, 2.0, 0.0));
                assert!((angle - 0.7).abs() < 1e-6);
            }
            other => panic!("expected Rotate, got {other:?}"),
        }
    }

    #[test]
    fn translate_does_not_fold_into_scale_or_vice_versa() {
        // (translate offset (scale s leaf)) cannot be expressed as
        // a single transform of either kind.
        let n = Node::Translate {
            offset: Vec3::new(1.0, 0.0, 0.0),
            child: Box::new(Node::Scale {
                factor: Vec3::new(2.0, 1.0, 1.0),
                child: Box::new(unit_box()),
            }),
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn folding_is_idempotent() {
        let n = Node::Translate {
            offset: Vec3::new(1.0, 1.0, 1.0),
            child: Box::new(Node::Translate {
                offset: Vec3::new(2.0, 2.0, 2.0),
                child: Box::new(Node::Rotate {
                    axis: Vec3::new(0.0, 1.0, 0.0),
                    angle: 0.1,
                    child: Box::new(Node::Rotate {
                        axis: Vec3::new(0.0, 1.0, 0.0),
                        angle: 0.2,
                        child: Box::new(unit_box()),
                    }),
                }),
            }),
        };
        let once = simplify(&n);
        let twice = simplify(&once);
        assert_eq!(once, twice);
    }
}

#[cfg(test)]
mod difference_rewrite_tests {
    use super::*;

    fn unit_box() -> Node {
        Node::Box {
            x: 1.0,
            y: 1.0,
            z: 1.0,
            color: 0,
        }
    }

    fn box_at(x: f32, y: f32, z: f32) -> Node {
        let unit = unit_box();
        if (x, y, z) == (0.0, 0.0, 0.0) {
            unit
        } else {
            Node::Translate {
                offset: Vec3::new(x, y, z),
                child: Box::new(unit),
            }
        }
    }

    #[test]
    fn single_disjoint_subtractor_drops_to_bare_base() {
        let n = Node::Difference {
            base: Box::new(unit_box()),
            subtract: vec![box_at(10.0, 0.0, 0.0)],
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn one_of_two_disjoint_subtractors_is_pruned() {
        // First subtractor overlaps base, second doesn't — keep only first.
        let n = Node::Difference {
            base: Box::new(Node::Box {
                x: 4.0,
                y: 4.0,
                z: 4.0,
                color: 0,
            }),
            subtract: vec![box_at(0.0, 0.0, 0.0), box_at(20.0, 0.0, 0.0)],
        };
        let s = simplify(&n);
        match s {
            Node::Difference { subtract, .. } => {
                assert_eq!(subtract.len(), 1);
                assert_eq!(subtract[0], box_at(0.0, 0.0, 0.0));
            }
            other => panic!("expected Difference, got {other:?}"),
        }
    }

    #[test]
    fn all_disjoint_subtractors_collapse_to_base() {
        let n = Node::Difference {
            base: Box::new(unit_box()),
            subtract: vec![box_at(10.0, 0.0, 0.0), box_at(0.0, 10.0, 0.0)],
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn no_subtractors_are_pruned_when_all_overlap() {
        let n = Node::Difference {
            base: Box::new(Node::Box {
                x: 4.0,
                y: 4.0,
                z: 4.0,
                color: 0,
            }),
            subtract: vec![box_at(0.0, 0.0, 0.0), box_at(0.5, 0.0, 0.0)],
        };
        let s = simplify(&n);
        match s {
            Node::Difference { subtract, .. } => assert_eq!(subtract.len(), 2),
            other => panic!("expected Difference, got {other:?}"),
        }
    }

    #[test]
    fn touching_face_subtractor_is_kept() {
        // Pin: closed-set semantics from intersects mean shared-face
        // counts as overlapping → don't prune.
        let n = Node::Difference {
            base: Box::new(Node::Box {
                x: 1.0,
                y: 1.0,
                z: 1.0,
                color: 0,
            }),
            subtract: vec![Node::Translate {
                offset: Vec3::new(1.0, 0.0, 0.0),
                child: Box::new(Node::Box {
                    x: 1.0,
                    y: 1.0,
                    z: 1.0,
                    color: 1,
                }),
            }],
        };
        let s = simplify(&n);
        assert!(matches!(s, Node::Difference { .. }));
    }

    #[test]
    fn difference_distributes_over_leaf_union_base() {
        // (difference (union A B) Y) where A overlaps Y, B doesn't.
        // Distribution + per-arm AABB-prune + disjoint-union rewrites
        // collapse the whole thing to a composition.
        let big_a = Node::Box {
            x: 4.0,
            y: 4.0,
            z: 4.0,
            color: 0,
        };
        let big_b = Node::Translate {
            offset: Vec3::new(20.0, 0.0, 0.0),
            child: Box::new(Node::Box {
                x: 4.0,
                y: 4.0,
                z: 4.0,
                color: 1,
            }),
        };
        let cutter = Node::Box {
            x: 1.0,
            y: 1.0,
            z: 1.0,
            color: 9,
        };
        let n = Node::Difference {
            base: Box::new(Node::Union {
                children: vec![big_a.clone(), big_b.clone()],
            }),
            subtract: vec![cutter.clone()],
        };
        let s = simplify(&n);
        match s {
            Node::Composition(items) => {
                assert_eq!(items.len(), 2);
                assert!(
                    matches!(&items[0], Node::Difference { .. }),
                    "first arm should be Difference (A overlaps cutter), got {:?}",
                    items[0]
                );
                assert_eq!(items[1], big_b);
            }
            other => panic!("expected Composition after full chain, got {other:?}"),
        }
    }

    #[test]
    fn distribution_blocked_by_composite_subtractor() {
        // Subtractor contains a Union → not a CSG-leaf → distribution
        // skipped, structure preserved.
        let composite_subtractor = Node::Union {
            children: vec![
                Node::Box {
                    x: 1.0,
                    y: 1.0,
                    z: 1.0,
                    color: 1,
                },
                Node::Translate {
                    offset: Vec3::new(0.5, 0.0, 0.0),
                    child: Box::new(Node::Box {
                        x: 1.0,
                        y: 1.0,
                        z: 1.0,
                        color: 2,
                    }),
                },
            ],
        };
        let n = Node::Difference {
            base: Box::new(Node::Union {
                children: vec![
                    Node::Box {
                        x: 4.0,
                        y: 4.0,
                        z: 4.0,
                        color: 0,
                    },
                    Node::Box {
                        x: 4.0,
                        y: 4.0,
                        z: 4.0,
                        color: 1,
                    },
                ],
            }),
            subtract: vec![composite_subtractor],
        };
        let s = simplify(&n);
        assert!(
            matches!(s, Node::Difference { .. }),
            "expected Difference (composite subtractor blocks distribution), got {s:?}"
        );
    }

    #[test]
    fn distribution_blocked_when_union_child_is_composite() {
        let composite_child = Node::Difference {
            base: Box::new(Node::Box {
                x: 4.0,
                y: 4.0,
                z: 4.0,
                color: 0,
            }),
            subtract: vec![Node::Box {
                x: 1.0,
                y: 1.0,
                z: 1.0,
                color: 9,
            }],
        };
        let n = Node::Difference {
            base: Box::new(Node::Union {
                children: vec![
                    composite_child,
                    Node::Box {
                        x: 4.0,
                        y: 4.0,
                        z: 4.0,
                        color: 1,
                    },
                ],
            }),
            subtract: vec![Node::Box {
                x: 1.0,
                y: 1.0,
                z: 1.0,
                color: 2,
            }],
        };
        let s = simplify(&n);
        assert!(matches!(s, Node::Difference { .. }));
    }

    #[test]
    fn rewrite_is_idempotent() {
        let n = Node::Difference {
            base: Box::new(Node::Union {
                children: vec![
                    Node::Box {
                        x: 4.0,
                        y: 4.0,
                        z: 4.0,
                        color: 0,
                    },
                    Node::Translate {
                        offset: Vec3::new(20.0, 0.0, 0.0),
                        child: Box::new(Node::Box {
                            x: 4.0,
                            y: 4.0,
                            z: 4.0,
                            color: 1,
                        }),
                    },
                ],
            }),
            subtract: vec![Node::Box {
                x: 1.0,
                y: 1.0,
                z: 1.0,
                color: 9,
            }],
        };
        let once = simplify(&n);
        let twice = simplify(&once);
        assert_eq!(once, twice);
    }
}

#[cfg(test)]
mod profile_axis_closed_tests {
    use super::*;

    #[test]
    fn profile_starting_and_ending_at_axis_is_closed() {
        assert!(profile_axis_closed(&[
            [0.0, -0.5],
            [0.5, -0.5],
            [0.5, 0.5],
            [0.0, 0.5]
        ]));
    }

    #[test]
    fn profile_open_at_start_is_not_closed() {
        assert!(!profile_axis_closed(&[[0.5, -0.5], [0.5, 0.5], [0.0, 0.5]]));
    }

    #[test]
    fn profile_open_at_end_is_not_closed() {
        assert!(!profile_axis_closed(&[
            [0.0, -0.5],
            [0.5, -0.5],
            [0.5, 0.5]
        ]));
    }

    #[test]
    fn empty_or_single_vertex_profile_is_not_closed() {
        assert!(!profile_axis_closed(&[]));
        assert!(!profile_axis_closed(&[[0.0, 0.0]]));
    }
}

#[cfg(test)]
mod lathe_decomp_tests {
    use super::*;

    fn closed_profile() -> Vec<[f32; 2]> {
        vec![[0.0, -0.5], [0.5, -0.5], [0.5, 0.5], [0.0, 0.5]]
    }

    #[test]
    fn axis_closed_lathe_rewrites_to_union_of_segments() {
        let n = Node::Lathe {
            profile: closed_profile(),
            segments: 8,
            color: 5,
        };
        let s = simplify(&n);
        match s {
            Node::Union { children } => {
                assert_eq!(children.len(), 8);
                for (i, child) in children.iter().enumerate() {
                    match child {
                        Node::LatheSegment {
                            segments,
                            segment_index,
                            color,
                            ..
                        } => {
                            assert_eq!(*segments, 8);
                            assert_eq!(*segment_index, i as u32);
                            assert_eq!(*color, 5);
                        }
                        other => panic!("expected LatheSegment, got {other:?}"),
                    }
                }
            }
            other => panic!("expected Union, got {other:?}"),
        }
    }

    #[test]
    fn open_profile_lathe_is_unchanged() {
        let n = Node::Lathe {
            profile: vec![[0.5, -0.5], [0.5, 0.5]],
            segments: 8,
            color: 0,
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn lathe_with_too_few_segments_is_unchanged() {
        let n = Node::Lathe {
            profile: closed_profile(),
            segments: 2,
            color: 0,
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn lathe_segment_passes_through_simplify_unchanged() {
        let n = Node::LatheSegment {
            profile: closed_profile(),
            segments: 8,
            segment_index: 3,
            color: 0,
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn lathe_segment_aabb_matches_lathe_aabb() {
        // Conservative bound — same as parent lathe per the v1 design.
        // Pinning so a future tighter per-wedge bound is a deliberate
        // change.
        let lathe_bound = compute_aabb(&Node::Lathe {
            profile: closed_profile(),
            segments: 16,
            color: 0,
        });
        let segment_bound = compute_aabb(&Node::LatheSegment {
            profile: closed_profile(),
            segments: 16,
            segment_index: 0,
            color: 0,
        });
        assert_eq!(lathe_bound, segment_bound);
    }

    #[test]
    fn difference_of_decomposed_lathe_distributes_through_pipeline() {
        // The full chain: Lathe → Union<LatheSegment> via wedge
        // decomp; then `Difference(Union, leaf_cutter)` distributes
        // → Union of per-segment differences. Pin the final shape so
        // a regression in any step shows up loudly — issue 300's fix
        // load-bearing.
        let n = Node::Difference {
            base: Box::new(Node::Lathe {
                profile: closed_profile(),
                segments: 4,
                color: 0,
            }),
            subtract: vec![Node::Box {
                x: 0.5,
                y: 2.0,
                z: 0.5,
                color: 1,
            }],
        };
        let s = simplify(&n);
        match s {
            Node::Union { children } => {
                assert_eq!(children.len(), 4, "expected 4 per-segment arms");
                for child in children {
                    match child {
                        Node::Difference { base, subtract } => {
                            assert!(
                                matches!(*base, Node::LatheSegment { .. }),
                                "arm base should be LatheSegment, got {base:?}"
                            );
                            assert_eq!(subtract.len(), 1);
                        }
                        other => panic!("arm should be Difference, got {other:?}"),
                    }
                }
            }
            other => panic!("expected Union of per-segment differences, got {other:?}"),
        }
    }

    #[test]
    fn lathe_decomp_is_idempotent() {
        let n = Node::Lathe {
            profile: closed_profile(),
            segments: 8,
            color: 0,
        };
        let once = simplify(&n);
        let twice = simplify(&once);
        assert_eq!(once, twice);
    }
}
