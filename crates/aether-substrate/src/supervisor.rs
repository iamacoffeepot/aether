//! Substrate-side surface of the wasm component supervisor (issue 603).
//!
//! Defines the trait the [`crate::Mailer`] consults for component-bound
//! mail and the structured outcomes the chassis frame loop matches on
//! when draining (ADR-0063). The implementation lives in
//! `aether-capabilities::ControlPlaneCapability`; `aether-substrate`
//! holds only the interface so chassis-side consumers (frame_loop,
//! Mailer routing) stay capability-agnostic.

use std::time::Duration;

use crate::mail::{Mail, MailboxId};

/// Routing handle for a chassis-installed wasm-component supervisor.
/// The supervisor (today: `ControlPlaneCapability`) owns the live
/// component table and per-component dispatcher threads; substrate
/// runtime code that needs to forward mail to a component goes through
/// this trait. Installed via [`crate::Mailer::install_component_router`]
/// during the supervisor's `init`.
///
/// `Send + Sync` because [`crate::Mailer::push`] is called from any
/// thread.
pub trait ComponentRouter: Send + Sync {
    /// Forward `mail` to the component at `recipient`. Returns a
    /// structured outcome the caller logs differently depending on
    /// whether the mail reached the inbox, the component is dead /
    /// closed, or the id never bound.
    fn route(&self, recipient: MailboxId, mail: Mail) -> ComponentSendOutcome;

    /// Drain every live component's inbox under `budget`. Returns a
    /// [`DrainSummary`] the chassis frame loop matches on (per
    /// ADR-0063): non-empty `deaths` or any `wedged` triggers
    /// [`crate::lifecycle::fatal_abort`]. Empty summary on a
    /// supervisor with no live components.
    fn drain_all_with_budget(&self, budget: Duration) -> DrainSummary;
}

/// Outcome of [`ComponentRouter::route`]. The substrate's mailer
/// re-warns differently for each so an agent reading `engine_logs`
/// can tell "component shut down" from "actor died from a trap" from
/// "id never bound".
#[derive(Debug, Clone, Copy)]
pub enum ComponentSendOutcome {
    /// Mail accepted into the component's inbox.
    Sent,
    /// The supervisor saw the id but the dispatcher transitioned to
    /// `Dead` after a panic / trap. Subsequent mail to this id stays
    /// `Dead` until the chassis tears down or a `replace_component`
    /// brings a fresh dispatcher up.
    Dead,
    /// The supervisor saw the id but the inbox channel was closed
    /// (component drop in flight). Mail is dropped.
    Closed,
    /// The supervisor has no record of this id. Either the mailbox
    /// was never bound to a component or it predates the supervisor's
    /// install (boot-window race) — the caller falls through to
    /// whatever bubbles-up policy the runtime has wired.
    Unknown,
}

/// Aggregate outcome of [`ComponentRouter::drain_all_with_budget`].
/// The chassis matches on this each frame and routes abnormal cases
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
