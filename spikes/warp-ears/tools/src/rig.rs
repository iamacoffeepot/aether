//! Rig estimate: the base→tip axis of the ear and the head plane it folds onto.

use crate::extract::Box3;

pub struct Rig {
    pub base: [f64; 3],
    pub tip: [f64; 3],
    pub axis: [f64; 3],
    pub length: f64,
    pub joint2: [f64; 3],
    pub contact_point: [f64; 3],
    pub contact_normal: [f64; 3],
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize(v: [f64; 3]) -> [f64; 3] {
    let length = dot(v, v).sqrt();
    if length == 0.0 {
        [0.0, 0.0, 0.0]
    } else {
        [v[0] / length, v[1] / length, v[2] / length]
    }
}

/// The ear's long axis by PCA, then base and tip as the centroids of the
/// extreme deciles along it. Deciles rather than single extreme voxels: one
/// stray shell voxel should not define where the bone starts.
///
/// `head_centre` is in the same box-local space and orients the contact
/// plane — the plane is tangent to the skull at the base, so its normal is
/// the outward radial direction there.
pub fn estimate(box3: &Box3, head_centre: [f64; 3]) -> Rig {
    let points: Vec<[f64; 3]> = (0..box3.dims[0])
        .flat_map(|i| (0..box3.dims[1]).flat_map(move |j| (0..box3.dims[2]).map(move |k| (i, j, k))))
        .filter(|&(i, j, k)| box3.cells[box3.offset([i, j, k])] != 0)
        .map(|(i, j, k)| [i as f64, j as f64, k as f64])
        .collect();

    let count = points.len() as f64;
    let mut centroid = [0.0; 3];
    for p in &points {
        for axis in 0..3 {
            centroid[axis] += p[axis] / count;
        }
    }

    // Power iteration on the covariance — three components, so a handful of
    // passes converges well past the precision this estimate is used at.
    let mut covariance = [[0.0f64; 3]; 3];
    for p in &points {
        let d = sub(*p, centroid);
        for a in 0..3 {
            for b in 0..3 {
                covariance[a][b] += d[a] * d[b] / count;
            }
        }
    }

    let mut direction = [0.0, 1.0, 0.0];
    for _ in 0..200 {
        let mut next = [0.0f64; 3];
        for a in 0..3 {
            for b in 0..3 {
                next[a] += covariance[a][b] * direction[b];
            }
        }
        direction = normalize(next);
    }
    // Point it up: the ear grows away from the head, and the up axis is the
    // box's axis 1.
    if direction[1] < 0.0 {
        direction = [-direction[0], -direction[1], -direction[2]];
    }

    let mut projections: Vec<(f64, [f64; 3])> =
        points.iter().map(|p| (dot(sub(*p, centroid), direction), *p)).collect();
    projections.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("voxel coordinates are finite"));

    let decile = (projections.len() / 10).max(1);
    let mean = |slice: &[(f64, [f64; 3])]| {
        let n = slice.len() as f64;
        let mut out = [0.0; 3];
        for (_, p) in slice {
            for axis in 0..3 {
                out[axis] += p[axis] / n;
            }
        }
        out
    };

    let base = mean(&projections[..decile]);
    let tip = mean(&projections[projections.len() - decile..]);
    let span = sub(tip, base);
    let length = dot(span, span).sqrt();
    let axis = normalize(span);
    let joint2 = [base[0] + 0.4 * span[0], base[1] + 0.4 * span[1], base[2] + 0.4 * span[2]];

    Rig { base, tip, axis, length, joint2, contact_point: base, contact_normal: normalize(sub(base, head_centre)) }
}
