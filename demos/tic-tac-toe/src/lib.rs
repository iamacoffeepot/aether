//! Tic-tac-toe demo trunk rlib (ADR-0066). Hosts the kind types and
//! well-known mailbox names every component in the demo wires
//! against. The runtime `Component` impl + `aether_component::export!`
//! live in the sibling `aether-demo-tic-tac-toe-server` cdylib;
//! consumers (e.g. `aether-demo-tic-tac-toe-client`) depend on this
//! crate for the wire shapes without pulling in the server's
//! `#[handlers]` section emissions, which would stack on top of their
//! own and trip the substrate's "duplicate Component record" check
//! (issue 442).
//!
//! The demo as a whole exists to stress the ADR-0035 headless chassis
//! end-to-end plus the multi-session reply / broadcast paths the hub
//! supports — see the server crate's top-level docstring for the
//! agent-driving narrative.

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
}
