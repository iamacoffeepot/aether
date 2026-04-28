//! Probe — runs the canonical issue 370 repro (sphere × sphere) and
//! the four LHS curved primitives through the boundary-edge provenance
//! analyzer, printing the per-edge stage history. Marked `#[ignore]`
//! so it runs only on demand. Will be removed once the underlying bug
//! is identified and fixed.

use aether_dsl_mesh::csg::cleanup::provenance::analyze_unmatched_boundaries;
use aether_dsl_mesh::mesh::mesh_polygons_pre_cleanup;
use aether_dsl_mesh::parse;

fn run(label: &str, dsl: &str) {
    let parsed = parse(dsl).expect("parse");
    let polys = mesh_polygons_pre_cleanup(&parsed).expect("mesh");
    let report = analyze_unmatched_boundaries(polys);
    println!("\n=== {label} ===");
    println!("DSL: {dsl}");
    println!("unmatched edges: {}", report.len());
    for r in &report {
        println!(
            "  edge=({}, {}) coords={:?} → {:?} poly={} color={} \
             reverse[w={}, t={}, m={}, s={}]",
            r.edge.0,
            r.edge.1,
            (r.coords.0.x, r.coords.0.y, r.coords.0.z),
            (r.coords.1.x, r.coords.1.y, r.coords.1.z),
            r.polygon_idx,
            r.color,
            r.reverse_post_weld,
            r.reverse_post_tjunctions,
            r.reverse_post_merge,
            r.reverse_post_slivers,
        );
        println!(
            "    plane=(n=({:.4}, {:.4}, {:.4}) d={:.4})",
            r.plane.n_x as f64 / 65536.0,
            r.plane.n_y as f64 / 65536.0,
            r.plane.n_z as f64 / 65536.0,
            r.plane.d as f64 / (65536.0_f64 * 65536.0_f64),
        );
    }
}

#[test]
#[ignore = "probe only — run on demand for issue 370 diagnosis"]
fn probe_curved_x_sphere_provenance() {
    run(
        "sphere × sphere (union)",
        "(union (sphere 0.5 8 :color 0) (translate (0.3 0.15 0.05) (sphere 0.5 8 :color 1)))",
    );
    run(
        "cylinder × sphere (union)",
        "(union (cylinder 0.5 1.0 8 :color 0) (translate (0.3 0.15 0.05) (sphere 0.5 8 :color 1)))",
    );
    run(
        "lathe × sphere (union)",
        "(union (lathe ((0 -0.5) (0.5 -0.5) (0.5 0.5) (0 0.5)) 8 :color 0) (translate (0.3 0.15 0.05) (sphere 0.5 8 :color 1)))",
    );
    run(
        "torus × sphere (union)",
        "(union (torus 0.35 0.1 8 6 :color 0) (translate (0.3 0.15 0.05) (sphere 0.5 8 :color 1)))",
    );
}
