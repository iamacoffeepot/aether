//! Navigation stack entries. Each variant owns only its view state.

mod board;

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::keys::{KeyHint, Outcome};
use crate::store::{ResourceKey, Store};

pub use board::{Alert, BloomRow, Board, BoardRow, MemberRow, RowId, member_status_state};

/// One frame on the shell's stack. The board is the sole variant today.
pub enum Screen {
    Board(Board),
}

impl Screen {
    #[must_use]
    pub fn board() -> Self {
        Self::Board(Board::new())
    }

    #[must_use]
    pub fn subscriptions(&self) -> &'static [ResourceKey] {
        match self {
            Self::Board(board) => board.subscriptions(),
        }
    }

    #[must_use]
    pub fn key_hints(&self) -> &'static [KeyHint] {
        match self {
            Self::Board(board) => board.key_hints(),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        match self {
            Self::Board(board) => board.handle_key(key, store),
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        match self {
            Self::Board(board) => board.reseat(store),
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        match self {
            Self::Board(board) => board.render(frame, area, store),
        }
    }
}
