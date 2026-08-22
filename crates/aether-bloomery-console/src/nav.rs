//! Navigation the screens ask the shell to perform.
//!
//! Screens cannot touch the stack. They return [`crate::keys::Outcome::Push`]
//! and the shell opens the frame.

use crate::dto::DigestHex;
use crate::warroom::Focus;

/// One frame the shell should push.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Nav {
    Focus(Focus),
    History,
    Journal { bloom: Option<DigestHex> },
    Timeline { bloom: DigestHex },
    Days,
    Cost,
    Backlog,
}

impl Nav {
    #[must_use]
    pub fn focus(focus: Focus) -> Self {
        Self::Focus(focus)
    }

    #[must_use]
    pub fn journal(bloom: Option<DigestHex>) -> Self {
        Self::Journal { bloom }
    }

    #[must_use]
    pub fn transcript(nonce: impl Into<String>) -> Self {
        Self::Focus(Focus::transcript(nonce))
    }

    #[must_use]
    pub fn timeline(bloom: DigestHex) -> Self {
        Self::Timeline { bloom }
    }

    #[must_use]
    pub fn days() -> Self {
        Self::Days
    }

    #[must_use]
    pub fn cost() -> Self {
        Self::Cost
    }

    #[must_use]
    pub fn backlog() -> Self {
        Self::Backlog
    }

    /// Crumb the footer trail paints. Exhaustive so a new variant must name itself.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Focus(focus) => focus.label(),
            Self::History => "history".to_owned(),
            Self::Journal { bloom: Some(id) } => format!("journal {}", id.prefix()),
            Self::Journal { bloom: None } => "journal".to_owned(),
            Self::Timeline { bloom } => format!("timeline {}", bloom.prefix()),
            Self::Days => "days".to_owned(),
            Self::Cost => "cost".to_owned(),
            Self::Backlog => "backlog".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Nav;
    use crate::dto::DigestHex;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    #[test]
    fn a_bloom_filtered_journal_and_a_timeline_name_their_bloom() {
        // The plausible bug: a crumb that says `journal` for every journal
        // frame, so a bloom-filtered stream is indistinguishable from the
        // whole ledger.
        let id = digest(0xab);
        let prefix = id.prefix();
        assert!(Nav::journal(Some(id)).label().contains(&prefix));
        assert!(Nav::timeline(id).label().contains(&prefix));
        assert!(!Nav::journal(None).label().contains(&prefix));
    }
}
