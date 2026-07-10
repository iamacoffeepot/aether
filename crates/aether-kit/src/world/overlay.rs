#![allow(clippy::cast_precision_loss)]

//! Pure terrain-anchored mark overlay geometry.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::f32::consts::TAU;

use aether_capabilities::render::{DrawTriangle, Vertex};
use aether_math::Rgb;

use crate::mark::{Mark, MarkGeometry, MarkId, MarkRef};

use super::{World, WorldPoint};

/// Lift over the sampled top surface, preventing ground z-fighting.
pub const MARK_OVERLAY_LIFT_METERS: f32 = 0.02;
/// Ordinary point-marker radius.
pub const MARK_POINT_RADIUS_METERS: f32 = 0.12;
/// Ordinary path/area ribbon half-width.
pub const MARK_PATH_HALF_WIDTH_METERS: f32 = 0.05;
/// Selected path/area outline half-width.
pub const MARK_SELECTED_HALF_WIDTH_METERS: f32 = 0.09;
/// Selected vertex-handle radius.
pub const MARK_SELECTED_HANDLE_RADIUS_METERS: f32 = 0.10;
/// Stable ordinary overlay color.
pub const MARK_OVERLAY_COLOR: Rgb = Rgb::from_srgb8(50, 220, 235);
/// Stable selected overlay color.
pub const MARK_SELECTED_COLOR: Rgb = Rgb::from_srgb8(255, 190, 48);

const POINT_SEGMENTS: usize = 8;
const OCTIMETERS_PER_METER: f32 = 256.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct GroundPointMeters {
    x_meters: f32,
    z_meters: f32,
}

impl From<WorldPoint> for GroundPointMeters {
    fn from(point: WorldPoint) -> Self {
        Self {
            x_meters: point.x_octimeters as f32 / OCTIMETERS_PER_METER,
            z_meters: point.z_octimeters as f32 / OCTIMETERS_PER_METER,
        }
    }
}

fn anchored_vertex(world: &World, point: GroundPointMeters, color: Rgb) -> Option<Vertex> {
    let surface = world.terrain_surface_at(point.x_meters, point.z_meters)?;
    Some(Vertex {
        x: point.x_meters,
        y: surface.height_meters + MARK_OVERLAY_LIFT_METERS,
        z: point.z_meters,
        color,
    })
}

fn push_triangle(
    world: &World,
    triangles: &mut Vec<DrawTriangle>,
    first: GroundPointMeters,
    second: GroundPointMeters,
    third: GroundPointMeters,
    color: Rgb,
) {
    let Some(first) = anchored_vertex(world, first, color) else {
        return;
    };
    let Some(second) = anchored_vertex(world, second, color) else {
        return;
    };
    let Some(third) = anchored_vertex(world, third, color) else {
        return;
    };
    triangles.push(DrawTriangle {
        verts: [first, second, third],
    });
}

fn disc_points(center: GroundPointMeters, radius_meters: f32) -> Vec<GroundPointMeters> {
    (0..POINT_SEGMENTS)
        .map(|index| {
            let angle = index as f32 * TAU / POINT_SEGMENTS as f32;
            GroundPointMeters {
                x_meters: angle.cos().mul_add(radius_meters, center.x_meters),
                z_meters: angle.sin().mul_add(radius_meters, center.z_meters),
            }
        })
        .collect()
}

fn emit_disc(
    world: &World,
    triangles: &mut Vec<DrawTriangle>,
    center: GroundPointMeters,
    radius_meters: f32,
    color: Rgb,
) {
    let ring = disc_points(center, radius_meters);
    for index in 0..ring.len() {
        push_triangle(
            world,
            triangles,
            center,
            ring[index],
            ring[(index + 1) % ring.len()],
            color,
        );
    }
}

fn emit_segment(
    world: &World,
    triangles: &mut Vec<DrawTriangle>,
    start: GroundPointMeters,
    end: GroundPointMeters,
    half_width_meters: f32,
    color: Rgb,
) {
    let delta_x = end.x_meters - start.x_meters;
    let delta_z = end.z_meters - start.z_meters;
    let length_squared = delta_x.mul_add(delta_x, delta_z * delta_z);
    if !length_squared.is_finite() || length_squared == 0.0 {
        return;
    }
    let scale = half_width_meters / length_squared.sqrt();
    let offset_x = -delta_z * scale;
    let offset_z = delta_x * scale;
    let start_left = GroundPointMeters {
        x_meters: start.x_meters + offset_x,
        z_meters: start.z_meters + offset_z,
    };
    let start_right = GroundPointMeters {
        x_meters: start.x_meters - offset_x,
        z_meters: start.z_meters - offset_z,
    };
    let end_left = GroundPointMeters {
        x_meters: end.x_meters + offset_x,
        z_meters: end.z_meters + offset_z,
    };
    let end_right = GroundPointMeters {
        x_meters: end.x_meters - offset_x,
        z_meters: end.z_meters - offset_z,
    };
    push_triangle(world, triangles, start_left, start_right, end_left, color);
    push_triangle(world, triangles, end_left, start_right, end_right, color);
}

fn emit_line(
    world: &World,
    triangles: &mut Vec<DrawTriangle>,
    points: &[WorldPoint],
    closed: bool,
    selected: bool,
) {
    if points.len() < 2 {
        return;
    }
    let color = if selected {
        MARK_SELECTED_COLOR
    } else {
        MARK_OVERLAY_COLOR
    };
    let half_width_meters = if selected {
        MARK_SELECTED_HALF_WIDTH_METERS
    } else {
        MARK_PATH_HALF_WIDTH_METERS
    };
    for pair in points.windows(2) {
        emit_segment(
            world,
            triangles,
            pair[0].into(),
            pair[1].into(),
            half_width_meters,
            color,
        );
    }
    if closed {
        emit_segment(
            world,
            triangles,
            points[points.len() - 1].into(),
            points[0].into(),
            half_width_meters,
            color,
        );
    }
    let join_radius = if selected {
        MARK_SELECTED_HANDLE_RADIUS_METERS
    } else {
        MARK_PATH_HALF_WIDTH_METERS
    };
    for point in points {
        emit_disc(world, triangles, (*point).into(), join_radius, color);
    }
}

fn emit_mark(world: &World, triangles: &mut Vec<DrawTriangle>, mark: &Mark, selected: bool) {
    match &mark.geometry {
        MarkGeometry::Point(point) => emit_disc(
            world,
            triangles,
            (*point).into(),
            if selected {
                MARK_SELECTED_HANDLE_RADIUS_METERS
            } else {
                MARK_POINT_RADIUS_METERS
            },
            if selected {
                MARK_SELECTED_COLOR
            } else {
                MARK_OVERLAY_COLOR
            },
        ),
        MarkGeometry::Path(points) => emit_line(world, triangles, points, false, selected),
        MarkGeometry::Area(points) => emit_line(world, triangles, points, true, selected),
    }
}

/// Generate one stable, terrain-anchored overlay batch from the latest
/// authoritative `MarkBook` projection.
pub(super) fn mark_overlay_triangles(
    world: &World,
    marks: &BTreeMap<MarkId, Mark>,
    selected: Option<MarkRef>,
) -> Vec<DrawTriangle> {
    let mut triangles = Vec::new();
    for mark in marks.values() {
        emit_mark(
            world,
            &mut triangles,
            mark,
            selected.is_some_and(|reference| reference == mark.reference()),
        );
    }
    triangles
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mark::MarkId;
    use crate::world::{CellPos, Chunk, ChunkPos, Material};

    fn flat_world(height_octimeters: i32) -> World {
        let mut chunk = Chunk::empty_boxed();
        chunk.underlay.fill(Material::Stone);
        chunk.height.fill(height_octimeters);
        let mut world = World::new();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world
    }

    fn mark(id: u32, geometry: MarkGeometry) -> Mark {
        Mark {
            id: MarkId::new(id),
            revision: 1,
            geometry,
            label: alloc::format!("mark-{id}"),
        }
    }

    fn assert_finite(triangles: &[DrawTriangle]) {
        assert!(!triangles.is_empty());
        for vertex in triangles.iter().flat_map(|triangle| triangle.verts) {
            assert!(vertex.x.is_finite());
            assert!(vertex.y.is_finite());
            assert!(vertex.z.is_finite());
        }
    }

    #[test]
    fn point_path_and_area_are_finite_and_area_closes() {
        let world = flat_world(0);
        let point = mark(1, MarkGeometry::Point(WorldPoint::new(512, 512)));
        let path = mark(
            2,
            MarkGeometry::Path(vec![
                WorldPoint::new(768, 512),
                WorldPoint::new(1024, 512),
                WorldPoint::new(1024, 512),
                WorldPoint::new(1024, 768),
            ]),
        );
        let area_points = vec![
            WorldPoint::new(1280, 512),
            WorldPoint::new(1536, 512),
            WorldPoint::new(1536, 768),
        ];
        let open_area_path = mark(4, MarkGeometry::Path(area_points.clone()));
        let area = mark(3, MarkGeometry::Area(area_points));

        let point_triangles = {
            let marks = BTreeMap::from([(point.id, point)]);
            mark_overlay_triangles(&world, &marks, None)
        };
        assert_eq!(point_triangles.len(), POINT_SEGMENTS);
        assert_finite(&point_triangles);

        let path_triangles = {
            let marks = BTreeMap::from([(path.id, path)]);
            mark_overlay_triangles(&world, &marks, None)
        };
        assert_finite(&path_triangles);

        let area_triangles = {
            let marks = BTreeMap::from([(area.id, area)]);
            mark_overlay_triangles(&world, &marks, None)
        };
        assert_finite(&area_triangles);
        let open_area_triangles = {
            let marks = BTreeMap::from([(open_area_path.id, open_area_path)]);
            mark_overlay_triangles(&world, &marks, None)
        };
        assert_eq!(
            area_triangles.len(),
            open_area_triangles.len() + 2,
            "the area adds exactly one two-triangle closing segment"
        );
    }

    #[test]
    fn vertices_reanchor_and_selected_marks_use_wider_named_style() {
        let world = flat_world(256);
        let selected_mark = mark(
            7,
            MarkGeometry::Path(vec![WorldPoint::new(512, 512), WorldPoint::new(1024, 512)]),
        );
        let reference = selected_mark.reference();
        let marks = BTreeMap::from([(selected_mark.id, selected_mark)]);
        let ordinary = mark_overlay_triangles(&world, &marks, None);
        let selected = mark_overlay_triangles(&world, &marks, Some(reference));
        assert!(selected.len() >= ordinary.len());
        assert!(
            ordinary
                .iter()
                .flat_map(|triangle| triangle.verts)
                .all(|vertex| (vertex.y - 1.02).abs() < 0.0001)
        );
        assert!(
            ordinary
                .iter()
                .flat_map(|triangle| triangle.verts)
                .all(|vertex| vertex.color == MARK_OVERLAY_COLOR)
        );
        assert!(
            selected
                .iter()
                .flat_map(|triangle| triangle.verts)
                .all(|vertex| vertex.color == MARK_SELECTED_COLOR)
        );
    }

    #[test]
    fn geometry_over_void_is_omitted() {
        let marks = BTreeMap::from([(
            MarkId::new(1),
            mark(1, MarkGeometry::Point(WorldPoint::new(128, 128))),
        )]);
        assert!(mark_overlay_triangles(&World::new(), &marks, None).is_empty());

        let world = flat_world(0);
        assert!(
            world
                .terrain_surface_at(
                    CellPos { x: 0, z: 0 }.x as f32 + 0.5,
                    CellPos { x: 0, z: 0 }.z as f32 + 0.5,
                )
                .is_some()
        );
    }
}
