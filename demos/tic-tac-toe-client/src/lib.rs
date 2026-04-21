//! Tic-tac-toe client component: renders the board the
//! `aether-demo-tic-tac-toe` server publishes. Registers itself under
//! the well-known mailbox name the server fans state to
//! (`tic_tac_toe.client`), stores every incoming `GameState`, and
//! re-emits the board as colored quads to the `render` sink on each
//! tick.
//!
//! No input handling yet — moves are driven by MCP `send_mail`
//! (`tic_tac_toe.play_move`) against the server component on the
//! same substrate. Mouse-click → row/col is the next step.
//!
//! Rendering:
//! - 3×3 grid of cell quads, slightly inset inside the window.
//! - Cell background: dark gray.
//! - Occupied cells get a smaller inner mark quad — red for X,
//!   blue for O.
//! - Last-moved cell gets a slightly brighter background so it
//!   stands out after a move.
//!
//! That's as shape-y as this first pass gets; proper X / O glyphs
//! are a rendering-polish pass for later.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers};
use aether_demo_tic_tac_toe::{CELL_EMPTY, GameState, LAST_MOVE_NONE, PLAYER_X};
use aether_kinds::{DrawTriangle, Tick, Vertex};

/// Clip-space half-extent the board uses. Board lives inside
/// `[-BOARD_EXTENT, BOARD_EXTENT]²`; the rest of the viewport is the
/// window's clear color.
const BOARD_EXTENT: f32 = 0.9;
/// Gap between cells (clip-space units). Shows as a thin unrendered
/// strip between cells so the grid reads as a grid rather than one
/// big rectangle.
const CELL_GAP: f32 = 0.02;
/// Inner-mark inset as a fraction of cell width. A value of `0.25`
/// means the mark is 50% of cell edge length (25% margin on every
/// side).
const MARK_INSET: f32 = 0.25;

const CELL_BG: (f32, f32, f32) = (0.15, 0.15, 0.18);
/// Last-moved cell tint — same hue family as the base cell, a
/// notch brighter. Subtle on purpose; the primary mark-vs-empty
/// signal is the inner quad.
const CELL_BG_RECENT: (f32, f32, f32) = (0.25, 0.25, 0.30);
/// Brighter border tint used once the game has ended — makes the
/// win/draw state visually obvious without a separate status glyph.
const CELL_BG_GAMEOVER: (f32, f32, f32) = (0.10, 0.22, 0.10);

const MARK_X: (f32, f32, f32) = (0.90, 0.25, 0.25);
const MARK_O: (f32, f32, f32) = (0.25, 0.45, 0.95);

/// Per-component state. Starts with an empty default `GameState` so
/// the first tick can render before the server has sent anything —
/// produces a dark empty grid until the first move lands.
pub struct TicTacToeClient {
    state: GameState,
    render: Sink<DrawTriangle>,
}

impl Default for TicTacToeClient {
    fn default() -> Self {
        Self {
            // `GameState::new_game()` (not `::default()`) — default is
            // zero-init which leaves `last_move_row`/`last_move_col`
            // at 0, and the renderer's "recent move" check would
            // highlight cell (0,0) on an untouched board. `new_game`
            // sets the LAST_MOVE_NONE sentinel (255) the server also
            // uses.
            state: GameState::new_game(),
            render: aether_component::resolve_sink::<DrawTriangle>("render"),
        }
    }
}

/// Tic-tac-toe board renderer. Listens for `tic_tac_toe.game_state`
/// mail addressed to its well-known mailbox (`tic_tac_toe.client`)
/// and reflects whatever state it last received on every render
/// tick.
///
/// # Agent
/// Load this component under the mailbox name `tic_tac_toe.client`
/// so the server's local-observer fan-out lands here. Drive moves
/// against the `tic_tac_toe` server component with
/// `tic_tac_toe.play_move`; the board should update on the next
/// frame. Use `capture_frame` to verify rendering — the 3×3 grid
/// should be visible, occupied cells carry a colored inner square
/// (red X, blue O), and a game-over state tints every cell green.
#[handlers]
impl Component for TicTacToeClient {
    fn init(_ctx: &mut InitCtx<'_>) -> Self {
        TicTacToeClient::default()
    }

    /// Cached copy of the latest authoritative state. Overwrites
    /// unconditionally — the server is the source of truth and we
    /// don't try to merge or validate.
    ///
    /// # Agent
    /// You shouldn't send this mail directly. It arrives from the
    /// `tic_tac_toe` server's `client_observer` fan-out whenever a
    /// move or reset applies.
    #[handler]
    fn on_game_state(&mut self, _ctx: &mut Ctx<'_>, state: GameState) {
        self.state = state;
    }

    /// Per-tick render: draw nine cell quads plus up to nine
    /// inner-mark quads depending on occupancy. Sends one batch of
    /// up to eighteen `DrawTriangle`s to the `render` sink.
    ///
    /// # Agent
    /// Not useful to mail directly — the substrate drives ticks.
    /// Use `capture_frame` to observe the output.
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        self.render(ctx);
    }
}

impl TicTacToeClient {
    fn render(&self, ctx: &mut Ctx<'_>) {
        // 9 cell quads + up to 9 mark quads; each quad is 2
        // triangles. Budget 36 slots.
        let mut tris: [DrawTriangle; 36] = [DrawTriangle::default(); 36];
        let mut n = 0;

        // Per-cell edge length. Subtract gaps so there's negative
        // space between cells.
        let span = 2.0 * BOARD_EXTENT;
        let cell_edge = (span - 2.0 * CELL_GAP) / 3.0;

        let game_over = self.state.status != 0;

        for row in 0..3u8 {
            for col in 0..3u8 {
                // Clip-space box for this cell. Row 0 is the top
                // row — y decreases as row increases.
                let x0 = -BOARD_EXTENT + col as f32 * (cell_edge + CELL_GAP);
                let x1 = x0 + cell_edge;
                let y1 = BOARD_EXTENT - row as f32 * (cell_edge + CELL_GAP);
                let y0 = y1 - cell_edge;

                let is_recent = !game_over
                    && row == self.state.last_move_row
                    && col == self.state.last_move_col
                    && self.state.last_move_row != LAST_MOVE_NONE;
                let bg = if game_over {
                    CELL_BG_GAMEOVER
                } else if is_recent {
                    CELL_BG_RECENT
                } else {
                    CELL_BG
                };
                push_quad(&mut tris, &mut n, x0, y0, x1, y1, bg);

                let occupant = self.state.board[row as usize][col as usize];
                if occupant != CELL_EMPTY {
                    let color = if occupant == PLAYER_X { MARK_X } else { MARK_O };
                    let inset = cell_edge * MARK_INSET;
                    push_quad(
                        &mut tris,
                        &mut n,
                        x0 + inset,
                        y0 + inset,
                        x1 - inset,
                        y1 - inset,
                        color,
                    );
                }
            }
        }

        ctx.send_many(&self.render, &tris[..n]);
    }
}

/// Push two triangles covering the axis-aligned rectangle
/// `[x0, x1] × [y0, y1]` with solid color `(r, g, b)` into `tris`.
/// Vertex winding is `(tl, tr, br)` then `(tl, br, bl)` so the
/// front face matches the substrate's existing render setup.
fn push_quad(
    tris: &mut [DrawTriangle],
    n: &mut usize,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    color: (f32, f32, f32),
) {
    let (r, g, b) = color;
    let tl = Vertex {
        x: x0,
        y: y1,
        r,
        g,
        b,
    };
    let tr = Vertex {
        x: x1,
        y: y1,
        r,
        g,
        b,
    };
    let br = Vertex {
        x: x1,
        y: y0,
        r,
        g,
        b,
    };
    let bl = Vertex {
        x: x0,
        y: y0,
        r,
        g,
        b,
    };
    tris[*n] = DrawTriangle {
        verts: [tl, tr, br],
    };
    tris[*n + 1] = DrawTriangle {
        verts: [tl, br, bl],
    };
    *n += 2;
}

aether_component::export!(TicTacToeClient);
