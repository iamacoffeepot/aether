//! Sokoban demo: a turn-based puzzle component that exists to stress
//! the Claude-in-harness loop. A Claude session drives gameplay via
//! mail — `SokobanMove`, `SokobanReset`, `SokobanLoadLevel` — and the
//! component replies with a `SokobanState` snapshot after every
//! action. The grid is also rendered each tick as `DrawTriangle`s so
//! `capture_frame` shows a human-readable board.
//!
//! Scope: this is a harness exercise, not a game. No undo, no replay,
//! no move history — just enough state to play a level, observe, and
//! reset. Soft-lock detection is deliberately absent; seeing Claude
//! recognise and recover from unwinnable states is part of the point.
//!
//! Grid is capped at 16×16 as a carryover from the pre-ADR-0028
//! `LoadKind` wire shape, which only supported fixed-size scalar
//! and array fields. Now that the kind vocabulary rides in the
//! wasm's `aether.kinds` custom section (ADR-0028), the full
//! `SchemaType` vocabulary is available — including `Vec<u8>` —
//! and this cap can be lifted whenever we want to extend the
//! demo. Left as-is for now so this PR stays focused on the wire
//! removal.

use aether_component::{Component, Ctx, InitCtx, KindId, Sink, handlers};
use aether_kinds::{DrawTriangle, Tick, Vertex};
use aether_mail::{Kind, Schema};
use bytemuck::{Pod, Zeroable};

pub const GRID_MAX: usize = 16;
pub const CELLS_MAX: usize = GRID_MAX * GRID_MAX;

pub const CELL_FLOOR: u8 = 0;
pub const CELL_WALL: u8 = 1;
pub const CELL_BOX: u8 = 2;
pub const CELL_TARGET: u8 = 3;
pub const CELL_BOX_ON_TARGET: u8 = 4;
pub const CELL_PLAYER: u8 = 5;
pub const CELL_PLAYER_ON_TARGET: u8 = 6;

pub const DIR_NORTH: u8 = 0;
pub const DIR_SOUTH: u8 = 1;
pub const DIR_EAST: u8 = 2;
pub const DIR_WEST: u8 = 3;

/// Claude → component: move the player one cell in the given direction.
/// `direction`: 0 = north, 1 = south, 2 = east, 3 = west. Illegal
/// directions or blocked moves are no-ops — the component still
/// replies with a fresh state so the caller always sees ground truth.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Pod, Zeroable, Kind, Schema)]
#[kind(name = "demo.sokoban.move")]
pub struct SokobanMove {
    pub direction: u8,
    pub _pad: [u8; 3],
}

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
/// `height` before indexing.
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
/// `@` player, `$` box, `.` target-less floor, `T` target,
/// `*` box on target, `+` player on target. Lines must be rectangular
/// (shorter lines are padded with floor). The levels are deliberately
/// small — they exist to be played by Claude, not to be fun.
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
    render: Sink<DrawTriangle>,
}

#[handlers]
impl Component for Sokoban {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        let mut me = Sokoban {
            state: SokobanState::default(),
            state_kind: ctx.resolve::<SokobanState>(),
            render: ctx.resolve_sink::<DrawTriangle>("render"),
        };
        me.load_level(0);
        me
    }

    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        self.render_grid(ctx);
    }

    #[handler]
    fn on_move(&mut self, ctx: &mut Ctx<'_>, mv: SokobanMove) {
        self.apply_move(mv.direction);
        self.reply_state(ctx);
    }

    #[handler]
    fn on_reset(&mut self, ctx: &mut Ctx<'_>, _rst: SokobanReset) {
        self.load_level(self.state.level_id);
        self.reply_state(ctx);
    }

    #[handler]
    fn on_load_level(&mut self, ctx: &mut Ctx<'_>, load: SokobanLoadLevel) {
        self.load_level(load.id);
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
                        CELL_PLAYER
                    }
                    '+' => {
                        player = (x as u32, y as u32);
                        CELL_PLAYER_ON_TARGET
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

    fn apply_move(&mut self, direction: u8) {
        if self.state.solved == 1 {
            return;
        }
        let (dx, dy): (i32, i32) = match direction {
            DIR_NORTH => (0, -1),
            DIR_SOUTH => (0, 1),
            DIR_EAST => (1, 0),
            DIR_WEST => (-1, 0),
            _ => return,
        };
        let px = self.state.player_x as i32;
        let py = self.state.player_y as i32;
        let tx = px + dx;
        let ty = py + dy;
        if !in_bounds(&self.state, tx, ty) {
            return;
        }

        let target = cell_at(&self.state, tx as u32, ty as u32);
        match target {
            CELL_WALL => return,
            CELL_FLOOR | CELL_TARGET => {
                self.step_player(px as u32, py as u32, tx as u32, ty as u32);
            }
            CELL_BOX | CELL_BOX_ON_TARGET => {
                let bx = tx + dx;
                let by = ty + dy;
                if !in_bounds(&self.state, bx, by) {
                    return;
                }
                let beyond = cell_at(&self.state, bx as u32, by as u32);
                let (box_after, player_into) = match beyond {
                    CELL_FLOOR => (CELL_BOX, (target == CELL_BOX_ON_TARGET)),
                    CELL_TARGET => (CELL_BOX_ON_TARGET, (target == CELL_BOX_ON_TARGET)),
                    _ => return,
                };
                set_cell(&mut self.state, bx as u32, by as u32, box_after);
                // Player's new cell: was a box; if box had been on a
                // target, the underlying target remains → player-on-target.
                let new_player_cell = if player_into {
                    CELL_PLAYER_ON_TARGET
                } else {
                    CELL_PLAYER
                };
                self.move_player_raw(px as u32, py as u32, tx as u32, ty as u32, new_player_cell);
            }
            _ => return,
        }

        self.state.moves += 1;
        self.state.solved = u32::from(is_solved(&self.state));
    }

    fn step_player(&mut self, px: u32, py: u32, tx: u32, ty: u32) {
        let target = cell_at(&self.state, tx, ty);
        let new_player_cell = if target == CELL_TARGET {
            CELL_PLAYER_ON_TARGET
        } else {
            CELL_PLAYER
        };
        self.move_player_raw(px, py, tx, ty, new_player_cell);
    }

    fn move_player_raw(&mut self, px: u32, py: u32, tx: u32, ty: u32, new_cell: u8) {
        let prev = cell_at(&self.state, px, py);
        let vacated = if prev == CELL_PLAYER_ON_TARGET {
            CELL_TARGET
        } else {
            CELL_FLOOR
        };
        set_cell(&mut self.state, px, py, vacated);
        set_cell(&mut self.state, tx, ty, new_cell);
        self.state.player_x = tx;
        self.state.player_y = ty;
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
        // World-space grid: one world unit per cell, centered on the
        // origin with +Y up. Tile `(gx, gy)` (row-major, gy=0 is the
        // top row) covers world rect
        //   x ∈ [gx - w/2, gx - w/2 + 1]
        //   y ∈ [h/2 - gy - 1, h/2 - gy]
        // and sits at z=0. A top-down ortho camera with extent ≥
        // max(w, h)/2 frames the whole grid; the substrate's default
        // identity camera preserves world coords as clip-space, which
        // means pre-camera rendering shrinks from full-window to a
        // unit-sized box on screen — loading the topdown camera is
        // the expected workflow.
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
    // Solved ⇔ no uncovered target cells. Player-on-target counts as
    // uncovered (the player doesn't complete a goal; a box does).
    for y in 0..state.height as usize {
        for x in 0..state.width as usize {
            let c = state.cells[y * GRID_MAX + x];
            if c == CELL_TARGET || c == CELL_PLAYER_ON_TARGET {
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
        CELL_PLAYER => (0.30, 0.55, 0.90),
        CELL_PLAYER_ON_TARGET => (0.70, 0.45, 0.85),
        _ => (0.0, 0.0, 0.0),
    }
}

aether_component::export!(Sokoban);
