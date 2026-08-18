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
}
