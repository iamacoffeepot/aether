//! Drain-summary types the chassis frame loop matches on
//! (ADR-0063).
//!
//! Pre-Phase-4 this module also held the `ComponentRouter` trait
//! (`route()` + `drain_all_with_budget()`) the wasm-component
//! supervisor implemented. Issue 634 Phase 4 retired the routing
//! half: trampolines are `NativeActor`s registered as
//! `MailboxEntry::Closure` like every other actor, so the framework
//! handles routing. The drain types stay because the frame loop's
//! fail-fast format-strings still reference them; Phase 4 PR 2
//! reframes the drain barrier against the `ActorRegistry` and
//! retires `DrainSummary` / `DrainDeath` outright.

use std::time::Duration;

use crate::mail::MailboxId;

/// Aggregate outcome of a frame-bound drain pass. The chassis frame
/// loop matches on this each frame and routes abnormal cases
/// through [`crate::lifecycle::fatal_abort`] (ADR-0063).
#[derive(Debug, Default, Clone)]
pub struct DrainSummary {
    pub deaths: Vec<DrainDeath>,
    /// First wedged entry encountered. Walking stops on the first
    /// wedge — the substrate is going down regardless, so collecting
    /// further state isn't useful.
    pub wedged: Option<(MailboxId, Duration)>,
}

/// Structured information about a dispatcher death. Emitted by the
/// supervisor's dispatcher loop when a wasmtime trap or host-side
/// panic kills an actor; ridden through [`DrainSummary::deaths`] so
/// the chassis can fail-fast (ADR-0063) without scraping log text.
#[derive(Debug, Clone)]
pub struct DrainDeath {
    pub mailbox: MailboxId,
    pub mailbox_name: String,
    pub last_kind: String,
    pub reason: String,
}

/// Per-entry drain outcome, surfaced by the supervisor's internal
/// drain loop. Aggregated into [`DrainSummary`].
#[derive(Debug, Clone)]
pub enum DrainOutcome {
    /// Pending counter reached zero with the entry still live.
    Quiesced,
    /// The dispatcher transitioned to Dead during the wait.
    Died(DrainDeath),
    /// The budget expired with `pending > 0`. The dispatcher is
    /// either mid-trap or wedged in host code; the chassis treats
    /// this as a fatal substrate event.
    Wedged { waited: Duration },
}
