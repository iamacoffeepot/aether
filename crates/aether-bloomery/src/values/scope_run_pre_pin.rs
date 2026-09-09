//! Frozen pre-pin wire shape of the durable [`ScopeRun`]
//! record (ADR-0187 / ADR-0214).
//!
//! The run record gained `instructions` so a pre-bloom scoping run can pin the
//! bundle it dispatches under. The wire encoding is positional and untagged, so
//! a row written without that field cannot be read by a decoder that expects
//! it. This module freezes the pre-pin shape so those rows upcast with the pin
//! absent rather than refusing. Never edit these fields: a later run-record
//! change adds its own frozen mirror beside this one.
//!
//! This type exists to *decode*. The pre-pin identity itself is the pinned
//! `SCOPE_RUN_PRE_PIN_DIGEST` literal in the persisted registry, never computed
//! from this type (#5500).

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::ids::WorkpieceId;

use super::ScopeRun;

/// The pre-pin [`ScopeRun`]: the input triple and attempt identity, no bundle.
///
/// Field names and order mirror the run as it stood before the instruction pin,
/// because the positional codec reads them by position and the schema digest
/// renders them by name.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeRunPrePin {
    /// The commission being scoped.
    pub commission: WorkpieceId,
    /// Which attempt on that commission this is, from `1`.
    pub ordinal: u64,
    /// The commission's stored intent statement.
    pub intent: Digest,
    /// The observed mainline the run reads code at.
    pub base: Digest,
    /// The run's content-addressed subject.
    pub subject: Digest,
}

impl From<ScopeRunPrePin> for ScopeRun {
    /// Carry a pre-pin run forward with the bundle pin absent.
    ///
    /// Absent is the honest fill: a run opened before the host selected a
    /// bundle names no process, and inventing a pin here would attribute
    /// authorization the operator never recorded. Dispatch of such a run
    /// refuses at the provenance gate, the same way an unpinned bloom does.
    fn from(prior: ScopeRunPrePin) -> Self {
        Self {
            commission: prior.commission,
            ordinal: prior.ordinal,
            intent: prior.intent,
            base: prior.base,
            subject: prior.subject,
            instructions: None,
        }
    }
}
