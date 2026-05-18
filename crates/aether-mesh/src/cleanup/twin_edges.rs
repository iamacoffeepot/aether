//! Twin-edge cancellation primitive shared by [`super::merge`] and
//! [`super::provenance`].
//!
//! Both modules need the same "for each canonical edge, push the
//! surplus copies of whichever direction dominates" walk over a
//! directed-edge multiplicity map. Merge runs it per
//! `(plane, color)` bucket (`boundary_edges_after_twin_cancellation`);
//! provenance runs it globally over the whole mesh
//! (`unmatched_edges`). Per issue #350, the naive
//! "both directions present ⇒ cancel" boolean overcounts by one and
//! tears the boundary open; the multiplicity-preserving form here is
//! the correct cancellation.

use super::mesh::VertexId;
use std::collections::{HashMap, HashSet};

/// Collect surviving directed edges after twin-pair cancellation.
/// For each canonical (smaller-id-first) edge, the surplus copies of
/// the dominant direction are pushed in that direction.
pub(super) fn surviving_directed_edges(
    directed: &HashMap<(VertexId, VertexId), u32>,
) -> Vec<(VertexId, VertexId)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut keys: Vec<(VertexId, VertexId)> = directed.keys().copied().collect();
    keys.sort_unstable();
    for (a, b) in keys {
        let canonical = if a < b { (a, b) } else { (b, a) };
        if !seen.insert(canonical) {
            continue;
        }
        let forward = directed.get(&(a, b)).copied().unwrap_or(0);
        let reverse = directed.get(&(b, a)).copied().unwrap_or(0);
        match forward.cmp(&reverse) {
            std::cmp::Ordering::Greater => {
                for _ in 0..(forward - reverse) {
                    out.push((a, b));
                }
            }
            std::cmp::Ordering::Less => {
                for _ in 0..(reverse - forward) {
                    out.push((b, a));
                }
            }
            std::cmp::Ordering::Equal => {}
        }
    }
    out
}
