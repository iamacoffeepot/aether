// Fixed-point simulation state casts to floats only at the render boundary.
#![allow(clippy::cast_precision_loss)]
// `#[handler]` methods take decoded mail by value per the actor ABI.
#![allow(clippy::needless_pass_by_value)]

//! [`TurnSim`] — a deterministic, tick-native reference turn simulation.
//!
//! Intents received between lifecycle ticks share one entity-keyed bin. A
//! later intent for an entity replaces its earlier intent, then the next
//! [`Tick`] applies the bin in stable entity-id order. Every completed turn
//! emits one atomic [`TickBundle`] and retains it in a bounded catch-up ring.

pub use aether_game::{
    CellPosition, EntityState, GridBounds, MoveDirection, MoveIntent, Poll, PollResult, SimConfig, Spawn, StateSummary,
    TickBundle, TrajectoryEvent, TrajectoryKind,
};

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;
use core::mem;

use aether_actor::{ActorInitError, Manual, OutboundReply, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::{Render, Tick};
use aether_lifecycle::LifecycleCapability;
use aether_lifecycle::LifecycleMailboxExt;
use aether_math::{Mat4, Rgb, Vec3};
use aether_render::{DrawTriangle, RenderCapability, Vertex, ViewProjection};

use crate::world::CellPos;

const MAX_RENDER_GRID_EDGE: i64 = 32;
const MAX_RING_DEPTH: u32 = 1024;
const GRID_INSET: f32 = 0.035;
const MARKER_INSET: f32 = 0.22;
const GRID_Y_METERS: f32 = 0.0;
const MARKER_Y_METERS: f32 = 0.025;
const CAMERA_HEIGHT_METERS: f32 = 20.0;
const CAMERA_NEAR_METERS: f32 = 0.1;
const CAMERA_FAR_METERS: f32 = 100.0;
const CAMERA_MARGIN_CELLS: f32 = 0.5;
const GRID_EVEN_COLOR: Rgb = Rgb::new(0.10, 0.14, 0.18);
const GRID_ODD_COLOR: Rgb = Rgb::new(0.13, 0.18, 0.22);
const ENTITY_COLOR: Rgb = Rgb::new(0.95, 0.62, 0.18);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingIntent {
    Spawn(CellPos),
    Move(MoveDirection),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnResult {
    entities: BTreeMap<u64, CellPos>,
    bundle: TickBundle,
}

/// The fixed-tick toy world implementing ADR-0144's reference vocabulary.
pub struct TurnSim {
    fact_sink: Option<aether_data::MailboxId>,
    ring_depth: usize,
    grid_bounds: GridBounds,
    current_tick: u64,
    entities: BTreeMap<u64, CellPos>,
    intents: BTreeMap<u64, PendingIntent>,
    bundles: VecDeque<TickBundle>,
}

#[actor]
impl WasmActor for TurnSim {
    type Config = SimConfig;
    const NAMESPACE: &'static str = "aether.kit.sim";

    fn init(config: SimConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self {
            fact_sink: config.fact_sink,
            ring_depth: bounded_ring_depth(config.ring_depth),
            grid_bounds: config.grid_bounds,
            current_tick: 0,
            entities: BTreeMap::new(),
            intents: BTreeMap::new(),
            bundles: VecDeque::new(),
        })
    }

    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        let lifecycle = ctx.actor::<LifecycleCapability>();
        lifecycle.subscribe::<Tick>();
        lifecycle.subscribe::<Render>();
    }

    #[handler::single]
    fn on_spawn(&mut self, _ctx: &mut WasmCtx<'_>, spawn: Spawn) {
        self.intents.insert(spawn.entity_id, PendingIntent::Spawn(CellPos { x: spawn.cell_x, z: spawn.cell_z }));
    }

    #[handler::single]
    fn on_move_intent(&mut self, _ctx: &mut WasmCtx<'_>, intent: MoveIntent) {
        self.intents.insert(intent.entity_id, PendingIntent::Move(intent.direction));
    }

    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        let next_tick = self.current_tick.checked_add(1).expect("aether.kit.sim tick counter overflowed");
        let result = step_turn(&self.entities, mem::take(&mut self.intents), next_tick, self.grid_bounds);
        self.current_tick = next_tick;
        self.entities = result.entities;

        if let Some(fact_sink) = self.fact_sink {
            ctx.send_to(fact_sink, &result.bundle);
        }
        self.push_bundle(result.bundle);
    }

    #[handler::manual]
    fn on_poll(&mut self, ctx: &mut WasmCtx<'_, Manual>, poll: Poll) {
        ctx.reply(&poll_result(&self.bundles, poll.since_tick, self.current_tick));
    }

    #[handler::single]
    fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _render: Render) {
        let Some(bounds) = visible_render_bounds(self.grid_bounds) else {
            return;
        };
        let render = ctx.actor::<RenderCapability>();
        render.send(&ViewProjection { view_proj: view_projection(bounds) });
        let triangles = render_triangles(bounds, &self.entities);
        if !triangles.is_empty() {
            render.send_many(&triangles);
        }
    }
}

impl TurnSim {
    fn push_bundle(&mut self, bundle: TickBundle) {
        while self.bundles.len() >= self.ring_depth {
            self.bundles.pop_front();
        }
        self.bundles.push_back(bundle);
    }
}

fn bounded_ring_depth(configured: u32) -> usize {
    usize::try_from(configured.clamp(1, MAX_RING_DEPTH)).expect("bounded ring depth fits usize")
}

trait GridBoundsExt {
    fn contains(self, position: CellPos) -> bool;
    fn valid(self) -> bool;
}

impl GridBoundsExt for GridBounds {
    fn contains(self, position: CellPos) -> bool {
        position.x >= self.min_cell_x
            && position.x <= self.max_cell_x
            && position.z >= self.min_cell_z
            && position.z <= self.max_cell_z
    }

    fn valid(self) -> bool {
        self.min_cell_x <= self.max_cell_x && self.min_cell_z <= self.max_cell_z
    }
}

trait MoveDirectionExt {
    fn target(self, from: CellPos) -> Option<CellPos>;
}

impl MoveDirectionExt for MoveDirection {
    fn target(self, from: CellPos) -> Option<CellPos> {
        match self {
            Self::North => Some(CellPos { x: from.x, z: from.z.checked_sub(1)? }),
            Self::East => Some(CellPos { x: from.x.checked_add(1)?, z: from.z }),
            Self::South => Some(CellPos { x: from.x, z: from.z.checked_add(1)? }),
            Self::West => Some(CellPos { x: from.x.checked_sub(1)?, z: from.z }),
        }
    }
}

impl From<CellPos> for CellPosition {
    fn from(position: CellPos) -> Self {
        Self { cell_x: position.x, cell_z: position.z }
    }
}

fn step_turn(
    state: &BTreeMap<u64, CellPos>,
    intents: BTreeMap<u64, PendingIntent>,
    tick: u64,
    bounds: GridBounds,
) -> TurnResult {
    let mut entities = state.clone();
    let mut occupied: BTreeSet<CellPos> = entities.values().copied().collect();
    let mut trajectory = Vec::new();

    for (entity_id, intent) in intents {
        match intent {
            PendingIntent::Spawn(position) => {
                if entities.contains_key(&entity_id) || !bounds.contains(position) || occupied.contains(&position) {
                    continue;
                }
                entities.insert(entity_id, position);
                occupied.insert(position);
                trajectory.push(TrajectoryEvent { tick, entity_id, kind: TrajectoryKind::Spawned });
            }
            PendingIntent::Move(direction) => {
                let Some(from) = entities.get(&entity_id).copied() else {
                    continue;
                };
                let Some(to) = direction.target(from) else {
                    continue;
                };
                if !bounds.contains(to) || occupied.contains(&to) {
                    continue;
                }
                occupied.remove(&from);
                occupied.insert(to);
                entities.insert(entity_id, to);
                trajectory.push(TrajectoryEvent {
                    tick,
                    entity_id,
                    kind: TrajectoryKind::Moved { from: from.into(), to: to.into() },
                });
            }
        }
    }

    let summary = StateSummary {
        tick,
        entities: entities
            .iter()
            .map(|(&entity_id, position)| EntityState { entity_id, cell_x: position.x, cell_z: position.z })
            .collect(),
    };
    TurnResult { entities, bundle: TickBundle { tick, superseded_through: tick, trajectory, summary } }
}

fn poll_result(ring: &VecDeque<TickBundle>, since_tick: u64, current_tick: u64) -> PollResult {
    let Some(oldest) = ring.front() else {
        return PollResult { bundles: Vec::new(), current_tick };
    };

    let first_requested = since_tick.saturating_add(1);
    if first_requested < oldest.tick {
        let reset = TickBundle {
            tick: oldest.tick,
            superseded_through: oldest.tick,
            trajectory: Vec::new(),
            summary: oldest.summary.clone(),
        };
        let mut bundles = Vec::with_capacity(ring.len());
        bundles.push(reset);
        bundles.extend(ring.iter().skip(1).cloned());
        return PollResult { bundles, current_tick };
    }

    PollResult { bundles: ring.iter().filter(|bundle| bundle.tick > since_tick).cloned().collect(), current_tick }
}

#[derive(Debug, Clone, Copy)]
struct RenderRect {
    min_x: f32,
    max_x: f32,
    min_z: f32,
    max_z: f32,
}

fn visible_render_bounds(bounds: GridBounds) -> Option<GridBounds> {
    if !bounds.valid() {
        return None;
    }
    let min_x = i64::from(bounds.min_cell_x);
    let max_x = i64::from(bounds.max_cell_x);
    let min_z = i64::from(bounds.min_cell_z);
    let max_z = i64::from(bounds.max_cell_z);
    let width = max_x - min_x + 1;
    let depth = max_z - min_z + 1;
    let visible_width = width.min(MAX_RENDER_GRID_EDGE);
    let visible_depth = depth.min(MAX_RENDER_GRID_EDGE);
    let visible_min_x = min_x + (width - visible_width) / 2;
    let visible_min_z = min_z + (depth - visible_depth) / 2;
    Some(GridBounds {
        min_cell_x: i32::try_from(visible_min_x).expect("cropped x minimum remains within configured i32 bounds"),
        max_cell_x: i32::try_from(visible_min_x + visible_width - 1)
            .expect("cropped x maximum remains within configured i32 bounds"),
        min_cell_z: i32::try_from(visible_min_z).expect("cropped z minimum remains within configured i32 bounds"),
        max_cell_z: i32::try_from(visible_min_z + visible_depth - 1)
            .expect("cropped z maximum remains within configured i32 bounds"),
    })
}

fn view_projection(bounds: GridBounds) -> [f32; 16] {
    let min_x = bounds.min_cell_x as f32;
    let max_x = bounds.max_cell_x as f32 + 1.0;
    let min_z = bounds.min_cell_z as f32;
    let max_z = bounds.max_cell_z as f32 + 1.0;
    let center_x = (min_x + max_x) * 0.5;
    let center_z = (min_z + max_z) * 0.5;
    let extent = (max_x - min_x).max(max_z - min_z).mul_add(0.5, CAMERA_MARGIN_CELLS);
    let eye = Vec3::new(center_x, CAMERA_HEIGHT_METERS, center_z);
    let target = Vec3::new(center_x, GRID_Y_METERS, center_z);
    let view = Mat4::look_at_rh(eye, target, Vec3::new(0.0, 0.0, -1.0));
    let projection = Mat4::orthographic_rh(-extent, extent, -extent, extent, CAMERA_NEAR_METERS, CAMERA_FAR_METERS);
    (projection * view).to_cols_array()
}

fn render_triangles(bounds: GridBounds, entities: &BTreeMap<u64, CellPos>) -> Vec<DrawTriangle> {
    let width = i64::from(bounds.max_cell_x) - i64::from(bounds.min_cell_x) + 1;
    let depth = i64::from(bounds.max_cell_z) - i64::from(bounds.min_cell_z) + 1;
    let mut triangles = Vec::with_capacity(usize::try_from(width * depth * 2).unwrap_or(0) + entities.len() * 2);

    for cell_z in bounds.min_cell_z..=bounds.max_cell_z {
        for cell_x in bounds.min_cell_x..=bounds.max_cell_x {
            let color = if cell_x.wrapping_add(cell_z) & 1 == 0 {
                GRID_EVEN_COLOR
            } else {
                GRID_ODD_COLOR
            };
            push_quad(
                &mut triangles,
                RenderRect {
                    min_x: cell_x as f32 + GRID_INSET,
                    max_x: cell_x as f32 + 1.0 - GRID_INSET,
                    min_z: cell_z as f32 + GRID_INSET,
                    max_z: cell_z as f32 + 1.0 - GRID_INSET,
                },
                GRID_Y_METERS,
                color,
            );
        }
    }

    for position in entities.values().filter(|position| bounds.contains(**position)) {
        push_quad(
            &mut triangles,
            RenderRect {
                min_x: position.x as f32 + MARKER_INSET,
                max_x: position.x as f32 + 1.0 - MARKER_INSET,
                min_z: position.z as f32 + MARKER_INSET,
                max_z: position.z as f32 + 1.0 - MARKER_INSET,
            },
            MARKER_Y_METERS,
            ENTITY_COLOR,
        );
    }
    triangles
}

fn push_quad(out: &mut Vec<DrawTriangle>, rect: RenderRect, y: f32, color: Rgb) {
    let vertex = |x: f32, z: f32| Vertex { x, y, z, color };
    let north_west = vertex(rect.min_x, rect.min_z);
    let north_east = vertex(rect.max_x, rect.min_z);
    let south_east = vertex(rect.max_x, rect.max_z);
    let south_west = vertex(rect.min_x, rect.max_z);
    out.push(DrawTriangle { verts: [north_west, south_east, north_east] });
    out.push(DrawTriangle { verts: [north_west, south_west, south_east] });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> GridBounds {
        GridBounds { min_cell_x: -4, max_cell_x: 4, min_cell_z: -4, max_cell_z: 4 }
    }

    #[test]
    fn ring_depth_is_clamped_to_the_supported_range() {
        assert_eq!(bounded_ring_depth(0), 1);
        assert_eq!(bounded_ring_depth(64), 64);
        assert_eq!(bounded_ring_depth(u32::MAX), bounded_ring_depth(MAX_RING_DEPTH));
    }

    #[test]
    fn turn_is_deterministic_and_lower_entity_id_wins_a_collision() {
        let state = BTreeMap::from([(7, CellPos { x: 0, z: 0 }), (9, CellPos { x: 2, z: 0 })]);
        let intents = BTreeMap::from([
            (7, PendingIntent::Move(MoveDirection::East)),
            (9, PendingIntent::Move(MoveDirection::West)),
        ]);

        let first = step_turn(&state, intents.clone(), 12, bounds());
        let replay = step_turn(&state, intents, 12, bounds());

        assert_eq!(first, replay, "the same state and bin must produce the same complete turn");
        assert_eq!(first.bundle.superseded_through, 12);
        assert_eq!(
            first.bundle.trajectory,
            vec![TrajectoryEvent {
                tick: 12,
                entity_id: 7,
                kind: TrajectoryKind::Moved {
                    from: CellPosition { cell_x: 0, cell_z: 0 },
                    to: CellPosition { cell_x: 1, cell_z: 0 },
                },
            }]
        );
        assert_eq!(
            first.bundle.summary.entities,
            vec![
                EntityState { entity_id: 7, cell_x: 1, cell_z: 0 },
                EntityState { entity_id: 9, cell_x: 2, cell_z: 0 },
            ]
        );
    }

    #[test]
    fn poll_below_ring_floor_starts_with_a_summary_reset() {
        let mut ring = VecDeque::new();
        for tick in 3..=5 {
            ring.push_back(TickBundle {
                tick,
                superseded_through: tick,
                trajectory: vec![TrajectoryEvent { tick, entity_id: 1, kind: TrajectoryKind::Spawned }],
                summary: StateSummary {
                    tick,
                    entities: vec![EntityState {
                        entity_id: 1,
                        cell_x: i32::try_from(tick).expect("small test tick fits i32"),
                        cell_z: 0,
                    }],
                },
            });
        }

        let result = poll_result(&ring, 0, 5);

        assert_eq!(result.current_tick, 5);
        assert_eq!(result.bundles.iter().map(|bundle| bundle.tick).collect::<Vec<_>>(), vec![3, 4, 5]);
        assert!(result.bundles[0].trajectory.is_empty(), "the floor bundle is a wholesale summary reset");
        assert_eq!(result.bundles[0].summary.entities[0].cell_x, 3);
        assert_eq!(result.bundles[1].trajectory.len(), 1, "later retained bundles keep their incremental facts");
    }
}
