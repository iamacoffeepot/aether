//! The durable pre-bloom scoping-run record (ADR-0208, ADR-0214).
//!
//! A scoping run is dispatched before any bloom exists, so it cannot pin an
//! instruction bundle in a sealed registry. ADR-0214 §"Resolve defaults before
//! sealing" puts that pin on the run itself: the host resolves the selected
//! bundle when the run is created and retains it across retries. [`ScopeRun`]
//! is that persisted shape.

use crate::digest::Digest;
use crate::ids::WorkpieceId;

/// One pre-bloom scoping run, including the instruction-bundle pin it dispatches
/// under (ADR-0214).
///
/// `instructions` is absent when the host authorized no unique default at
/// creation. Dispatch then refuses at the provenance gate, the same way an
/// unpinned bloom does — the run is created, not the process invented.
#[aether_data::kind(name = "aether.bloomery.scope_run", eq)]
#[serde(deny_unknown_fields)]
pub struct ScopeRun {
    /// The commission being scoped — the pre-freeze identity.
    pub commission: WorkpieceId,
    /// Which attempt on that commission this is, from `1`.
    pub ordinal: u64,
    /// The commission's stored intent statement.
    pub intent: Digest,
    /// The observed mainline the run reads code at.
    pub base: Digest,
    /// The run's content-addressed subject.
    pub subject: Digest,
    /// The instruction-bundle address this run dispatches under, when the host
    /// selected one at creation. Same address type a bloom registry pins.
    pub instructions: Option<Digest>,
}
