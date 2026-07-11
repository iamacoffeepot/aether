use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use aether_capabilities::render::{DrawTriangle, Vertex};

use super::constants::OCTIMETERS_PER_METER;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[allow(clippy::struct_field_names)] // Axis fields keep the octimeter unit explicit at every assertion site.
pub(super) struct QuantizedVertexOctimeters {
    pub(super) x_octimeters: i64,
    pub(super) y_octimeters: i64,
    pub(super) z_octimeters: i64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct QuantizedGroundPointOctimeters {
    pub(super) x_octimeters: i64,
    pub(super) z_octimeters: i64,
}

impl QuantizedVertexOctimeters {
    pub(super) fn ground(self) -> QuantizedGroundPointOctimeters {
        QuantizedGroundPointOctimeters { x_octimeters: self.x_octimeters, z_octimeters: self.z_octimeters }
    }
}

#[derive(Clone, Copy, Debug)]
struct HeightSpanMeters {
    low_y_meters: f32,
    high_y_meters: f32,
}

#[derive(Clone, Copy, Debug)]
#[allow(clippy::struct_field_names)] // Semantic seam geometry keeps axis and meter units named, per the atlas contract.
pub(super) struct CapSplitMeters {
    pub(super) x_meters: f32,
    pub(super) z_meters: f32,
    pub(super) low_y_meters: f32,
    pub(super) high_y_meters: f32,
}

pub(super) fn quantize_meters_to_octimeters(meters: f32) -> i64 {
    (meters * OCTIMETERS_PER_METER).round() as i64
}

pub(super) fn quantized_vertex_octimeters(vertex: &Vertex) -> QuantizedVertexOctimeters {
    QuantizedVertexOctimeters {
        x_octimeters: quantize_meters_to_octimeters(vertex.x),
        y_octimeters: quantize_meters_to_octimeters(vertex.y),
        z_octimeters: quantize_meters_to_octimeters(vertex.z),
    }
}

/// The vertical span of a triangle. A wall face stands well above its cap,
/// while the cap itself remains near-flat.
pub(super) fn y_span(triangle: &DrawTriangle) -> f32 {
    let max_y_meters = triangle.verts.iter().map(|vertex| vertex.y).fold(f32::MIN, f32::max);
    let min_y_meters = triangle.verts.iter().map(|vertex| vertex.y).fold(f32::MAX, f32::min);
    max_y_meters - min_y_meters
}

/// Signed twice-area of a triangle projected onto the XZ ground plane.
pub(super) fn signed_xz_area_doubled(triangle: &DrawTriangle) -> f32 {
    let [a, b, c] = &triangle.verts;
    (b.x - a.x) * (c.z - a.z) - (c.x - a.x) * (b.z - a.z)
}

/// Twice the XZ-projected area of a triangle. A vertical wall projects to a
/// line while every cap triangle keeps a footprint.
pub(super) fn xz_area_doubled(triangle: &DrawTriangle) -> f32 {
    signed_xz_area_doubled(triangle).abs()
}

fn wall_top_edge_length(triangle: &DrawTriangle, min_top_y_meters: f32) -> Option<f32> {
    let tops: Vec<&Vertex> = triangle.verts.iter().filter(|vertex| vertex.y > min_top_y_meters).collect();
    (tops.len() == 2).then(|| (tops[1].x - tops[0].x).hypot(tops[1].z - tops[0].z))
}

pub(super) fn total_wall_top_edge_length(mesh: &[DrawTriangle], min_top_y_meters: f32) -> f32 {
    mesh.iter()
        .filter(|triangle| y_span(triangle) > 0.5)
        .filter_map(|triangle| wall_top_edge_length(triangle, min_top_y_meters))
        .sum()
}

/// Quantized XZ positions where cap vertices occupy two plates more than half
/// a meter apart: every such split must be spanned by a wall at both plates.
pub(super) fn cap_splits(mesh: &[DrawTriangle]) -> Vec<CapSplitMeters> {
    let mut heights = BTreeMap::<QuantizedGroundPointOctimeters, HeightSpanMeters>::new();
    for vertex in mesh.iter().filter(|triangle| xz_area_doubled(triangle) > 1e-6).flat_map(|triangle| &triangle.verts) {
        let point = quantized_vertex_octimeters(vertex).ground();
        let span = heights.entry(point).or_insert(HeightSpanMeters { low_y_meters: vertex.y, high_y_meters: vertex.y });
        span.low_y_meters = span.low_y_meters.min(vertex.y);
        span.high_y_meters = span.high_y_meters.max(vertex.y);
    }
    heights
        .into_iter()
        .filter(|(_, span)| span.high_y_meters - span.low_y_meters > 0.5)
        .map(|(point, span)| CapSplitMeters {
            x_meters: point.x_octimeters as f32 / OCTIMETERS_PER_METER,
            z_meters: point.z_octimeters as f32 / OCTIMETERS_PER_METER,
            low_y_meters: span.low_y_meters,
            high_y_meters: span.high_y_meters,
        })
        .collect()
}

fn wall_covers_plate_edge(walls: &[&DrawTriangle], x_meters: f32, z_meters: f32, y_meters: f32) -> bool {
    walls.iter().any(|triangle| {
        (0..3).any(|edge_index| {
            let start = &triangle.verts[edge_index];
            let end = &triangle.verts[(edge_index + 1) % 3];
            let length_squared_meters = (end.x - start.x).powi(2) + (end.z - start.z).powi(2);
            if length_squared_meters < 1e-8 {
                return false;
            }
            if ((end.x - start.x) * (z_meters - start.z) - (end.z - start.z) * (x_meters - start.x)).abs() > 1e-3 {
                return false;
            }
            let edge_fraction = ((x_meters - start.x) * (end.x - start.x) + (z_meters - start.z) * (end.z - start.z))
                / length_squared_meters;
            (-1e-4..=1.0 + 1e-4).contains(&edge_fraction)
                && (start.y + (end.y - start.y) * edge_fraction - y_meters).abs() < 1e-3
        })
    })
}

pub(super) fn assert_height_break_walls_close_where(
    mesh: &[DrawTriangle],
    case_name: &str,
    mut include_split: impl FnMut(CapSplitMeters) -> bool,
) {
    let splits: Vec<CapSplitMeters> = cap_splits(mesh).into_iter().filter(|split| include_split(*split)).collect();
    assert!(!splits.is_empty(), "{case_name}: the authored height break must split the caps");
    let walls: Vec<&DrawTriangle> =
        mesh.iter().filter(|triangle| xz_area_doubled(triangle) < 1e-6 && y_span(triangle) > 0.25).collect();
    for split in splits {
        assert!(
            wall_covers_plate_edge(&walls, split.x_meters, split.z_meters, split.high_y_meters),
            "{case_name}: split at ({}, {}) has no wall top at {}",
            split.x_meters,
            split.z_meters,
            split.high_y_meters,
        );
        assert!(
            wall_covers_plate_edge(&walls, split.x_meters, split.z_meters, split.low_y_meters),
            "{case_name}: split at ({}, {}) has no wall base at {}",
            split.x_meters,
            split.z_meters,
            split.low_y_meters,
        );
    }
}

pub(super) fn assert_height_break_walls_close(mesh: &[DrawTriangle], case_name: &str) {
    assert_height_break_walls_close_where(mesh, case_name, |_| true);
}
