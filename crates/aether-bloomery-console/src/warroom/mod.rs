//! War-room chrome rules: the loud set and the owner-authority set.
//!
//! Severity lives in [`mod@alerts`]; owner-authority lives in [`mod@interrupts`].
//! The two files are siblings so the sets cannot drift into each other.
//! [`mod@needs_you`] folds them into one row per subject.

pub mod alerts;
pub mod interrupts;
pub mod needs_you;

use crate::dto::DigestHex;

pub use alerts::{Alert, AlertKind, alerts};
pub use interrupts::{Interrupt, InterruptKind, interrupts};
pub use needs_you::{NeedsYouRow, Severity, rows};

/// The subject a chrome token or drill-in frame is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Focus {
    Bloom { id: DigestHex },
    Member { bloom: DigestHex, workpiece: String },
    Composition { bloom: DigestHex },
    Seal,
    Dispatch { bloom: DigestHex, workpiece: String },
    Record { sequence: u64 },
    Artifact { digest: DigestHex },
    Transcript { nonce: String },
    Workpiece { id: String },
}

impl Focus {
    #[must_use]
    pub fn bloom(id: DigestHex) -> Self {
        Self::Bloom { id }
    }

    #[must_use]
    pub fn member(bloom: DigestHex, workpiece: impl Into<String>) -> Self {
        let workpiece = workpiece.into();
        if workpiece.is_empty() {
            Self::Bloom { id: bloom }
        } else {
            Self::Member { bloom, workpiece }
        }
    }

    #[must_use]
    pub fn composition(id: DigestHex) -> Self {
        Self::Composition { bloom: id }
    }

    #[must_use]
    pub fn dispatch(bloom: DigestHex, workpiece: impl Into<String>) -> Self {
        Self::Dispatch { bloom, workpiece: workpiece.into() }
    }

    #[must_use]
    pub fn record(sequence: u64) -> Self {
        Self::Record { sequence }
    }

    #[must_use]
    pub fn artifact(digest: DigestHex) -> Self {
        Self::Artifact { digest }
    }

    #[must_use]
    pub fn transcript(nonce: impl Into<String>) -> Self {
        Self::Transcript { nonce: nonce.into() }
    }

    #[must_use]
    pub fn workpiece(id: impl Into<String>) -> Self {
        Self::Workpiece { id: id.into() }
    }

    /// The one line a pushed subject frame paints.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Bloom { id } => format!("bloom {}", id.prefix()),
            Self::Member { workpiece, .. } => format!("member {workpiece}"),
            Self::Composition { bloom } => format!("composition {}", bloom.prefix()),
            Self::Seal => "seal door".to_owned(),
            Self::Dispatch { workpiece, .. } => format!("dispatch {workpiece}"),
            Self::Record { sequence } => format!("record {sequence}"),
            Self::Artifact { digest } => format!("artifact {}", digest.prefix()),
            Self::Transcript { nonce } => format!("transcript {nonce}"),
            Self::Workpiece { id } => format!("workpiece {id}"),
        }
    }

    /// Short name the needs-you row uses for this subject.
    #[must_use]
    pub fn subject(&self) -> String {
        match self {
            Self::Bloom { id } | Self::Composition { bloom: id } | Self::Artifact { digest: id } => id.prefix(),
            Self::Member { workpiece, .. } | Self::Dispatch { workpiece, .. } | Self::Workpiece { id: workpiece } => {
                workpiece.clone()
            }
            Self::Seal => "seal".to_owned(),
            Self::Record { sequence } => format!("record {sequence}"),
            Self::Transcript { nonce } => nonce.clone(),
        }
    }

    /// Stable parent used when the exact subject has vanished.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        match self {
            Self::Dispatch { bloom, workpiece } => Some(Self::member(*bloom, workpiece.clone())),
            Self::Member { bloom, .. } | Self::Composition { bloom } => Some(Self::bloom(*bloom)),
            Self::Bloom { .. }
            | Self::Seal
            | Self::Record { .. }
            | Self::Artifact { .. }
            | Self::Transcript { .. }
            | Self::Workpiece { .. } => None,
        }
    }
}
