//! Grid vertex clustering — the decimation behind the silhouette pass.
//!
//! A silhouette is where the surface turns away from the eye, which is a
//! property of the form rather than of the carving, so it does not need the
//! face count the creases do. Clustering is the right decimation for that
//! and the wrong one for anything else: it preserves the large-scale shape
//! and destroys exactly the fine relief the crease pass is looking for, so
//! creases and occlusion stay on the fine mesh.
//!
//! Grid clustering rather than edge collapse because the cost has to be
//! paid at load and the quality difference does not survive the level set.
//! Clustering is one pass over the vertices and one over the faces; a
//! quadric collapse is a priority queue over every edge, for a curve whose
//! shape is decided by the silhouette solve rather than by the triangles it
//! walks.

use aether_math::Vec3;

use std::collections::{BTreeMap, BTreeSet};

/// Snap every vertex to a lattice cell, average the vertices that land in
/// each, and rebuild the face list over the representatives.
///
/// `cells` is the lattice resolution along the longest axis. The other two
/// share the same cell size, so a tall subject is not squashed into the
/// same cell count as a wide one.
pub fn cluster(positions: &[Vec3], faces: &[[u32; 3]], min: Vec3, max: Vec3, cells: u32) -> (Vec<Vec3>, Vec<[u32; 3]>) {
    let extent = max - min;
    let size = extent.x.max(extent.y).max(extent.z) / cells.max(1) as f32;
    // A subject with no extent, or one whose bounds came back as NaN, has
    // no lattice to snap to — hand back what arrived rather than dividing
    // by it.
    if !(size.is_finite() && size > 0.0) {
        return (positions.to_vec(), faces.to_vec());
    }

    // Accumulate a running sum per occupied cell, so the representative is
    // the centroid of what landed there rather than the cell centre —
    // which keeps a thin feature (an ear blade, a hem) on the surface
    // instead of pushing it to the middle of its cell.
    let mut cell_of_vertex: Vec<u32> = Vec::with_capacity(positions.len());
    let mut occupied: BTreeMap<(i32, i32, i32), u32> = BTreeMap::new();
    let mut sums: Vec<Vec3> = Vec::new();
    let mut counts: Vec<f32> = Vec::new();

    for &p in positions {
        let key = (
            ((p.x - min.x) / size).floor() as i32,
            ((p.y - min.y) / size).floor() as i32,
            ((p.z - min.z) / size).floor() as i32,
        );
        let next = occupied.len() as u32;
        let index = *occupied.entry(key).or_insert(next);
        if index as usize == sums.len() {
            sums.push(Vec3::new(0.0, 0.0, 0.0));
            counts.push(0.0);
        }
        sums[index as usize] += p;
        counts[index as usize] += 1.0;
        cell_of_vertex.push(index);
    }

    let coarse_positions: Vec<Vec3> =
        sums.iter().zip(&counts).map(|(&sum, &count)| sum * (1.0 / count.max(1.0))).collect();

    let mut seen: BTreeSet<[u32; 3]> = BTreeSet::new();
    let mut coarse_faces = Vec::new();
    for face in faces {
        let mapped =
            [cell_of_vertex[face[0] as usize], cell_of_vertex[face[1] as usize], cell_of_vertex[face[2] as usize]];
        // Two corners in one cell leaves no triangle, only a degenerate
        // sliver whose normal is noise.
        if mapped[0] == mapped[1] || mapped[1] == mapped[2] || mapped[2] == mapped[0] {
            continue;
        }
        if seen.insert(canonical(mapped)) {
            coarse_faces.push(mapped);
        }
    }

    (coarse_positions, coarse_faces)
}

/// The rotation of a face starting at its smallest index.
///
/// Rotation, never a sort. A sorted index triple collapses a face and its
/// mirror onto the same key, so deduplicating on one silently keeps
/// whichever of the two arrived first and randomises the normal sign across
/// the mesh. Nothing errors: the drawing simply comes back without a
/// silhouette, because the zero set of `view . normal` has no coherent
/// answer to find. Rotation preserves the cyclic order, so it identifies a
/// repeated face while telling a face from its mirror.
fn canonical(face: [u32; 3]) -> [u32; 3] {
    let [a, b, c] = face;
    if a <= b && a <= c {
        [a, b, c]
    } else if b <= a && b <= c {
        [b, c, a]
    } else {
        [c, a, b]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: dedup keeps a face apart from its mirror.
    ///
    /// This is the one decision in this module that fails silently. A
    /// sorted key maps `[0,1,2]` and `[0,2,1]` — a face and the same face
    /// wound the other way — onto the same entry, so one of them is
    /// dropped and the survivor's winding is whichever the input order
    /// happened to put first. The result is a mesh with no consistent
    /// orientation and an empty silhouette, with nothing having errored.
    #[test]
    fn canonical_rotation_separates_a_face_from_its_mirror() {
        assert_eq!(canonical([2, 0, 1]), canonical([0, 1, 2]), "a rotation of a face is the same face");
        assert_ne!(canonical([0, 2, 1]), canonical([0, 1, 2]), "a mirrored face is a different face");
    }
}
