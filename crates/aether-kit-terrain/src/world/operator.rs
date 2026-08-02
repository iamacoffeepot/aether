//! Bounded terrain-operator execution shared by live commits and proposal
//! staging. The core mutates only the supplied [`super::World`], reports exact
//! accounting, and contains no actor-mail or render-cache policy.

// Brush interpolation stays between already-validated i32 endpoints, and the
// usize-to-u32 casts narrow the fixed 256-subcell cell plane.
#![allow(clippy::cast_possible_truncation)]

use alloc::collections::{BTreeSet, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use core::cmp::Ordering;

use crate::mark::MarkRef;

use super::{
    AutomatonRule, BrushParameters, CHUNK_BITS, ChunkPos, MAX_STAMP_VERTICES, Material, OperatorBudget, OperatorCell,
    OperatorChunk, OperatorError, OperatorResult, OperatorStats, SUBCELLS_PER_CELL, WorldPoint, mesher, proposal,
    raster,
};
use crate::OCTIMETERS_PER_TILE;

/// One operator run plus the internal chunk addresses the actor must remesh.
pub(super) struct OperatorExecution {
    pub(super) result: OperatorResult,
    pub(super) touched: BTreeSet<ChunkPos>,
}

#[derive(Debug)]
struct ExecutionState {
    budget: OperatorBudget,
    steps_run: u32,
    subcells_written: u32,
    touched: BTreeSet<ChunkPos>,
}

impl ExecutionState {
    fn new(budget: OperatorBudget) -> Self {
        Self { budget, steps_run: 0, subcells_written: 0, touched: BTreeSet::new() }
    }

    fn charge_step(&mut self) -> Result<(), OperatorError> {
        if self.steps_run == self.budget.max_steps {
            return Err(OperatorError::StepBudgetExhausted);
        }
        self.steps_run += 1;
        Ok(())
    }

    fn charge_cell_write(&mut self) -> Result<(), OperatorError> {
        if self.steps_run == self.budget.max_steps {
            return Err(OperatorError::StepBudgetExhausted);
        }
        let cell_subcells = SUBCELLS_PER_CELL as u32;
        if cell_subcells > self.budget.max_subcells - self.subcells_written {
            return Err(OperatorError::SubcellBudgetExhausted);
        }
        self.steps_run += 1;
        self.subcells_written += cell_subcells;
        Ok(())
    }

    fn remaining_subcells(&self) -> u32 {
        self.budget.max_subcells - self.subcells_written
    }

    fn finish(self, source: MarkRef, error: Option<OperatorError>) -> OperatorExecution {
        let stats = OperatorStats {
            steps_run: self.steps_run,
            subcells_written: self.subcells_written,
            touched_chunks: self.touched.iter().copied().map(OperatorChunk::from).collect(),
        };
        let result = match error {
            Some(error) => OperatorResult::Failed { source, error, stats },
            None => OperatorResult::Applied { source, stats },
        };
        OperatorExecution { result, touched: self.touched }
    }
}

/// Apply a reference disc brush along `path` at stable world-octimeter
/// spacing. The first point is always a placement; later placements carry
/// spacing across path-segment boundaries.
pub(super) fn apply_brush<T: proposal::MutationTarget + ?Sized>(
    world: &mut T,
    source: MarkRef,
    path: &[WorldPoint],
    brush: BrushParameters,
    budget: OperatorBudget,
) -> OperatorExecution {
    if let Err(error) = validate_brush(path, brush) {
        return ExecutionState::new(budget).finish(source, Some(error));
    }

    let mut state = ExecutionState::new(budget);
    if let Err(error) = stamp_brush_point(world, path[0], brush, &mut state) {
        return state.finish(source, Some(error));
    }

    let spacing = f64::from(brush.spacing_octimeters);
    let mut distance_to_next = spacing;
    for segment in path.windows(2) {
        let start = segment[0];
        let end = segment[1];
        let dx = f64::from(end.x_octimeters) - f64::from(start.x_octimeters);
        let dz = f64::from(end.z_octimeters) - f64::from(start.z_octimeters);
        let length = dx.mul_add(dx, dz * dz).sqrt();
        if length == 0.0 {
            continue;
        }
        while distance_to_next.total_cmp(&length) != Ordering::Greater {
            let fraction = distance_to_next / length;
            let point = WorldPoint::new(
                dx.mul_add(fraction, f64::from(start.x_octimeters)).round() as i32,
                dz.mul_add(fraction, f64::from(start.z_octimeters)).round() as i32,
            );
            if let Err(error) = stamp_brush_point(world, point, brush, &mut state) {
                return state.finish(source, Some(error));
            }
            distance_to_next += spacing;
        }
        distance_to_next -= length;
    }

    state.finish(source, None)
}

fn validate_brush(path: &[WorldPoint], brush: BrushParameters) -> Result<(), OperatorError> {
    if path.is_empty() || path.len() > MAX_STAMP_VERTICES {
        return Err(invalid_parameters(format!(
            "brush path requires 1..={MAX_STAMP_VERTICES} world points; got {}",
            path.len()
        )));
    }
    if brush.radius_octimeters == 0 {
        return Err(invalid_parameters("brush radius must be non-zero"));
    }
    if brush.spacing_octimeters == 0 {
        return Err(invalid_parameters("brush spacing must be non-zero"));
    }
    if Material::from_u8_or_void(brush.material) == Material::Void {
        return Err(invalid_parameters("brush material must be a known non-Void material byte"));
    }
    let Ok(radius) = i32::try_from(brush.radius_octimeters) else {
        return Err(invalid_parameters("brush radius exceeds the coordinate range"));
    };
    let mut min_x = i32::MAX;
    let mut min_z = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_z = i32::MIN;
    for point in path {
        let (Some(point_min_x), Some(point_max_x), Some(point_min_z), Some(point_max_z)) = (
            point.x_octimeters.checked_sub(radius),
            point.x_octimeters.checked_add(radius),
            point.z_octimeters.checked_sub(radius),
            point.z_octimeters.checked_add(radius),
        ) else {
            return Err(invalid_parameters("brush radius extends outside the world coordinate range"));
        };
        min_x = min_x.min(point_min_x);
        min_z = min_z.min(point_min_z);
        max_x = max_x.max(point_max_x);
        max_z = max_z.max(point_max_z);
    }
    let octimeters_per_cell = i64::from(OCTIMETERS_PER_TILE);
    validate_remesh_extent(
        i64::from(min_x).div_euclid(octimeters_per_cell),
        (i64::from(max_x) - 1).div_euclid(octimeters_per_cell),
        i64::from(min_z).div_euclid(octimeters_per_cell),
        (i64::from(max_z) - 1).div_euclid(octimeters_per_cell),
        "brush",
    )
}

fn invalid_parameters(reason: impl Into<String>) -> OperatorError {
    OperatorError::InvalidParameters { reason: reason.into() }
}

fn validate_remesh_extent(
    min_cell_x: i64,
    max_cell_x: i64,
    min_cell_z: i64,
    max_cell_z: i64,
    operator: &str,
) -> Result<(), OperatorError> {
    let (Ok(min_cell_x), Ok(max_cell_x), Ok(min_cell_z), Ok(max_cell_z)) =
        (i32::try_from(min_cell_x), i32::try_from(max_cell_x), i32::try_from(min_cell_z), i32::try_from(max_cell_z))
    else {
        return Err(invalid_parameters(format!("{operator} extent exceeds the cell coordinate range")));
    };
    let min_chunk = ChunkPos { x: min_cell_x >> CHUNK_BITS, z: min_cell_z >> CHUNK_BITS };
    let max_chunk = ChunkPos { x: max_cell_x >> CHUNK_BITS, z: max_cell_z >> CHUNK_BITS };
    if !mesher::chunk_remesh_extent_is_coordinate_safe(min_chunk)
        || !mesher::chunk_remesh_extent_is_coordinate_safe(max_chunk)
    {
        return Err(invalid_parameters(format!("{operator} extent exceeds the mesher's apron-safe coordinate range")));
    }
    Ok(())
}

fn stamp_brush_point<T: proposal::MutationTarget + ?Sized>(
    world: &mut T,
    center: WorldPoint,
    brush: BrushParameters,
    state: &mut ExecutionState,
) -> Result<(), OperatorError> {
    state.charge_step()?;
    let vertices = raster::disc_vertices(center, brush.radius_octimeters);
    let stamp = raster::stamp_polygon_bounded(
        world,
        &vertices,
        Material::from_u8_or_void(brush.material),
        state.remaining_subcells(),
    );
    state.subcells_written += stamp.subcells_written;
    state.touched.extend(stamp.touched);
    if stamp.exhausted {
        return Err(OperatorError::SubcellBudgetExhausted);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct FrontierCell {
    cell: OperatorCell,
    generation: u32,
}

/// Run the reference cell automaton with an explicit deterministic frontier.
pub(super) fn run_automaton<T: proposal::MutationTarget + ?Sized>(
    world: &mut T,
    source: MarkRef,
    seed: OperatorCell,
    rule: AutomatonRule,
    budget: OperatorBudget,
) -> OperatorExecution {
    let AutomatonRule::Grow { material, generations } = rule;
    if Material::from_u8_or_void(material) == Material::Void {
        return ExecutionState::new(budget)
            .finish(source, Some(invalid_parameters("automaton material must be a known non-Void material byte")));
    }
    let generation_radius = i64::from(generations);
    if let Err(error) = validate_remesh_extent(
        i64::from(seed.cell_x) - generation_radius,
        i64::from(seed.cell_x) + generation_radius,
        i64::from(seed.cell_z) - generation_radius,
        i64::from(seed.cell_z) + generation_radius,
        "automaton",
    ) {
        return ExecutionState::new(budget).finish(source, Some(error));
    }

    let mut state = ExecutionState::new(budget);
    let mut frontier = VecDeque::from([FrontierCell { cell: seed, generation: 0 }]);
    let mut visited = BTreeSet::from([seed]);
    let points = vec![material; SUBCELLS_PER_CELL];

    while let Some(current) = frontier.pop_front() {
        if let Err(error) = state.charge_cell_write() {
            return state.finish(source, Some(error));
        }
        let cell = current.cell.cell_pos();
        world.set_cell_points(cell, &points);
        state.touched.insert(cell.chunk());

        if current.generation < generations {
            let next_generation = current.generation + 1;
            push_neighbor(
                &mut frontier,
                &mut visited,
                current.cell.cell_x.checked_add(1),
                Some(current.cell.cell_z),
                next_generation,
            );
            push_neighbor(
                &mut frontier,
                &mut visited,
                current.cell.cell_x.checked_sub(1),
                Some(current.cell.cell_z),
                next_generation,
            );
            push_neighbor(
                &mut frontier,
                &mut visited,
                Some(current.cell.cell_x),
                current.cell.cell_z.checked_add(1),
                next_generation,
            );
            push_neighbor(
                &mut frontier,
                &mut visited,
                Some(current.cell.cell_x),
                current.cell.cell_z.checked_sub(1),
                next_generation,
            );
        }
    }

    state.finish(source, None)
}

fn push_neighbor(
    frontier: &mut VecDeque<FrontierCell>,
    visited: &mut BTreeSet<OperatorCell>,
    cell_x: Option<i32>,
    cell_z: Option<i32>,
    generation: u32,
) {
    let (Some(cell_x), Some(cell_z)) = (cell_x, cell_z) else {
        return;
    };
    let cell = OperatorCell { cell_x, cell_z };
    if visited.insert(cell) {
        frontier.push_back(FrontierCell { cell, generation });
    }
}

#[cfg(test)]
mod tests {
    use crate::mark::MarkId;

    use super::*;
    use crate::world::{CellPos, World};

    fn source() -> MarkRef {
        MarkRef { id: MarkId::new(7), revision: 3 }
    }

    fn stats(result: &OperatorResult) -> (&OperatorStats, Option<&OperatorError>) {
        match result {
            OperatorResult::Applied { stats, .. } => (stats, None),
            OperatorResult::Failed { error, stats, .. } => (stats, Some(error)),
        }
    }

    #[test]
    fn brush_subcell_budget_stops_before_the_first_over_cap_sample() {
        let mut world = World::new();
        let execution = apply_brush(
            &mut world,
            source(),
            &[WorldPoint::new(8, 8)],
            BrushParameters { radius_octimeters: 8, spacing_octimeters: 16, material: Material::Stone.to_u8() },
            OperatorBudget { max_steps: 1, max_subcells: 0 },
        );

        let (stats, error) = stats(&execution.result);
        assert_eq!(error, Some(&OperatorError::SubcellBudgetExhausted));
        assert_eq!(stats.steps_run, 1);
        assert_eq!(stats.subcells_written, 0);
        assert!(stats.touched_chunks.is_empty());
        assert_eq!(world.overlay(CellPos { x: 0, z: 0 }), Material::Void);
    }

    #[test]
    fn brush_spacing_carries_across_path_segments() {
        let mut world = World::new();
        let execution = apply_brush(
            &mut world,
            source(),
            &[WorldPoint::new(576, 1088), WorldPoint::new(832, 1088), WorldPoint::new(1088, 1088)],
            BrushParameters { radius_octimeters: 64, spacing_octimeters: 256, material: Material::Stone.to_u8() },
            OperatorBudget { max_steps: 3, max_subcells: 1_000 },
        );

        let (stats, error) = stats(&execution.result);
        assert_eq!(error, None);
        assert_eq!(stats.steps_run, 3, "placements are at x=576, 832, and 1088");
        assert_eq!(stats.subcells_written, 180);
    }

    #[test]
    fn brush_step_exhaustion_keeps_only_the_exact_accepted_prefix() {
        let mut world = World::new();
        let execution = apply_brush(
            &mut world,
            source(),
            &[WorldPoint::new(576, 1088), WorldPoint::new(832, 1088)],
            BrushParameters { radius_octimeters: 64, spacing_octimeters: 256, material: Material::Stone.to_u8() },
            OperatorBudget { max_steps: 1, max_subcells: 1_000 },
        );

        let (stats, error) = stats(&execution.result);
        assert_eq!(error, Some(&OperatorError::StepBudgetExhausted));
        assert_eq!(stats.steps_run, 1);
        assert_eq!(stats.subcells_written, 60);
        assert_eq!(world.overlay(CellPos { x: 2, z: 4 }), Material::Stone);
        assert_eq!(
            world.overlay(CellPos { x: 3, z: 4 }),
            Material::Void,
            "the second placement is the rejected over-step write",
        );
    }

    #[test]
    fn edge_brush_is_rejected_before_mutating_an_unmeshable_chunk() {
        let mut world = World::new();
        let execution = apply_brush(
            &mut world,
            source(),
            &[WorldPoint::new(i32::MAX - 16, 128)],
            BrushParameters { radius_octimeters: 16, spacing_octimeters: 16, material: Material::Stone.to_u8() },
            OperatorBudget { max_steps: 1, max_subcells: 1_000 },
        );

        let (stats, error) = stats(&execution.result);
        assert!(matches!(
            error,
            Some(OperatorError::InvalidParameters { reason })
                if reason.contains("apron-safe coordinate range")
        ));
        assert_eq!(stats.steps_run, 0);
        assert_eq!(stats.subcells_written, 0);
        assert!(stats.touched_chunks.is_empty());
        assert_eq!(world.chunks().count(), 0, "rejection must precede mutation");
    }

    #[test]
    fn automaton_grows_a_known_five_cell_region() {
        let mut world = World::new();
        let execution = run_automaton(
            &mut world,
            source(),
            OperatorCell { cell_x: 4, cell_z: 4 },
            AutomatonRule::Grow { material: Material::Grass.to_u8(), generations: 1 },
            OperatorBudget { max_steps: 5, max_subcells: 5 * SUBCELLS_PER_CELL as u32 },
        );

        let (stats, error) = stats(&execution.result);
        assert_eq!(error, None);
        assert_eq!(stats.steps_run, 5);
        assert_eq!(stats.subcells_written, 5 * SUBCELLS_PER_CELL as u32);
        assert_eq!(stats.touched_chunks, vec![OperatorChunk { chunk_x: 0, chunk_z: 0 }]);
        for cell in [
            CellPos { x: 4, z: 4 },
            CellPos { x: 5, z: 4 },
            CellPos { x: 3, z: 4 },
            CellPos { x: 4, z: 5 },
            CellPos { x: 4, z: 3 },
        ] {
            assert_eq!(world.underlay_point(cell, 0, 0), Material::Grass);
        }
    }

    #[test]
    fn huge_automaton_seed_is_rejected_before_mutation() {
        let mut world = World::new();
        let seed = OperatorCell { cell_x: 10_000_000, cell_z: 0 };
        let execution = run_automaton(
            &mut world,
            source(),
            seed,
            AutomatonRule::Grow { material: Material::Grass.to_u8(), generations: 0 },
            OperatorBudget { max_steps: 1, max_subcells: SUBCELLS_PER_CELL as u32 },
        );

        let (stats, error) = stats(&execution.result);
        assert!(matches!(
            error,
            Some(OperatorError::InvalidParameters { reason })
                if reason.contains("apron-safe coordinate range")
        ));
        assert_eq!(stats.steps_run, 0);
        assert_eq!(stats.subcells_written, 0);
        assert!(stats.touched_chunks.is_empty());
        assert_eq!(world.chunks().count(), 0, "rejection must precede mutation");
    }

    #[test]
    fn automaton_subcell_exhaustion_keeps_an_exact_consistent_prefix() {
        let mut world = World::new();
        let execution = run_automaton(
            &mut world,
            source(),
            OperatorCell { cell_x: 4, cell_z: 4 },
            AutomatonRule::Grow { material: Material::Sand.to_u8(), generations: 1 },
            OperatorBudget { max_steps: 5, max_subcells: 4 * SUBCELLS_PER_CELL as u32 },
        );

        let (stats, error) = stats(&execution.result);
        assert_eq!(error, Some(&OperatorError::SubcellBudgetExhausted));
        assert_eq!(stats.steps_run, 4);
        assert_eq!(stats.subcells_written, 4 * SUBCELLS_PER_CELL as u32);
        assert_eq!(
            world.underlay_point(CellPos { x: 4, z: 3 }, 0, 0),
            Material::Void,
            "the fifth frontier cell is the rejected over-cap write",
        );
    }

    #[test]
    fn automaton_step_exhaustion_does_not_charge_a_rejected_cell() {
        let mut world = World::new();
        let execution = run_automaton(
            &mut world,
            source(),
            OperatorCell { cell_x: 0, cell_z: 0 },
            AutomatonRule::Grow { material: Material::Dirt.to_u8(), generations: 1 },
            OperatorBudget { max_steps: 2, max_subcells: 5 * SUBCELLS_PER_CELL as u32 },
        );

        let (stats, error) = stats(&execution.result);
        assert_eq!(error, Some(&OperatorError::StepBudgetExhausted));
        assert_eq!(stats.steps_run, 2);
        assert_eq!(stats.subcells_written, 2 * SUBCELLS_PER_CELL as u32);
    }
}
