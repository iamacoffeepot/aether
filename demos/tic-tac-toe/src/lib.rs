//! Tic-tac-toe demo: a two-player game component that exists to stress
//! the ADR-0035 headless chassis end-to-end, plus the multi-session
//! reply / broadcast paths that the hub already supports. Two Claude
//! sessions attach to the same hub, take turns sending
//! `tic_tac_toe.play_move` mail, and observe `tic_tac_toe.game_state`
//! broadcasts that fan out to every attached session after each
//! accepted move.
//!
//! Scope: no identity tracking. The component doesn't care which
//! session sent a move — it just alternates turns starting with X.
//! Any session can play either side as long as it's that side's turn
//! to move. Identity + join flow is a follow-up if the demo feels
//! weak without it; this version is the smallest thing that exercises
//! the cross-session paths.
//!
//! No render sink — the demo was built against the headless chassis
//! and doesn't emit `DrawTriangle`. Desktop is welcome to load it but
//! the window will stay blank since nothing draws.

use aether_component::{Component, Ctx, InitCtx, KindId, Sink, handlers};
use aether_mail::{Kind, Schema};
use bytemuck::{Pod, Zeroable};

// Cell / player codes. A cell is `CELL_EMPTY` until a move lands; then
// it holds the player code (`PLAYER_X` or `PLAYER_O`). `GameState.turn`
// uses the same player codes plus `PLAYER_NONE` for "game over, no
// next turn."
pub const CELL_EMPTY: u8 = 0;
pub const PLAYER_NONE: u8 = 0;
pub const PLAYER_X: u8 = 1;
pub const PLAYER_O: u8 = 2;

// `GameState.status` values.
pub const GAME_PLAYING: u8 = 0;
pub const GAME_WON_X: u8 = 1;
pub const GAME_WON_O: u8 = 2;
pub const GAME_DRAW: u8 = 3;

// `MoveResult.status` values. The move was accepted when `status ==
// MOVE_OK`; any other value means the move was rejected and the
// component's state is unchanged. `state` on the reply always
// reflects the current board either way.
pub const MOVE_OK: u8 = 0;
pub const MOVE_OUT_OF_BOUNDS: u8 = 1;
pub const MOVE_CELL_OCCUPIED: u8 = 2;
pub const MOVE_GAME_OVER: u8 = 3;

// Sentinel used in `GameState.last_move_*` before the first move of a
// game. Any real row/col is in `0..=2`, so `255` is unambiguous.
pub const LAST_MOVE_NONE: u8 = 255;

/// Claude → component: place a mark at `(row, col)`. The component
/// assigns the current turn's player (X first, then alternating) —
/// the sender doesn't pick a side. Rejected moves (out-of-bounds,
/// occupied cell, game over) come back on the reply with a non-zero
/// `MoveResult.status` and no state change.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, Kind, Schema)]
#[kind(name = "tic_tac_toe.play_move")]
pub struct PlayMove {
    pub row: u8,
    pub col: u8,
    pub _pad: [u8; 2],
}

/// Claude → component: reset to a fresh game (empty board, X to
/// move). Replies to the sender with a `MoveResult` carrying the new
/// state and broadcasts the new state so other sessions see the
/// reset.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, Kind, Schema)]
#[kind(name = "tic_tac_toe.reset")]
pub struct Reset;

/// Component → Claude (broadcast + reply member): full game snapshot.
/// `board` is row-major; each cell is one of `CELL_EMPTY`, `PLAYER_X`,
/// or `PLAYER_O`. `turn` is the player whose move is next (or
/// `PLAYER_NONE` after the game ends). `status` discriminates between
/// an in-progress game, a win, or a draw. The `last_move_*` fields
/// describe the move that produced this state — `LAST_MOVE_NONE` in
/// the row/col sentinels and `PLAYER_NONE` in the player slot mean
/// "fresh game, nobody has moved yet."
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, Kind, Schema)]
#[kind(name = "tic_tac_toe.game_state")]
pub struct GameState {
    pub board: [[u8; 3]; 3],
    pub turn: u8,
    pub status: u8,
    pub last_move_row: u8,
    pub last_move_col: u8,
    pub last_move_player: u8,
    pub _pad: [u8; 2],
}

impl GameState {
    /// Starting position: empty board, X to move, no last move.
    pub const fn new_game() -> Self {
        GameState {
            board: [[CELL_EMPTY; 3]; 3],
            turn: PLAYER_X,
            status: GAME_PLAYING,
            last_move_row: LAST_MOVE_NONE,
            last_move_col: LAST_MOVE_NONE,
            last_move_player: PLAYER_NONE,
            _pad: [0; 2],
        }
    }
}

/// Component → Claude (reply-to-sender): outcome of a `PlayMove` or
/// `Reset`. `status == MOVE_OK` means the move (or reset) took effect
/// and `state` is the post-move snapshot; any other `status` is a
/// rejection, and `state` holds the unchanged pre-call snapshot so the
/// sender still sees ground truth in one reply.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, Kind, Schema)]
#[kind(name = "tic_tac_toe.move_result")]
pub struct MoveResult {
    pub status: u8,
    pub _pad: [u8; 7],
    pub state: GameState,
}

/// Per-instance component state. Holds the live game board plus cached
/// kind / sink handles — resolved once in `init` and reused across
/// every move.
pub struct TicTacToe {
    state: GameState,
    move_result_kind: KindId<MoveResult>,
    broadcast: Sink<GameState>,
    client_observer: Sink<GameState>,
}

/// Well-known local mailbox name the server fans `GameState` to in
/// addition to `hub.claude.broadcast`. Any component loaded under
/// this name on the same substrate as the server will receive every
/// state update — the intended consumer is the `aether-demo-tic-tac-
/// toe-client` renderer, but any observer-style component works.
/// If no component is registered under this name the send is a
/// harmless "mailbox unknown" warn-drop.
pub const CLIENT_OBSERVER: &str = "tic_tac_toe.client";

/// Well-known mailbox name the authoritative server is conventionally
/// loaded under. Callers that want to mail `PlayMove` / `Reset` to
/// the server can resolve this name at init; matches the default
/// `load_component` name the server advertises via its top-level
/// rustdoc.
pub const SERVER: &str = "tic_tac_toe";

/// Authoritative tic-tac-toe server. Accepts `PlayMove` and `Reset`
/// from any attached Claude session, replies to the sender with the
/// outcome, and broadcasts the new `GameState` to
/// `hub.claude.broadcast` whenever the board changes.
///
/// # Agent
/// Alternating turns start with X. Send `tic_tac_toe.play_move` with
/// `{ row, col }` in `0..=2` — the component assigns whichever player
/// is on turn. The reply is `tic_tac_toe.move_result` where
/// `status == 0` (MOVE_OK) means accepted; non-zero means rejected
/// (`1` out-of-bounds, `2` cell-occupied, `3` game-over) and the
/// board is unchanged. Watch `tic_tac_toe.game_state` on your
/// `receive_mail` stream to see every state update regardless of who
/// sent the move — that's the broadcast path the hub fans out to
/// every session. Send `tic_tac_toe.reset` (empty payload) to start a
/// fresh game.
#[handlers]
impl Component for TicTacToe {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        TicTacToe {
            state: GameState::new_game(),
            move_result_kind: ctx.resolve::<MoveResult>(),
            broadcast: ctx.resolve_sink::<GameState>("hub.claude.broadcast"),
            client_observer: ctx.resolve_sink::<GameState>(CLIENT_OBSERVER),
        }
    }

    /// Applies a move if legal, then replies to the sender with the
    /// outcome. On acceptance the new state is also broadcast so every
    /// attached session sees the update — not just the one that moved.
    ///
    /// # Agent
    /// Send `{ row, col }` with both in `0..=2`. The reply's `status`
    /// is the authoritative outcome; `state` is the board after the
    /// move if accepted, or the unchanged board if rejected. Don't
    /// infer side from the payload — the component picks based on
    /// whose turn it is, and the resulting `state.last_move_player`
    /// tells you which side actually got placed.
    #[handler]
    fn on_play_move(&mut self, ctx: &mut Ctx<'_>, mv: PlayMove) {
        let status = self.apply_move(mv.row, mv.col);
        self.reply(ctx, status);
        if status == MOVE_OK {
            ctx.send(&self.broadcast, &self.state);
            ctx.send(&self.client_observer, &self.state);
        }
    }

    /// Resets to a fresh game, replies to the caller, and broadcasts
    /// the new empty state so other sessions notice.
    ///
    /// # Agent
    /// Use this to start over after a win/draw or to abandon an
    /// in-progress game. The reply always has `status == MOVE_OK`
    /// and the broadcast is fire-and-forget.
    #[handler]
    fn on_reset(&mut self, ctx: &mut Ctx<'_>, _r: Reset) {
        self.state = GameState::new_game();
        self.reply(ctx, MOVE_OK);
        ctx.send(&self.broadcast, &self.state);
        ctx.send(&self.client_observer, &self.state);
    }
}

impl TicTacToe {
    fn apply_move(&mut self, row: u8, col: u8) -> u8 {
        if self.state.status != GAME_PLAYING {
            return MOVE_GAME_OVER;
        }
        if row >= 3 || col >= 3 {
            return MOVE_OUT_OF_BOUNDS;
        }
        let (r, c) = (row as usize, col as usize);
        if self.state.board[r][c] != CELL_EMPTY {
            return MOVE_CELL_OCCUPIED;
        }

        let player = self.state.turn;
        self.state.board[r][c] = player;
        self.state.last_move_row = row;
        self.state.last_move_col = col;
        self.state.last_move_player = player;

        if is_winner(&self.state.board, player) {
            self.state.status = if player == PLAYER_X {
                GAME_WON_X
            } else {
                GAME_WON_O
            };
            self.state.turn = PLAYER_NONE;
        } else if is_board_full(&self.state.board) {
            self.state.status = GAME_DRAW;
            self.state.turn = PLAYER_NONE;
        } else {
            self.state.turn = if player == PLAYER_X {
                PLAYER_O
            } else {
                PLAYER_X
            };
        }
        MOVE_OK
    }

    fn reply(&self, ctx: &mut Ctx<'_>, status: u8) {
        let Some(sender) = ctx.reply_to() else {
            return;
        };
        let result = MoveResult {
            status,
            _pad: [0; 7],
            state: self.state,
        };
        ctx.reply(sender, self.move_result_kind, &result);
    }
}

fn is_winner(board: &[[u8; 3]; 3], player: u8) -> bool {
    const LINES: [[(usize, usize); 3]; 8] = [
        [(0, 0), (0, 1), (0, 2)],
        [(1, 0), (1, 1), (1, 2)],
        [(2, 0), (2, 1), (2, 2)],
        [(0, 0), (1, 0), (2, 0)],
        [(0, 1), (1, 1), (2, 1)],
        [(0, 2), (1, 2), (2, 2)],
        [(0, 0), (1, 1), (2, 2)],
        [(0, 2), (1, 1), (2, 0)],
    ];
    LINES
        .iter()
        .any(|line| line.iter().all(|&(r, c)| board[r][c] == player))
}

fn is_board_full(board: &[[u8; 3]; 3]) -> bool {
    for row in board {
        for &cell in row {
            if cell == CELL_EMPTY {
                return false;
            }
        }
    }
    true
}

// Gated on wasm32: the `export!` macro emits `#[no_mangle]`
// `init` / `receive_p32` / `on_drop` / `on_replace` /
// `on_rehydrate_p32` shims the substrate calls over FFI. Those
// symbols only mean something in a wasm guest — emitting them in
// host builds causes duplicate-symbol link errors when a sibling
// rlib (e.g. the tic-tac-toe-client demo) depends on this crate
// and also has its own `export!`. Host unit tests don't need them.
#[cfg(target_arch = "wasm32")]
aether_component::export!(TicTacToe);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_game_defaults() {
        let s = GameState::new_game();
        assert_eq!(s.turn, PLAYER_X);
        assert_eq!(s.status, GAME_PLAYING);
        assert_eq!(s.last_move_player, PLAYER_NONE);
        for row in s.board {
            for cell in row {
                assert_eq!(cell, CELL_EMPTY);
            }
        }
    }

    fn new_component() -> TicTacToe {
        // Host-side unit tests can't route through the SDK's
        // `InitCtx`, so fabricate a minimal instance with the same
        // starting state the runtime would build. The kind / sink
        // handles are dummies — `apply_move` never touches them.
        TicTacToe {
            state: GameState::new_game(),
            move_result_kind: aether_component::resolve::<MoveResult>(),
            broadcast: aether_component::resolve_sink::<GameState>("hub.claude.broadcast"),
            client_observer: aether_component::resolve_sink::<GameState>(CLIENT_OBSERVER),
        }
    }

    #[test]
    fn alternating_turns() {
        let mut c = new_component();
        assert_eq!(c.apply_move(0, 0), MOVE_OK);
        assert_eq!(c.state.board[0][0], PLAYER_X);
        assert_eq!(c.state.turn, PLAYER_O);
        assert_eq!(c.apply_move(1, 1), MOVE_OK);
        assert_eq!(c.state.board[1][1], PLAYER_O);
        assert_eq!(c.state.turn, PLAYER_X);
    }

    #[test]
    fn out_of_bounds_rejected() {
        let mut c = new_component();
        assert_eq!(c.apply_move(3, 0), MOVE_OUT_OF_BOUNDS);
        assert_eq!(c.apply_move(0, 3), MOVE_OUT_OF_BOUNDS);
        assert_eq!(c.state.turn, PLAYER_X);
    }

    #[test]
    fn occupied_cell_rejected() {
        let mut c = new_component();
        assert_eq!(c.apply_move(0, 0), MOVE_OK);
        assert_eq!(c.apply_move(0, 0), MOVE_CELL_OCCUPIED);
        assert_eq!(c.state.turn, PLAYER_O);
    }

    #[test]
    fn row_win_detects() {
        let mut c = new_component();
        // X: (0,0), O: (1,0), X: (0,1), O: (1,1), X: (0,2) — X wins row 0.
        c.apply_move(0, 0);
        c.apply_move(1, 0);
        c.apply_move(0, 1);
        c.apply_move(1, 1);
        assert_eq!(c.apply_move(0, 2), MOVE_OK);
        assert_eq!(c.state.status, GAME_WON_X);
        assert_eq!(c.state.turn, PLAYER_NONE);
    }

    #[test]
    fn diagonal_win_detects() {
        let mut c = new_component();
        // X: (0,0), O: (0,1), X: (1,1), O: (0,2), X: (2,2) — X wins main diagonal.
        c.apply_move(0, 0);
        c.apply_move(0, 1);
        c.apply_move(1, 1);
        c.apply_move(0, 2);
        assert_eq!(c.apply_move(2, 2), MOVE_OK);
        assert_eq!(c.state.status, GAME_WON_X);
    }

    #[test]
    fn draw_detects() {
        let mut c = new_component();
        // A board that ends without a winner.
        //   X O X
        //   X O O
        //   O X X
        let moves = [
            (0, 0),
            (0, 1),
            (0, 2),
            (1, 1),
            (1, 0),
            (1, 2),
            (2, 1),
            (2, 0),
            (2, 2),
        ];
        for (r, col) in moves {
            assert_eq!(c.apply_move(r, col), MOVE_OK);
        }
        assert_eq!(c.state.status, GAME_DRAW);
        assert_eq!(c.state.turn, PLAYER_NONE);
    }

    #[test]
    fn moves_after_win_rejected() {
        let mut c = new_component();
        c.apply_move(0, 0);
        c.apply_move(1, 0);
        c.apply_move(0, 1);
        c.apply_move(1, 1);
        c.apply_move(0, 2); // X wins row 0
        assert_eq!(c.apply_move(2, 2), MOVE_GAME_OVER);
    }
}
