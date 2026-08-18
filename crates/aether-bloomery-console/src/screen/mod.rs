//! Navigation stack entries. Each variant owns only its view state.

mod artifact;
mod board;
mod detail;
mod journal;
mod metrics;
mod partition;
mod transcript;

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::dto::DigestHex;
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::store::{ResourceKey, Store};
use crate::warroom::Focus;

pub use board::{BloomRow, Board, BoardLane, BoardRow, MemberRow, RowId, member_status_state};
pub use detail::Detail;
pub use metrics::{Breakdown, Dashboard, Days, Timeline, compose};
pub use partition::{is_history_status, is_live_status};
pub use transcript::{LineBuffer, Transcript};

use artifact::Artifact;
use journal::{Journal, Record};

/// One frame on the shell's stack.
pub enum Screen {
    Board(Board),
    Detail(Detail),
    Journal(Journal),
    Record(Record),
    Artifact(Artifact),
    Transcript(Transcript),
    Timeline(Timeline),
    Days(Days),
    Cost(Breakdown),
}

impl Screen {
    #[must_use]
    pub fn board() -> Self {
        Self::Board(Board::new())
    }

    #[must_use]
    pub fn history() -> Self {
        Self::Board(Board::history())
    }

    #[must_use]
    pub fn subject(focus: Focus) -> Self {
        Self::from_nav(Nav::focus(focus))
    }

    #[must_use]
    pub fn from_nav(nav: Nav) -> Self {
        match nav {
            Nav::Focus(Focus::Record { sequence }) => Self::Record(Record::new(sequence)),
            Nav::Focus(Focus::Artifact { digest }) => Self::Artifact(Artifact::new(digest)),
            Nav::Focus(Focus::Transcript { nonce }) => Self::Transcript(Transcript::new(nonce)),
            Nav::Focus(focus) => Self::Detail(Detail::new(focus)),
            Nav::History => Self::history(),
            Nav::Journal { bloom } => Self::Journal(Journal::new(bloom)),
            Nav::Timeline { bloom } => Self::Timeline(Timeline::new(bloom)),
            Nav::Days => Self::Days(Days::new()),
            Nav::Cost => Self::Cost(Breakdown::new()),
        }
    }

    #[must_use]
    pub fn focus(&self) -> Option<Focus> {
        match self {
            Self::Detail(detail) => Some(detail.focus().clone()),
            Self::Record(record) => Some(record.focus()),
            Self::Artifact(artifact) => Some(artifact.focus()),
            Self::Transcript(transcript) => Some(transcript.focus()),
            Self::Board(_) | Self::Journal(_) | Self::Timeline(_) | Self::Days(_) | Self::Cost(_) => None,
        }
    }

    #[must_use]
    pub fn scroll(&self) -> usize {
        match self {
            Self::Board(board) => board.scroll(),
            Self::Detail(detail) => detail.scroll(),
            Self::Journal(_)
            | Self::Record(_)
            | Self::Artifact(_)
            | Self::Transcript(_)
            | Self::Timeline(_)
            | Self::Days(_)
            | Self::Cost(_) => 0,
        }
    }

    #[must_use]
    pub fn selected_key(&self) -> Option<String> {
        match self {
            Self::Board(board) => board.cursor().selected().map(|id| format!("{id:?}")),
            Self::Detail(detail) => detail.selected_key().map(|key| format!("{key:?}")),
            Self::Journal(_)
            | Self::Record(_)
            | Self::Artifact(_)
            | Self::Transcript(_)
            | Self::Timeline(_)
            | Self::Days(_)
            | Self::Cost(_) => None,
        }
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        match self {
            Self::Board(board) => board.subscriptions(),
            Self::Detail(detail) => detail.subscriptions(),
            Self::Journal(journal) => journal.subscriptions(),
            Self::Record(_) => Record::subscriptions(),
            Self::Artifact(artifact) => artifact.subscriptions(),
            Self::Transcript(transcript) => transcript.subscriptions(),
            Self::Timeline(timeline) => timeline.subscriptions(),
            Self::Days(days) => days.subscriptions(),
            Self::Cost(cost) => cost.subscriptions(),
        }
    }

    #[must_use]
    pub fn key_hints(&self) -> &'static [KeyHint] {
        match self {
            Self::Board(board) => board.key_hints(),
            Self::Detail(detail) => detail.key_hints(),
            Self::Journal(_) => Journal::key_hints(),
            Self::Record(_) => Record::key_hints(),
            Self::Artifact(_) => Artifact::key_hints(),
            Self::Transcript(_) => Transcript::key_hints(),
            Self::Timeline(_) => Timeline::key_hints(),
            Self::Days(_) => Days::key_hints(),
            Self::Cost(_) => Breakdown::key_hints(),
        }
    }

    #[must_use]
    pub fn digest_under_cursor(&self) -> Option<DigestHex> {
        match self {
            Self::Board(board) => board.digest_under_cursor(),
            Self::Detail(detail) => detail.digest_under_cursor(),
            Self::Journal(_) | Self::Record(_) | Self::Transcript(_) | Self::Days(_) | Self::Cost(_) => None,
            Self::Artifact(artifact) => Some(artifact.digest_under_cursor()),
            Self::Timeline(timeline) => Some(timeline.bloom()),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        match self {
            Self::Board(board) => board.handle_key(key, store),
            Self::Detail(detail) => detail.handle_key(key, store),
            Self::Journal(journal) => journal.handle_key(key, store),
            Self::Record(record) => record.handle_key(key, store),
            Self::Artifact(artifact) => artifact.handle_key(key, store),
            Self::Transcript(transcript) => transcript.handle_key(key, store),
            Self::Timeline(timeline) => timeline.handle_key(key, store),
            Self::Days(days) => days.handle_key(key, store),
            Self::Cost(cost) => cost.handle_key(key, store),
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        match self {
            Self::Board(board) => board.reseat(store),
            Self::Detail(detail) => detail.reseat(store),
            Self::Journal(journal) => journal.reseat(store),
            Self::Transcript(transcript) => transcript.reseat(store),
            Self::Timeline(timeline) => timeline.reseat(store),
            Self::Cost(cost) => cost.reseat(store),
            Self::Record(_) | Self::Artifact(_) | Self::Days(_) => {}
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        match self {
            Self::Board(board) => board.render(frame, area, store),
            Self::Detail(detail) => detail.render(frame, area, store),
            Self::Journal(journal) => journal.render(frame, area, store),
            Self::Record(record) => record.render(frame, area, store),
            Self::Artifact(artifact) => artifact.render(frame, area, store),
            Self::Transcript(transcript) => transcript.render(frame, area, store),
            Self::Timeline(timeline) => timeline.render(frame, area, store),
            Self::Days(days) => days.render(frame, area, store),
            Self::Cost(cost) => cost.render(frame, area, store),
        }
    }

    /// True when k should leave this frame for the chrome above it.
    #[must_use]
    pub fn selected_is_first(&self, store: &Store) -> bool {
        match self {
            Self::Board(board) => board.selected_is_first(store),
            Self::Detail(detail) => detail.selected_is_first(),
            Self::Journal(journal) => journal.selected_is_first(store),
            Self::Transcript(transcript) => transcript.selected_is_first(),
            Self::Timeline(timeline) => timeline.selected_is_first(store),
            Self::Cost(cost) => cost.selected_is_first(store),
            Self::Days(days) => days.selected_is_first(),
            Self::Record(_) | Self::Artifact(_) => true,
        }
    }
}
