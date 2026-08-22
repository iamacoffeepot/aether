//! Pane identity and the Tab focus ring.

/// A focus stop on the root workspace.
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
