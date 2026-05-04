//! Tic-tac-toe client component: renders the board the
//! `aether-demo-tic-tac-toe` server publishes and turns mouse clicks
//! into `PlayMove` mail. Registers itself under the well-known name
//! the server fans state to (`tic_tac_toe.client`), caches the latest
//! `GameState`, re-emits the board as colored quads each tick, and
//! maps `MouseButton` presses against the latest `MouseMove` +
//! `WindowSize` to send a move to the server's well-known mailbox
//! (`tic_tac_toe`).
//!
//! State source depends on deployment:
//! - **Same substrate** (server + client co-loaded): the server's
//!   `client_observer` fan-out lands in `on_game_state` on every
//!   move. Works pre-ADR-0037.
//! - **Client on desktop, server on hub-substrate** (ADR-0037): the
//!   hub→desktop fan-out isn't wired, so `on_game_state` never fires.
//!   Instead, `PlayMove` bubbles up to the hub, the server's reply
//!   travels the ADR-0037 Phase 2 reply path back to this mailbox,
//!   and `on_move_result` captures the state from the reply. Known
//!   limitation: the client only sees moves it originates — moves
//!   initiated by attached Claude sessions via MCP go to
//!   `hub.claude.broadcast`, which fans to sessions, not to
//!   components.
//!
//! Hub-hosted deployment (ADR-0037 Phase 3):
//! ```text
//! cargo build -p aether-demo-tic-tac-toe-server --target wasm32-unknown-unknown --release
//! cargo build -p aether-demo-tic-tac-toe-client --target wasm32-unknown-unknown --release
//! cargo run -p aether-substrate-hub            # hub boots its own substrate
//! # From a Claude session attached to the hub:
//! #   load_component(hub_engine_id, "<path>/aether_demo_tic_tac_toe_server.wasm", name="tic_tac_toe")
//! #   spawn_substrate("<path>/aether-substrate-desktop")
//! #   load_component(desktop_engine_id, "<path>/aether_demo_tic_tac_toe_client.wasm",
//! #                  name="tic_tac_toe.client")
//! # Click the desktop window; capture_frame to observe the board.
//! ```
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

use aether_component::{BootError, Component, Ctx, InitCtx, Mailbox, handlers};
use aether_demo_tic_tac_toe::{
    CELL_EMPTY, GameState, LAST_MOVE_NONE, MoveResult, PLAYER_X, PlayMove, SERVER,
};
use aether_kinds::{DrawTriangle, MouseButton, MouseMove, Tick, Vertex, WindowSize};

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
    /// Latest cursor position in physical pixels (window-local).
    /// `None` until the first `MouseMove` arrives.
    mouse: Option<(f32, f32)>,
    /// Latest window size. `None` until the desktop chassis has
    /// published a `WindowSize` — clicks with no known size can't be
    /// mapped to a cell, so they're dropped until the first publish
    /// lands (happens within one tick of subscribing).
    window: Option<(u32, u32)>,
    render: Mailbox<DrawTriangle>,
    server: Mailbox<PlayMove>,
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
            mouse: None,
            window: None,
            render: aether_component::resolve_mailbox::<DrawTriangle>("aether.render"),
            server: aether_component::resolve_mailbox::<PlayMove>(SERVER),
        }
    }
}

/// Tic-tac-toe board renderer + input shim. Listens for
/// `tic_tac_toe.game_state` mail addressed to its well-known mailbox
/// (`tic_tac_toe.client`) and reflects whatever state it last
/// received on every render tick. On mouse click, maps the cursor
/// against the current board geometry and mails `tic_tac_toe.play_move`
/// to the `tic_tac_toe` server.
///
/// # Agent
/// Load this component under the mailbox name `tic_tac_toe.client`
/// so the server's local-observer fan-out lands here. Moves can be
/// driven either by clicking inside the window or by mailing
/// `tic_tac_toe.play_move` directly against the `tic_tac_toe` server
/// component; the board updates the same way either route. Use
/// `capture_frame` to verify rendering.
#[handlers]
impl Component for TicTacToeClient {
    const NAMESPACE: &'static str = "tic_tac_toe_client";

    fn init(_ctx: &mut InitCtx<'_>) -> Result<Self, BootError> {
        Ok(TicTacToeClient::default())
    }

    /// Cached copy of the latest authoritative state. Overwrites
    /// unconditionally — the server is the source of truth and we
    /// don't try to merge or validate. Only fires in same-substrate
    /// deployments where the server's `client_observer` fan-out
    /// resolves locally; cross-substrate state arrives via the
    /// `MoveResult` reply instead (see `on_move_result`).
    ///
    /// # Agent
    /// You shouldn't send this mail directly. It arrives from the
    /// `tic_tac_toe` server's `client_observer` fan-out whenever a
    /// move or reset applies.
    #[handler]
    fn on_game_state(&mut self, _ctx: &mut Ctx<'_>, state: GameState) {
        self.state = state;
    }

    /// The server replies to every `PlayMove` / `Reset` with a
    /// `MoveResult` carrying the current board (post-move on
    /// acceptance, unchanged on rejection — either way authoritative).
    /// This is the state channel that works across substrates: the
    /// ADR-0037 reply path routes it back to this component's mailbox
    /// regardless of which engine the server runs on.
    ///
    /// # Agent
    /// You shouldn't send this mail directly. It's the reply this
    /// component receives after mailing `tic_tac_toe.play_move` or
    /// `tic_tac_toe.reset` to the server.
    #[handler]
    fn on_move_result(&mut self, _ctx: &mut Ctx<'_>, result: MoveResult) {
        self.state = result.state;
    }

    /// Cache the latest cursor position so the next mouse click has
    /// something to hit-test. No per-frame processing — the position
    /// only matters at click time.
    ///
    /// # Agent
    /// Not useful to mail directly — the desktop chassis drives this.
    #[handler]
    fn on_mouse_move(&mut self, _ctx: &mut Ctx<'_>, mv: MouseMove) {
        self.mouse = Some((mv.x, mv.y));
    }

    /// Cache the latest window size. The chassis re-publishes every
    /// tick, so new subscribers pick up a value within one frame and
    /// resizes propagate immediately.
    ///
    /// # Agent
    /// Not useful to mail directly — the desktop chassis drives this.
    #[handler]
    fn on_window_size(&mut self, _ctx: &mut Ctx<'_>, sz: WindowSize) {
        self.window = Some((sz.width, sz.height));
    }

    /// Mouse-button press: hit-test the latest cursor against the
    /// board layout and, if the click lands inside a cell, mail a
    /// `PlayMove` to the `tic_tac_toe` server. Out-of-board clicks
    /// and clicks before the first `MouseMove`/`WindowSize` are
    /// dropped — the server is still the authority and will reject
    /// duplicates or game-over moves anyway.
    ///
    /// # Agent
    /// Not useful to mail directly — use `tic_tac_toe.play_move`
    /// addressed to the `tic_tac_toe` server to drive moves from
    /// Claude.
    #[handler]
    fn on_mouse_button(&mut self, ctx: &mut Ctx<'_>, _: MouseButton) {
        let Some((mx, my)) = self.mouse else { return };
        let Some((w, h)) = self.window else { return };
        if let Some((row, col)) = hit_test(mx, my, w, h) {
            ctx.send(
                &self.server,
                &PlayMove {
                    row,
                    col,
                    _pad: [0; 2],
                },
            );
        }
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

        let cell_edge = cell_edge();

        let game_over = self.state.status != 0;

        for row in 0..3u8 {
            for col in 0..3u8 {
                let (x0, y0, x1, y1) = cell_rect(row, col);

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

/// Clip-space edge length of one cell. Shared between renderer and
/// hit-tester so a layout tweak in one place doesn't desync the
/// other.
fn cell_edge() -> f32 {
    let span = 2.0 * BOARD_EXTENT;
    (span - 2.0 * CELL_GAP) / 3.0
}

/// Clip-space `(x0, y0, x1, y1)` rectangle for the given cell. Row
/// 0 is the top row — y decreases as row increases.
fn cell_rect(row: u8, col: u8) -> (f32, f32, f32, f32) {
    let cell_edge = cell_edge();
    let x0 = -BOARD_EXTENT + col as f32 * (cell_edge + CELL_GAP);
    let x1 = x0 + cell_edge;
    let y1 = BOARD_EXTENT - row as f32 * (cell_edge + CELL_GAP);
    let y0 = y1 - cell_edge;
    (x0, y0, x1, y1)
}

/// Map a physical-pixel click at `(mouse_x, mouse_y)` against a
/// `(w, h)` window to a board cell. Returns `None` if the click is
/// in a cell gap, outside the board, or the window has a zero
/// dimension (defensive — `publish_window_size` already filters
/// zero-dim events on the chassis side).
fn hit_test(mouse_x: f32, mouse_y: f32, w: u32, h: u32) -> Option<(u8, u8)> {
    if w == 0 || h == 0 {
        return None;
    }
    // Window coords: origin top-left, y grows downward. Clip space:
    // origin center, y grows upward. The render stretches the
    // clip-space board across the whole window so the mapping is 1:1
    // in each axis independently — aspect ratio doesn't distort the
    // hit test because the rendered board has the same distortion.
    let x_clip = (mouse_x / w as f32) * 2.0 - 1.0;
    let y_clip = 1.0 - (mouse_y / h as f32) * 2.0;
    for row in 0..3u8 {
        for col in 0..3u8 {
            let (x0, y0, x1, y1) = cell_rect(row, col);
            if x_clip >= x0 && x_clip <= x1 && y_clip >= y0 && y_clip <= y1 {
                return Some((row, col));
            }
        }
    }
    None
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
        z: 0.0,
        r,
        g,
        b,
    };
    let tr = Vertex {
        x: x1,
        y: y1,
        z: 0.0,
        r,
        g,
        b,
    };
    let br = Vertex {
        x: x1,
        y: y0,
        z: 0.0,
        r,
        g,
        b,
    };
    let bl = Vertex {
        x: x0,
        y: y0,
        z: 0.0,
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

// Gated on wasm32 for the same reason the server crate gates its
// `export!` — host test builds link against the server rlib and
// would hit duplicate `init` / `receive_p32` / ... symbols.
#[cfg(target_arch = "wasm32")]
aether_component::export!(TicTacToeClient);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn center_click_lands_on_middle_cell() {
        // Dead center of a square window maps to clip-space (0, 0),
        // which falls inside cell (1, 1).
        assert_eq!(hit_test(500.0, 500.0, 1000, 1000), Some((1, 1)));
    }

    #[test]
    fn top_left_cell() {
        // Physical pixel near the top-left of a 1000x1000 window,
        // well inside the board and inside cell (0, 0).
        assert_eq!(hit_test(60.0, 60.0, 1000, 1000), Some((0, 0)));
    }

    #[test]
    fn bottom_right_cell() {
        assert_eq!(hit_test(940.0, 940.0, 1000, 1000), Some((2, 2)));
    }

    #[test]
    fn click_outside_board_rejected() {
        // Corner pixel — outside the `[-BOARD_EXTENT, BOARD_EXTENT]`
        // square.
        assert_eq!(hit_test(1.0, 1.0, 1000, 1000), None);
    }

    #[test]
    fn click_in_gap_rejected() {
        // The CELL_GAP strip between cells falls between cell_rect
        // ranges; a click right on the column-0/column-1 boundary in
        // clip space should miss.
        let cell_edge = cell_edge();
        // Clip-space x right in the middle of the col-0/col-1 gap.
        let x_clip = -BOARD_EXTENT + cell_edge + CELL_GAP * 0.5;
        let pixel_x = (x_clip + 1.0) * 0.5 * 1000.0;
        assert_eq!(hit_test(pixel_x, 500.0, 1000, 1000), None);
    }

    #[test]
    fn zero_dim_window_rejected() {
        assert_eq!(hit_test(100.0, 100.0, 0, 100), None);
        assert_eq!(hit_test(100.0, 100.0, 100, 0), None);
    }
}
