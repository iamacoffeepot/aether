//! Navigation stack entries. Each variant owns only its view state.

mod board;
mod subject;

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::keys::{KeyHint, Outcome};
use crate::store::{ResourceKey, Store};
use crate::warroom::Focus;

pub use board::{BloomRow, Board, BoardRow, MemberRow, RowId, member_status_state};
pub use subject::Subject;

/// One frame on the shell's stack.
pub enum Screen {
    Board(Board),
    Subject(Subject),
}

impl Screen {
    #[must_use]
    pub fn board() -> Self {
        Self::Board(Board::new())
    }

    #[must_use]
    pub fn subject(focus: Focus) -> Self {
        Self::Subject(Subject::new(focus))
    }

    #[must_use]
    pub fn subscriptions(&self) -> &'static [ResourceKey] {
        match self {
            Self::Board(board) => board.subscriptions(),
            Self::Subject(subject) => subject.subscriptions(),
        }
    }

    #[must_use]
    pub fn key_hints(&self) -> &'static [KeyHint] {
        match self {
            Self::Board(board) => board.key_hints(),
            Self::Subject(subject) => subject.key_hints(),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        match self {
            Self::Board(board) => board.handle_key(key, store),
            Self::Subject(subject) => subject.handle_key(key, store),
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        match self {
            Self::Board(board) => board.reseat(store),
            Self::Subject(subject) => subject.reseat(store),
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        match self {
            Self::Board(board) => board.render(frame, area, store),
            Self::Subject(subject) => subject.render(frame, area, store),
        }
    }

    /// True when k should leave this frame for the chrome above it.
    #[must_use]
    pub fn selected_is_first(&self, store: &Store) -> bool {
        match self {
            Self::Board(board) => board.selected_is_first(store),
            Self::Subject(_) => true,
        }
    }
}
