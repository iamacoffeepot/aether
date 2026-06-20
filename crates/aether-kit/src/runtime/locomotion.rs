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
//! This actor owns a perspective camera that trails the mover at a
//! three-quarter overhead angle the arrow keys orbit (yaw freely, pitch
//! within a slice above the horizon): it publishes the `view_proj` to
//! `aether.render` each frame and casts a ray from the click pixel through
//! that same camera onto the ground plane to find the world tile — so
//! picking stays correct at any orbit angle. A click runs `astar`
//! (8-connected, iterative) from the current tile to the clicked one,
//! smooths the result, and follows it, highlighting the exact destination
//! cell along the way; any WASD press cancels the path and hands back to
//! manual control.
//!
//! The mover is drawn as a capped-cylinder capsule at human dimensions
//! (1.8 m tall, 0.3 m radius), shaded against a fixed light so it reads as
//! a solid 3D body standing on the ground.
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
//! - [`Key`] / [`KeyRelease`] — WASD set / clear a held movement direction;
//!   the arrow keys orbit the camera; `Tab` (press) cycles the movement
//!   granularity.
//! - [`MouseMove`] / [`MouseButton`] — track the cursor; a click paths to
//!   that tile. [`WindowSize`] feeds the camera aspect and picking.
//! - [`Tick`] — advance the mover one step.
//! - [`Render`] — publish the camera + emit the ground grid and mover to
//!   `aether.render`.
//! - [`Teleport`] — jump the mover to a tile center.
//! - [`SetWalkable`] — toggle a tile's walkability.
//! - [`SetGranularity`] — set the movement-cell size (same dial as `Tab`).

use core::f32::consts::{FRAC_PI_2, PI, TAU};
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, VecDeque};

use aether_actor::{BootError, FfiActor, FfiCtx, FfiInitCtx, actor};
use aether_capabilities::input::InputMailboxExt;
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::{InputCapability, LifecycleCapability, RenderCapability, UiCapability};
use aether_kinds::{
    Camera, DrawTriangle, Key, KeyRelease, MouseButton, MouseMove, Render, Tick, UiBar, UiPanel,
    Vertex, WindowSize, keycode,
};
use aether_math::{Mat4, Vec3};

use crate::arena::{Arena, HH, HW, SUB, ShapeClass};
use crate::{OCTIMETERS_PER_TILE, Preview, SetGranularity, SetWalkable, TILE_BITS, Teleport};

/// Walkable map dimensions, in tiles.
pub(crate) const GRID_W: i32 = 16;
pub(crate) const GRID_H: i32 = 16;
/// Tile count, for fixed-size pathfinding scratch arrays.
#[allow(clippy::cast_sign_loss)]
pub(crate) const GRID_TILES: usize = (GRID_W * GRID_H) as usize;

/// Ground speed: octimeters/tick the mover travels toward its committed
/// cell. `8` ≈ 1.9 m/s at a 60 Hz tick. Independent of the cell size.
const SPEED_OCTIMETERS_PER_TICK: i32 = 8;

/// Per-axis speed for a diagonal manual move: `SPEED_OCTIMETERS_PER_TICK / √2`
/// rounded (`round(8/√2) = 6`), so holding two directions at once covers the
/// same ground per tick as holding one — no √2 diagonal speed-up. Click-to-move
/// gets the same normalization continuously from `step_toward`.
const SPEED_DIAGONAL_OCTIMETERS_PER_TICK: i32 = 6;

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

/// Vertical field of view of the follow camera.
const CAMERA_FOV_Y: f32 = PI / 3.0;
/// Starting pitch of the camera above the horizontal, in radians (~52°) — a
/// three-quarter overhead angle that reads as 3D while keeping the ground
/// legible. The arrow keys orbit from here within [`CAMERA_PITCH_MIN`]..=
/// [`CAMERA_PITCH_MAX`].
const CAMERA_PITCH: f32 = 0.9;
/// Pitch clamp: the eye always stays in a slice above the horizon (never at
/// or below it, never straight overhead — `look_at` degenerates as the view
/// axis nears world `Y`).
const CAMERA_PITCH_MIN: f32 = 0.15;
const CAMERA_PITCH_MAX: f32 = 1.45;
/// Arrow-key orbit rates, radians per tick. Left/right sweep yaw, up/down
/// raise/lower pitch — tuned slow for a gentle, controllable orbit.
const CAMERA_YAW_SPEED: f32 = 0.0135;
const CAMERA_PITCH_SPEED: f32 = 0.0067;
/// Distance from the camera target (the mover) to the eye, in metres.
const CAMERA_DISTANCE: f32 = 12.0;
/// Height above the ground the camera looks at — roughly the mover's
/// mid-height, so the capsule sits centred in frame.
const CAMERA_TARGET_HEIGHT: f32 = 0.9;
const CAMERA_Z_NEAR: f32 = 0.1;
const CAMERA_Z_FAR: f32 = 100.0;
/// Aspect used until the first `WindowSize` arrives.
const DEFAULT_ASPECT: f32 = 16.0 / 9.0;

/// Highlight tint for the click-to-move destination cell — the exact region
/// the mover will come to rest in.
const DEST_COLOR: (f32, f32, f32) = (0.20, 0.95, 0.65);

/// Each level lasts 30 s at the 60 Hz tick.
const LEVEL_TICKS: u32 = 1800;

/// The five levels: a shape class and the intensity envelope it ramps across
/// (start → end). The three classes are reused, each repeat starting harder, so
/// difficulty climbs both within a level and across the run.
const LEVELS: [(ShapeClass, i32, i32); 5] = [
    (ShapeClass::Ring, 0, 50),
    (ShapeClass::Wave, 10, 60),
    (ShapeClass::Wall, 15, 65),
    (ShapeClass::Ring, 45, 90),
    (ShapeClass::Wall, 55, 100),
];

/// Full health. A larger pool than it needs to be so the bar drains smoothly.
const HEALTH_MAX: i32 = 1000;
/// Health lost per tick while standing on a striking (red) sub-cell — about two
/// seconds of continuous contact to die from full.
const DAMAGE_PER_TICK: i32 = 8;
/// Pause after death before the run restarts: 10 s at the 60 Hz tick.
const RESTART_TICKS: u32 = 600;

/// Player capsule (a capped cylinder) at human dimensions: 1.8 m tall,
/// 0.3 m radius. The bottom cap rests on the ground (`y = 0`).
const PLAYER_HEIGHT: f32 = 1.8;
const PLAYER_RADIUS: f32 = 0.3;

/// Direction the scene light travels. The capsule bakes a simple Lambert
/// shade against it into vertex colours so it reads as a solid 3D form
/// (the render pipeline carries no lighting of its own).
const LIGHT_DIR: Vec3 = Vec3::new(-0.4, -1.0, -0.3);

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

/// Which arrow keys are currently held, driving the camera orbit.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default)]
struct CamHeld {
    left: bool,
    right: bool,
    up: bool,
    down: bool,
}

impl CamHeld {
    /// Yaw direction: right sweeps one way, left the other.
    fn yaw_dir(self) -> f32 {
        f32::from(self.right) - f32::from(self.left)
    }

    /// Pitch direction: up raises the camera (more overhead), down lowers it
    /// toward the horizon.
    fn pitch_dir(self) -> f32 {
        f32::from(self.up) - f32::from(self.down)
    }
}

/// A walkable tile grid, row-major as `z * GRID_W + x`.
struct TileMap {
    blocked: Vec<bool>,
}

impl TileMap {
    /// An open arena — every tile walkable. The hazard game plays on open
    /// ground; walls can still be added at runtime via [`SetWalkable`].
    fn new() -> Self {
        Self {
            blocked: vec![false; GRID_TILES],
        }
    }

    /// Build a map with exactly the listed tiles blocked, for tests that need
    /// a specific scenario rather than the demo maze.
    #[cfg(test)]
    fn from_blocked(cells: &[(i32, i32)]) -> Self {
        let mut map = Self {
            blocked: vec![false; GRID_TILES],
        };
        for &(tx, tz) in cells {
            map.blocked[Self::idx(tx, tz)] = true;
        }
        map
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
    /// Click-to-move destination (octimeter center of the rest cell), shown
    /// as a highlight while a path is active. `None` under manual control.
    dest: Option<(i32, i32)>,
    /// Camera orbit angles around the mover: `cam_yaw` is the azimuth (0 puts
    /// the eye behind, `+Z`), `cam_pitch` the elevation above the horizon.
    cam_yaw: f32,
    cam_pitch: f32,
    /// Which arrow keys are held, orbiting the camera each tick.
    cam_held: CamHeld,
    /// The hazard game: tiles telegraph then turn lethal on a deterministic
    /// schedule.
    arena: Arena,
    /// Whether to draw the orange warning telegraph. Off is a hardcore mode —
    /// only the red strikes (and the blue refuge) show. Toggled with `O`.
    show_warnings: bool,
    /// Design-aid preview (not play): when non-zero the live game is frozen and
    /// the field shows a static parameter matrix of one shape (see [`Preview`]),
    /// viewed top-down. `0` is normal play.
    preview: u32,
    /// Index into [`LEVELS`] of the level being played.
    level: usize,
    /// Ticks elapsed in the current level, `0..LEVEL_TICKS`; drives the
    /// continuous difficulty ramp.
    level_clock: u32,
    /// Remaining health, `0..=HEALTH_MAX`. Drains while standing on a red cell.
    health: i32,
    /// When dead, ticks left before the run restarts; `None` while alive.
    dead_clock: Option<u32>,
}

#[actor]
impl FfiActor for Locomotion {
    const NAMESPACE: &'static str = "aether.locomotion";

    fn init(_ctx: &mut FfiInitCtx<'_>) -> Result<Self, BootError> {
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
            dest: None,
            cam_yaw: 0.0,
            cam_pitch: CAMERA_PITCH,
            cam_held: CamHeld::default(),
            arena: Arena::new(),
            show_warnings: true,
            preview: 0,
            level: 0,
            level_clock: 0,
            health: HEALTH_MAX,
            dead_clock: None,
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
        // In preview mode the game is frozen: the static matrix in the field
        // must persist, so skip the simulation entirely.
        if self.preview != 0 {
            return;
        }
        // The camera still orbits whether alive or dead.
        self.orbit_camera();
        // Dead: the game is frozen; count down, then restart the run.
        if let Some(remaining) = self.dead_clock {
            match remaining.checked_sub(1).filter(|&r| r > 0) {
                Some(r) => self.dead_clock = Some(r),
                None => self.restart(),
            }
            return;
        }
        self.advance_level();
        self.arena.tick();
        self.apply_damage();
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
        // The HUD composes on the `aether.ui` cap in screen space, kept
        // separate from the world geometry above.
        self.send_hud(ctx);
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
        match key.code {
            keycode::KEY_TAB => self.cycle_granularity(),
            keycode::KEY_O => {
                self.show_warnings = !self.show_warnings;
                tracing::info!(show_warnings = self.show_warnings, "locomotion warnings");
            }
            keycode::KEY_K => {
                tracing::info!(speed_percent = self.arena.speed_up(), "hazard speed");
            }
            keycode::KEY_J => {
                tracing::info!(speed_percent = self.arena.speed_down(), "hazard speed");
            }
            _ => self.set_held(key.code, true),
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

    #[handler]
    fn on_preview(&mut self, _ctx: &mut FfiCtx<'_>, mail: Preview) {
        self.preview = mail.shape;
        if mail.shape != 0 {
            self.arena.show_matrix(mail.shape);
        }
        tracing::info!(shape = mail.shape, "locomotion preview");
    }
}

impl Locomotion {
    /// WASD moves the mover (W is `-Z`, world-forward); the arrow keys orbit
    /// the camera instead of moving.
    fn set_held(&mut self, code: u32, down: bool) {
        match code {
            keycode::KEY_W => self.held.neg_z = down,
            keycode::KEY_S => self.held.pos_z = down,
            keycode::KEY_A => self.held.neg_x = down,
            keycode::KEY_D => self.held.pos_x = down,
            keycode::KEY_LEFT => self.cam_held.left = down,
            keycode::KEY_RIGHT => self.cam_held.right = down,
            keycode::KEY_UP => self.cam_held.up = down,
            keycode::KEY_DOWN => self.cam_held.down = down,
            _ => {}
        }
    }

    /// Drive the level director one tick: ramp the current level's intensity
    /// across its clock, push it to the arena, and roll to the next level (wrap
    /// at the end) when the clock expires.
    fn advance_level(&mut self) {
        let (class, lo, hi) = LEVELS[self.level];
        self.arena
            .set_level(class, lerp_level(lo, hi, self.level_clock, LEVEL_TICKS));
        self.level_clock += 1;
        if self.level_clock >= LEVEL_TICKS {
            self.level_clock = 0;
            self.level = (self.level + 1) % LEVELS.len();
        }
    }

    /// Drain health while the mover's tile stands on a striking (red) sub-cell;
    /// at zero, enter the death pause that restarts the run.
    fn apply_damage(&mut self) {
        let sub = OCTIMETERS_PER_TILE / SUB;
        if self.arena.is_danger(self.mover.x / sub, self.mover.z / sub) {
            self.health = (self.health - DAMAGE_PER_TICK).max(0);
            if self.health == 0 {
                self.dead_clock = Some(RESTART_TICKS);
            }
        }
    }

    /// Restart the run from level one: full health, fresh arena, mover recentred.
    fn restart(&mut self) {
        self.level = 0;
        self.level_clock = 0;
        self.health = HEALTH_MAX;
        self.dead_clock = None;
        self.arena.reset();
        self.mover = Mover {
            x: tile_center_octimeters(GRID_W / 2),
            z: tile_center_octimeters(GRID_H / 2),
        };
        self.path.clear();
        self.dest = None;
        self.target_x = None;
        self.target_z = None;
    }

    /// Orbit the camera one tick from the held arrow keys: sweep yaw freely,
    /// raise/lower pitch within the above-horizon clamp.
    fn orbit_camera(&mut self) {
        let (yaw, pitch) = step_camera(
            self.cam_yaw,
            self.cam_pitch,
            self.cam_held.yaw_dir(),
            self.cam_held.pitch_dir(),
        );
        self.cam_yaw = yaw;
        self.cam_pitch = pitch;
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
            self.dest = None;
        }
        let Some(&(wx, wz)) = self.path.front() else {
            // Manual cell-movement. A diagonal (both axes held) gets the
            // reduced per-axis speed so it doesn't outrun a cardinal move.
            let speed = if self.held.dir_x() != 0 && self.held.dir_z() != 0 {
                SPEED_DIAGONAL_OCTIMETERS_PER_TICK
            } else {
                SPEED_OCTIMETERS_PER_TICK
            };
            self.advance_x(speed);
            self.advance_z(speed);
            return;
        };
        let (nx, nz) = step_toward(
            (self.mover.x, self.mover.z),
            (wx, wz),
            SPEED_OCTIMETERS_PER_TICK,
        );
        self.mover.x = nx;
        self.mover.z = nz;
        if self.mover.x == wx && self.mover.z == wz {
            self.path.pop_front();
            if self.path.is_empty() {
                self.dest = None;
            }
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

    /// World-space eye and target for the follow camera: it looks at a point
    /// just above the mover and orbits it on a sphere of radius
    /// `CAMERA_DISTANCE` at the current `cam_yaw` / `cam_pitch`, so the view
    /// trails the mover as it walks and the arrow keys swing it around.
    fn camera_eye_target(&self) -> (Vec3, Vec3) {
        let to_metres = |oct: i32| oct as f32 / OCTIMETERS_PER_TILE as f32;
        let target = Vec3::new(
            to_metres(self.mover.x),
            CAMERA_TARGET_HEIGHT,
            to_metres(self.mover.z),
        );
        // `yaw = 0` puts the eye behind the mover (`+Z`); higher pitch lifts it
        // toward straight overhead.
        let horizontal = CAMERA_DISTANCE * self.cam_pitch.cos();
        let offset = Vec3::new(
            horizontal * self.cam_yaw.sin(),
            CAMERA_DISTANCE * self.cam_pitch.sin(),
            horizontal * self.cam_yaw.cos(),
        );
        (target + offset, target)
    }

    /// Perspective `view_proj`: the follow camera in play, or a fixed top-down
    /// view framing the whole grid in preview (so the parameter matrix reads
    /// like a contact sheet).
    fn view_proj(&self) -> [f32; 16] {
        let (eye, target, up) = if self.preview == 0 {
            let (eye, target) = self.camera_eye_target();
            (eye, target, Vec3::Y)
        } else {
            preview_camera()
        };
        let view = Mat4::look_at_rh(eye, target, up);
        let proj = Mat4::perspective_rh(CAMERA_FOV_Y, self.aspect(), CAMERA_Z_NEAR, CAMERA_Z_FAR);
        (proj * view).to_cols_array()
    }

    /// Cast a ray from the cursor pixel through the follow camera onto the
    /// ground plane (`y = 0`) and return the hit as an octimeter position.
    /// `None` if it misses the grid or before the first window size.
    #[allow(clippy::cast_possible_truncation)]
    fn pick_world(&self) -> Option<(i32, i32)> {
        let (w, h) = self.window;
        if w == 0 || h == 0 {
            return None;
        }
        let (px, py) = self.cursor;
        let ndc_x = (px / w as f32).mul_add(2.0, -1.0);
        let ndc_y = (py / h as f32).mul_add(-2.0, 1.0);

        let (eye, target) = self.camera_eye_target();
        // Camera basis, matching `look_at_rh`: `fwd` points into the scene,
        // `right` and `up` span the image plane.
        let fwd = (target - eye).normalize();
        let right = fwd.cross(Vec3::Y).normalize();
        let up = right.cross(fwd);
        // Ray direction through the pixel for a vertical FOV of `CAMERA_FOV_Y`.
        let tan = (CAMERA_FOV_Y * 0.5).tan();
        let dir = fwd + right * (ndc_x * tan * self.aspect()) + up * (ndc_y * tan);

        let (hit_x, hit_z) = intersect_ground(eye, dir)?;
        if !TileMap::in_bounds(hit_x.floor() as i32, hit_z.floor() as i32) {
            return None;
        }
        let to_octimeters = |metres: f32| (metres * OCTIMETERS_PER_TILE as f32) as i32;
        Some((to_octimeters(hit_x), to_octimeters(hit_z)))
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
        let start_tile = (self.mover.x >> TILE_BITS, self.mover.z >> TILE_BITS);
        let goal = (dest.0 >> TILE_BITS, dest.1 >> TILE_BITS);
        let Some(tiles) = astar(&self.map, start_tile, goal) else {
            return;
        };
        self.path = smooth_path(&self.map, (self.mover.x, self.mover.z), &tiles, dest);
        self.dest = Some(dest);
        self.target_x = None;
        self.target_z = None;
    }

    /// Advance the X axis one tick at `speed`: commit to the next cell when
    /// idle and a direction is held (if its tile is walkable), then glide
    /// toward the committed target and clear on arrival.
    fn advance_x(&mut self, speed: i32) {
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
            self.mover.x = approach(self.mover.x, target, speed);
            if self.mover.x == target {
                self.target_x = None;
            }
        }
    }

    fn advance_z(&mut self, speed: i32) {
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
            self.mover.z = approach(self.mover.z, target, speed);
            if self.mover.z == target {
                self.target_z = None;
            }
        }
    }

    /// Ground grid (checkerboard, blocked tiles red) at `y = 0` plus the
    /// player capsule — tinted by granularity, shaded — standing on it. The
    /// only floats in the system live here, at the render boundary; they
    /// never feed back into the sim.
    fn render_triangles(&self) -> Vec<DrawTriangle> {
        let mut out = Vec::with_capacity((GRID_W * GRID_H * 2) as usize + 2048);
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
        // Hazard overlay at sub-cell resolution, laid just above the floor. In
        // preview the orange telegraph always shows (the matrix is about seeing
        // both bands), independent of the hardcore toggle.
        let show_warnings = self.show_warnings || self.preview != 0;
        let sub = SUB as f32;
        let half = 0.5 / sub;
        for sz in 0..HH {
            for sx in 0..HW {
                if let Some(color) = self.arena.subcell_color(sx, sz, show_warnings) {
                    let cx = (sx as f32 + 0.5) / sub;
                    let cz = (sz as f32 + 0.5) / sub;
                    push_quad(&mut out, cx, cz, half, 0.02, color);
                }
            }
        }
        // The preview is a static contact sheet: no player or destination, just
        // the parameter matrix on the floor.
        if self.preview != 0 {
            return out;
        }
        // Click-to-move destination: the exact cell the mover will rest in,
        // sized to the active granularity. Laid above the hazard overlay
        // (which sits at 0.02) so the selector never z-fights the paint.
        if let Some((dx, dz)) = self.dest {
            let cell_metres = self.cell as f32 / OCTIMETERS_PER_TILE as f32;
            let cx = dx as f32 / OCTIMETERS_PER_TILE as f32;
            let cz = dz as f32 / OCTIMETERS_PER_TILE as f32;
            push_quad(&mut out, cx, cz, cell_metres * 0.5, 0.06, DEST_COLOR);
        }
        let ax = self.mover.x as f32 / OCTIMETERS_PER_TILE as f32;
        let az = self.mover.z as f32 / OCTIMETERS_PER_TILE as f32;
        push_capsule(&mut out, ax, az, mover_color(self.cell));
        out
    }

    /// Mail the screen-anchored health HUD to the `aether.ui` cap: a dark
    /// backing `panel` plate under a health `bar` whose fill shrinks and
    /// reddens as health drops. The rects are screen-pixel rects derived from
    /// the cached window size and resent every frame in immediate mode.
    /// Skipped in preview (the parameter contact sheet carries no mover or
    /// HUD) and before the first `WindowSize` establishes a real viewport.
    fn send_hud(&self, ctx: &mut FfiCtx<'_>) {
        if self.preview != 0 {
            return;
        }
        let (width, height) = self.window;
        if width == 0 || height == 0 {
            return;
        }
        let (width, height) = (width as f32, height as f32);
        // A centered strip near the top edge: the dark plate, then a fill
        // track inset within it. Both are fractions of the window so the HUD
        // holds its screen position at any size — the anchoring the
        // camera-ray projector used to stand in for.
        let plate = [0.225 * width, 0.040 * height, 0.550 * width, 0.040 * height];
        let track = [
            0.240 * width,
            0.0475 * height,
            0.520 * width,
            0.025 * height,
        ];
        let frac = self.health as f32 / HEALTH_MAX as f32;
        let (fill_r, fill_g, fill_b) = health_color(frac);
        let plate_color = [0.10, 0.10, 0.13, 1.0];
        let ui = ctx.actor::<UiCapability>();
        ui.send(&UiPanel {
            rect: plate,
            color: plate_color,
        });
        ui.send(&UiBar {
            rect: track,
            frac,
            track_color: plate_color,
            fill_color: [fill_r, fill_g, fill_b, 1.0],
        });
    }
}

/// Health-bar fill colour: red at empty through green at full.
fn health_color(frac: f32) -> (f32, f32, f32) {
    let lerp = |a: f32, b: f32| (b - a).mul_add(frac, a);
    (lerp(0.85, 0.25), lerp(0.20, 0.82), lerp(0.18, 0.32))
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

/// Smooth the A* tile path into the fewest octimeter waypoints that still
/// clear every wall — string-pulling by line of sight. The candidate points
/// are the actual sub-tile `start` (the mover's position), each interior
/// tile's center, and the snapped sub-tile `dest`. Walking them, each one is
/// dropped whenever the next is directly visible from the current anchor, so
/// a stretch with nothing in the way collapses to a single straight segment
/// and a corner survives only where a wall genuinely sits between the anchor
/// and the point past it. Anchoring on the real start/dest (not tile centers)
/// is what keeps an off-center straight line from kinking through a center.
fn smooth_path(
    map: &TileMap,
    start: (i32, i32),
    tiles: &VecDeque<(i32, i32)>,
    dest: (i32, i32),
) -> VecDeque<(i32, i32)> {
    // Candidates: start, every tile center *before* the goal tile, then dest
    // (which replaces the goal tile's center — it sits inside that tile).
    let mut pts = Vec::with_capacity(tiles.len() + 1);
    pts.push(start);
    let interior = tiles.len().saturating_sub(1);
    pts.extend(
        tiles
            .iter()
            .take(interior)
            .map(|&(tx, tz)| (tile_center_octimeters(tx), tile_center_octimeters(tz))),
    );
    pts.push(dest);

    let mut path = VecDeque::new();
    let mut anchor = pts[0];
    for i in 1..pts.len() - 1 {
        // Keep pts[i] only when the point past it is occluded from the anchor —
        // then it's a real corner. Otherwise the anchor can see straight past
        // it, so drop it.
        if !los(map, anchor, pts[i + 1]) {
            path.push_back(pts[i]);
            anchor = pts[i];
        }
    }
    path.push_back(dest);
    path
}

/// Whether the straight segment between two octimeter points crosses only
/// walkable tiles — an integer grid traversal (Amanatides–Woo) over the
/// 1-tile interaction grid. Steps from boundary to boundary, comparing the
/// two axes' distances by cross-multiplication so it stays integer-only and
/// deterministic. Diagonal corner crossings are allowed (only the entered
/// tile is checked), matching `astar`'s 8-connected moves.
fn los(map: &TileMap, a: (i32, i32), b: (i32, i32)) -> bool {
    let (mut x, mut z) = (a.0 >> TILE_BITS, a.1 >> TILE_BITS);
    let (xe, ze) = (b.0 >> TILE_BITS, b.1 >> TILE_BITS);
    if !map.walkable(x, z) {
        return false;
    }
    let (step_x, step_z) = ((b.0 - a.0).signum(), (b.1 - a.1).signum());
    let adx = i64::from((b.0 - a.0).abs());
    let adz = i64::from((b.1 - a.1).abs());
    // Octimeters from the start point to the next tile boundary on each axis;
    // each crossing then advances that axis's accumulator by one whole tile.
    let mut cx = match step_x {
        1 => i64::from(((x + 1) << TILE_BITS) - a.0),
        -1 => i64::from(a.0 - (x << TILE_BITS)),
        _ => 0,
    };
    let mut cz = match step_z {
        1 => i64::from(((z + 1) << TILE_BITS) - a.1),
        -1 => i64::from(a.1 - (z << TILE_BITS)),
        _ => 0,
    };
    let tile = i64::from(OCTIMETERS_PER_TILE);
    while x != xe || z != ze {
        // Step the axis whose next boundary is nearer (t = c / ad, compared as
        // cx·adz vs cz·adx); on an exact tie cross the corner diagonally. An
        // axis already at its end never steps.
        let (take_x, take_z) = if x == xe {
            (false, true)
        } else if z == ze {
            (true, false)
        } else {
            match (cx * adz).cmp(&(cz * adx)) {
                Ordering::Less => (true, false),
                Ordering::Greater => (false, true),
                Ordering::Equal => (true, true),
            }
        };
        if take_x {
            x += step_x;
            cx += tile;
        }
        if take_z {
            z += step_z;
            cz += tile;
        }
        if !map.walkable(x, z) {
            return false;
        }
    }
    true
}

/// Advance a point `speed` octimeters *along the straight line to* `target` —
/// the same Euclidean distance per tick in every direction (so a diagonal
/// doesn't run √2 faster than a cardinal). Each axis moves its share of the
/// step scaled by the true direction `(dx, dz) / |(dx, dz)|`, rounded to the
/// nearest octimeter, and the move snaps exactly onto `target` once within one
/// step. Integer-only via `isqrt` and recomputed from the live delta each
/// tick, so it stays deterministic and rounding never accumulates.
#[allow(clippy::cast_possible_truncation)]
fn step_toward(cur: (i32, i32), target: (i32, i32), speed: i32) -> (i32, i32) {
    let dx = i64::from(target.0 - cur.0);
    let dz = i64::from(target.1 - cur.1);
    let dist = (dx * dx + dz * dz).isqrt();
    let speed = i64::from(speed);
    if dist <= speed {
        return target;
    }
    // Round speed·d / dist to nearest, away from zero on a tie.
    let round_div = |num: i64| {
        let half = dist / 2;
        if num >= 0 {
            (num + half) / dist
        } else {
            (num - half) / dist
        }
    };
    // |speed·d / dist| ≤ speed, so the result fits an i32 axis step.
    (
        cur.0 + round_div(speed * dx) as i32,
        cur.1 + round_div(speed * dz) as i32,
    )
}

/// Move `cur` toward `target` by at most `step` octimeters, never
/// overshooting.
fn approach(cur: i32, target: i32, step: i32) -> i32 {
    match cur.cmp(&target) {
        Ordering::Less => (cur + step).min(target),
        Ordering::Greater => (cur - step).max(target),
        Ordering::Equal => cur,
    }
}

/// Advance the camera orbit one tick. `yaw_dir` / `pitch_dir` are the held
/// direction signs (`-1`, `0`, `+1`); yaw wraps freely while pitch stays
/// clamped to a slice above the horizon so the view never dips below it or
/// reaches the degenerate straight-overhead pose.
fn step_camera(yaw: f32, pitch: f32, yaw_dir: f32, pitch_dir: f32) -> (f32, f32) {
    let yaw = yaw_dir.mul_add(CAMERA_YAW_SPEED, yaw).rem_euclid(TAU);
    let pitch = pitch_dir
        .mul_add(CAMERA_PITCH_SPEED, pitch)
        .clamp(CAMERA_PITCH_MIN, CAMERA_PITCH_MAX);
    (yaw, pitch)
}

/// Intensity at `clock`/`span` of the way through a level's `lo`..`hi` ramp.
#[allow(clippy::cast_possible_wrap)] // clock/span are small (<= LEVEL_TICKS)
fn lerp_level(lo: i32, hi: i32, clock: u32, span: u32) -> i32 {
    lo + (hi - lo) * clock as i32 / span as i32
}

/// Fixed top-down camera for the preview matrix: looks straight down at the
/// grid center from high enough that the whole 16×16 floor frames with a
/// margin. Returns `(eye, target, up)`; `up = -Z` puts grid `+Z` toward the
/// bottom of frame, so matrix rows read top-to-bottom.
fn preview_camera() -> (Vec3, Vec3, Vec3) {
    let cx = GRID_W as f32 / 2.0;
    let cz = GRID_H as f32 / 2.0;
    let eye = Vec3::new(cx, 20.0, cz);
    let target = Vec3::new(cx, 0.0, cz);
    (eye, target, Vec3::new(0.0, 0.0, -1.0))
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

/// Intersect the ray `eye + t·dir` (`t ≥ 0`) with the ground plane `y = 0`,
/// returning the world `(x, z)` of the hit, or `None` when the ray points away
/// from the ground (so it never crosses it in front of the eye, which sits
/// above the plane).
fn intersect_ground(eye: Vec3, dir: Vec3) -> Option<(f32, f32)> {
    if dir.y >= 0.0 {
        return None;
    }
    let t = -eye.y / dir.y;
    Some((dir.x.mul_add(t, eye.x), dir.z.mul_add(t, eye.z)))
}

/// Append a shaded capsule (a capped cylinder) standing on the ground at
/// `(cx, cz)`, in metres, tinted `base`. Built as a stack of horizontal rings
/// from the bottom pole to the top pole — two hemisphere caps of
/// [`PLAYER_RADIUS`] joined by a cylinder — each pair of rings bridged by a
/// band of triangles. Per-vertex normals carry a Lambert shade against
/// [`LIGHT_DIR`] baked into the colour, so the form reads as solid 3D under a
/// pipeline that has no lighting of its own.
fn push_capsule(out: &mut Vec<DrawTriangle>, cx: f32, cz: f32, base: (f32, f32, f32)) {
    /// Vertices around each ring.
    const RADIAL: usize = 16;
    /// Rings per hemisphere cap (pole to equator inclusive of the equator).
    const CAP_RINGS: usize = 6;

    let radius = PLAYER_RADIUS;
    let cylinder_height = 2.0f32.mul_add(-radius, PLAYER_HEIGHT);
    let bottom_center = radius;
    let top_center = radius + cylinder_height;

    // Each ring is `RADIAL` (position, normal) pairs. A ring at latitude `phi`
    // (−π/2 at the bottom pole, +π/2 at the top) sits at height `center + r·sin
    // φ` with horizontal radius `r·cos φ`; the normal is the outward direction
    // `(cos φ·cos θ, sin φ, cos φ·sin θ)`, already unit length.
    let ring = |center_y: f32, phi: f32| -> [(Vec3, Vec3); RADIAL] {
        let (s, c) = (phi.sin(), phi.cos());
        let y = radius.mul_add(s, center_y);
        let mut verts = [(Vec3::ZERO, Vec3::ZERO); RADIAL];
        for (j, v) in verts.iter_mut().enumerate() {
            let theta = TAU * j as f32 / RADIAL as f32;
            let (ct, st) = (theta.cos(), theta.sin());
            let normal = Vec3::new(c * ct, s, c * st);
            let rc = radius * c;
            let pos = Vec3::new(rc.mul_add(ct, cx), y, rc.mul_add(st, cz));
            *v = (pos, normal);
        }
        verts
    };

    // Bottom cap (pole → equator) then top cap (equator → pole). The bottom
    // equator and the top equator bound the cylinder body, so the band between
    // them is the cylinder wall.
    let mut rings: Vec<[(Vec3, Vec3); RADIAL]> = Vec::with_capacity(2 * CAP_RINGS);
    for i in 0..CAP_RINGS {
        let phi = -FRAC_PI_2 * (1.0 - i as f32 / (CAP_RINGS - 1) as f32);
        rings.push(ring(bottom_center, phi));
    }
    for i in 0..CAP_RINGS {
        let phi = FRAC_PI_2 * (i as f32 / (CAP_RINGS - 1) as f32);
        rings.push(ring(top_center, phi));
    }

    let to_light = (LIGHT_DIR * -1.0).normalize();
    let shade = |normal: Vec3| {
        let lambert = normal.dot(to_light).max(0.0);
        let f = 0.65f32.mul_add(lambert, 0.35);
        (base.0 * f, base.1 * f, base.2 * f)
    };
    let vert = |p: Vec3, rgb: (f32, f32, f32)| Vertex {
        x: p.x,
        y: p.y,
        z: p.z,
        r: rgb.0,
        g: rgb.1,
        b: rgb.2,
    };

    for band in 0..rings.len() - 1 {
        let (lo, hi) = (rings[band], rings[band + 1]);
        for j in 0..RADIAL {
            let k = (j + 1) % RADIAL;
            let (l0, hi0, l1, hi1) = (lo[j], hi[j], lo[k], hi[k]);
            out.push(DrawTriangle {
                verts: [
                    vert(l0.0, shade(l0.1)),
                    vert(hi0.0, shade(hi0.1)),
                    vert(hi1.0, shade(hi1.1)),
                ],
            });
            out.push(DrawTriangle {
                verts: [
                    vert(l0.0, shade(l0.1)),
                    vert(hi1.0, shade(hi1.1)),
                    vert(l1.0, shade(l1.1)),
                ],
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_ground_hits_below_and_misses_above() {
        // Straight down from 5 m up lands at the eye's ground footprint.
        let hit = intersect_ground(Vec3::new(2.0, 5.0, 3.0), Vec3::new(0.0, -1.0, 0.0));
        assert_eq!(hit, Some((2.0, 3.0)));
        // A 45° downward ray travels one metre out per metre of drop.
        let (hx, hz) = intersect_ground(Vec3::new(0.0, 4.0, 0.0), Vec3::new(0.0, -1.0, -1.0))
            .expect("downward ray hits the ground");
        assert!(
            hx.abs() < 1e-4 && (hz + 4.0).abs() < 1e-4,
            "got ({hx}, {hz})"
        );
        // A ray angled upward never reaches the ground ahead of the eye.
        assert!(intersect_ground(Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.0, 0.5, -1.0)).is_none());
    }

    #[test]
    fn camera_orbit_clamps_pitch_above_the_horizon_and_wraps_yaw() {
        // Holding "down" forever floors pitch at the above-horizon minimum;
        // holding "up" ceils it below the straight-overhead maximum — the view
        // can never dip below the horizon or hit the degenerate top-down pose.
        let (mut yaw, mut pitch) = (0.0, CAMERA_PITCH);
        for _ in 0..10_000 {
            (yaw, pitch) = step_camera(yaw, pitch, 0.0, -1.0);
        }
        assert!(
            (pitch - CAMERA_PITCH_MIN).abs() < 1e-5,
            "pitch floored: {pitch}"
        );
        for _ in 0..10_000 {
            (yaw, pitch) = step_camera(yaw, pitch, 0.0, 1.0);
        }
        assert!(
            (pitch - CAMERA_PITCH_MAX).abs() < 1e-5,
            "pitch ceiled: {pitch}"
        );
        // Yaw wraps into [0, τ) rather than growing without bound.
        for _ in 0..10_000 {
            (yaw, pitch) = step_camera(yaw, pitch, 1.0, 0.0);
        }
        assert!((0.0..TAU).contains(&yaw), "yaw stayed wrapped: {yaw}");
    }

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

    /// A single wall column at x=6, z=4..9 — a focused scenario for the
    /// pathfinding tests, independent of the demo maze.
    fn wall_column() -> TileMap {
        TileMap::from_blocked(&[(6, 4), (6, 5), (6, 6), (6, 7), (6, 8), (6, 9)])
    }

    #[test]
    fn astar_routes_around_the_wall() {
        // A path from the right of a wall to the left must detour around —
        // never through it — and stay a contiguous 8-connected walk ending on
        // the goal.
        let map = wall_column();
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
        assert!(astar(&wall_column(), (8, 8), (6, 6)).is_none());
    }

    #[test]
    fn maze_is_fully_connected_from_spawn() {
        // The demo maze must have no walled-off pockets: every open tile is
        // reachable from the spawn room by cardinal steps (so click-to-move
        // can always find a route). Guards against a doorway typo sealing a
        // ring.
        let map = TileMap::new();
        let spawn = (GRID_W / 2, GRID_H / 2);
        assert!(map.walkable(spawn.0, spawn.1), "spawn tile must be open");
        let mut seen = [false; GRID_TILES];
        let mut queue = VecDeque::from([spawn]);
        seen[TileMap::idx(spawn.0, spawn.1)] = true;
        let mut reached = 0;
        while let Some((x, z)) = queue.pop_front() {
            reached += 1;
            for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let (nx, nz) = (x + dx, z + dz);
                if map.walkable(nx, nz) && !seen[TileMap::idx(nx, nz)] {
                    seen[TileMap::idx(nx, nz)] = true;
                    queue.push_back((nx, nz));
                }
            }
        }
        let open = (0..GRID_W)
            .flat_map(|x| (0..GRID_H).map(move |z| (x, z)))
            .filter(|&(x, z)| map.walkable(x, z))
            .count();
        assert_eq!(reached, open, "maze has an unreachable open region");
    }

    #[test]
    fn smooth_path_collapses_open_space_to_a_straight_line() {
        // With nothing between start and dest, line-of-sight smoothing drops
        // every interior tile center: the mover walks one straight segment to
        // the sub-tile dest, with no kink through a center.
        let map = TileMap::from_blocked(&[]);
        let tiles: VecDeque<(i32, i32)> = [(9, 8), (10, 8), (11, 8), (12, 8), (13, 8)].into();
        let start = (tile_center_octimeters(8), tile_center_octimeters(8));
        let dest = (13 * OCTIMETERS_PER_TILE + 100, 8 * OCTIMETERS_PER_TILE + 80);
        assert_eq!(
            smooth_path(&map, start, &tiles, dest),
            VecDeque::from([dest])
        );
    }

    #[test]
    fn smooth_path_keeps_a_corner_around_the_wall() {
        // When the wall genuinely sits between start and dest, smoothing keeps
        // the corner(s) it must, and every segment the mover walks stays
        // wall-clear.
        let map = wall_column();
        let (start_tile, goal_tile) = ((9, 6), (2, 6));
        let start = (tile_center_octimeters(9), tile_center_octimeters(6));
        let dest = (tile_center_octimeters(2), tile_center_octimeters(6));
        assert!(!los(&map, start, dest), "direct line should cross the wall");
        let tiles = astar(&map, start_tile, goal_tile).expect("reachable");
        let path = smooth_path(&map, start, &tiles, dest);
        assert!(path.len() >= 2, "a detour must retain at least one corner");
        assert_eq!(path.back().copied(), Some(dest));
        let mut anchor = start;
        for &wp in &path {
            assert!(
                los(&map, anchor, wp),
                "segment {anchor:?} -> {wp:?} crosses a wall"
            );
            anchor = wp;
        }
    }

    #[test]
    fn los_is_clear_in_the_open_and_blocked_through_the_wall() {
        let map = wall_column();
        let center = |tx, tz| (tile_center_octimeters(tx), tile_center_octimeters(tz));
        // Open row east of the wall.
        assert!(los(&map, center(8, 8), center(13, 8)));
        // Straight across the wall — tile (6, 6) is blocked.
        assert!(!los(&map, center(9, 6), center(2, 6)));
    }

    #[test]
    fn astar_keeps_a_straight_line_straight() {
        // A horizontal target in open space must be a pure eastward run —
        // octile cost forbids the equal-length diagonal zigzag a uniform cost
        // would allow.
        let path = astar(&TileMap::from_blocked(&[]), (8, 8), (13, 8)).expect("reachable");
        let expected: VecDeque<(i32, i32)> = [(9, 8), (10, 8), (11, 8), (12, 8), (13, 8)].into();
        assert_eq!(path, expected);
    }

    #[test]
    fn step_toward_tracks_the_straight_line() {
        // A shallow sub-tile segment (slope ≠ 0, 1, ∞) must be followed as a
        // straight line, not an axis-by-axis L. Walk it tick by tick and
        // assert every intermediate point stays hard against the ideal line
        // from start to target — and that it lands exactly on the target.
        let start = (8 * OCTIMETERS_PER_TILE + 128, 8 * OCTIMETERS_PER_TILE + 128);
        let target = (5 * OCTIMETERS_PER_TILE + 64, 11 * OCTIMETERS_PER_TILE);
        let (dx, dz) = (i64::from(target.0 - start.0), i64::from(target.1 - start.1));
        let len = ((dx * dx + dz * dz) as f64).sqrt();
        let mut p = start;
        for _ in 0..10_000 {
            if p == target {
                break;
            }
            p = step_toward(p, target, SPEED_OCTIMETERS_PER_TICK);
            // Perpendicular distance of p from the line start→target.
            let (px, pz) = (i64::from(p.0 - start.0), i64::from(p.1 - start.1));
            let perp = (dx * pz - dz * px).abs() as f64 / len;
            // Hugs the line within an eighth of a tile (≈ 12 cm) — an order of
            // magnitude tighter than the axis-by-axis L this replaces, which
            // peels off by the segment's whole minor extent (here 640).
            assert!(
                perp <= f64::from(OCTIMETERS_PER_TILE / 8),
                "strayed {perp} octimeters from the line at {p:?}"
            );
        }
        assert_eq!(p, target, "did not converge onto the target");
    }

    #[test]
    fn step_toward_speed_is_uniform_across_directions() {
        // A diagonal step covers the same ground per tick as a cardinal one —
        // no √2 speed-up. The cardinal move takes the full speed on one axis;
        // the 45° diagonal splits it so the Euclidean distance still ≈ speed.
        let speed = SPEED_OCTIMETERS_PER_TICK;
        let origin = (1000, 1000);

        let cardinal = step_toward(origin, (1000 + 320, 1000), speed);
        assert_eq!(cardinal, (1000 + speed, 1000), "cardinal moves full speed");

        let diagonal = step_toward(origin, (1000 - 320, 1000 + 320), speed);
        let (mx, mz) = (diagonal.0 - 1000, diagonal.1 - 1000);
        assert_eq!(-mx, mz, "the 45° split is symmetric across the axes");
        let moved = f64::from(mx * mx + mz * mz).sqrt();
        assert!(
            (moved - f64::from(speed)).abs() <= 1.0,
            "diagonal distance {moved} should be ≈ {speed}, not {speed}·√2"
        );
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
