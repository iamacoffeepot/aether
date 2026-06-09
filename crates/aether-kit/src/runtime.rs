// Octimeter → world-meter and tile → world casts are domain-correct
// fixed-point-to-float conversions at the render boundary only.
#![allow(clippy::cast_precision_loss)]
// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload.
#![allow(clippy::needless_pass_by_value)]

//! [`Locomotion`] — tile-grid movement on a fixed-point ground plane.
//!
//! Holds one controllable mover on a walkable tile map, driven two ways:
//! held WASD / arrow keys (manual cell-movement), or a mouse click that
//! pathfinds to the clicked tile and walks it (click-to-move). The mover
//! advances each [`Tick`] and the scene re-renders each [`Render`] pulse.
//! The whole step is a pure fixed-point function of `(state, input)`, so
//! it is deterministic — the precondition for a server-authoritative
//! split.
//!
//! # Camera and picking
//!
//! This actor owns a fixed overhead orthographic camera: it publishes the
//! `view_proj` to `aether.render` each frame and reuses the same bounds to
//! map a click pixel to a world tile (a linear map — no matrix inverse).
//! A click runs `astar` (8-connected, iterative) from the current tile to
//! the clicked one, smooths the result, and follows it; any WASD press
//! cancels the path and hands back to manual control.
//!
//! # Movement granularity
//!
//! Position is octimeters (256 per tile, world XZ plane, Y up). Movement
//! resolves on a tunable **movement cell** of `cell` octimeters: the
//! mover commits to the adjacent cell center in the held direction,
//! glides there at `SPEED_OCTIMETERS_PER_TICK`, snaps, and re-commits.
//! One dial spans the whole feel:
//!
//! - `cell = 256` (a full tile) — classic tile-to-tile, `RuneScape`-like
//!   cadence; the mover rests on tile centers.
//! - smaller cells — the mover lands on sub-tiles (halves, quarters, …).
//! - `cell = 8` (one tick of travel) — it re-commits every tick, so
//!   direction is free and it reads as continuous glide.
//!
//! Speed and granularity are independent: `cell` sets *where the mover can
//! stop*, `SPEED_OCTIMETERS_PER_TICK` sets *how fast it crosses*.
//! Commitment is per-axis, so the mover slides along a wall rather than
//! sticking. Collision always derives from the fixed 1-tile interaction
//! grid (`pos >> TILE_BITS`), independent of the movement cell.
//!
//! `Tab` cycles `CELL_PRESETS` live; [`SetGranularity`] sets the cell from
//! mail. The mover is tinted by preset (`PRESET_COLORS`) so the active
//! granularity is visible while driving.
//!
//! # Mail surface
//!
//! - [`Key`] / [`KeyRelease`] — set / clear a held direction (WASD or
//!   arrows); `Tab` (press) cycles the movement granularity.
//! - [`MouseMove`] / [`MouseButton`] — track the cursor; a click paths to
//!   that tile. [`WindowSize`] feeds the camera aspect and picking.
//! - [`Tick`] — advance the mover one step.
//! - [`Render`] — publish the camera + emit the ground grid and mover to
//!   `aether.render`.
//! - [`Teleport`] — jump the mover to a tile center.
//! - [`SetWalkable`] — toggle a tile's walkability.
//! - [`SetGranularity`] — set the movement-cell size (same dial as `Tab`).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use aether_actor::{BootError, FfiActor, FfiCtx, Resolver, actor};
use aether_capabilities::input::InputMailboxExt;
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::{InputCapability, LifecycleCapability, RenderCapability};
use aether_kinds::{
    Camera, DrawTriangle, Key, KeyRelease, MouseButton, MouseMove, Render, Tick, Vertex,
    WindowSize, keycode,
};
use aether_math::{Mat4, Vec3};

use crate::{OCTIMETERS_PER_TILE, SetGranularity, SetWalkable, TILE_BITS, Teleport};

/// Walkable map dimensions, in tiles.
const GRID_W: i32 = 16;
const GRID_H: i32 = 16;
/// Tile count, for fixed-size pathfinding scratch arrays.
#[allow(clippy::cast_sign_loss)]
const GRID_TILES: usize = (GRID_W * GRID_H) as usize;

/// Ground speed: octimeters/tick the mover travels toward its committed
/// cell. `8` ≈ 1.9 m/s at a 60 Hz tick. Independent of the cell size.
const SPEED_OCTIMETERS_PER_TICK: i32 = 8;

/// Movement-cell presets `Tab` cycles, coarsest (a full tile, classic
/// tile stepping) to finest (one tick of travel, effectively continuous).
const CELL_PRESETS: [i32; 5] = [
    OCTIMETERS_PER_TILE,       // full tile
    OCTIMETERS_PER_TILE / 2,   // half
    OCTIMETERS_PER_TILE / 4,   // quarter
    OCTIMETERS_PER_TILE / 8,   // eighth
    SPEED_OCTIMETERS_PER_TICK, // continuous
];

/// Mover tint per [`CELL_PRESETS`] entry, so the active granularity is
/// legible while driving — coarse (orange) through fine (purple).
const PRESET_COLORS: [(f32, f32, f32); 5] = [
    (0.95, 0.55, 0.20),
    (0.92, 0.80, 0.25),
    (0.40, 0.82, 0.35),
    (0.30, 0.72, 0.88),
    (0.55, 0.45, 0.95),
];

/// World half-height of the overhead orthographic view, in metres —
/// sized to frame the grid with a margin. Half-width is this times the
/// window aspect.
const CAMERA_HALF_EXTENT: f32 = 9.0;
/// Eye height above the ground for the overhead camera. Orthographic
/// projection is translation-invariant along the view axis, so this only
/// needs to sit inside the near/far planes.
const CAMERA_EYE_HEIGHT: f32 = 10.0;
const CAMERA_Z_NEAR: f32 = 0.1;
const CAMERA_Z_FAR: f32 = 100.0;
/// Aspect used until the first `WindowSize` arrives.
const DEFAULT_ASPECT: f32 = 16.0 / 9.0;
/// World-space center of the grid (metres), the camera target.
const GRID_CENTER_X: f32 = GRID_W as f32 / 2.0;
const GRID_CENTER_Z: f32 = GRID_H as f32 / 2.0;

/// The controllable mover. Position is octimeters on the world XZ plane.
#[derive(Debug, Clone, Copy, Default)]
struct Mover {
    x: i32,
    z: i32,
}

/// Which direction keys are currently held. Four independent flags so
/// pressing opposite keys (A+D) resolves to a zero axis rather than the
/// last one winning.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default)]
struct Held {
    neg_x: bool,
    pos_x: bool,
    neg_z: bool,
    pos_z: bool,
}

impl Held {
    fn dir_x(self) -> i32 {
        i32::from(self.pos_x) - i32::from(self.neg_x)
    }

    fn dir_z(self) -> i32 {
        i32::from(self.pos_z) - i32::from(self.neg_z)
    }
}

/// A walkable tile grid, row-major as `z * GRID_W + x`.
struct TileMap {
    blocked: Vec<bool>,
}

impl TileMap {
    fn new() -> Self {
        let mut blocked = vec![false; (GRID_W * GRID_H) as usize];
        // A short wall to feel collision + wall-sliding against.
        for tz in 4..10 {
            blocked[Self::idx(6, tz)] = true;
        }
        Self { blocked }
    }

    fn in_bounds(tx: i32, tz: i32) -> bool {
        tx >= 0 && tz >= 0 && tx < GRID_W && tz < GRID_H
    }

    /// Flat index for an in-bounds tile. Callers guarantee `tx`/`tz` are
    /// non-negative and in range, so the cast is lossless.
    #[allow(clippy::cast_sign_loss)]
    fn idx(tx: i32, tz: i32) -> usize {
        (tz * GRID_W + tx) as usize
    }

    fn walkable(&self, tx: i32, tz: i32) -> bool {
        Self::in_bounds(tx, tz) && !self.blocked[Self::idx(tx, tz)]
    }

    /// Returns `false` if the tile is off-map.
    fn set(&mut self, tx: i32, tz: i32, walkable: bool) -> bool {
        if !Self::in_bounds(tx, tz) {
            return false;
        }
        self.blocked[Self::idx(tx, tz)] = !walkable;
        true
    }
}

/// Octimeter coordinate of a tile's center.
fn tile_center_octimeters(tile: i32) -> i32 {
    tile * OCTIMETERS_PER_TILE + OCTIMETERS_PER_TILE / 2
}

/// Phase of the movement grid: a tile center. Anchoring rest points here
/// (rather than on sub-cell centers) keeps the tile center reachable at
/// every cell size and makes finer grids nest through the coarser ones.
const GRID_PHASE: i32 = OCTIMETERS_PER_TILE / 2;

/// Nearest movement-grid rest point to `pos` for the given cell size. The
/// grid is `{ GRID_PHASE + cell·k }`, so tile centers (and, below a full
/// tile, the points between them) are the rest positions.
fn snap_rest(pos: i32, cell: i32) -> i32 {
    GRID_PHASE + (pos - GRID_PHASE + cell / 2).div_euclid(cell) * cell
}

/// First gameplay system: tile-grid locomotion.
pub struct Locomotion {
    map: TileMap,
    mover: Mover,
    held: Held,
    /// Movement-cell size in octimeters: the grid the mover snaps to.
    cell: i32,
    /// Per-axis octimeter target the mover is gliding toward, or `None`
    /// when that axis is at rest on a cell center and free to re-commit.
    target_x: Option<i32>,
    target_z: Option<i32>,
    /// Cached window size (logical pixels) for the camera aspect and
    /// click-to-tile picking.
    window: (u32, u32),
    /// Cached cursor position (logical pixels), updated on mouse move.
    cursor: (f32, f32),
    /// Click-to-move waypoint positions (octimeters) still to reach, in
    /// order — tile centers, then the snapped sub-tile destination. Empty
    /// when under manual (WASD) control.
    path: VecDeque<(i32, i32)>,
}

#[actor]
impl FfiActor for Locomotion {
    const NAMESPACE: &'static str = "locomotion";

    fn init<C: Resolver>(_ctx: &mut C) -> Result<Self, BootError> {
        let mover = Mover {
            x: tile_center_octimeters(GRID_W / 2),
            z: tile_center_octimeters(GRID_H / 2),
        };
        // Start at the coarsest preset (a full tile); a tile center is also
        // a cell center there, so the mover spawns aligned with no drift.
        Ok(Self {
            map: TileMap::new(),
            mover,
            held: Held::default(),
            cell: CELL_PRESETS[0],
            target_x: None,
            target_z: None,
            window: (0, 0),
            cursor: (0.0, 0.0),
            path: VecDeque::new(),
        })
    }

    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        let input = ctx.actor::<InputCapability>();
        input.subscribe::<Key>();
        input.subscribe::<KeyRelease>();
        input.subscribe::<MouseButton>();
        input.subscribe::<MouseMove>();
        input.subscribe::<WindowSize>();
        let lifecycle = ctx.actor::<LifecycleCapability>();
        lifecycle.subscribe::<Tick>();
        lifecycle.subscribe::<Render>();
    }

    #[handler]
    fn on_tick(&mut self, _ctx: &mut FfiCtx<'_>, _tick: Tick) {
        self.advance();
    }

    #[handler]
    fn on_render(&mut self, ctx: &mut FfiCtx<'_>, _render: Render) {
        let render = ctx.actor::<RenderCapability>();
        // This actor owns the overhead camera: publish the view each frame
        // (latest-wins), then the geometry.
        render.send(&Camera {
            view_proj: self.view_proj(),
        });
        render.send_many(&self.render_triangles());
    }

    #[handler]
    fn on_mouse_move(&mut self, _ctx: &mut FfiCtx<'_>, mail: MouseMove) {
        self.cursor = (mail.x, mail.y);
    }

    #[handler]
    fn on_window_size(&mut self, _ctx: &mut FfiCtx<'_>, mail: WindowSize) {
        self.window = (mail.width, mail.height);
    }

    #[handler]
    fn on_mouse_button(&mut self, _ctx: &mut FfiCtx<'_>, _mail: MouseButton) {
        self.click_to_move();
    }

    #[handler]
    fn on_key(&mut self, _ctx: &mut FfiCtx<'_>, key: Key) {
        if key.code == keycode::KEY_TAB {
            self.cycle_granularity();
        } else {
            self.set_held(key.code, true);
        }
    }

    #[handler]
    fn on_key_release(&mut self, _ctx: &mut FfiCtx<'_>, key: KeyRelease) {
        self.set_held(key.code, false);
    }

    #[handler]
    fn on_teleport(&mut self, _ctx: &mut FfiCtx<'_>, mail: Teleport) {
        if self.map.walkable(mail.tile_x, mail.tile_z) {
            self.mover.x = tile_center_octimeters(mail.tile_x);
            self.mover.z = tile_center_octimeters(mail.tile_z);
            self.target_x = None;
            self.target_z = None;
        } else {
            tracing::warn!(
                tile_x = mail.tile_x,
                tile_z = mail.tile_z,
                "teleport target off-map or blocked"
            );
        }
    }

    #[handler]
    fn on_set_walkable(&mut self, _ctx: &mut FfiCtx<'_>, mail: SetWalkable) {
        if !self.map.set(mail.tile_x, mail.tile_z, mail.walkable) {
            tracing::warn!(
                tile_x = mail.tile_x,
                tile_z = mail.tile_z,
                "set_walkable target off-map"
            );
        }
    }

    #[handler]
    fn on_set_granularity(&mut self, _ctx: &mut FfiCtx<'_>, mail: SetGranularity) {
        self.set_cell(mail.cell_octimeters);
    }
}

impl Locomotion {
    /// W / arrows map to a held direction. W is "forward" (`-Z`, toward the
    /// top of the overhead view).
    fn set_held(&mut self, code: u32, down: bool) {
        match code {
            keycode::KEY_W | keycode::KEY_UP => self.held.neg_z = down,
            keycode::KEY_S | keycode::KEY_DOWN => self.held.pos_z = down,
            keycode::KEY_A | keycode::KEY_LEFT => self.held.neg_x = down,
            keycode::KEY_D | keycode::KEY_RIGHT => self.held.pos_x = down,
            _ => {}
        }
    }

    /// Advance to the next [`CELL_PRESETS`] size (wrapping).
    fn cycle_granularity(&mut self) {
        let next = CELL_PRESETS
            .iter()
            .position(|&c| c == self.cell)
            .map_or(CELL_PRESETS[0], |i| {
                CELL_PRESETS[(i + 1) % CELL_PRESETS.len()]
            });
        self.set_cell(next);
    }

    /// Set the movement-cell size and re-settle each axis onto the new
    /// grid (gliding to the nearest rest point). Clamped to a useful range:
    /// at least one tick of travel, at most a full tile.
    fn set_cell(&mut self, cell_octimeters: i32) {
        self.cell = cell_octimeters.clamp(SPEED_OCTIMETERS_PER_TICK, OCTIMETERS_PER_TILE);
        self.target_x = Some(snap_rest(self.mover.x, self.cell));
        self.target_z = Some(snap_rest(self.mover.z, self.cell));
        tracing::info!(cell_octimeters = self.cell, "locomotion granularity");
    }

    /// One tick of movement: follow an active click-to-move path, else fall
    /// back to manual cell-movement. Any held direction cancels the path.
    fn advance(&mut self) {
        if self.held.dir_x() != 0 || self.held.dir_z() != 0 {
            self.path.clear();
        }
        let Some(&(wx, wz)) = self.path.front() else {
            self.advance_x();
            self.advance_z();
            return;
        };
        self.mover.x = approach(self.mover.x, wx, SPEED_OCTIMETERS_PER_TICK);
        self.mover.z = approach(self.mover.z, wz, SPEED_OCTIMETERS_PER_TICK);
        if self.mover.x == wx && self.mover.z == wz {
            self.path.pop_front();
        }
    }

    /// Window aspect (width / height), falling back before the first
    /// `WindowSize`.
    fn aspect(&self) -> f32 {
        let (w, h) = self.window;
        if w == 0 || h == 0 {
            DEFAULT_ASPECT
        } else {
            w as f32 / h as f32
        }
    }

    /// Overhead orthographic `view_proj` framing the grid, looking straight
    /// down world `-Y` at the XZ ground. Up is world `-Z`, so screen-up is
    /// `-Z` and screen-right is `+X`.
    fn view_proj(&self) -> [f32; 16] {
        let half_w = CAMERA_HALF_EXTENT * self.aspect();
        let proj = Mat4::orthographic_rh(
            -half_w,
            half_w,
            -CAMERA_HALF_EXTENT,
            CAMERA_HALF_EXTENT,
            CAMERA_Z_NEAR,
            CAMERA_Z_FAR,
        );
        let center = Vec3::new(GRID_CENTER_X, 0.0, GRID_CENTER_Z);
        let eye = Vec3::new(GRID_CENTER_X, CAMERA_EYE_HEIGHT, GRID_CENTER_Z);
        let view = Mat4::look_at_rh(eye, center, Vec3::new(0.0, 0.0, -1.0));
        (proj * view).to_cols_array()
    }

    /// Map the cached cursor pixel to a world octimeter position through the
    /// same ortho bounds the camera uses — a linear map, no matrix inverse.
    /// `None` if off-grid or before the first window size.
    #[allow(clippy::cast_possible_truncation)]
    fn pick_world(&self) -> Option<(i32, i32)> {
        let (w, h) = self.window;
        if w == 0 || h == 0 {
            return None;
        }
        let (px, py) = self.cursor;
        let ndc_x = (px / w as f32).mul_add(2.0, -1.0);
        let ndc_y = (py / h as f32).mul_add(-2.0, 1.0);
        let world_x = (ndc_x * CAMERA_HALF_EXTENT).mul_add(self.aspect(), GRID_CENTER_X);
        let world_z = ndc_y.mul_add(-CAMERA_HALF_EXTENT, GRID_CENTER_Z);
        if !TileMap::in_bounds(world_x.floor() as i32, world_z.floor() as i32) {
            return None;
        }
        let to_octimeters = |metres: f32| (metres * OCTIMETERS_PER_TILE as f32) as i32;
        Some((to_octimeters(world_x), to_octimeters(world_z)))
    }

    /// Pathfind to the clicked tile and follow it, finishing on the
    /// movement-cell division nearest the click rather than the tile center —
    /// so click precision tracks the current granularity.
    fn click_to_move(&mut self) {
        let Some((click_x, click_z)) = self.pick_world() else {
            return;
        };
        // Snap the click onto the active movement grid (the same one manual
        // movement rests on); A* paths to the tile that division sits in.
        let dest = (snap_rest(click_x, self.cell), snap_rest(click_z, self.cell));
        let start = (self.mover.x >> TILE_BITS, self.mover.z >> TILE_BITS);
        let goal = (dest.0 >> TILE_BITS, dest.1 >> TILE_BITS);
        let Some(tiles) = astar(&self.map, start, goal) else {
            return;
        };
        self.path = smooth_path(start, &tiles, dest);
        self.target_x = None;
        self.target_z = None;
    }

    /// Advance the X axis one tick: commit to the next cell when idle and a
    /// direction is held (if its tile is walkable), then glide toward the
    /// committed target and clear on arrival.
    fn advance_x(&mut self) {
        if self.target_x.is_none() {
            let dir = self.held.dir_x();
            if dir != 0 {
                let target = snap_rest(self.mover.x, self.cell) + dir * self.cell;
                let tz = self.mover.z >> TILE_BITS;
                if self.map.walkable(target >> TILE_BITS, tz) {
                    self.target_x = Some(target);
                }
            }
        }
        if let Some(target) = self.target_x {
            self.mover.x = approach(self.mover.x, target, SPEED_OCTIMETERS_PER_TICK);
            if self.mover.x == target {
                self.target_x = None;
            }
        }
    }

    fn advance_z(&mut self) {
        if self.target_z.is_none() {
            let dir = self.held.dir_z();
            if dir != 0 {
                let target = snap_rest(self.mover.z, self.cell) + dir * self.cell;
                let tx = self.mover.x >> TILE_BITS;
                if self.map.walkable(tx, target >> TILE_BITS) {
                    self.target_z = Some(target);
                }
            }
        }
        if let Some(target) = self.target_z {
            self.mover.z = approach(self.mover.z, target, SPEED_OCTIMETERS_PER_TICK);
            if self.mover.z == target {
                self.target_z = None;
            }
        }
    }

    /// Ground grid (checkerboard, blocked tiles red) at `y = 0` plus the
    /// mover quad — tinted by granularity — just above it. The only float
    /// in the system lives here, at the render boundary; it never feeds
    /// back into the sim.
    fn render_triangles(&self) -> Vec<DrawTriangle> {
        let mut out = Vec::with_capacity((GRID_W * GRID_H * 2 + 2) as usize);
        for tz in 0..GRID_H {
            for tx in 0..GRID_W {
                let color = if !self.map.walkable(tx, tz) {
                    (0.60, 0.16, 0.16)
                } else if (tx + tz) % 2 == 0 {
                    (0.52, 0.54, 0.58)
                } else {
                    (0.40, 0.42, 0.46)
                };
                // Slightly under-fill the tile so grid lines show.
                push_quad(&mut out, tx as f32 + 0.5, tz as f32 + 0.5, 0.48, 0.0, color);
            }
        }
        let ax = self.mover.x as f32 / OCTIMETERS_PER_TILE as f32;
        let az = self.mover.z as f32 / OCTIMETERS_PER_TILE as f32;
        push_quad(&mut out, ax, az, 0.25, 0.10, mover_color(self.cell));
        out
    }
}

/// Mover tint for the active cell size — its [`CELL_PRESETS`] color, or a
/// neutral blue for an off-preset size set via [`SetGranularity`].
fn mover_color(cell: i32) -> (f32, f32, f32) {
    CELL_PRESETS
        .iter()
        .position(|&c| c == cell)
        .map_or((0.20, 0.62, 0.95), |i| PRESET_COLORS[i])
}

/// 8-connected A* on the walkable tile grid. Returns the waypoint tiles
/// from just past `start` through `goal`, or `None` if unreachable.
/// Iterative and bounded by the grid — never recursive.
fn astar(map: &TileMap, start: (i32, i32), goal: (i32, i32)) -> Option<VecDeque<(i32, i32)>> {
    if !map.walkable(goal.0, goal.1) {
        return None;
    }
    // Octile distance: cardinal = 10, diagonal = 14 (≈ 10·√2). Used as both
    // step cost and heuristic so a straight run is strictly cheaper than an
    // equal-length diagonal zigzag — paths hug the direct route.
    let octile = |a: (i32, i32), b: (i32, i32)| {
        let dx = (a.0 - b.0).abs();
        let dy = (a.1 - b.1).abs();
        let lo = dx.min(dy);
        14 * lo + 10 * (dx.max(dy) - lo)
    };
    let mut g_score = [i32::MAX; GRID_TILES];
    let mut came_from: [Option<(i32, i32)>; GRID_TILES] = [None; GRID_TILES];
    let mut open = BinaryHeap::new();
    g_score[TileMap::idx(start.0, start.1)] = 0;
    // Heap key (f, h, tile): ties in f break toward the smaller h (closer to
    // the goal), which keeps the path from drifting off the straight line.
    let h0 = octile(start, goal);
    open.push(Reverse((h0, h0, start)));
    while let Some(Reverse((_, _, cur))) = open.pop() {
        if cur == goal {
            let mut path = VecDeque::new();
            let mut node = goal;
            while node != start {
                path.push_front(node);
                node = came_from[TileMap::idx(node.0, node.1)]?;
            }
            return Some(path);
        }
        let cur_g = g_score[TileMap::idx(cur.0, cur.1)];
        for dz in -1..=1 {
            for dx in -1..=1 {
                if dx == 0 && dz == 0 {
                    continue;
                }
                let nb = (cur.0 + dx, cur.1 + dz);
                if !map.walkable(nb.0, nb.1) {
                    continue;
                }
                let step = if dx != 0 && dz != 0 { 14 } else { 10 };
                let nb_g = cur_g + step;
                let i = TileMap::idx(nb.0, nb.1);
                if nb_g < g_score[i] {
                    g_score[i] = nb_g;
                    came_from[i] = Some(cur);
                    let h = octile(nb, goal);
                    open.push(Reverse((nb_g + h, h, nb)));
                }
            }
        }
    }
    None
}

/// Turn the A* tile path (the steps after `start`) into octimeter
/// waypoints, collapsing straight runs: only tiles where the direction
/// changes become waypoints (at their centers), and the path always ends
/// on the snapped sub-tile `dest`. Collapsing the colinear middle keeps an
/// off-center straight line from bulging up to the tile-center row and back.
fn smooth_path(
    start: (i32, i32),
    tiles: &VecDeque<(i32, i32)>,
    dest: (i32, i32),
) -> VecDeque<(i32, i32)> {
    let dir = |a: (i32, i32), b: (i32, i32)| ((b.0 - a.0).signum(), (b.1 - a.1).signum());
    let mut full = Vec::with_capacity(tiles.len() + 1);
    full.push(start);
    full.extend(tiles.iter().copied());
    let mut path = VecDeque::new();
    for i in 1..full.len().saturating_sub(1) {
        if dir(full[i - 1], full[i]) != dir(full[i], full[i + 1]) {
            path.push_back((
                tile_center_octimeters(full[i].0),
                tile_center_octimeters(full[i].1),
            ));
        }
    }
    path.push_back(dest);
    path
}

/// Move `cur` toward `target` by at most `step` octimeters, never
/// overshooting.
fn approach(cur: i32, target: i32, step: i32) -> i32 {
    use core::cmp::Ordering;
    match cur.cmp(&target) {
        Ordering::Less => (cur + step).min(target),
        Ordering::Greater => (cur - step).max(target),
        Ordering::Equal => cur,
    }
}

/// Append a flat axis-aligned quad (two triangles) on the XZ plane.
fn push_quad(
    out: &mut Vec<DrawTriangle>,
    cx: f32,
    cz: f32,
    half: f32,
    y: f32,
    rgb: (f32, f32, f32),
) {
    let (r, g, b) = rgb;
    let vert = |x: f32, z: f32| Vertex { x, y, z, r, g, b };
    let (x0, x1, z0, z1) = (cx - half, cx + half, cz - half, cz + half);
    out.push(DrawTriangle {
        verts: [vert(x0, z0), vert(x1, z0), vert(x1, z1)],
    });
    out.push(DrawTriangle {
        verts: [vert(x0, z0), vert(x1, z1), vert(x0, z1)],
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_centers_stay_reachable_at_every_granularity() {
        // The movement grid nests through tile centers: at every preset
        // cell size, a tile center is itself a rest point.
        for &cell in &CELL_PRESETS {
            for tile in 0..GRID_W {
                let center = tile_center_octimeters(tile);
                assert_eq!(snap_rest(center, cell), center, "cell={cell} tile={tile}");
            }
        }
    }

    #[test]
    fn astar_routes_around_the_wall() {
        // The default map has a wall at x=6, z=4..9. A path from the right
        // of it to the left must detour around — never through it — and stay
        // a contiguous 8-connected walk ending on the goal.
        let map = TileMap::new();
        let start = (8, 8);
        let goal = (2, 7);
        let path = astar(&map, start, goal).expect("goal is reachable");
        assert_eq!(path.back().copied(), Some(goal));
        let mut prev = start;
        for &step in &path {
            assert!(map.walkable(step.0, step.1), "stepped onto a blocked tile");
            let d = (step.0 - prev.0).abs().max((step.1 - prev.1).abs());
            assert_eq!(d, 1, "non-adjacent hop {prev:?} -> {step:?}");
            prev = step;
        }
    }

    #[test]
    fn astar_returns_none_for_blocked_goal() {
        assert!(astar(&TileMap::new(), (8, 8), (6, 6)).is_none());
    }

    #[test]
    fn smooth_path_collapses_a_straight_run_to_just_the_dest() {
        // A straight tile run leaves no intermediate tile-center waypoints —
        // only the sub-tile dest — so an off-center straight line stays
        // straight instead of bulging to the center row and back.
        let tiles: VecDeque<(i32, i32)> = [(9, 8), (10, 8), (11, 8), (12, 8), (13, 8)].into();
        let dest = (13 * OCTIMETERS_PER_TILE + 100, 8 * OCTIMETERS_PER_TILE + 80);
        assert_eq!(smooth_path((8, 8), &tiles, dest), VecDeque::from([dest]));
    }

    #[test]
    fn smooth_path_keeps_the_corner_of_an_l() {
        // An L-shaped path keeps the turn tile (as a center) plus the dest.
        let tiles: VecDeque<(i32, i32)> = [(9, 8), (10, 8), (10, 7), (10, 6)].into();
        let dest = (10 * OCTIMETERS_PER_TILE + 50, 6 * OCTIMETERS_PER_TILE + 50);
        let corner = (tile_center_octimeters(10), tile_center_octimeters(8));
        assert_eq!(
            smooth_path((8, 8), &tiles, dest),
            VecDeque::from([corner, dest])
        );
    }

    #[test]
    fn astar_keeps_a_straight_line_straight() {
        // A horizontal target in open space (right of the wall) must be a
        // pure eastward run — octile cost forbids the equal-length diagonal
        // zigzag a uniform cost would allow.
        let path = astar(&TileMap::new(), (8, 8), (13, 8)).expect("reachable");
        let expected: VecDeque<(i32, i32)> = [(9, 8), (10, 8), (11, 8), (12, 8), (13, 8)].into();
        assert_eq!(path, expected);
    }

    #[test]
    fn finer_cells_add_rest_points_between_centers() {
        // A quarter-tile grid puts a rest point a quarter-tile off the
        // center — the sub-tile landing a full-tile grid can't reach.
        let center = tile_center_octimeters(8);
        let quarter = OCTIMETERS_PER_TILE / 4;
        let off_center = center + quarter;
        assert_eq!(snap_rest(off_center, quarter), off_center);
        // ...which the full-tile grid snaps back to the center.
        assert_eq!(snap_rest(off_center, OCTIMETERS_PER_TILE), center);
    }
}

aether_actor::export!(Locomotion);
