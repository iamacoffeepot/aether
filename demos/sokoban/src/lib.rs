//! Sokoban demo: a grid-based puzzle world. The world owns walls,
//! boxes, targets, and the player's grid position; an external player
//! component (`aether-player-component` in tile-step mode, loaded as
//! `"player"`) renders itself and drives motion.
//!
//! Protocol:
//!
//! 1. On `SokobanLoadLevel` / `SokobanReset`, sokoban parses the level,
//!    initializes the grid, stores the player's starting cell, and
//!    emits `PlayerSetPosition` to the `"player"` mailbox to place the
//!    external body at the starting world coordinate.
//! 2. The player (in tile-step mode) emits
//!    `PlayerRequestStep { dx, dy }` on each movement keypress,
//!    addressed to the mailbox named `"world"` — i.e. this component.
//! 3. Sokoban resolves the outcome (wall block, floor pass, box
//!    push-or-block) and replies with `PlayerStepResult`, carrying the
//!    authoritative world-space position. The player applies it.
//!
//! The world is rendered each tick (floor, walls, boxes, targets).
//! The external player renders itself — the grid no longer tracks
//! `CELL_PLAYER` state.
//!
//! Grid is still capped at 16×16 (pre-ADR-0028 carryover).

use aether_component::{Component, Ctx, InitCtx, KindId, Sink, handlers};
use aether_kinds::{
    DrawTriangle, PlayerRequestStep, PlayerSetPosition, PlayerStepResult, Tick, Vertex,
};
use aether_mail::{Kind, Schema};
use bytemuck::{Pod, Zeroable};

pub const GRID_MAX: usize = 16;
pub const CELLS_MAX: usize = GRID_MAX * GRID_MAX;

pub const CELL_FLOOR: u8 = 0;
pub const CELL_WALL: u8 = 1;
pub const CELL_BOX: u8 = 2;
pub const CELL_TARGET: u8 = 3;
pub const CELL_BOX_ON_TARGET: u8 = 4;

/// Claude → component: reload the currently-active level. No payload.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Pod, Zeroable, Kind, Schema)]
#[kind(name = "demo.sokoban.reset")]
pub struct SokobanReset;

/// Claude → component: swap to a different built-in level by index.
/// Out-of-range ids are treated as no-ops (state reply still fires).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Pod, Zeroable, Kind, Schema)]
#[kind(name = "demo.sokoban.load_level")]
pub struct SokobanLoadLevel {
    pub id: u32,
}

/// Component → Claude (reply-to-sender): full board snapshot. Always
/// the same wire size regardless of the live grid dimensions — unused
/// cells in `cells` are `CELL_FLOOR` (0). Consumers read `width` and
/// `height` before indexing. `player_x` / `player_y` are grid cell
/// coordinates (not world).
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable, Kind, Schema)]
#[kind(name = "demo.sokoban.state")]
pub struct SokobanState {
    pub width: u32,
    pub height: u32,
    pub player_x: u32,
    pub player_y: u32,
    pub moves: u32,
    /// `1` when every target cell holds a box, `0` otherwise.
    pub solved: u32,
    /// Active level id (matches the last `SokobanLoadLevel.id`).
    pub level_id: u32,
    pub cells: [u8; CELLS_MAX],
}

impl Default for SokobanState {
    fn default() -> Self {
        SokobanState {
            width: 0,
            height: 0,
            player_x: 0,
            player_y: 0,
            moves: 0,
            solved: 0,
            level_id: 0,
            cells: [CELL_FLOOR; CELLS_MAX],
        }
    }
}

/// Hand-authored starter levels. ASCII: `#` wall, `.` floor,
/// `@` player start, `$` box, `T` target, `*` box on target,
/// `+` player start on target. The levels are deliberately small —
/// they exist to be played, not to be fun.
const LEVELS: &[&[&str]] = &[
    // 0: trivial — one box, one target, one push.
    &[
        "#####", //
        "#@$T#", //
        "#####",
    ],
    // 1: small — two boxes, needs a short detour.
    &[
        "#######", //
        "#.T...#", //
        "#.$...#", //
        "#.@.$T#", //
        "#######",
    ],
    // 2: small planning — push order matters or you corner a box.
    &[
        "########", //
        "#..T...#", //
        "#.$$...#", //
        "#.@..T.#", //
        "########",
    ],
];

pub struct Sokoban {
    state: SokobanState,
    state_kind: KindId<SokobanState>,
    step_result_kind: KindId<PlayerStepResult>,
    render: Sink<DrawTriangle>,
    player: Sink<PlayerSetPosition>,
}

/// Sokoban world. Owns the grid and the player's grid cell; the
/// external `aether-player-component` (loaded as `"player"`, mode set
/// to tile-step) drives motion via `PlayerRequestStep`.
///
/// # Agent
/// Load as `"world"` alongside the external player (`"player"`) and
/// the multi-camera component (`"camera"`). The camera should have a
/// topdown-mode camera named `"main"` (the bootstrap default is
/// orbit, so send `aether.camera.set_mode { name: "main", mode:
/// Topdown(..) }` after load). On load, sokoban emits
/// `PlayerSetPosition` to the external player so it arrives at the
/// level's starting cell.
///
/// - `SokobanLoadLevel { id }` — switch levels. Out-of-range is a
///   no-op.
/// - `SokobanReset` — reload the active level.
/// - `PlayerRequestStep { dx, dy }` — typically sent by the player on
///   each WASD press; you can also send it directly for scripted
///   moves. `dx`/`dy` are integer cell deltas (+1 east, +1 north in
///   the engine's +Y-up world). Reply is `PlayerStepResult` with the
///   authoritative post-move world coordinate.
#[handlers]
impl Component for Sokoban {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        let mut me = Sokoban {
            state: SokobanState::default(),
            state_kind: ctx.resolve::<SokobanState>(),
            step_result_kind: ctx.resolve::<PlayerStepResult>(),
            render: ctx.resolve_sink::<DrawTriangle>("aether.sink.render"),
            player: ctx.resolve_sink::<PlayerSetPosition>("player"),
        };
        me.load_level(0);
        me
    }

    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        self.render_grid(ctx);
    }

    /// Apply a player step request. Resolves wall/floor/box outcomes
    /// against the current grid, updates state, and replies to the
    /// sender with the authoritative new world position.
    #[handler]
    fn on_request_step(&mut self, ctx: &mut Ctx<'_>, req: PlayerRequestStep) {
        let accepted = self.apply_step(req.dx, req.dy);
        let (world_x, world_y) = self.player_world_pos();
        if let Some(sender) = ctx.reply_to() {
            ctx.reply(
                sender,
                self.step_result_kind,
                &PlayerStepResult {
                    accepted: u32::from(accepted),
                    new_x: world_x,
                    new_y: world_y,
                },
            );
        }
    }

    #[handler]
    fn on_reset(&mut self, ctx: &mut Ctx<'_>, _rst: SokobanReset) {
        self.load_level(self.state.level_id);
        self.sync_player(ctx);
        self.reply_state(ctx);
    }

    #[handler]
    fn on_load_level(&mut self, ctx: &mut Ctx<'_>, load: SokobanLoadLevel) {
        self.load_level(load.id);
        self.sync_player(ctx);
        self.reply_state(ctx);
    }
}

impl Sokoban {
    fn load_level(&mut self, id: u32) {
        let Some(level) = LEVELS.get(id as usize) else {
            // Out-of-range: keep current state but record the attempt.
            return;
        };
        let height = level.len();
        let width = level.iter().map(|row| row.len()).max().unwrap_or(0);
        if width > GRID_MAX || height > GRID_MAX {
            return;
        }

        let mut cells = [CELL_FLOOR; CELLS_MAX];
        let mut player = (0u32, 0u32);
        for (y, row) in level.iter().enumerate() {
            for (x, ch) in row.chars().enumerate() {
                let cell = match ch {
                    '#' => CELL_WALL,
                    '.' => CELL_FLOOR,
                    '$' => CELL_BOX,
                    'T' => CELL_TARGET,
                    '*' => CELL_BOX_ON_TARGET,
                    '@' => {
                        player = (x as u32, y as u32);
                        CELL_FLOOR
                    }
                    '+' => {
                        player = (x as u32, y as u32);
                        CELL_TARGET
                    }
                    _ => CELL_FLOOR,
                };
                cells[y * GRID_MAX + x] = cell;
            }
        }

        self.state = SokobanState {
            width: width as u32,
            height: height as u32,
            player_x: player.0,
            player_y: player.1,
            moves: 0,
            solved: 0,
            level_id: id,
            cells,
        };
        self.state.solved = u32::from(is_solved(&self.state));
    }

    /// Resolve a step request against the grid. `(dx, dy)` follows the
    /// engine's world convention: +X east, +Y north. Sokoban's grid
    /// stores rows top-down (gy=0 is top), so +Y north means gy - 1.
    /// Returns `true` when the player moved; `false` when the step
    /// was rejected (wall, out of bounds, unpushable box, post-solve,
    /// diagonal or invalid delta).
    fn apply_step(&mut self, dx: i32, dy: i32) -> bool {
        if self.state.solved == 1 {
            return false;
        }
        let (delta_gx, delta_gy) = match (dx.signum(), dy.signum()) {
            (1, 0) => (1i32, 0i32),
            (-1, 0) => (-1, 0),
            (0, 1) => (0, -1), // world +Y → grid -gy
            (0, -1) => (0, 1),
            _ => return false,
        };
        let px = self.state.player_x as i32;
        let py = self.state.player_y as i32;
        let tx = px + delta_gx;
        let ty = py + delta_gy;
        if !in_bounds(&self.state, tx, ty) {
            return false;
        }

        let target = cell_at(&self.state, tx as u32, ty as u32);
        match target {
            CELL_WALL => return false,
            CELL_FLOOR | CELL_TARGET => {}
            CELL_BOX | CELL_BOX_ON_TARGET => {
                let bx = tx + delta_gx;
                let by = ty + delta_gy;
                if !in_bounds(&self.state, bx, by) {
                    return false;
                }
                let beyond = cell_at(&self.state, bx as u32, by as u32);
                let box_after = match beyond {
                    CELL_FLOOR => CELL_BOX,
                    CELL_TARGET => CELL_BOX_ON_TARGET,
                    _ => return false,
                };
                set_cell(&mut self.state, bx as u32, by as u32, box_after);
                // Vacate the box's old cell: if it was a box-on-target,
                // the underlying target is re-exposed as CELL_TARGET.
                let vacated = if target == CELL_BOX_ON_TARGET {
                    CELL_TARGET
                } else {
                    CELL_FLOOR
                };
                set_cell(&mut self.state, tx as u32, ty as u32, vacated);
            }
            _ => return false,
        }

        self.state.player_x = tx as u32;
        self.state.player_y = ty as u32;
        self.state.moves += 1;
        self.state.solved = u32::from(is_solved(&self.state));
        true
    }

    /// Grid-space player cell → world-space coordinate. The rendering
    /// mapping: one world unit per cell, centered on origin, +Y up.
    /// Tile `(gx, gy)` has center at `(gx - w/2 + 0.5, h/2 - gy - 0.5)`.
    fn player_world_pos(&self) -> (f32, f32) {
        let w = self.state.width as f32;
        let h = self.state.height as f32;
        let gx = self.state.player_x as f32;
        let gy = self.state.player_y as f32;
        (gx - w * 0.5 + 0.5, h * 0.5 - gy - 0.5)
    }

    /// Emit a `PlayerSetPosition` to the external player with the
    /// current player cell's world coordinate. Called after load /
    /// reset so the rendered body snaps to the level's starting cell.
    fn sync_player(&self, ctx: &mut Ctx<'_>) {
        let (x, y) = self.player_world_pos();
        ctx.send(&self.player, &PlayerSetPosition { x, y });
    }

    fn reply_state(&self, ctx: &mut Ctx<'_>) {
        let Some(sender) = ctx.reply_to() else {
            return;
        };
        ctx.reply(sender, self.state_kind, &self.state);
    }

    fn render_grid(&self, ctx: &mut Ctx<'_>) {
        let w = self.state.width as usize;
        let h = self.state.height as usize;
        if w == 0 || h == 0 {
            return;
        }
        let cell = 1.0_f32;
        let origin_x = -(w as f32) * 0.5;
        let origin_y = (h as f32) * 0.5;

        let mut tris = [DrawTriangle::default(); CELLS_MAX * 2];
        let mut n = 0;
        for y in 0..h {
            for x in 0..w {
                let kind = cell_at(&self.state, x as u32, y as u32);
                let (r, g, b) = cell_color(kind);
                let x0 = origin_x + x as f32 * cell;
                let x1 = x0 + cell;
                let y0 = origin_y - y as f32 * cell;
                let y1 = y0 - cell;
                // Two triangles per quad (tl, tr, br) and (tl, br, bl).
                tris[n] = DrawTriangle {
                    verts: [
                        Vertex {
                            x: x0,
                            y: y0,
                            z: 0.0,
                            r,
                            g,
                            b,
                        },
                        Vertex {
                            x: x1,
                            y: y0,
                            z: 0.0,
                            r,
                            g,
                            b,
                        },
                        Vertex {
                            x: x1,
                            y: y1,
                            z: 0.0,
                            r,
                            g,
                            b,
                        },
                    ],
                };
                tris[n + 1] = DrawTriangle {
                    verts: [
                        Vertex {
                            x: x0,
                            y: y0,
                            z: 0.0,
                            r,
                            g,
                            b,
                        },
                        Vertex {
                            x: x1,
                            y: y1,
                            z: 0.0,
                            r,
                            g,
                            b,
                        },
                        Vertex {
                            x: x0,
                            y: y1,
                            z: 0.0,
                            r,
                            g,
                            b,
                        },
                    ],
                };
                n += 2;
            }
        }
        ctx.send_many(&self.render, &tris[..n]);
    }
}

fn in_bounds(state: &SokobanState, x: i32, y: i32) -> bool {
    x >= 0 && y >= 0 && (x as u32) < state.width && (y as u32) < state.height
}

fn cell_at(state: &SokobanState, x: u32, y: u32) -> u8 {
    state.cells[(y as usize) * GRID_MAX + (x as usize)]
}

fn set_cell(state: &mut SokobanState, x: u32, y: u32, value: u8) {
    state.cells[(y as usize) * GRID_MAX + (x as usize)] = value;
}

fn is_solved(state: &SokobanState) -> bool {
    // Solved ⇔ no uncovered target cells.
    for y in 0..state.height as usize {
        for x in 0..state.width as usize {
            let c = state.cells[y * GRID_MAX + x];
            if c == CELL_TARGET {
                return false;
            }
        }
    }
    true
}

fn cell_color(cell: u8) -> (f32, f32, f32) {
    match cell {
        CELL_WALL => (0.08, 0.08, 0.12),
        CELL_FLOOR => (0.18, 0.18, 0.22),
        CELL_TARGET => (0.35, 0.22, 0.10),
        CELL_BOX => (0.65, 0.50, 0.28),
        CELL_BOX_ON_TARGET => (0.30, 0.70, 0.35),
        _ => (0.0, 0.0, 0.0),
    }
}

aether_component::export!(Sokoban);
