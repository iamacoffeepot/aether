//! The sealed execution limit, as the lane sees it (#5998).
//!
//! A dispatched lane runs under a wall-clock limit its bloom's stage catalog
//! sealed, and until now it did not know: the coordinator cancelled the run when
//! the limit passed, recorded a host fault, and the lane learned nothing because
//! it was dead. Two things go wrong with that. The model cannot plan to leave a
//! candidate before the limit, and the lane's *own* post-model work — the
//! mechanical fixers, the scoped lint bar, the one repair turn it buys — spends
//! the same clock with no idea how much of it is left. On bloom `7a2ff988…`
//! (2026-09-14) two of three cancellations landed in that post-model stretch:
//! fifty-odd minutes of building, five or six in the bar, cancelled two minutes
//! into the repair turn the bar had just bought.
//!
//! The executor now names the absolute deadline in the child's environment
//! ([`EXECUTION_DEADLINE_ENV`]), and this is what the lane reads it as: the time
//! remaining, a clamp for every harness-side budget, and the `## Budget` section
//! of the assembled prompt. Absent — an older executor, a store-less backend, a
//! hand-run lane — leaves every caller exactly where it was, unbounded.

use std::env;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aether_bloomery::EXECUTION_DEADLINE_ENV;

/// The longest budget the lane will believe.
///
/// The seal door refuses an authored wall clock above one day
/// ([`ExecutionLimits::MAX_WALL_CLOCK_SECS`](aether_bloomery::ExecutionLimits::MAX_WALL_CLOCK_SECS)),
/// so a remaining budget past that is a misread number rather than a very
/// patient coordinator — and believing one would hand every clamp below an
/// unbounded answer while looking like a bounded lane.
const LONGEST_CREDIBLE_BUDGET: Duration = Duration::from_secs(86_400);

/// How much of the limit the lane keeps clear of the cancel.
///
/// Handing off with two minutes unspent is the whole point: the candidate, its
/// evidence envelope, and the commit-message deliverable are all written after
/// the last piece of work, and a cancel that lands between the work and the
/// write loses the run. Two minutes is generous for those writes and
/// insignificant against an hour.
const HANDOFF_MARGIN: Duration = Duration::from_mins(2);

/// What a dispatched lane has left of its sealed execution limit.
#[derive(Clone, Copy, Debug)]
pub(super) struct Budget {
    /// When the coordinator cancels this run, on this process's monotonic
    /// clock. Converted on the way in rather than compared as wall-clock time,
    /// so a host clock step cannot move a deadline the lane is already working
    /// against.
    deadline: Instant,
}

impl Budget {
    /// The budget this dispatch's environment names.
    ///
    /// A process-level fact about the run this binary *is*, not configuration of
    /// anything it hosts — the coordinator that forked this lane is the only
    /// writer, and there is no argv to carry it (see the module docs).
    #[allow(clippy::disallowed_methods)] // aether-suppression-request: the coordinator's deadline for this child, not cap config
    pub(super) fn resolve() -> Option<Self> {
        Self::at(env::var(EXECUTION_DEADLINE_ENV).ok()?.trim().parse().ok()?)
    }

    /// The budget a stated deadline names.
    ///
    /// `None` when the deadline is already in the past — a lane that launched
    /// past its own cancel is about to be killed, and pretending it has a budget
    /// of zero would make every caller below skip its work and hand off an
    /// unfixed tree a moment before the run is discarded anyway. `None` too past
    /// [`LONGEST_CREDIBLE_BUDGET`], which is not a budget at all.
    pub(super) fn at(deadline_unix_millis: u64) -> Option<Self> {
        let now_unix_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX));
        let remaining = Duration::from_millis(deadline_unix_millis.checked_sub(now_unix_millis)?);
        if remaining.is_zero() || remaining > LONGEST_CREDIBLE_BUDGET {
            return None;
        }

        Some(Self { deadline: Instant::now() + remaining })
    }

    /// How long until the cancel.
    pub(super) fn remaining(self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// How long until the lane means to have handed off — the remaining limit
    /// less the margin it keeps clear of the cancel.
    pub(super) fn usable(self) -> Duration {
        self.remaining().saturating_sub(HANDOFF_MARGIN)
    }

    /// `want`, or what is usable when that is less — the clamp every
    /// harness-side budget passes through, so no piece of the lane's own work
    /// can outlive the run it is tidying.
    pub(super) fn clamp(self, want: Duration) -> Duration {
        want.min(self.usable())
    }

    /// Whether the lane can still fit `want` before the handoff.
    pub(super) fn fits(self, want: Duration) -> bool {
        self.usable() >= want
    }

    /// The `## Budget` section of the assembled construct prompt: what the
    /// limit is, and how much of it this dispatch has left.
    ///
    /// Facts only. What to do about them is process policy, and process policy
    /// lives in the authorized instruction bundle (ADR-0214), never in text
    /// this crate composes.
    pub(super) fn section(self) -> String {
        format!(
            "\n## Budget\n\nThis dispatch is cancelled when its sealed execution limit passes. {} remain as this \
             prompt is assembled, of which the lane keeps the last {} for capturing your candidate and running its \
             own post-run bar.\n",
            render(self.remaining()),
            render(HANDOFF_MARGIN),
        )
    }
}

/// A duration as the prompt states it: whole minutes, or whole seconds under a
/// minute. The lane is planning an hour of work, so seconds of precision past
/// that are noise a reader has to discard.
fn render(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        return format!("{secs} second(s)");
    }
    format!("{} minute(s)", secs / 60)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::{Budget, HANDOFF_MARGIN, render};

    fn with_remaining(remaining: Duration) -> Budget {
        Budget { deadline: Instant::now() + remaining }
    }

    // Tripwire: the margin is what makes the lane hand off rather than be
    // killed between its last piece of work and the write that records it. A
    // clamp that handed back the whole remaining limit would let a fixer or the
    // lint bar run right up to the cancel, which is the shape #5945 died in.
    #[test]
    fn every_clamp_leaves_the_handoff_margin_unspent() {
        let budget = with_remaining(Duration::from_mins(30));

        assert!(budget.clamp(Duration::from_mins(15)) == Duration::from_mins(15), "work that fits is not shortened");
        let clamped = budget.clamp(Duration::from_mins(60));
        assert!(clamped <= Duration::from_mins(28), "work past the limit is cut to the usable remainder: {clamped:?}");
        assert!(clamped >= Duration::from_mins(27), "and not cut further than the margin: {clamped:?}");
    }

    // Tripwire: a lane inside the margin has no room for any of its own work.
    // Reporting otherwise would buy a scoped compile the run is cancelled in
    // the middle of, losing the candidate the model already finished.
    #[test]
    fn a_lane_inside_the_margin_fits_nothing() {
        let budget = with_remaining(HANDOFF_MARGIN / 2);

        assert_eq!(budget.usable(), Duration::ZERO);
        assert!(!budget.fits(Duration::from_secs(1)), "there is no room left for work of any size");
        assert_eq!(budget.clamp(Duration::from_mins(15)), Duration::ZERO);
    }

    // Tripwire: a deadline already past is a run about to be killed. It must
    // resolve nothing rather than a zero budget, which every caller below would
    // read as "skip your work" — a tree left unfixed a moment before the run is
    // discarded anyway. (A dispatch that names no deadline at all is the same
    // `None`, one layer up, and is the old unbounded behaviour.)
    #[test]
    fn a_passed_deadline_resolves_no_budget() {
        let now_unix_millis = u64::try_from(
            SystemTime::now().duration_since(UNIX_EPOCH).expect("the host clock is past the epoch").as_millis(),
        )
        .expect("the host clock fits");

        assert!(Budget::at(0).is_none(), "a deadline at the epoch is long past");
        assert!(Budget::at(now_unix_millis + 60_000).is_some(), "a deadline a minute out resolves a budget");
        // A misread number, not a very patient coordinator: the seal door
        // refuses an authored limit past a day, so nothing downstream should
        // believe a remaining budget of half a million years and clamp nothing.
        assert!(Budget::at(u64::MAX).is_none(), "a deadline past any sealable limit resolves nothing");
    }

    #[test]
    fn the_budget_section_states_the_limit_and_what_is_left_of_it() {
        // A second past the whole minute, because the render truncates and the
        // clock moves between constructing the budget and reading it.
        let section = with_remaining(Duration::from_mins(58) + Duration::from_secs(1)).section();

        assert!(section.starts_with("\n## Budget\n\n"), "the budget is a context slot, not inlined instructions");
        assert!(section.contains("58 minute(s)"), "got: {section}");
        assert!(section.contains("2 minute(s)"), "the reserved margin is stated too: {section}");
    }

    #[test]
    fn a_duration_renders_in_the_unit_a_reader_planning_an_hour_needs() {
        assert_eq!(render(Duration::from_secs(45)), "45 second(s)");
        assert_eq!(render(Duration::from_secs(60)), "1 minute(s)");
        assert_eq!(render(Duration::from_secs(3_599)), "59 minute(s)");
    }
}
