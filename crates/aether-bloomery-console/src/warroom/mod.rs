//! War-room chrome rules: the loud set and the owner-authority set.
//!
//! Severity lives in [`alerts`]; owner-authority lives in [`interrupts`].
//! The two files are siblings so the sets cannot drift into each other.

pub mod alerts;
pub mod interrupts;

use crate::dto::DigestHex;

pub use alerts::{Alert, AlertKind, alerts};
pub use interrupts::{Interrupt, InterruptKind, interrupts};

/// The subject an alert token or interrupt entry jumps to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Focus {
    Bloom { id: DigestHex },
    Member { bloom: DigestHex, workpiece: String },
    Composition { bloom: DigestHex },
    Seal,
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

    /// The one line a pushed subject frame paints.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Bloom { id } => format!("bloom {}", id.prefix()),
            Self::Member { workpiece, .. } => format!("member {workpiece}"),
            Self::Composition { bloom } => format!("composition {}", bloom.prefix()),
            Self::Seal => "seal door".to_owned(),
        }
    }
}

/// One selectable chrome item. Interrupts sort before alerts in the walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChromeId {
    Interrupt { kind: InterruptKind, focus: Focus },
    Alert { kind: AlertKind, focus: Focus },
}

impl ChromeId {
    #[must_use]
    pub fn from_interrupt(entry: &Interrupt) -> Self {
        Self::Interrupt { kind: entry.kind, focus: entry.focus.clone() }
    }

    #[must_use]
    pub fn from_alert(alert: &Alert) -> Self {
        Self::Alert { kind: alert.kind, focus: alert.focus.clone() }
    }

    #[must_use]
    pub fn focus(&self) -> &Focus {
        match self {
            Self::Interrupt { focus, .. } | Self::Alert { focus, .. } => focus,
        }
    }
}

/// Selectable chrome items: owner queue first, then the louder alert tokens.
#[must_use]
pub fn chrome_ids(interrupts: &[Interrupt], alerts: &[Alert]) -> Vec<ChromeId> {
    interrupts.iter().map(ChromeId::from_interrupt).chain(alerts.iter().map(ChromeId::from_alert)).collect()
}
