//! Bounded terrain top-surface intersection.

use super::{
    PickTerrainResult, TerrainPickError, TerrainRay, TerrainSurface, TerrainSurfaceHit, World, WorldDirection,
    WorldPositionMeters,
};

/// Furthest terrain pick accepted from a caller.
pub const MAX_TERRAIN_PICK_DISTANCE_METERS: f32 = 512.0;
/// March at one authored height/material subcell per step.
#[allow(clippy::cast_precision_loss)] // the shared resolution is the exactly-representable value 16
pub const TERRAIN_PICK_STEP_METERS: f32 = 1.0 / super::SUBCELLS_PER_CELL_EDGE as f32;
/// Height residual both sides of a refined crossing must converge within.
#[allow(clippy::cast_precision_loss)] // the shared resolution is the exactly-representable value 16
pub const TERRAIN_PICK_EPSILON_METERS: f32 = TERRAIN_PICK_STEP_METERS / super::SUBCELLS_PER_CELL_EDGE as f32;
/// Fixed bisection work after the first above-to-surface crossing.
pub const TERRAIN_PICK_REFINEMENT_STEPS: u32 = 12;

#[derive(Clone, Copy)]
struct NormalizedDirection {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Clone, Copy)]
struct SurfaceSample {
    distance_meters: f32,
    x_meters: f32,
    ray_y_meters: f32,
    z_meters: f32,
    surface: TerrainSurface,
}

impl SurfaceSample {
    fn height_delta_meters(self) -> f32 {
        self.ray_y_meters - self.surface.height_meters
    }

    fn into_hit(self) -> TerrainSurfaceHit {
        TerrainSurfaceHit {
            position: WorldPositionMeters {
                x_meters: self.x_meters,
                y_meters: self.surface.height_meters,
                z_meters: self.z_meters,
            },
            surface: self.surface,
            ray_distance_meters: self.distance_meters,
        }
    }
}

fn normalized(direction: WorldDirection) -> Result<NormalizedDirection, TerrainPickError> {
    let magnitude_squared = direction.x_unitless.mul_add(
        direction.x_unitless,
        direction.y_unitless.mul_add(direction.y_unitless, direction.z_unitless * direction.z_unitless),
    );
    if !magnitude_squared.is_finite() {
        return Err(TerrainPickError::NonFiniteRay);
    }
    if magnitude_squared == 0.0 {
        return Err(TerrainPickError::ZeroDirection);
    }
    let inverse_magnitude = magnitude_squared.sqrt().recip();
    Ok(NormalizedDirection {
        x: direction.x_unitless * inverse_magnitude,
        y: direction.y_unitless * inverse_magnitude,
        z: direction.z_unitless * inverse_magnitude,
    })
}

fn sample(
    world: &World,
    ray: TerrainRay,
    direction: NormalizedDirection,
    distance_meters: f32,
) -> Option<SurfaceSample> {
    let x_meters = direction.x.mul_add(distance_meters, ray.origin.x_meters);
    let ray_y_meters = direction.y.mul_add(distance_meters, ray.origin.y_meters);
    let z_meters = direction.z.mul_add(distance_meters, ray.origin.z_meters);
    let surface = world.terrain_surface_at(x_meters, z_meters)?;
    Some(SurfaceSample { distance_meters, x_meters, ray_y_meters, z_meters, surface })
}

fn refine_crossing(
    world: &World,
    ray: TerrainRay,
    direction: NormalizedDirection,
    mut above: SurfaceSample,
    mut below: SurfaceSample,
) -> Option<TerrainSurfaceHit> {
    for _ in 0..TERRAIN_PICK_REFINEMENT_STEPS {
        let middle_distance_meters = (above.distance_meters + below.distance_meters) * 0.5;
        let middle = sample(world, ray, direction, middle_distance_meters)?;
        if middle.height_delta_meters() <= 0.0 {
            below = middle;
        } else {
            above = middle;
        }
    }
    let above_error = above.height_delta_meters().abs();
    let below_error = below.height_delta_meters().abs();
    if above_error > TERRAIN_PICK_EPSILON_METERS || below_error > TERRAIN_PICK_EPSILON_METERS {
        return None;
    }
    Some(if above_error < below_error {
        above.into_hit()
    } else {
        below.into_hit()
    })
}

/// Intersect a validated, bounded world ray with the first markable terrain
/// top surface. Work is iterative and capped by the public distance ceiling.
pub(super) fn pick_terrain(world: &World, ray: TerrainRay) -> PickTerrainResult {
    let values = [
        ray.origin.x_meters,
        ray.origin.y_meters,
        ray.origin.z_meters,
        ray.direction.x_unitless,
        ray.direction.y_unitless,
        ray.direction.z_unitless,
        ray.max_distance_meters,
    ];
    if values.iter().any(|value| !value.is_finite()) {
        return PickTerrainResult::Rejected { error: TerrainPickError::NonFiniteRay };
    }
    if ray.max_distance_meters <= 0.0 || ray.max_distance_meters > MAX_TERRAIN_PICK_DISTANCE_METERS {
        return PickTerrainResult::Rejected { error: TerrainPickError::InvalidMaxDistance };
    }
    let direction = match normalized(ray.direction) {
        Ok(direction) => direction,
        Err(error) => return PickTerrainResult::Rejected { error },
    };

    let mut previous_above: Option<SurfaceSample> = None;
    let mut distance_meters = 0.0;
    loop {
        match sample(world, ray, direction, distance_meters) {
            Some(current) if current.height_delta_meters() <= 0.0 => {
                if let Some(above) = previous_above
                    && let Some(hit) = refine_crossing(world, ray, direction, above, current)
                {
                    return PickTerrainResult::Hit { hit };
                }
                previous_above = None;
            }
            Some(current) => previous_above = Some(current),
            None => previous_above = None,
        }
        if distance_meters >= ray.max_distance_meters {
            break;
        }
        distance_meters = (distance_meters + TERRAIN_PICK_STEP_METERS).min(ray.max_distance_meters);
    }
    PickTerrainResult::Miss
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{CellPos, Chunk, ChunkPos, Material, WaterPlane};

    fn world_with_surface(cell: CellPos, material: Material, height_octimeters: i32) -> World {
        let mut world = World::new();
        let mut chunk = Chunk::empty_boxed();
        let index = cell.chunk_index();
        chunk.underlay[index] = material;
        chunk.height[index] = height_octimeters;
        world.insert_chunk(cell.chunk(), chunk);
        world
    }

    fn downward_ray(x_meters: f32, z_meters: f32, max_distance_meters: f32) -> TerrainRay {
        TerrainRay {
            origin: WorldPositionMeters { x_meters, y_meters: 5.0, z_meters },
            direction: WorldDirection { x_unitless: 0.0, y_unitless: -2.0, z_unitless: 0.0 },
            max_distance_meters,
        }
    }

    fn hit(result: PickTerrainResult) -> TerrainSurfaceHit {
        match result {
            PickTerrainResult::Hit { hit } => hit,
            other => panic!("expected terrain hit, got {other:?}"),
        }
    }

    #[test]
    fn raised_and_negative_surfaces_hit_the_drawn_height() {
        let raised = world_with_surface(CellPos { x: 0, z: 0 }, Material::Stone, 256);
        let raised_hit = hit(pick_terrain(&raised, downward_ray(0.5, 0.5, 10.0)));
        assert!((raised_hit.position.y_meters - 1.0).abs() < 0.001);
        assert!((raised_hit.ray_distance_meters - 4.0).abs() < TERRAIN_PICK_STEP_METERS);
        assert_eq!(raised_hit.surface.mark_point, super::super::WorldPoint::new(128, 128));

        let negative = world_with_surface(CellPos { x: -1, z: -1 }, Material::Grass, 128);
        let negative_hit = hit(pick_terrain(&negative, downward_ray(-0.5, -0.5, 10.0)));
        assert_eq!(negative_hit.surface.cell, CellPos { x: -1, z: -1 });
        assert_eq!(negative_hit.surface.mark_point, super::super::WorldPoint::new(-128, -128));
        assert!((negative_hit.position.y_meters - 0.5).abs() < 0.001);
    }

    #[test]
    fn water_hits_its_plane_and_void_or_short_rays_miss() {
        let mut water = world_with_surface(CellPos { x: 0, z: 0 }, Material::Water, -256);
        water.insert_water_plane(1, WaterPlane { level_octimeters: 512 });
        let mut chunk = water.chunk(ChunkPos { x: 0, z: 0 }).expect("water chunk").clone();
        chunk.water_plane[0] = 1;
        water.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        let water_hit = hit(pick_terrain(&water, downward_ray(0.5, 0.5, 10.0)));
        assert!((water_hit.position.y_meters - 2.0).abs() < 0.001);

        assert_eq!(pick_terrain(&World::new(), downward_ray(0.5, 0.5, 10.0)), PickTerrainResult::Miss);
        assert_eq!(pick_terrain(&water, downward_ray(0.5, 0.5, 1.0)), PickTerrainResult::Miss);
    }

    #[test]
    fn horizontal_rays_do_not_turn_side_entry_or_a_cliff_step_into_top_hits() {
        let raised = world_with_surface(CellPos { x: 0, z: 0 }, Material::Stone, 256);
        let side_entry = TerrainRay {
            origin: WorldPositionMeters { x_meters: -0.5, y_meters: 0.5, z_meters: 0.5 },
            direction: WorldDirection { x_unitless: 1.0, y_unitless: 0.0, z_unitless: 0.0 },
            max_distance_meters: 2.0,
        };
        assert_eq!(pick_terrain(&raised, side_entry), PickTerrainResult::Miss);

        let mut cliff = World::new();
        let mut chunk = Chunk::empty_boxed();
        chunk.underlay[CellPos { x: 0, z: 0 }.chunk_index()] = Material::Stone;
        chunk.underlay[CellPos { x: 1, z: 0 }.chunk_index()] = Material::Stone;
        chunk.height[CellPos { x: 1, z: 0 }.chunk_index()] = 256;
        cliff.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        let cliff_step = TerrainRay {
            origin: WorldPositionMeters { x_meters: 0.5, y_meters: 0.5, z_meters: 0.5 },
            direction: side_entry.direction,
            max_distance_meters: 1.5,
        };
        assert_eq!(pick_terrain(&cliff, cliff_step), PickTerrainResult::Miss);
    }

    #[test]
    fn invalid_rays_reject_before_marching() {
        let world = World::new();
        let mut ray = downward_ray(0.0, 0.0, 1.0);
        ray.origin.x_meters = f32::NAN;
        assert_eq!(pick_terrain(&world, ray), PickTerrainResult::Rejected { error: TerrainPickError::NonFiniteRay });

        let mut ray = downward_ray(0.0, 0.0, 1.0);
        ray.direction = WorldDirection { x_unitless: 0.0, y_unitless: 0.0, z_unitless: 0.0 };
        assert_eq!(pick_terrain(&world, ray), PickTerrainResult::Rejected { error: TerrainPickError::ZeroDirection });

        let mut ray = downward_ray(0.0, 0.0, 0.0);
        assert_eq!(
            pick_terrain(&world, ray),
            PickTerrainResult::Rejected { error: TerrainPickError::InvalidMaxDistance }
        );
        ray.max_distance_meters = MAX_TERRAIN_PICK_DISTANCE_METERS + 1.0;
        assert_eq!(
            pick_terrain(&world, ray),
            PickTerrainResult::Rejected { error: TerrainPickError::InvalidMaxDistance }
        );
    }
}
