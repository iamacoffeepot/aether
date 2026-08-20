//! Pane identity, the Tab focus ring, and the bordered block every pane paints.

use ratatui::widgets::{Block, Borders};

use crate::palette;

/// A focus stop on the root workspace. `fleet` is bordered and titled but is
/// not in this ring — it has one line and no cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneId {
    Board,
    NeedsYou,
    Quiet,
}

impl PaneId {
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Self::Board => Self::NeedsYou,
            Self::NeedsYou => Self::Quiet,
            Self::Quiet => Self::Board,
        }
    }

    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Board => "board",
            Self::NeedsYou => "needs you",
            Self::Quiet => "quiet",
        }
    }
}

/// Bordered, titled pane chrome. Focused panes use the blossom ring.
#[must_use]
pub fn pane_block(title: &'static str, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(if focused {
            palette::border_focused()
        } else {
            palette::border()
        })
        .style(palette::body())
}
