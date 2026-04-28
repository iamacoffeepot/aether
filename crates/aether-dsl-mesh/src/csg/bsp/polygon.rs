//! Convex polygon over rational [`BspPoint3`] vertices, plus the
//! split-against-plane routine and global canonicalization pass that
//! Phase 3 of ADR-0061 will wire into BSP composition.
//!
//! `BspPolygon` mirrors [`crate::csg::polygon::Polygon`]'s structure
//! field-for-field, swapping integer [`Point3`] for rational
//! [`BspPoint3`]. Plane and color are unchanged: planes are integer
//! everywhere (constructed from input mesh vertices, never refined),
//! and color is a `u32` tag.
//!
//! # Phase 2 invariants
//!
//! - **Parity (lifted-integer fixtures only).** Lifting a `Polygon` to
//!   `BspPolygon` and running [`BspPolygon::split`] then [`canonicalize`]
//!   yields the same partition classification (coplanar-front /
//!   coplanar-back / front / back) and the same integer vertex
//!   coordinates as today's [`Polygon::split`]. Parity is *not*
//!   claimed for rational fragments arising mid-recursion in Phase 3 —
//!   those may classify more precisely than today.
//!
//! - **Single canonical snap.** [`canonicalize`] interns equal
//!   [`BspPoint3`] values across the entire input and snaps each
//!   canonical rational to a fixed-grid [`Point3`] *exactly once*. Two
//!   `BspPoint3`s with equal normalized form anywhere in the input
//!   share both their canonical id and their rounded integer.
//!
//! - **Round-trip identity.** For any integer [`Polygon`] `p`,
//!   `canonicalize(vec![BspPolygon::lift(&p)])` produces a
//!   single-element vector whose only `Polygon` equals `p`
//!   field-for-field.
//!
//! - **Determinism.** `canonicalize` is deterministic for a fixed
//!   input.
//!
//! # Boundary
//!
//! Phase 2 does not wire `BspPolygon` or `canonicalize` into
//! [`crate::csg::bsp::BspTree`], [`crate::csg::ops::union_raw`],
//! [`crate::csg::ops::intersection_raw`], or
//! [`crate::csg::ops::difference_raw`]. The integer pipeline is
//! unchanged. Matrix verdict bit-identical to main.

#![allow(dead_code)] // phase 2 boundary: callers land in phase 3.

use std::collections::HashMap;

use super::point::BspPoint3;
use crate::csg::CsgError;
use crate::csg::plane::Plane3;
use crate::csg::point::Point3;
use crate::csg::polygon::Polygon;

const COPLANAR: i32 = 0;
const FRONT: i32 = 1;
const BACK: i32 = 2;
const SPANNING: i32 = 3;

/// Rational mirror of [`Polygon`]. Vertices are [`BspPoint3`] in
/// fully-reduced normal form; `plane` and `color` are unchanged.
///
/// The internal representation reserves an extension point for vertex
/// provenance per ADR-0061's Decision section. No provenance semantics
/// are required in Phase 2 and no field is added here — the design
/// note flags the seam for the next ADR if rim-emission topology
/// asymmetry survives the rational fix.
#[derive(Debug, Clone)]
pub(super) struct BspPolygon {
    pub vertices: Vec<BspPoint3>,
    pub plane: Plane3,
    pub color: u32,
}

impl BspPolygon {
    /// Lift an integer [`Polygon`] to a `BspPolygon` by lifting every
    /// vertex (each lands at `den == 1`). The plane is reused as-is;
    /// color carries through.
    pub(super) fn lift(p: &Polygon) -> BspPolygon {
        BspPolygon {
            vertices: p.vertices.iter().map(|&v| BspPoint3::lift(v)).collect(),
            plane: p.plane,
            color: p.color,
        }
    }

    /// Classify this polygon against `partitioner` and route it into
    /// one (or two) of the four output buckets — rational mirror of
    /// [`Polygon::split`].
    ///
    /// Returns `Err(NumericOverflow)` only if checked rational
    /// arithmetic overflows `i128`. At ADR-0054 coordinate bounds
    /// (`|coord| ≤ 256` fixed units) this should never trigger; if it
    /// does, that's a bug to investigate, not absorb.
    pub(super) fn split(
        &self,
        partitioner: &Plane3,
        coplanar_front: &mut Vec<BspPolygon>,
        coplanar_back: &mut Vec<BspPolygon>,
        front: &mut Vec<BspPolygon>,
        back: &mut Vec<BspPolygon>,
    ) -> Result<(), CsgError> {
        // Plane-identity short-circuit (mirrors Polygon::split). Stored
        // plane structurally matches the partitioner → coplanar by
        // construction, regardless of any per-vertex rational drift.
        if self.plane.canonical_key() == partitioner.canonical_key() {
            if partitioner.normal_dot_sign(&self.plane) > 0 {
                coplanar_front.push(self.clone());
            } else {
                coplanar_back.push(self.clone());
            }
            return Ok(());
        }

        // Threshold check is integer-side: side_scaled / den compared
        // to threshold. Multiply both by den (positive) to compare
        // integer-vs-integer. For lifted-integer points (den == 1)
        // this collapses to today's exact comparison.
        let threshold = partitioner.coplanar_threshold();
        let mut polygon_type = COPLANAR;
        let mut types: Vec<i32> = Vec::with_capacity(self.vertices.len());
        for v in &self.vertices {
            let s_scaled = side_scaled(partitioner, v)?;
            let threshold_scaled =
                threshold
                    .checked_mul(v.den())
                    .ok_or(CsgError::NumericOverflow {
                        stage: "BspPolygon::split",
                        context: "threshold * den overflow",
                    })?;
            let t = if s_scaled > threshold_scaled {
                FRONT
            } else if s_scaled < -threshold_scaled {
                BACK
            } else {
                COPLANAR
            };
            polygon_type |= t;
            types.push(t);
        }

        match polygon_type {
            COPLANAR => {
                if partitioner.normal_dot_sign(&self.plane) > 0 {
                    coplanar_front.push(self.clone());
                } else {
                    coplanar_back.push(self.clone());
                }
            }
            FRONT => front.push(self.clone()),
            BACK => back.push(self.clone()),
            _ => {
                // SPANNING: walk edges, produce front/back fragments
                // with rational split vertices.
                let n = self.vertices.len();
                let mut f = Vec::with_capacity(n + 1);
                let mut b = Vec::with_capacity(n + 1);
                for i in 0..n {
                    let j = (i + 1) % n;
                    let ti = types[i];
                    let tj = types[j];
                    let vi = self.vertices[i];
                    let vj = self.vertices[j];
                    if ti != BACK {
                        f.push(vi);
                    }
                    if ti != FRONT {
                        b.push(vi);
                    }
                    if (ti | tj) == SPANNING {
                        let split_pt = compute_intersection_rat(&vi, &vj, partitioner)?;
                        f.push(split_pt);
                        b.push(split_pt);
                    }
                }
                if f.len() >= 3 {
                    front.push(BspPolygon {
                        vertices: f,
                        plane: self.plane,
                        color: self.color,
                    });
                }
                if b.len() >= 3 {
                    back.push(BspPolygon {
                        vertices: b,
                        plane: self.plane,
                        color: self.color,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Edge-vs-plane intersection in exact rationals. Returns a
/// [`BspPoint3`] in fully-reduced normal form; the snap-rounding step
/// is deferred to [`canonicalize`] so equal rationals across split
/// sites cannot round to different integers.
///
/// For lifted-integer endpoints (both `p0.den == p1.den == 1`),
/// numerator and denominator coincide with today's integer
/// `compute_intersection` formula — `snap()` of the result equals the
/// integer path's output by construction.
///
/// Returns `Err(NumericOverflow)` if any checked `i128` operation
/// overflows. The "edge does not cross plane" case (zero denominator)
/// surfaces as `Err` too — callers must gate on SPANNING classification.
fn compute_intersection_rat(
    p0: &BspPoint3,
    p1: &BspPoint3,
    plane: &Plane3,
) -> Result<BspPoint3, CsgError> {
    let s0 = side_scaled(plane, p0)?;
    let s1 = side_scaled(plane, p1)?;

    // Working from `I_k = (s0_rat · p1_k - s1_rat · p0_k) / (s0_rat -
    // s1_rat)` with `s0_rat = s0 / p0.den` and `p0_k = p0.num[k] /
    // p0.den` (and likewise for p1), and multiplying numerator and
    // denominator by `p0.den · p1.den`:
    //
    //   numerator[k] = s0 · p1.num[k] - s1 · p0.num[k]
    //   denominator  = s0 · p1.den    - s1 · p0.den
    //
    // For lifted-integer inputs (p0.den = p1.den = 1) this reduces to
    // today's `s0 * p1.x - s1 * p0.x` over `s0 - s1`, byte-identical
    // to `csg::polygon::compute_intersection` pre-`round_div`.
    let p0n = p0.num();
    let p1n = p1.num();
    let p0d = p0.den();
    let p1d = p1.den();

    let make_minor =
        |a: i128, b: i128, c: i128, d: i128, ctx: &'static str| -> Result<i128, CsgError> {
            let ab = a.checked_mul(b).ok_or(CsgError::NumericOverflow {
                stage: "compute_intersection_rat",
                context: ctx,
            })?;
            let cd = c.checked_mul(d).ok_or(CsgError::NumericOverflow {
                stage: "compute_intersection_rat",
                context: ctx,
            })?;
            ab.checked_sub(cd).ok_or(CsgError::NumericOverflow {
                stage: "compute_intersection_rat",
                context: ctx,
            })
        };

    let den = make_minor(s0, p1d, s1, p0d, "denominator")?;
    if den == 0 {
        return Err(CsgError::NumericOverflow {
            stage: "compute_intersection_rat",
            context: "edge does not cross plane (s0_rat == s1_rat)",
        });
    }
    let num = [
        make_minor(s0, p1n[0], s1, p0n[0], "numerator x")?,
        make_minor(s0, p1n[1], s1, p0n[1], "numerator y")?,
        make_minor(s0, p1n[2], s1, p0n[2], "numerator z")?,
    ];

    BspPoint3::new(num, den)
}

/// Scaled signed side: `n · num - plane.d · den`. Sign matches the
/// rational `n · p - plane.d` because `den > 0`. For lifted-integer
/// (`den == 1`) this equals [`Plane3::side`] — same byte sequence,
/// same sign comparisons, same threshold result.
///
/// Returns `Err(NumericOverflow)` on checked `i128` overflow.
fn side_scaled(plane: &Plane3, p: &BspPoint3) -> Result<i128, CsgError> {
    let nx = plane.n_x as i128;
    let ny = plane.n_y as i128;
    let nz = plane.n_z as i128;
    let [num_x, num_y, num_z] = p.num();
    let den = p.den();

    let term_x = nx.checked_mul(num_x).ok_or(overflow("side term x"))?;
    let term_y = ny.checked_mul(num_y).ok_or(overflow("side term y"))?;
    let term_z = nz.checked_mul(num_z).ok_or(overflow("side term z"))?;
    let dot = term_x
        .checked_add(term_y)
        .and_then(|s| s.checked_add(term_z))
        .ok_or(overflow("side dot accumulate"))?;
    let term_d = plane.d.checked_mul(den).ok_or(overflow("side d * den"))?;
    dot.checked_sub(term_d).ok_or(overflow("side - d"))
}

fn overflow(context: &'static str) -> CsgError {
    CsgError::NumericOverflow {
        stage: "side_scaled",
        context,
    }
}

/// Global canonicalization at the BSP-to-cleanup boundary. Walks every
/// `BspPoint3` across all polygons, interns equal normalized forms to
/// a shared snapped [`Point3`], and emits integer [`Polygon`]s for
/// the cleanup pipeline.
///
/// **Load-bearing invariant.** Two `BspPoint3`s with equal normalized
/// form anywhere in the input map to the same `Point3` in the output.
/// This is what eliminates the "two BSP sides round the same rational
/// to different integers" failure class per ADR-0061.
///
/// **Determinism.** Output is a function of input only; running the
/// pass twice on the same input produces identical results.
///
/// **Round-trip.** `canonicalize(vec![BspPolygon::lift(&p)])` produces
/// `vec![p]` for any integer [`Polygon`] `p`.
pub(super) fn canonicalize(input: Vec<BspPolygon>) -> Result<Vec<Polygon>, CsgError> {
    let mut intern: HashMap<BspPoint3, Point3> = HashMap::new();
    let mut output = Vec::with_capacity(input.len());
    for bp in input {
        let mut snapped_verts = Vec::with_capacity(bp.vertices.len());
        for v in &bp.vertices {
            let snapped = match intern.get(v) {
                Some(&existing) => existing,
                None => {
                    let s = v.snap()?;
                    intern.insert(*v, s);
                    s
                }
            };
            snapped_verts.push(snapped);
        }
        output.push(Polygon {
            vertices: snapped_verts,
            plane: bp.plane,
            color: bp.color,
        });
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: i32, y: i32, z: i32) -> Point3 {
        Point3 { x, y, z }
    }

    fn integer_triangle(a: Point3, b: Point3, c: Point3, color: u32) -> Polygon {
        Polygon::from_triangle(a, b, c, color).expect("non-degenerate test fixture")
    }

    fn axis_plane_x_zero() -> Plane3 {
        // n = (1, 0, 0), d = 0 — the x = 0 plane.
        Plane3::from_points(p(0, 0, 0), p(0, 1, 0), p(0, 0, 1))
    }

    #[test]
    fn lift_then_canonicalize_is_identity() {
        let original = integer_triangle(p(1, 2, 3), p(4, 5, 6), p(7, 8, 0), 42);
        let lifted = BspPolygon::lift(&original);
        let round_tripped = canonicalize(vec![lifted]).unwrap();
        assert_eq!(round_tripped.len(), 1);
        let r = &round_tripped[0];
        assert_eq!(r.vertices, original.vertices);
        assert_eq!(r.color, original.color);
        assert_plane_eq(&r.plane, &original.plane);
    }

    #[test]
    fn lift_then_canonicalize_handles_multiple_polygons() {
        let a = integer_triangle(p(0, 0, 0), p(1, 0, 0), p(0, 1, 0), 1);
        let b = integer_triangle(p(0, 0, 1), p(1, 0, 1), p(0, 1, 1), 2);
        let lifted = vec![BspPolygon::lift(&a), BspPolygon::lift(&b)];
        let out = canonicalize(lifted).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].vertices, a.vertices);
        assert_eq!(out[0].color, 1);
        assert_eq!(out[1].vertices, b.vertices);
        assert_eq!(out[1].color, 2);
    }

    #[test]
    fn equal_rationals_across_polygons_share_snapped_integer() {
        // Two distinct BspPolygons share a vertex via two different
        // `(num, den)` spellings of the same rational. Canonicalize
        // must snap that rational once and stamp the same integer
        // into both polygons.
        let shared_a = BspPoint3::new([1, 2, 3], 2).unwrap();
        let shared_b = BspPoint3::new([2, 4, 6], 4).unwrap();
        assert_eq!(shared_a, shared_b);

        let other = BspPoint3::lift(p(10, 10, 10));

        let plane = Plane3::from_points(p(0, 0, 0), p(1, 0, 0), p(0, 1, 0));
        let poly_a = BspPolygon {
            vertices: vec![shared_a, other, BspPoint3::lift(p(20, 20, 20))],
            plane,
            color: 0,
        };
        let poly_b = BspPolygon {
            vertices: vec![shared_b, other, BspPoint3::lift(p(30, 30, 30))],
            plane,
            color: 0,
        };

        let out = canonicalize(vec![poly_a, poly_b]).unwrap();
        // Snap of (1/2, 2/2, 3/2) = (1/2, 1, 3/2) → (1, 1, 2) (ties up).
        assert_eq!(out[0].vertices[0], p(1, 1, 2));
        assert_eq!(out[1].vertices[0], p(1, 1, 2));
        // The two shared-int rationals snap to identical integers
        // because they intern to the same canonical entry — this is
        // the load-bearing invariant for ADR-0061.
        assert_eq!(out[0].vertices[0], out[1].vertices[0]);
    }

    #[test]
    fn canonicalize_is_deterministic() {
        let pts = vec![
            BspPoint3::new([1, 2, 3], 2).unwrap(),
            BspPoint3::new([2, 4, 6], 4).unwrap(),
            BspPoint3::lift(p(7, 7, 7)),
            BspPoint3::new([3, 6, 9], 6).unwrap(),
        ];
        let plane = Plane3::from_points(p(0, 0, 0), p(1, 0, 0), p(0, 1, 0));
        let poly = BspPolygon {
            vertices: pts,
            plane,
            color: 99,
        };

        let run_a = canonicalize(vec![poly.clone()]).unwrap();
        let run_b = canonicalize(vec![poly.clone()]).unwrap();
        let run_c = canonicalize(vec![poly]).unwrap();
        assert_eq!(run_a[0].vertices, run_b[0].vertices);
        assert_eq!(run_a[0].vertices, run_c[0].vertices);
    }

    #[test]
    fn parity_coplanar_polygon_routes_to_coplanar_front() {
        // Triangle on x=0 plane with normal +x → coplanar_front.
        let xy_plane_partitioner = axis_plane_x_zero();
        let coplanar_poly = integer_triangle(p(0, 0, 0), p(0, 4, 0), p(0, 0, 4), 7);

        // Expectation: same plane → coplanar_front via
        // canonical-key short-circuit. normal_dot_sign should be > 0
        // since the triangle's normal aligns with the partitioner.
        assert_eq!(
            coplanar_poly.plane.canonical_key(),
            xy_plane_partitioner.canonical_key()
        );

        let mut int_cf = Vec::new();
        let mut int_cb = Vec::new();
        let mut int_f = Vec::new();
        let mut int_b = Vec::new();
        coplanar_poly.split(
            &xy_plane_partitioner,
            &mut int_cf,
            &mut int_cb,
            &mut int_f,
            &mut int_b,
        );

        let bp = BspPolygon::lift(&coplanar_poly);
        let mut rat_cf = Vec::new();
        let mut rat_cb = Vec::new();
        let mut rat_f = Vec::new();
        let mut rat_b = Vec::new();
        bp.split(
            &xy_plane_partitioner,
            &mut rat_cf,
            &mut rat_cb,
            &mut rat_f,
            &mut rat_b,
        )
        .unwrap();
        let rat_cf = canonicalize(rat_cf).unwrap();
        let rat_cb = canonicalize(rat_cb).unwrap();
        let rat_f = canonicalize(rat_f).unwrap();
        let rat_b = canonicalize(rat_b).unwrap();

        assert_buckets_match(&int_cf, &rat_cf);
        assert_buckets_match(&int_cb, &rat_cb);
        assert_buckets_match(&int_f, &rat_f);
        assert_buckets_match(&int_b, &rat_b);
        // Bucket the polygon should land in:
        assert_eq!(int_cf.len(), 1);
        assert!(int_cb.is_empty());
        assert!(int_f.is_empty());
        assert!(int_b.is_empty());
    }

    #[test]
    fn parity_all_front_polygon_routes_to_front() {
        let partitioner = axis_plane_x_zero();
        // Triangle entirely at x=5: all FRONT.
        let poly = integer_triangle(p(5, 0, 0), p(5, 4, 0), p(5, 0, 4), 0);
        run_parity(&poly, &partitioner);
    }

    #[test]
    fn parity_all_back_polygon_routes_to_back() {
        let partitioner = axis_plane_x_zero();
        let poly = integer_triangle(p(-5, 0, 0), p(-5, 4, 0), p(-5, 0, 4), 0);
        run_parity(&poly, &partitioner);
    }

    #[test]
    fn parity_spanning_polygon_splits_with_matching_intersection() {
        // Triangle (-2,0,0), (2,0,0), (0,4,0) split by x=0 plane.
        // Expected behaviour (from manual trace):
        //   types = [BACK, FRONT, COPLANAR]
        //   polygon_type = SPANNING
        //   front fragment: [(0,0,0)_split, (2,0,0), (0,4,0)]
        //   back fragment:  [(-2,0,0), (0,0,0)_split, (0,4,0)]
        // Both >= 3 vertices, both pushed.
        let partitioner = axis_plane_x_zero();
        let poly = integer_triangle(p(-2, 0, 0), p(2, 0, 0), p(0, 4, 0), 13);
        run_parity(&poly, &partitioner);

        // Cross-check: each output bucket has the expected count.
        let mut cf = Vec::new();
        let mut cb = Vec::new();
        let mut f = Vec::new();
        let mut b = Vec::new();
        poly.split(&partitioner, &mut cf, &mut cb, &mut f, &mut b);
        assert!(cf.is_empty() && cb.is_empty());
        assert_eq!(f.len(), 1, "front fragment");
        assert_eq!(b.len(), 1, "back fragment");
    }

    #[test]
    fn parity_diagonal_partitioner_off_axis_split() {
        // Use a partitioner with a non-axis-aligned normal so the
        // intersection vertex isn't a convenient integer. The integer
        // path still snaps to a specific i32 result; the rational
        // path must canonicalize to the same.
        // Plane through (1,1,0), (1,1,1), (3,-1,0) → normal in xy.
        let partitioner = Plane3::from_points(p(1, 1, 0), p(1, 1, 1), p(3, -1, 0));
        // Triangle with vertices on different sides.
        let poly = integer_triangle(p(-3, 5, 0), p(5, 5, 0), p(0, -2, 0), 0);
        run_parity(&poly, &partitioner);
    }

    #[test]
    fn compute_intersection_rat_lifted_integer_endpoints_match_round_div() {
        // Edge (-1,0,0) → (1,0,0) crossing x=0 plane: classical midpoint
        // (0,0,0). Rational form should normalize to that integer.
        let plane = axis_plane_x_zero();
        let p0 = BspPoint3::lift(p(-1, 0, 0));
        let p1 = BspPoint3::lift(p(1, 0, 0));
        let intersection = compute_intersection_rat(&p0, &p1, &plane).unwrap();
        assert_eq!(intersection.snap().unwrap(), p(0, 0, 0));
    }

    #[test]
    fn compute_intersection_rat_off_center_match() {
        // Edge (-2,0,0) → (4,0,0) crossing x=0: parametric t = 2/6 = 1/3,
        // intersection at (0,0,0). Snapped integer: (0,0,0).
        let plane = axis_plane_x_zero();
        let p0 = BspPoint3::lift(p(-2, 0, 0));
        let p1 = BspPoint3::lift(p(4, 0, 0));
        let intersection = compute_intersection_rat(&p0, &p1, &plane).unwrap();
        assert_eq!(intersection.snap().unwrap(), p(0, 0, 0));
    }

    #[test]
    fn compute_intersection_rat_preserves_rational_through_normalization() {
        // Edge (-1,1,1) → (3,5,9) crossing x=0: parametric t = 1/4.
        // Intersection: (0, 1+1, 1+2) = (0, 2, 3). Integer.
        let plane = axis_plane_x_zero();
        let p0 = BspPoint3::lift(p(-1, 1, 1));
        let p1 = BspPoint3::lift(p(3, 5, 9));
        let intersection = compute_intersection_rat(&p0, &p1, &plane).unwrap();
        assert_eq!(intersection.snap().unwrap(), p(0, 2, 3));
    }

    #[test]
    fn compute_intersection_rat_endpoints_same_side_errors() {
        // Both at x=1, x=0 plane: doesn't cross. denom = 0.
        let plane = axis_plane_x_zero();
        let p0 = BspPoint3::lift(p(1, 0, 0));
        let p1 = BspPoint3::lift(p(1, 1, 1));
        let err = compute_intersection_rat(&p0, &p1, &plane).unwrap_err();
        assert!(matches!(err, CsgError::NumericOverflow { .. }));
    }

    fn run_parity(poly: &Polygon, partitioner: &Plane3) {
        let mut int_cf = Vec::new();
        let mut int_cb = Vec::new();
        let mut int_f = Vec::new();
        let mut int_b = Vec::new();
        poly.split(
            partitioner,
            &mut int_cf,
            &mut int_cb,
            &mut int_f,
            &mut int_b,
        );

        let bp = BspPolygon::lift(poly);
        let mut rat_cf = Vec::new();
        let mut rat_cb = Vec::new();
        let mut rat_f = Vec::new();
        let mut rat_b = Vec::new();
        bp.split(
            partitioner,
            &mut rat_cf,
            &mut rat_cb,
            &mut rat_f,
            &mut rat_b,
        )
        .unwrap();
        let rat_cf = canonicalize(rat_cf).unwrap();
        let rat_cb = canonicalize(rat_cb).unwrap();
        let rat_f = canonicalize(rat_f).unwrap();
        let rat_b = canonicalize(rat_b).unwrap();

        assert_buckets_match(&int_cf, &rat_cf);
        assert_buckets_match(&int_cb, &rat_cb);
        assert_buckets_match(&int_f, &rat_f);
        assert_buckets_match(&int_b, &rat_b);
    }

    fn assert_buckets_match(int: &[Polygon], rat: &[Polygon]) {
        assert_eq!(int.len(), rat.len(), "bucket polygon count mismatch");
        for (i, (a, b)) in int.iter().zip(rat.iter()).enumerate() {
            assert_eq!(a.vertices, b.vertices, "polygon {i} vertex mismatch");
            assert_eq!(a.color, b.color, "polygon {i} color mismatch");
            assert_plane_eq(&a.plane, &b.plane);
        }
    }

    fn assert_plane_eq(a: &Plane3, b: &Plane3) {
        assert_eq!(a.n_x, b.n_x);
        assert_eq!(a.n_y, b.n_y);
        assert_eq!(a.n_z, b.n_z);
        assert_eq!(a.d, b.d);
    }
}
