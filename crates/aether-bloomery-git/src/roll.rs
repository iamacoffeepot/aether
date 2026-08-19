//! Day-roll advance of `heads/main` in the fleet repository (ADR-0203).
//!
//! Linearization of the day onto main stays a git-worktree rebase in the
//! operator command. This module is the barrier and the write: the coverage
//! map must be fully green, then `heads/main` moves under the same
//! compare-and-swap as a landing. Tomorrow's daily ref is cut from that
//! advanced main. GitHub is not consulted.

use std::error::Error;
use std::fmt;

use crate::client::{GitDataApi, GitDataError, GitRef, strip_heads};

/// The branch the day returns to.
const MAIN: &str = "heads/main";

/// The day's verification-ledger coverage, as the roll barrier reads it.
///
/// A fully-green map releases the advance. Anything else is a hold whose
/// detail is the work list — the roll does not invent one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayCoverage {
    fully_green: bool,
    hold: String,
}

impl DayCoverage {
    /// Every required closure is green on this host class.
    #[must_use]
    pub fn green() -> Self {
        Self { fully_green: true, hold: String::new() }
    }

    /// The map is not fully green. `detail` is the hold the operator sees.
    #[must_use]
    pub fn hold(detail: impl Into<String>) -> Self {
        Self { fully_green: false, hold: detail.into() }
    }

    /// Whether the advance may proceed.
    #[must_use]
    pub fn is_fully_green(&self) -> bool {
        self.fully_green
    }

    /// The hold text, empty when green.
    #[must_use]
    pub fn hold_detail(&self) -> &str {
        &self.hold
    }
}

/// Why the roll did not move `heads/main` or cut tomorrow.
#[derive(Debug)]
pub enum RollError {
    /// The coverage map is not fully green. Fleet main is unchanged.
    NotGreen(String),
    /// The compare-and-swap lost: fleet main moved under the linearized image.
    CasLost(String),
    /// `heads/main` is missing from the authority.
    MissingMain,
    /// Tomorrow's daily ref already exists on the authority.
    AlreadyCut(String),
    /// A git-data fault other than a lost compare or a missing main.
    Git(GitDataError),
}

impl fmt::Display for RollError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotGreen(hold) => write!(f, "refusing to advance main: coverage map is not green ({hold})"),
            Self::CasLost(detail) => write!(f, "refusing to advance main: compare-and-swap lost ({detail})"),
            Self::MissingMain => write!(f, "refusing to advance main: heads/main is missing"),
            Self::AlreadyCut(branch) => write!(f, "refusing to cut {branch}: the ref already exists"),
            Self::Git(error) => write!(f, "{error}"),
        }
    }
}

impl Error for RollError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Git(error) => Some(error),
            _ => None,
        }
    }
}

impl From<GitDataError> for RollError {
    fn from(error: GitDataError) -> Self {
        match error {
            GitDataError::RefConflict(detail) => Self::CasLost(detail),
            GitDataError::MissingObject(_) => Self::MissingMain,
            other => Self::Git(other),
        }
    }
}

/// Advance `heads/main` to `new_sha` if it still points at `expected` and the
/// coverage map is fully green.
///
/// A hold, a lost compare, or a missing main leaves the ref where it was.
///
/// # Errors
/// [`RollError::NotGreen`] when the map is not fully green;
/// [`RollError::CasLost`] when the compare lost;
/// [`RollError::MissingMain`] when the ref is gone;
/// [`RollError::Git`] for any other adapter fault.
pub fn advance_main(
    git: &impl GitDataApi,
    new_sha: &str,
    expected: &str,
    coverage: &DayCoverage,
) -> Result<GitRef, RollError> {
    if !coverage.is_fully_green() {
        return Err(RollError::NotGreen(coverage.hold_detail().to_owned()));
    }
    git.compare_and_swap_ref(MAIN, new_sha, expected).map_err(RollError::from)
}

/// Cut `heads/bloomery/daily/<date>` from the current `heads/main`.
///
/// # Errors
/// [`RollError::MissingMain`] when fleet main is absent;
/// [`RollError::AlreadyCut`] when tomorrow's ref already exists;
/// [`RollError::Git`] for any other adapter fault.
pub fn cut_daily(git: &impl GitDataApi, date: &str) -> Result<GitRef, RollError> {
    let main = git.get_ref(MAIN)?.ok_or(RollError::MissingMain)?;
    let name = daily_ref(date);
    match git.create_ref(&name, &main.sha) {
        Ok(created) => Ok(created),
        Err(GitDataError::RefConflict(_)) => Err(RollError::AlreadyCut(strip_heads(&name).to_owned())),
        Err(error) => Err(RollError::from(error)),
    }
}

fn daily_ref(date: &str) -> String {
    format!("heads/bloomery/daily/{date}")
}

#[cfg(test)]
mod tests {
    use super::{DayCoverage, RollError, advance_main, cut_daily, daily_ref};
    use crate::testing::FakeGithub;

    const MAIN_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DAY_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const DATE: &str = "2026-08-15";

    fn seeded() -> FakeGithub {
        let git = FakeGithub::new();
        git.seed_ref("heads/main", MAIN_SHA);
        git.seed_ref("heads/bloomery/daily/2026-08-14", DAY_SHA);
        git
    }

    #[test]
    fn sync_advances_main_under_cas() {
        let git = seeded();

        let advanced = advance_main(&git, DAY_SHA, MAIN_SHA, &DayCoverage::green()).expect("a green map advances");

        assert_eq!(advanced.sha, DAY_SHA);
        assert_eq!(git.ref_target("heads/main").as_deref(), Some(DAY_SHA), "fleet main is the linearized day");
    }

    #[test]
    fn a_non_green_map_refuses_and_leaves_main_unmoved() {
        let git = seeded();
        let coverage = DayCoverage::hold("red test crate::day_head at aabbcc");

        match advance_main(&git, DAY_SHA, MAIN_SHA, &coverage) {
            Err(RollError::NotGreen(hold)) => {
                assert!(hold.contains("crate::day_head"), "the hold names the missing coverage: {hold}");
            }
            other => panic!("expected NotGreen, got {other:?}"),
        }

        assert_eq!(
            git.ref_target("heads/main").as_deref(),
            Some(MAIN_SHA),
            "a held roll must not compare-and-swap main"
        );
    }

    #[test]
    fn cut_starts_from_the_advanced_main() {
        let git = seeded();
        advance_main(&git, DAY_SHA, MAIN_SHA, &DayCoverage::green()).expect("the day advances");

        let cut = cut_daily(&git, DATE).expect("tomorrow is cut from post-advance main");

        assert_eq!(cut.sha, DAY_SHA, "the cut is the main the advance wrote, not the pre-advance sha");
        assert_eq!(git.ref_target(&daily_ref(DATE)).as_deref(), Some(DAY_SHA));
        assert_eq!(git.ref_target("heads/main").as_deref(), Some(DAY_SHA), "cutting does not move main");
    }

    #[test]
    fn a_lost_compare_is_a_cas_refusal_not_a_write() {
        let git = seeded();
        git.seed_ref("heads/main", "cccccccccccccccccccccccccccccccccccccccc");

        match advance_main(&git, DAY_SHA, MAIN_SHA, &DayCoverage::green()) {
            Err(RollError::CasLost(_)) => {}
            other => panic!("expected CasLost, got {other:?}"),
        }
        assert_eq!(
            git.ref_target("heads/main").as_deref(),
            Some("cccccccccccccccccccccccccccccccccccccccc"),
            "a lost compare leaves the unexpected main in place"
        );
    }
}
