//! The property every lane-boundary scenario checks, whether or not it thought
//! to.
//!
//! A harness that only asserts "scenario X reaches outcome Y" is the same
//! whack-a-mole with a faster loop: it holds the failures someone already paid
//! for and says nothing about the next one. What generalizes is the property the
//! live coordinator violated —
//!
//! > **Quiescence with work outstanding is a failure.** A bloom is advancing, in
//! > a declared terminal state, or wedged with recorded evidence. Sealed, no
//! > wedge, empty outbox, nothing outstanding — the state a coordinator sat in
//! > for five hours — is a failure by construction.
//!
//! and its sibling, from the orders that sat undispatched all morning —
//!
//! > **Every dispatched order terminates.** An order left in
//! > `outstanding_orders` with no completion advances no counter, so no ceiling
//! > is ever reached and no wedge is ever recorded.
//!
//! Neither names a bug. That is the point: they fail on the next one too. They
//! are checked on every poll of every scenario's settle loop, so a scenario does
//! not opt in and cannot forget.

use std::fmt::Write as _;

use aether_bloomery::{BloomStatus, ViewDocument};

/// Everything a poll observes about whether the coordinator is doing anything.
///
/// Compared for equality between polls: unequal means the world moved, which is
/// what "advancing" means here. Deliberately coarse — it does not care *what*
/// moved, only that something did, so a step nobody predicted still counts as
/// progress and a new kind of stall still counts as a stall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// The rendered projection: bloom statuses, member resolutions, wedges,
    /// pending decisions.
    pub projection: String,
    /// The nonces the store still holds as outstanding orders.
    pub outstanding: Vec<String>,
    /// How many lane runs the mock has recorded.
    pub runs: usize,
}

impl Progress {
    /// Fold a poll's observations into a comparable fingerprint.
    #[must_use]
    pub fn observe(document: &ViewDocument, outstanding: Vec<String>, runs: usize) -> Self {
        Self { projection: render(document), outstanding, runs }
    }
}

/// The projection as a stable string — `Debug` over the view types, which
/// changes exactly when a field a scenario could care about changes, and needs
/// no per-field maintenance as the projection grows.
fn render(document: &ViewDocument) -> String {
    let mut rendered = String::new();
    for bloom in &document.blooms {
        let _ = write!(rendered, "{bloom:?}|");
    }
    rendered
}

/// Why a quiescent coordinator is allowed to be quiescent — or is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Quiescence {
    /// A declared terminal state: nothing further is owed.
    Terminal(String),
    /// Every unresolved member is wedged, and each wedge names the evidence that
    /// produced it. Stopped, but accountably.
    Wedged(String),
    /// Stopped with work still owed. The failure this harness exists for.
    Stalled(String),
}

/// Classify a coordinator that has stopped moving.
///
/// The order matters: an outstanding order is a stall *whatever* the projection
/// says, because a dispatched order with no completion is precisely the state
/// that advances no counter — a bloom can look wedged, or even resolved, while
/// an order it forgot sits in the table forever.
#[must_use]
pub fn classify(document: &ViewDocument, outstanding: &[String]) -> Quiescence {
    if !outstanding.is_empty() {
        return Quiescence::Stalled(format!(
            "{} dispatched order(s) never completed: {}. An order left outstanding advances no wedge counter, so no \
             ceiling is ever reached",
            outstanding.len(),
            outstanding.join(", "),
        ));
    }

    if document.base_alert.is_some() {
        return Quiescence::Wedged("red base holding the day".to_owned());
    }

    if document.blooms.is_empty() {
        return Quiescence::Stalled("no bloom reached the projection at all".to_owned());
    }

    let mut unresolved = Vec::new();
    for bloom in &document.blooms {
        match bloom.status {
            // Resolved is terminal for the lane boundary: every member has
            // passed its stages. Landing is the integrate/land reactors' step
            // and needs the GitHub side this tier deliberately does not mount.
            // A fully-withdrawn bloom is terminal too: every member left the
            // line by an operator's decision, so there is nothing waiting on
            // a lane (#5327).
            BloomStatus::Resolved | BloomStatus::Landed | BloomStatus::Superseded | BloomStatus::Withdrawn => continue,
            BloomStatus::Sealed => {}
        }
        for member in &bloom.members {
            if member.resolution.is_none()
                && member.wedge.is_none()
                && member.host_fault.is_none()
                && member.park.is_none()
                && member.awaiting_surface.is_none()
                // A withdrawn member is an accountable stop with a named
                // decider, not a member the machinery lost (#5327).
                && member.withdrawn.is_none()
                // An evicted member names the sibling and the file it waits
                // behind (ADR-0204); it re-dispatches on that sibling's
                // integration.
                && member.evicted_by.is_none()
            {
                unresolved.push(format!("{:?}/{}", bloom.id, member.workpiece.0));
            }
        }
    }

    if unresolved.is_empty() {
        let statuses: Vec<String> = document.blooms.iter().map(|bloom| format!("{:?}", bloom.status)).collect();
        // A sealed bloom whose every member carries a wedge has stopped for a
        // recorded reason; a bloom that reached a terminal status has stopped
        // because it is done. A bloom at its aggregate-review executor-fault
        // ceiling (ADR-0176) is the first stop of the recorded kind that is not
        // a *member's*: every member resolved and the fold is still held, so
        // without this it reads as a bloom that finished.
        let wedged = document.blooms.iter().any(|bloom| {
            bloom.members.iter().any(|member| {
                member.wedge.is_some()
                    || member.host_fault.is_some()
                    || member.park.is_some()
                    || member.awaiting_surface.is_some()
            }) || bloom.executor_fault.is_some_and(|fault| fault.terminal)
                || bloom.operator_hold.is_some()
                || bloom.review_park.is_some()
        });
        let summary = statuses.join(", ");
        return if wedged {
            Quiescence::Wedged(summary)
        } else {
            Quiescence::Terminal(summary)
        };
    }

    Quiescence::Stalled(format!(
        "sealed with {} member(s) neither resolved nor wedged and nothing in flight: {}",
        unresolved.len(),
        unresolved.join(", "),
    ))
}
