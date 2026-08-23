//! Navigation stack entries. Each variant owns only its view state.

mod artifact;
mod backlog;
mod board;
mod detail;
mod dispatch;
mod journal;
mod json;
mod metrics;
mod partition;
mod quiet;
mod transcript;

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::dto::DigestHex;
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::store::{ResourceKey, Store};
use crate::warroom::Focus;

pub use backlog::Backlog;
pub use board::{BloomRow, Board, BoardLane, BoardRow, MemberRow, RowId, member_status_state};
pub use detail::Detail;
pub use metrics::{Breakdown, Dashboard, Days, Timeline, compose};
pub use partition::{MemberState, is_history_status, is_live_status};
pub use quiet::{QuietLine, quiet_lines};
pub use transcript::{LineBuffer, Transcript};

use artifact::Artifact;
use backlog::Workpiece;
use dispatch::DispatchList;
use journal::{Journal, Record};

/// One pushed frame. The live board lives in the workspace; History is a
/// pushed `Board` on this stack.
pub enum Screen {
    Board(Board),
    Detail(Detail),
    DispatchList(DispatchList),
    Journal(Journal),
    Record(Record),
    Artifact(Artifact),
    /// Boxed: the transcript carries two paged line buffers, so inlining it
    /// would size every pushed frame after the largest one.
    Transcript(Box<Transcript>),
    Timeline(Timeline),
    Days(Days),
    Cost(Breakdown),
    Backlog(Backlog),
    Workpiece(Workpiece),
}

impl Screen {
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
            Nav::Focus(Focus::Transcript { nonce }) => Self::Transcript(Box::new(Transcript::new(nonce))),
            Nav::Focus(Focus::Dispatch { bloom, workpiece }) => Self::DispatchList(DispatchList::new(bloom, workpiece)),
            Nav::Focus(Focus::Workpiece { id }) => Self::Workpiece(Workpiece::new(id)),
            Nav::Focus(focus) => Self::Detail(Detail::new(focus)),
            Nav::History => Self::history(),
            Nav::Journal { bloom } => Self::Journal(Journal::new(bloom)),
            Nav::Timeline { bloom } => Self::Timeline(Timeline::new(bloom)),
            Nav::Days => Self::Days(Days::new()),
            Nav::Cost => Self::Cost(Breakdown::new()),
            Nav::Backlog => Self::Backlog(Backlog::new()),
        }
    }

    #[must_use]
    pub fn focus(&self) -> Option<Focus> {
        match self {
            Self::Detail(detail) => Some(detail.focus().clone()),
            Self::DispatchList(list) => Some(list.focus()),
            Self::Record(record) => Some(record.focus()),
            Self::Artifact(artifact) => Some(artifact.focus()),
            Self::Transcript(transcript) => Some(transcript.focus()),
            Self::Board(_)
            | Self::Journal(_)
            | Self::Timeline(_)
            | Self::Days(_)
            | Self::Cost(_)
            | Self::Backlog(_) => None,
            Self::Workpiece(workpiece) => Some(workpiece.focus()),
        }
    }

    /// Live crumb for the footer trail. Exhaustive so a new frame must name itself.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Detail(_)
            | Self::DispatchList(_)
            | Self::Record(_)
            | Self::Artifact(_)
            | Self::Transcript(_)
            | Self::Workpiece(_) => self.focus().as_ref().map_or_else(String::new, Focus::label),
            Self::Board(board) => {
                if board.lane() == BoardLane::History {
                    Nav::History.label()
                } else {
                    "board".to_owned()
                }
            }
            Self::Journal(journal) => Nav::journal(journal.bloom()).label(),
            Self::Timeline(timeline) => Nav::timeline(timeline.bloom()).label(),
            Self::Days(_) => Nav::days().label(),
            Self::Cost(_) => Nav::cost().label(),
            Self::Backlog(_) => Nav::backlog().label(),
        }
    }

    /// The three frames that paint one scrolling paragraph of served text
    /// read inside a titled pane; the list and chart frames carry their own
    /// structure and must not be double-framed, so the `None` arm is spelled
    /// out rather than defaulted.
    #[must_use]
    pub fn reading_title(&self) -> Option<String> {
        match self {
            Self::Artifact(_) | Self::Record(_) | Self::Transcript(_) => Some(self.label()),
            Self::Board(_)
            | Self::Detail(_)
            | Self::DispatchList(_)
            | Self::Journal(_)
            | Self::Timeline(_)
            | Self::Days(_)
            | Self::Cost(_)
            | Self::Backlog(_)
            | Self::Workpiece(_) => None,
        }
    }

    #[must_use]
    pub fn scroll(&self) -> usize {
        match self {
            Self::Board(board) => board.scroll(),
            Self::Detail(detail) => detail.scroll(),
            Self::Journal(_)
            | Self::DispatchList(_)
            | Self::Record(_)
            | Self::Artifact(_)
            | Self::Transcript(_)
            | Self::Timeline(_)
            | Self::Days(_)
            | Self::Cost(_)
            | Self::Backlog(_)
            | Self::Workpiece(_) => 0,
        }
    }

    #[must_use]
    pub fn selected_key(&self) -> Option<String> {
        match self {
            Self::Board(board) => board.cursor().selected().map(|id| format!("{id:?}")),
            Self::Detail(detail) => detail.selected_key().map(|key| format!("{key:?}")),
            Self::Backlog(backlog) => backlog.selected_key().cloned(),
            Self::Workpiece(workpiece) => workpiece.selected_key().map(|key| format!("{key:?}")),
            Self::DispatchList(list) => list.selected_key().cloned(),
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
            Self::DispatchList(list) => list.subscriptions(),
            Self::Journal(journal) => journal.subscriptions(),
            Self::Record(_) => Record::subscriptions(),
            Self::Artifact(artifact) => artifact.subscriptions(),
            Self::Transcript(transcript) => transcript.subscriptions(),
            Self::Timeline(timeline) => timeline.subscriptions(),
            Self::Days(days) => days.subscriptions(),
            Self::Cost(cost) => cost.subscriptions(),
            Self::Backlog(backlog) => backlog.subscriptions(),
            Self::Workpiece(workpiece) => workpiece.subscriptions(),
        }
    }

    #[must_use]
    pub fn key_hints(&self) -> &'static [KeyHint] {
        match self {
            Self::Board(board) => board.key_hints(),
            Self::Detail(detail) => detail.key_hints(),
            Self::DispatchList(list) => list.key_hints(),
            Self::Journal(_) => Journal::key_hints(),
            Self::Record(_) => Record::key_hints(),
            Self::Artifact(_) => Artifact::key_hints(),
            Self::Transcript(_) => Transcript::key_hints(),
            Self::Timeline(_) => Timeline::key_hints(),
            Self::Days(_) => Days::key_hints(),
            Self::Cost(_) => Breakdown::key_hints(),
            Self::Backlog(_) => Backlog::key_hints(),
            Self::Workpiece(_) => Workpiece::key_hints(),
        }
    }

    #[must_use]
    pub fn digest_under_cursor(&self) -> Option<DigestHex> {
        match self {
            Self::Board(board) => board.digest_under_cursor(),
            Self::Detail(detail) => detail.digest_under_cursor(),
            Self::Journal(_)
            | Self::DispatchList(_)
            | Self::Record(_)
            | Self::Transcript(_)
            | Self::Days(_)
            | Self::Cost(_) => None,
            Self::Artifact(artifact) => Some(artifact.digest_under_cursor()),
            Self::Timeline(timeline) => Some(timeline.bloom()),
            Self::Backlog(backlog) => backlog.digest_under_cursor(),
            Self::Workpiece(workpiece) => workpiece.digest_under_cursor(),
        }
    }

    /// Only a digest the coordinator `put` into `aether.artifacts` is openable.
    /// A bloom id, git tree, or git commit is an identity that would 404.
    #[must_use]
    pub fn openable_digest(&self) -> Option<DigestHex> {
        match self {
            Self::Board(_)
            | Self::Timeline(_)
            | Self::Artifact(_)
            | Self::Journal(_)
            | Self::DispatchList(_)
            | Self::Record(_)
            | Self::Transcript(_)
            | Self::Days(_)
            | Self::Cost(_) => None,
            Self::Detail(detail) => detail.openable_digest(),
            Self::Backlog(backlog) => backlog.digest_under_cursor(),
            Self::Workpiece(workpiece) => workpiece.openable_digest(),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        match self {
            Self::Board(board) => board.handle_key(key, store),
            Self::Detail(detail) => detail.handle_key(key, store),
            Self::DispatchList(list) => list.handle_key(key, store),
            Self::Journal(journal) => journal.handle_key(key, store),
            Self::Record(record) => record.handle_key(key, store),
            Self::Artifact(artifact) => artifact.handle_key(key, store),
            Self::Transcript(transcript) => transcript.handle_key(key, store),
            Self::Timeline(timeline) => timeline.handle_key(key, store),
            Self::Days(days) => days.handle_key(key, store),
            Self::Cost(cost) => cost.handle_key(key, store),
            Self::Backlog(backlog) => backlog.handle_key(key, store),
            Self::Workpiece(workpiece) => workpiece.handle_key(key, store),
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        match self {
            Self::Board(board) => board.reseat(store),
            Self::Detail(detail) => detail.reseat(store),
            Self::DispatchList(list) => list.reseat(store),
            Self::Journal(journal) => journal.reseat(store),
            Self::Transcript(transcript) => transcript.reseat(store),
            Self::Timeline(timeline) => timeline.reseat(store),
            Self::Cost(cost) => cost.reseat(store),
            Self::Backlog(backlog) => backlog.reseat(store),
            Self::Workpiece(workpiece) => workpiece.reseat(store),
            Self::Record(_) | Self::Artifact(_) | Self::Days(_) => {}
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        match self {
            Self::Board(board) => board.render(frame, area, store),
            Self::Detail(detail) => detail.render(frame, area, store),
            Self::DispatchList(list) => list.render(frame, area, store),
            Self::Journal(journal) => journal.render(frame, area, store),
            Self::Record(record) => record.render(frame, area, store),
            Self::Artifact(artifact) => artifact.render(frame, area, store),
            Self::Transcript(transcript) => transcript.render(frame, area, store),
            Self::Timeline(timeline) => timeline.render(frame, area, store),
            Self::Days(days) => days.render(frame, area, store),
            Self::Cost(cost) => cost.render(frame, area, store),
            Self::Backlog(backlog) => backlog.render(frame, area, store),
            Self::Workpiece(workpiece) => workpiece.render(frame, area, store),
        }
    }
}
