//! Raw-BSP probe — answers three questions for a single failing CSG
//! cell:
//!
//! 1. **At which step of `union_raw` does the directed-edge imbalance
//!    first appear?** (build → na.clip_to(nb) → nb.clip_to(na) →
//!    invert/clip/invert → final build).
//! 2. **Is the missing reverse edge geometrically present somewhere
//!    near the unmatched edge** (snap drift across plane buckets) **or
//!    completely absent** (BSP composition never created it)?
//! 3. **Is the imbalance symmetric** between A's surface and B's
//!    surface, or one-sided?
//!
//! Marked `#[ignore]` so it runs only on demand. Will outlive the
//! immediate fix as a regression diagnostic for issue 370 and any
//! future BSP rim-mismatch bugs.

use aether_dsl_mesh::csg::bsp::BspTree;
use aether_dsl_mesh::csg::polygon::Polygon;
use aether_dsl_mesh::mesh::mesh_polygons_pre_cleanup;
use aether_dsl_mesh::parse;
use std::collections::HashMap;

/// Vertex coordinate triple keyed in the directed-edge multiset.
type VertKey = (i32, i32, i32);
/// Directed edge keyed by its endpoint coordinate triples.
type EdgeKey = (VertKey, VertKey);

/// Build the directed-edge multiset across a polygon stream — same
/// shape as `cleanup::merge::build_global_directed`, but on raw
/// polygons before cleanup runs. Edges keyed on the polygon's own
/// fixed-point Point3 coords (not VertexId, because BSP output isn't
/// indexed yet).
fn directed_edges(polys: &[Polygon]) -> HashMap<EdgeKey, u32> {
    let mut map = HashMap::new();
    for poly in polys {
        let n = poly.vertices.len();
        for i in 0..n {
            let a = poly.vertices[i];
            let b = poly.vertices[(i + 1) % n];
            if a == b {
                continue;
            }
            let ka = (a.x, a.y, a.z);
            let kb = (b.x, b.y, b.z);
            *map.entry((ka, kb)).or_insert(0) += 1;
        }
    }
    map
}

/// Count edges whose forward and reverse counts differ. Each unmatched
/// directed edge contributes one to the count (so a single missing
/// twin shows up as 2: one forward unmatched, one reverse unmatched
/// from the perspective of canonical pair iteration). We just total
/// the absolute imbalance across canonical pairs.
fn imbalance_count(directed: &HashMap<EdgeKey, u32>) -> u32 {
    let mut total = 0;
    let mut seen = std::collections::HashSet::new();
    for &(a, b) in directed.keys() {
        let canonical = if a < b { (a, b) } else { (b, a) };
        if !seen.insert(canonical) {
            continue;
        }
        let forward = directed.get(&(a, b)).copied().unwrap_or(0);
        let reverse = directed.get(&(b, a)).copied().unwrap_or(0);
        total += forward.abs_diff(reverse);
    }
    total
}

/// For each canonical-pair imbalance (forward != reverse), classify the
/// unmatched directed edge by:
///
///  - whether ANY edge with both endpoints exactly matching the reverse
///    coords exists elsewhere in `polys` (count > 0 implies cleanup, not
///    BSP, must close the gap),
///  - the owning polygon's `color` (so we can split unmatched-on-A vs
///    unmatched-on-B).
fn report_unmatched(label: &str, polys: &[Polygon]) {
    let directed = directed_edges(polys);
    let mut canonicals: std::collections::HashSet<EdgeKey> = std::collections::HashSet::new();
    let mut unmatched: Vec<(VertKey, VertKey, u32, u32)> = Vec::new();
    for &(a, b) in directed.keys() {
        let canonical = if a < b { (a, b) } else { (b, a) };
        if !canonicals.insert(canonical) {
            continue;
        }
        let forward = directed.get(&(a, b)).copied().unwrap_or(0);
        let reverse = directed.get(&(b, a)).copied().unwrap_or(0);
        match forward.cmp(&reverse) {
            std::cmp::Ordering::Greater => unmatched.push((a, b, forward, reverse)),
            std::cmp::Ordering::Less => unmatched.push((b, a, reverse, forward)),
            std::cmp::Ordering::Equal => {}
        }
    }

    // Bucket every unmatched edge by owning-polygon color.
    let mut by_color: HashMap<u32, u32> = HashMap::new();
    for (a, b, _, _) in &unmatched {
        for poly in polys {
            let n = poly.vertices.len();
            for i in 0..n {
                let p = poly.vertices[i];
                let q = poly.vertices[(i + 1) % n];
                if (p.x, p.y, p.z) == *a && (q.x, q.y, q.z) == *b {
                    *by_color.entry(poly.color).or_insert(0) += 1;
                    break;
                }
            }
        }
    }

    // Vertex-coord identity buckets — does each unmatched endpoint
    // exist exactly nowhere else, or only in its single owning edge?
    // A unique-coord endpoint indicates BSP made a vertex that no other
    // polygon shares; a multi-coord endpoint indicates the rim point
    // exists multiple times but always with the same coord (so the
    // partner edge would just be a directed-flip away — yet it's not
    // there).
    let mut vertex_use: HashMap<VertKey, u32> = HashMap::new();
    for poly in polys {
        for v in &poly.vertices {
            *vertex_use.entry((v.x, v.y, v.z)).or_insert(0) += 1;
        }
    }

    println!("  {label}: {} unmatched directed edges", unmatched.len());
    let mut color_keys: Vec<u32> = by_color.keys().copied().collect();
    color_keys.sort();
    for c in &color_keys {
        println!("    by color {} (raw): {} edges", c, by_color[c]);
    }
    let mut endpoint_solo = 0;
    let mut endpoint_shared = 0;
    for (a, b, _, _) in &unmatched {
        if vertex_use.get(a).copied().unwrap_or(0) <= 1 {
            endpoint_solo += 1;
        } else {
            endpoint_shared += 1;
        }
        if vertex_use.get(b).copied().unwrap_or(0) <= 1 {
            endpoint_solo += 1;
        } else {
            endpoint_shared += 1;
        }
    }
    println!(
        "    endpoint usage: {} solo (vertex appears in ≤1 polygon), {} shared (≥2)",
        endpoint_solo, endpoint_shared
    );
}

fn run(label: &str, a_dsl: &str, b_dsl: &str) {
    println!("\n=== {label} ===");
    let polys_a = mesh_polygons_pre_cleanup(&parse(a_dsl).unwrap()).unwrap();
    let polys_b = mesh_polygons_pre_cleanup(&parse(b_dsl).unwrap()).unwrap();
    println!(
        "input: A = {} polys (color counts), B = {} polys",
        polys_a.len(),
        polys_b.len()
    );
    let imb_a_only = imbalance_count(&directed_edges(&polys_a));
    let imb_b_only = imbalance_count(&directed_edges(&polys_b));
    println!(
        "  A solo imbalance = {} (should be 0 for closed mesh)",
        imb_a_only
    );
    println!(
        "  B solo imbalance = {} (should be 0 for closed mesh)",
        imb_b_only
    );

    // Inline the union_raw sequence with snapshots between each step.
    let mut na = BspTree::new();
    let mut nb = BspTree::new();
    na.build(polys_a.clone()).unwrap();
    nb.build(polys_b.clone()).unwrap();

    let snap0_a = na.all_polygons();
    let snap0_b = nb.all_polygons();
    let combined0: Vec<Polygon> = snap0_a.iter().chain(snap0_b.iter()).cloned().collect();
    println!(
        "  step 0 (built):                  combined imbalance = {}",
        imbalance_count(&directed_edges(&combined0))
    );

    na.clip_to(&nb).unwrap();
    let snap1_a = na.all_polygons();
    let combined1: Vec<Polygon> = snap1_a.iter().chain(snap0_b.iter()).cloned().collect();
    println!(
        "  step 1 (na.clip_to(nb)):         na imbalance solo = {}, combined = {}",
        imbalance_count(&directed_edges(&snap1_a)),
        imbalance_count(&directed_edges(&combined1))
    );

    nb.clip_to(&na).unwrap();
    let snap2_b = nb.all_polygons();
    let combined2: Vec<Polygon> = snap1_a.iter().chain(snap2_b.iter()).cloned().collect();
    println!(
        "  step 2 (nb.clip_to(na)):         nb imbalance solo = {}, combined = {}",
        imbalance_count(&directed_edges(&snap2_b)),
        imbalance_count(&directed_edges(&combined2))
    );

    nb.invert();
    nb.clip_to(&na).unwrap();
    nb.invert();
    let snap3_b = nb.all_polygons();
    let combined3: Vec<Polygon> = snap1_a.iter().chain(snap3_b.iter()).cloned().collect();
    println!(
        "  step 3 (invert/clip/invert):     nb imbalance solo = {}, combined = {}",
        imbalance_count(&directed_edges(&snap3_b)),
        imbalance_count(&directed_edges(&combined3))
    );

    let extra = nb.all_polygons();
    na.build(extra).unwrap();
    let final_polys = na.all_polygons();
    println!(
        "  step 4 (na.build(nb)):           final {} polys, imbalance = {}",
        final_polys.len(),
        imbalance_count(&directed_edges(&final_polys))
    );

    // Detailed report on the final result — proximity scan for each
    // unmatched edge tells us whether the reverse is "nearby" (snap
    // drift) or absent.
    report_unmatched("FINAL", &final_polys);
}

#[test]
#[ignore = "probe only — run on demand for issue 370 raw-BSP diagnosis"]
fn probe_union_curved_x_sphere_raw() {
    run(
        "union cylinder × sphere (raw BSP)",
        "(cylinder 0.5 1.0 8 :color 0)",
        "(translate (0.3 0.15 0.05) (sphere 0.5 8 :color 1))",
    );
    run(
        "union sphere × sphere (raw BSP)",
        "(sphere 0.5 8 :color 0)",
        "(translate (0.3 0.15 0.05) (sphere 0.5 8 :color 1))",
    );
}
