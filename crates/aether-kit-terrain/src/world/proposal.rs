//! Copy-on-write terrain proposal staging and deterministic geometry digests.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use aether_render::DrawTriangle;

use super::mesher::mesh_chunk;
use super::mesher::style::StyleTable;
use super::{
    ApplyBrush, CellPos, Chunk, ChunkPos, HEIGHT_POINT_INHERIT, OperatorChunk, ProposalBounds, ProposalDigest,
    ProposalOperation, ProposalOperationResult, RunAutomaton, SUBCELLS_PER_CELL, SetCellHeights, SetCellPoints, World,
    raster,
};

/// The narrow mutation surface shared by committed terrain and a staged
/// overlay. Reads resolve through `chunk`; the first mutable access decides
/// whether to reuse committed storage or clone it into an overlay.
pub(super) trait MutationTarget {
    fn chunk(&self, at: ChunkPos) -> Option<&Chunk>;
    fn chunk_mut_or_insert(&mut self, at: ChunkPos) -> &mut Chunk;
    fn replace_chunk(&mut self, at: ChunkPos, chunk: Box<Chunk>);

    fn set_cell_points(&mut self, cell: CellPos, points: &[u8]) {
        let chunk = self.chunk_mut_or_insert(cell.chunk());
        let base = cell.chunk_index() * SUBCELLS_PER_CELL;
        for index in 0..SUBCELLS_PER_CELL {
            chunk.underlay_points[base + index] = points.get(index).copied().unwrap_or(super::UNDERLAY_POINT_INHERIT);
        }
    }

    fn set_cell_heights(&mut self, cell: CellPos, deltas: &[i16]) {
        let chunk = self.chunk_mut_or_insert(cell.chunk());
        let base = cell.chunk_index() * SUBCELLS_PER_CELL;
        for index in 0..SUBCELLS_PER_CELL {
            chunk.height_points[base + index] = deltas.get(index).copied().unwrap_or(HEIGHT_POINT_INHERIT);
        }
    }
}

/// Staged-first copy-on-write view over a committed world.
pub(super) struct ProposalOverlay<'a> {
    committed: &'a World,
    staged: BTreeMap<ChunkPos, Box<Chunk>>,
}

impl<'a> ProposalOverlay<'a> {
    pub(super) fn new(committed: &'a World) -> Self {
        Self { committed, staged: BTreeMap::new() }
    }

    fn into_staged(self) -> BTreeMap<ChunkPos, Box<Chunk>> {
        self.staged
    }
}

impl MutationTarget for ProposalOverlay<'_> {
    fn chunk(&self, at: ChunkPos) -> Option<&Chunk> {
        self.staged.get(&at).map_or_else(|| self.committed.chunk(at), |chunk| Some(chunk.as_ref()))
    }

    fn chunk_mut_or_insert(&mut self, at: ChunkPos) -> &mut Chunk {
        self.staged
            .entry(at)
            .or_insert_with(|| self.committed.clone_chunk_box(at).unwrap_or_else(Chunk::empty_boxed))
            .as_mut()
    }

    fn replace_chunk(&mut self, at: ChunkPos, chunk: Box<Chunk>) {
        self.staged.insert(at, chunk);
    }
}

/// A proposal after its operation, affected cache set, meshes, and digest have
/// all been materialized against one committed revision.
pub(super) struct StagedProposal {
    pub(super) proposed_at_revision: u64,
    pub(super) operation_result: ProposalOperationResult,
    pub(super) touched: BTreeSet<ChunkPos>,
    pub(super) affected: BTreeSet<ChunkPos>,
    pub(super) meshes: BTreeMap<ChunkPos, Vec<DrawTriangle>>,
    pub(super) digest: ProposalDigest,
    staged: BTreeMap<ChunkPos, Box<Chunk>>,
}

impl StagedProposal {
    pub(super) fn build(
        proposed_at_revision: u64,
        operation: ProposalOperation,
        world: &mut World,
        committed_meshes: &BTreeMap<ChunkPos, Vec<DrawTriangle>>,
        styles: &StyleTable,
    ) -> Result<Self, ProposalOperationResult> {
        let StagedOperation { operation_result, touched, mut staged } = stage_operation(world, operation);
        if touched.is_empty() {
            return Err(operation_result);
        }
        let affected = affected_cache_keys(&touched, committed_meshes);
        let meshes = with_installed(world, &mut staged, |installed| {
            affected.iter().map(|at| (*at, mesh_chunk(installed, *at, styles))).collect()
        });
        let digest = proposal_digest(&touched, &affected, committed_meshes, &meshes);
        Ok(Self { proposed_at_revision, operation_result, touched, affected, meshes, digest, staged })
    }

    pub(super) fn with_installed<R>(&mut self, world: &mut World, f: impl FnOnce(&World) -> R) -> R {
        with_installed(world, &mut self.staged, f)
    }

    pub(super) fn commit(self, world: &mut World) {
        for (at, chunk) in self.staged {
            world.replace_chunk(at, Some(chunk));
        }
    }
}

struct StagedOperation {
    operation_result: ProposalOperationResult,
    touched: BTreeSet<ChunkPos>,
    staged: BTreeMap<ChunkPos, Box<Chunk>>,
}

fn stage_operation(world: &World, operation: ProposalOperation) -> StagedOperation {
    let mut overlay = ProposalOverlay::new(world);
    let (operation_result, touched) = match operation {
        ProposalOperation::SetChunk { request } => {
            let at = request.chunk_pos();
            overlay.replace_chunk(at, request.into_chunk());
            (ProposalOperationResult::Mutation, BTreeSet::from([at]))
        }
        ProposalOperation::SetCellPoints { request } => {
            let SetCellPoints { x, z, points } = request;
            let cell = CellPos { x, z };
            overlay.set_cell_points(cell, &points);
            (ProposalOperationResult::Mutation, BTreeSet::from([cell.chunk()]))
        }
        ProposalOperation::SetCellHeights { request } => {
            let SetCellHeights { x, z, deltas } = request;
            let cell = CellPos { x, z };
            overlay.set_cell_heights(cell, &deltas);
            (ProposalOperationResult::Mutation, BTreeSet::from([cell.chunk()]))
        }
        ProposalOperation::StampPolygon { request } => {
            let touched = raster::stamp_polygon(
                &mut overlay,
                &request.points,
                super::Material::from_u8_or_void(request.material),
            );
            (ProposalOperationResult::Mutation, touched)
        }
        ProposalOperation::StampDisc { request } => {
            let vertices = raster::disc_vertices(request.center, request.radius_octimeters);
            let touched =
                raster::stamp_polygon(&mut overlay, &vertices, super::Material::from_u8_or_void(request.material));
            (ProposalOperationResult::Mutation, touched)
        }
        ProposalOperation::StampHexagon { request } => {
            let vertices = raster::regular_hexagon_vertices(request.center, request.radius_octimeters);
            let touched =
                raster::stamp_polygon(&mut overlay, &vertices, super::Material::from_u8_or_void(request.material));
            (ProposalOperationResult::Mutation, touched)
        }
        ProposalOperation::ApplyBrush { request } => {
            let ApplyBrush { source, path, brush, budget } = request;
            let execution = super::operator::apply_brush(&mut overlay, source, &path, brush, budget);
            (ProposalOperationResult::Operator { result: execution.result }, execution.touched)
        }
        ProposalOperation::RunAutomaton { request } => {
            let RunAutomaton { source, seed, rule, budget } = request;
            let execution = super::operator::run_automaton(&mut overlay, source, seed, rule, budget);
            (ProposalOperationResult::Operator { result: execution.result }, execution.touched)
        }
    };
    StagedOperation { operation_result, touched, staged: overlay.into_staged() }
}

/// Touched chunks plus only apron neighbours whose committed cache entry is
/// already resident. The sorted set is shared by preview and commit remeshing.
pub(super) fn affected_cache_keys(
    touched: &BTreeSet<ChunkPos>,
    meshes: &BTreeMap<ChunkPos, Vec<DrawTriangle>>,
) -> BTreeSet<ChunkPos> {
    let mut affected = touched.clone();
    for pos in touched {
        for delta_z in -1..=1 {
            for delta_x in -1..=1 {
                let (Some(x), Some(z)) = (pos.x.checked_add(delta_x), pos.z.checked_add(delta_z)) else {
                    continue;
                };
                let neighbor = ChunkPos { x, z };
                if meshes.contains_key(&neighbor) {
                    affected.insert(neighbor);
                }
            }
        }
    }
    affected
}

/// Install all staged boxes together, run one ordinary-return computation,
/// then restore both original boxes and original absence.
fn with_installed<R>(world: &mut World, staged: &mut BTreeMap<ChunkPos, Box<Chunk>>, f: impl FnOnce(&World) -> R) -> R {
    let positions: Vec<ChunkPos> = staged.keys().copied().collect();
    let mut originals = BTreeMap::new();
    for at in &positions {
        let staged_chunk = staged.remove(at).expect("staged position came from the map's keys");
        originals.insert(*at, world.replace_chunk(*at, Some(staged_chunk)));
    }

    let result = f(world);

    for at in positions {
        let original = originals.remove(&at).expect("every installed position records original presence");
        let staged_chunk =
            world.replace_chunk(at, original).expect("the staged chunk remains installed until restoration");
        staged.insert(at, staged_chunk);
    }
    result
}

fn proposal_digest(
    touched: &BTreeSet<ChunkPos>,
    affected: &BTreeSet<ChunkPos>,
    committed_meshes: &BTreeMap<ChunkPos, Vec<DrawTriangle>>,
    proposed_meshes: &BTreeMap<ChunkPos, Vec<DrawTriangle>>,
) -> ProposalDigest {
    let triangle_count = proposed_meshes.values().try_fold(0u64, |sum, mesh| {
        let count = u64::try_from(mesh.len()).ok()?;
        sum.checked_add(count)
    });
    let mut bounds = None;
    for at in affected {
        let committed = committed_meshes.get(at).map_or(&[][..], Vec::as_slice);
        let proposed = proposed_meshes.get(at).map_or(&[][..], Vec::as_slice);
        for index in 0..committed.len().max(proposed.len()) {
            match (committed.get(index), proposed.get(index)) {
                (Some(before), Some(after)) if before == after => {}
                (Some(before), Some(after)) => {
                    include_triangle(&mut bounds, before);
                    include_triangle(&mut bounds, after);
                }
                (Some(before), None) => include_triangle(&mut bounds, before),
                (None, Some(after)) => include_triangle(&mut bounds, after),
                (None, None) => {}
            }
        }
    }
    ProposalDigest {
        touched_chunks: touched.iter().copied().map(OperatorChunk::from).collect(),
        triangle_count: triangle_count.expect("a resident mesh vector count must fit in u64"),
        changed_geometry_bounds: bounds,
    }
}

fn include_triangle(bounds: &mut Option<ProposalBounds>, triangle: &DrawTriangle) {
    for vertex in triangle.verts {
        let current = bounds.get_or_insert(ProposalBounds {
            min_x_meters: vertex.x,
            min_y_meters: vertex.y,
            min_z_meters: vertex.z,
            max_x_meters: vertex.x,
            max_y_meters: vertex.y,
            max_z_meters: vertex.z,
        });
        current.min_x_meters = current.min_x_meters.min(vertex.x);
        current.min_y_meters = current.min_y_meters.min(vertex.y);
        current.min_z_meters = current.min_z_meters.min(vertex.z);
        current.max_x_meters = current.max_x_meters.max(vertex.x);
        current.max_y_meters = current.max_y_meters.max(vertex.y);
        current.max_z_meters = current.max_z_meters.max(vertex.z);
    }
}

#[cfg(test)]
mod tests {
    use core::ptr::from_mut;

    use aether_math::Rgb;
    use aether_render::Vertex;

    use super::*;
    use crate::mark::{MarkId, MarkRef};
    use crate::world::{
        AutomatonRule, BrushParameters, Material, OperatorBudget, OperatorCell, OperatorError, OperatorResult,
        SetChunk, StampDisc, StampHexagon, StampPolygon, WorldPoint,
    };

    fn source() -> MarkRef {
        MarkRef { id: MarkId::new(9), revision: 2 }
    }

    fn empty_set_chunk(at: ChunkPos) -> SetChunk {
        SetChunk {
            chunk_x: at.x,
            chunk_z: at.z,
            underlay: Vec::new(),
            underlay_points: Vec::new(),
            height_points: Vec::new(),
            overlay: Vec::new(),
            overlay_mask: Vec::new(),
            height: Vec::new(),
            region: Vec::new(),
            water_plane: Vec::new(),
            smoothing: Vec::new(),
        }
    }

    #[test]
    fn overlay_reads_staged_first_and_clones_each_touched_chunk_once() {
        let at = ChunkPos { x: 1, z: -2 };
        let mut committed = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay[0] = Material::Grass;
        committed.insert_chunk(at, chunk);
        let mut overlay = ProposalOverlay::new(&committed);

        let first = from_mut::<Chunk>(overlay.chunk_mut_or_insert(at));
        overlay.chunk_mut_or_insert(at).underlay[0] = Material::Stone;
        let second = from_mut::<Chunk>(overlay.chunk_mut_or_insert(at));
        assert_eq!(first, second, "later writes reuse the first staged clone");
        assert_eq!(overlay.chunk(at).expect("staged chunk").underlay[0], Material::Stone);
        assert_eq!(committed.chunk(at).expect("committed chunk").underlay[0], Material::Grass);
        assert_eq!(overlay.staged.len(), 1);
    }

    #[test]
    fn absent_chunk_staging_and_install_restore_preserve_absence() {
        let at = ChunkPos { x: 3, z: 4 };
        let mut world = World::new();
        let mut overlay = ProposalOverlay::new(&world);
        overlay.chunk_mut_or_insert(at).underlay[0] = Material::Sand;
        let mut staged = overlay.into_staged();

        let observed = with_installed(&mut world, &mut staged, |installed| {
            installed.chunk(at).expect("temporarily installed").underlay[0]
        });
        assert_eq!(observed, Material::Sand);
        assert!(world.chunk(at).is_none(), "original absence is restored");
        assert_eq!(staged.get(&at).expect("proposal retains staged box").underlay[0], Material::Sand);
    }

    #[test]
    fn every_bounded_operation_stages_without_changing_committed_world() {
        let operations = [
            ProposalOperation::SetChunk { request: empty_set_chunk(ChunkPos { x: 0, z: 0 }) },
            ProposalOperation::SetCellPoints {
                request: SetCellPoints { x: 0, z: 0, points: vec![Material::Grass.to_u8()] },
            },
            ProposalOperation::SetCellHeights { request: SetCellHeights { x: 0, z: 0, deltas: vec![32] } },
            ProposalOperation::StampPolygon {
                request: StampPolygon {
                    points: vec![
                        WorldPoint::new(0, 0),
                        WorldPoint::new(32, 0),
                        WorldPoint::new(32, 32),
                        WorldPoint::new(0, 32),
                    ],
                    material: Material::Stone.to_u8(),
                },
            },
            ProposalOperation::StampDisc {
                request: StampDisc {
                    center: WorldPoint::new(64, 64),
                    radius_octimeters: 32,
                    material: Material::Sand.to_u8(),
                },
            },
            ProposalOperation::StampHexagon {
                request: StampHexagon {
                    center: WorldPoint::new(64, 64),
                    radius_octimeters: 32,
                    material: Material::Dirt.to_u8(),
                },
            },
            ProposalOperation::ApplyBrush {
                request: ApplyBrush {
                    source: source(),
                    path: vec![WorldPoint::new(64, 64)],
                    brush: BrushParameters {
                        radius_octimeters: 16,
                        spacing_octimeters: 16,
                        material: Material::Stone.to_u8(),
                    },
                    budget: OperatorBudget { max_steps: 1, max_subcells: 16 },
                },
            },
            ProposalOperation::RunAutomaton {
                request: RunAutomaton {
                    source: source(),
                    seed: OperatorCell { cell_x: 0, cell_z: 0 },
                    rule: AutomatonRule::Grow { material: Material::Grass.to_u8(), generations: 0 },
                    budget: OperatorBudget {
                        max_steps: 1,
                        max_subcells: u32::try_from(SUBCELLS_PER_CELL).expect("fixed cell plane fits u32"),
                    },
                },
            },
        ];
        for operation in operations {
            let world = World::new();
            let before = world.clone();
            let staged = stage_operation(&world, operation);
            assert!(!staged.touched.is_empty());
            assert_eq!(world, before, "proposing never mutates committed terrain");
        }
    }

    #[test]
    fn bounded_operator_partial_failure_is_retained_in_the_overlay() {
        let world = World::new();
        let staged = stage_operation(
            &world,
            ProposalOperation::RunAutomaton {
                request: RunAutomaton {
                    source: source(),
                    seed: OperatorCell { cell_x: 0, cell_z: 0 },
                    rule: AutomatonRule::Grow { material: Material::Sand.to_u8(), generations: 1 },
                    budget: OperatorBudget {
                        max_steps: 2,
                        max_subcells: 2 * u32::try_from(SUBCELLS_PER_CELL).expect("fixed cell plane fits u32"),
                    },
                },
            },
        );
        assert!(matches!(
            staged.operation_result,
            ProposalOperationResult::Operator {
                result: OperatorResult::Failed { error: OperatorError::StepBudgetExhausted, .. }
            }
        ));
        assert_eq!(staged.touched, BTreeSet::from([ChunkPos { x: 0, z: 0 }]));
        assert_eq!(world.chunks().count(), 0);
    }

    #[test]
    fn affected_keys_add_only_resident_apron_neighbours() {
        let touched = BTreeSet::from([ChunkPos { x: 0, z: 0 }]);
        let resident = ChunkPos { x: 1, z: 1 };
        let far = ChunkPos { x: 4, z: 4 };
        let meshes = BTreeMap::from([(resident, Vec::new()), (far, Vec::new())]);
        assert_eq!(affected_cache_keys(&touched, &meshes), BTreeSet::from([ChunkPos { x: 0, z: 0 }, resident]));
    }

    fn triangle(x: f32) -> DrawTriangle {
        DrawTriangle {
            verts: [
                Vertex { x, y: 1.0, z: 2.0, color: Rgb::new(1.0, 0.0, 0.0) },
                Vertex { x: x + 1.0, y: 3.0, z: 4.0, color: Rgb::new(0.0, 1.0, 0.0) },
                Vertex { x: x + 2.0, y: 5.0, z: 6.0, color: Rgb::new(0.0, 0.0, 1.0) },
            ],
        }
    }

    #[test]
    fn digest_bounds_include_both_sides_and_removal_only_geometry() {
        let at = ChunkPos { x: 0, z: 0 };
        let touched = BTreeSet::from([at]);
        let committed = BTreeMap::from([(at, vec![triangle(-4.0), triangle(10.0)])]);
        let proposed = BTreeMap::from([(at, vec![triangle(-2.0)])]);
        let digest = proposal_digest(&touched, &touched, &committed, &proposed);
        assert_eq!(digest.triangle_count, 1);
        assert_eq!(
            digest.changed_geometry_bounds,
            Some(ProposalBounds {
                min_x_meters: -4.0,
                min_y_meters: 1.0,
                min_z_meters: 2.0,
                max_x_meters: 12.0,
                max_y_meters: 5.0,
                max_z_meters: 6.0,
            })
        );
    }

    #[test]
    fn unchanged_ordered_payload_has_no_changed_bounds() {
        let at = ChunkPos { x: 0, z: 0 };
        let touched = BTreeSet::from([at]);
        let meshes = BTreeMap::from([(at, vec![triangle(1.0)])]);
        let digest = proposal_digest(&touched, &touched, &meshes, &meshes);
        assert_eq!(digest.changed_geometry_bounds, None);
    }

    #[test]
    fn proposal_wire_vocabulary_uses_named_records_and_named_enum_fields() {
        let source = include_str!("kinds.rs");
        let proposal_source = source.split("pub struct ProposalId").nth(1).expect("proposal wire anchor");
        for type_name in [
            "ProposalId",
            "ProposalBounds",
            "ProposalDigest",
            "Propose",
            "CommitProposal",
            "DiscardProposal",
            "SetProposalPreview",
        ] {
            assert!(
                !proposal_source.contains(&format!("pub struct {type_name}(")),
                "{type_name} must not be a tuple struct"
            );
        }
        assert!(
            !proposal_source.lines().any(|line| line.trim_start().starts_with("pub ") && line.contains(": [")),
            "proposal semantic fields must not use fixed positional arrays",
        );
        for variant in [
            "SetChunk {",
            "SetCellPoints {",
            "SetCellHeights {",
            "StampPolygon {",
            "StampDisc {",
            "StampHexagon {",
            "ApplyBrush {",
            "RunAutomaton {",
            "Operator {",
        ] {
            assert!(proposal_source.contains(variant), "{variant} stays a named enum field");
        }
    }
}
