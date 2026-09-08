//! The golden task and the set it belongs to (ADR-0184 §Benchmark blooms).
//!
//! > a landed pull request supplies the work order (its issue text), the
//! > checkout (its pre-merge base), the bar (its landing CI), and a reference
//! > answer (its real diff).
//!
//! Three of those four are read off the pull request; the fourth is the set's.
//!
//! # Why extraction is typed to the fixture
//!
//! [`extract`] takes a [`FakeGithub`] rather than a `&dyn PullRequestApi`. That
//! is the refusal, expressed in the signature: a benchmark runs only in trial
//! mode (ADR-0184, and #5801 made the fixture backend and the trial store one
//! mode), so a live backend must never reach this path — and a parameter it
//! cannot be passed to is a stronger statement than a check it could pass. The
//! fixture is production code as of #5786, so this costs no build shape.
//!
//! It also buys the work-order read. The `GithubApi` write surface is
//! comments-only by construction and its one read verb is
//! [`issue_title`](aether_bloomery_git::GithubApi::issue_title); a golden
//! task needs the issue *body*, and widening the shared trait to reach it would
//! hand the live adapter a verb only this path wants.

use std::error::Error;
use std::fmt;

use aether_bloomery::{ContentAddressed, Digest, digest_of};
use aether_bloomery_git::{ChecksState, GitDataApi, PullRequestApi, fixture::FakeGithub};

/// One golden task: a landed pull request read back as a runnable work order.
///
/// The bar is not a field. A landed pull request whose CI did not pass is not a
/// golden task at all, so "its landing CI" is a precondition [`extract`]
/// enforces rather than a column a report could read past — and `ChecksState`
/// carries no check names on the passing arm, so a stored copy would say
/// nothing the refusal does not.
///
/// A wire kind, not a plain value: a run's rendered state carries the tasks it
/// drew, and that document crosses a mailbox on its way to the operator.
#[aether_data::kind(name = "aether.bloomery.golden_task", eq)]
pub struct GoldenTask {
    /// The landed pull request this task was drawn from.
    pub pull_request: u64,
    /// The issue the pull request closed — where the work order was written.
    pub issue: u64,
    /// The work order: that issue's text, as the repository holds it. This is
    /// what every cell's bloom replays, byte for byte.
    pub order: String,
    /// The commit the landing proposed — the head whose tree is the reference
    /// answer.
    pub landed_head: String,
    /// The reference answer: the tree the landed head carries. Nothing here
    /// grades against it; it is recorded so a grading pass has the answer the
    /// run was measured against without re-reading a repository that has moved.
    pub reference: String,
}

impl ContentAddressed for GoldenTask {
    const DOMAIN: &'static str = "aether.bloomery.golden_task";
}

/// A versioned set of golden tasks — the unit within which cells compare.
///
/// # The decision ADR-0184 left open
///
/// > Benchmark tasks drawn from landed history age with the codebase: a
/// > replayed order references the tree as it was, so cells are comparable
/// > within a benchmark set, and sets are versioned by their base, not silently
/// > mixed.
///
/// **A set is a name, a base, and the tasks drawn against it — and the base is
/// the version.** Every task in a set replays against the same
/// [`base`](Self::base), so "the same task under four profiles" and "these two
/// tasks under one profile" are both statements inside one set, and neither is
/// ever a statement across two. A set does not carry each task's own pre-merge
/// base: it carries one, because a set whose members checked out different trees
/// is exactly the silent mixture the ADR refuses, and per-task bases would make
/// that representable.
///
/// The version is the base rather than a hand-assigned number for the reason the
/// ADR gives — what ages a benchmark task is the tree it references, so the tree
/// is the thing a reader has to be told. [`version`](Self::version) is the whole
/// set's content digest, which moves when the base moves *and* when the task
/// list does, so a report naming it pins exactly which comparison produced its
/// cells; the base alone would let two different task lists share a version.
#[aether_data::kind(name = "aether.bloomery.golden_task_set", eq)]
pub struct GoldenTaskSet {
    /// What an operator calls this set.
    pub name: String,
    /// The one base every task in the set replays against — the checkout, and
    /// the thing that ages.
    pub base: Digest,
    /// The tasks, in the order they were named.
    pub tasks: Vec<GoldenTask>,
}

impl ContentAddressed for GoldenTaskSet {
    const DOMAIN: &'static str = "aether.bloomery.golden_task_set";
}

impl GoldenTaskSet {
    /// This set's version: the content digest of the whole value.
    #[must_use]
    pub fn version(&self) -> Digest {
        digest_of(self)
    }
}

/// Why a landed pull request could not be read back as a golden task.
///
/// Every arm is a refusal rather than a defaulted field: a benchmark cell is
/// only worth as much as the task behind it, and a run that replayed an empty
/// work order, or one drawn from a pull request whose CI never passed, would
/// produce cells that look exactly like honest ones.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GoldenTaskError {
    /// The backend holds no pull request by that number.
    NoSuchPullRequest(u64),
    /// The pull request exists but never merged. Landed history is the whole
    /// source of a golden task; an open or rejected proposal has no landing CI
    /// and no reference answer.
    NotLanded(u64),
    /// The pull request's prose names no issue with a closing keyword, so there
    /// is no work order to replay.
    NoWorkOrder(u64),
    /// The named issue is absent, or its text is blank.
    EmptyWorkOrder {
        /// The pull request that named it.
        pull_request: u64,
        /// The issue it named.
        issue: u64,
    },
    /// The landing CI did not pass on the proposed head — so there is no bar.
    BarNotPassed {
        /// The pull request whose landing was read.
        pull_request: u64,
        /// How the checks actually stood.
        checks: ChecksState,
    },
    /// The proposed head names no commit the backend can read, so there is no
    /// reference answer.
    UnreadableHead {
        /// The pull request whose head was read.
        pull_request: u64,
        /// The adapter's own words.
        detail: String,
    },
}

impl fmt::Display for GoldenTaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchPullRequest(number) => write!(f, "the fixture repository holds no pull request {number}"),
            Self::NotLanded(number) => {
                write!(f, "pull request {number} never merged, so it is not landed history")
            }
            Self::NoWorkOrder(number) => {
                write!(f, "pull request {number} names no issue with a closing keyword, so it carries no work order")
            }
            Self::EmptyWorkOrder { pull_request, issue } => {
                write!(f, "pull request {pull_request} names issue {issue}, whose text is absent or blank")
            }
            Self::BarNotPassed { pull_request, checks } => {
                write!(f, "pull request {pull_request} landed with checks {checks:?}, so it sets no bar to clear")
            }
            Self::UnreadableHead { pull_request, detail } => {
                write!(f, "pull request {pull_request}'s landed head does not read: {detail}")
            }
        }
    }
}

impl Error for GoldenTaskError {}

/// Read the landed pull request `pull_request` back as a golden task.
///
/// # Errors
/// Any [`GoldenTaskError`] — the pull request is absent or unlanded, it names
/// no work order, the work order is blank, its landing CI did not pass, or its
/// head does not read.
pub fn extract(github: &FakeGithub, pull_request: u64) -> Result<GoldenTask, GoldenTaskError> {
    let Ok(Some(landed)) = github.get_pull_request(pull_request) else {
        return Err(GoldenTaskError::NoSuchPullRequest(pull_request));
    };
    if !landed.merged {
        return Err(GoldenTaskError::NotLanded(pull_request));
    }

    let body = github.pull_request_proposal(pull_request).map(|(_, body)| body).unwrap_or_default();
    let Some(issue) = closed_issue(&body) else {
        return Err(GoldenTaskError::NoWorkOrder(pull_request));
    };
    let order = github.issue_body(issue).unwrap_or_default();
    if order.trim().is_empty() {
        return Err(GoldenTaskError::EmptyWorkOrder { pull_request, issue });
    }

    match github.checks_for_ref(&landed.head_sha) {
        Ok(ChecksState::Passed) => {}
        Ok(checks) => return Err(GoldenTaskError::BarNotPassed { pull_request, checks }),
        Err(error) => {
            return Err(GoldenTaskError::UnreadableHead { pull_request, detail: error.to_string() });
        }
    }

    let head = github
        .get_commit(&landed.head_sha)
        .map_err(|error| GoldenTaskError::UnreadableHead { pull_request, detail: error.to_string() })?;

    Ok(GoldenTask { pull_request, issue, order, landed_head: landed.head_sha, reference: head.tree })
}

/// The closing keywords GitHub itself acts on, lowercased. A pull request that
/// uses one is stating which work order it answers, which is exactly what a
/// golden task needs; every other `#N` in a body is a cross-reference.
const CLOSING_KEYWORDS: [&str; 9] =
    ["close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved"];

/// The first issue a pull-request body closes, if any.
///
/// First rather than every one, because a golden task replays *one* work order:
/// a pull request that closed several is a run whose order would have to be a
/// concatenation nobody wrote, so the earliest stated closure is the task and
/// the rest are context. Case-insensitive, and the number must end the token —
/// a `Closes #58201` is not a task about issue 5820.
fn closed_issue(body: &str) -> Option<u64> {
    let words: Vec<&str> = body.split_whitespace().collect();
    words.windows(2).find_map(|pair| {
        let keyword = pair[0].trim_matches(|character: char| !character.is_ascii_alphabetic()).to_ascii_lowercase();
        if !CLOSING_KEYWORDS.contains(&keyword.as_str()) {
            return None;
        }
        let reference = pair[1].trim_start_matches(|character: char| !character.is_ascii_digit() && character != '#');
        reference.strip_prefix('#')?.trim_end_matches(|character: char| !character.is_ascii_digit()).parse().ok()
    })
}
