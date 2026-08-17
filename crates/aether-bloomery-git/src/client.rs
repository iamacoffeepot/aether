//! Port types and traits the git source backend is generic over.
//!
//! These are the surfaces [`crate::GitSource`] talks to — git-data, pull
//! requests, issues, and the Actions executor the in-process fake implements
//! — so a local git implementation can sit behind the same vocabulary without
//! depending on the GitHub REST adapter.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::marker::Marker;

/// A comment projection: its id, current body, and parsed marker.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Comment {
    /// The comment id.
    pub id: u64,
    /// The current body (contains the marker when projected).
    pub body: String,
    /// The parsed marker, if the body carries one.
    pub marker: Option<Marker>,
}

/// The fields to create a new comment on an issue.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NewComment {
    /// The issue to comment on.
    pub issue_number: u64,
    /// The comment body, marker included.
    pub body: String,
}

/// A check-run conclusion — the *inward* channel's input vocabulary (a
/// reviewer verdict / check run normalizes through
/// [`aether_bloomery::normalize_stage_result`]). Kept here because it is the shape the
/// inward normalizer maps from; no outward check-run is written this slice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckConclusion {
    /// The stage passed.
    Success,
    /// The stage failed.
    Failure,
    /// Neither pass nor fail (skipped, cancelled).
    Neutral,
}

/// A check-run as the inward channel would observe it. Present as the
/// normalizer's input type; not produced by the outward projection.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CheckRun {
    /// The native id.
    pub id: u64,
    /// The stable `external_id` carrying the bloomery marker.
    pub external_id: String,
    /// The check name.
    pub name: String,
    /// The concluded result.
    pub conclusion: CheckConclusion,
}

/// How the checks on a commit stand, folded from the check-run list.
///
/// What the land watch turns on: a landing proposal whose checks failed cannot
/// merge and never will without a repair, which is the case the watch used to
/// read as indistinguishable from "still running" and poll forever.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ChecksState {
    /// No check has reported against this commit. Indistinguishable from checks
    /// that have not been created yet, so it reads as pending, never as passing.
    Absent,
    /// At least one check is still queued or running.
    Pending,
    /// Every check completed, none with a failure-shaped conclusion.
    Passed,
    /// At least one check completed as failed, timed out, or was cancelled.
    /// Carries the failing check names — the findings a repair is directed by.
    Failed {
        /// The names of the checks that did not pass, in listing order.
        failing: Vec<String>,
    },
}

/// The fields to create a check-run (inward-channel shape; unused this slice).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NewCheckRun {
    /// The check name.
    pub name: String,
    /// The `external_id` carrying the marker.
    pub external_id: String,
    /// The concluded result.
    pub conclusion: CheckConclusion,
}

/// A pull request as the land path reads it (ADR-0149 §The bloom). Bloomery
/// proposes a resolved bloom by opening one and admits its landing when it
/// observes the merge, so the fields here are the ones that decision turns on
/// — not a faithful mirror of the API object.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PullRequest {
    /// The pull request number — the handle the land watch re-reads it by.
    pub number: u64,
    /// The sha at the head of the proposing branch.
    pub head_sha: String,
    /// The branch proposing the change. What tells a landing pull request the
    /// coordinator opened from a human-flow one that happens to be numbered
    /// nearby: only a bloom's own landing branch proposes a bloom's landing.
    pub head_ref: String,
    /// The branch being merged into (`main` for a landing).
    pub base: String,
    /// Whether it is still open.
    pub state: PullRequestState,
    /// Whether it merged. Distinct from a `Closed` `state`: a closed pull
    /// request that never merged is a rejection, and the two terminal states
    /// land a bloom in different places.
    pub merged: bool,
    /// The commit mainline actually became, present only once
    /// [`merged`](Self::merged) is true.
    ///
    /// GitHub also populates the underlying field on an **open** pull request,
    /// where it names a throwaway *test-merge* commit that is not on any
    /// branch. Reading that as a landing would record a mainline head that
    /// exists nowhere, so the decode blanks it unless the pull request merged
    /// and this stays `None` until then.
    pub merge_commit_sha: Option<String>,
}

/// Whether a pull request is still open. GitHub's `state` is exactly these two
/// values; "merged" is a separate boolean, not a third state, which is why
/// [`PullRequest::merged`] is its own field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PullRequestState {
    /// Still open — the land watch keeps waiting.
    Open,
    /// Closed, merged or not.
    Closed,
}

/// The fields to open a pull request with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NewPullRequest {
    /// The title.
    pub title: String,
    /// The body.
    pub body: String,
    /// The proposing branch, in the short `heads/…`-less form the API expects
    /// for a same-repo pull request (e.g. `bloomery/land/<bloom>`).
    pub head: String,
    /// The branch to merge into.
    pub base: String,
}

/// A git ref as the source port reads and writes it: its short name (the
/// `heads/…` form, no leading `refs/`) and the 40/64-hex object sha it targets.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitRef {
    /// The ref name in `heads/…` form (no `refs/` prefix).
    pub name: String,
    /// The object sha the ref points at.
    pub sha: String,
}

/// A git commit object: its own sha, the tree sha it carries, and its message.
/// The source port reads a commit to derive a snapshot's tree, creates one to
/// advance an integration branch to a candidate tree, and (the claim registry)
/// carries a claiming bloom id on a parseable message line rather than in the
/// tree — the tree of a claim commit is always the well-known empty tree.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitCommit {
    /// The commit object sha.
    pub sha: String,
    /// The tree sha this commit points at.
    pub tree: String,
    /// The commit message.
    pub message: String,
}

/// The GitHub client contract the projection depends on. Both the real
/// ReqwestGithub and the test `FakeGithub` implement it,
/// so the projection logic is exercised without a token or network.
///
/// **Comments only on the write side.** There is deliberately no verb here that
/// writes an issue or pull-request title or body, opens an object, or closes
/// one: a projection owns the marker-keyed comments it wrote and nothing else,
/// and that bound holds by absence rather than by discipline (ADR-0149 §The
/// write surface). Closing a landed member's source issue lives on
/// [`IssueStateApi`] so nothing reachable from a projection can issue it.
/// Every lookup is scoped to one named object, so no path here enumerates
/// repository-wide issue history either.
pub trait GithubApi {
    /// The title of issue `number`, or `None` when the repository holds no such
    /// object — a clean 404 is `Ok(None)`, not an error.
    ///
    /// A read, and the bound above is about writes: the landing assembly falls
    /// back to a member's source issue title when its lane named no commit
    /// message, so the adapter has to be able to see the one it is falling back
    /// to. Nothing here writes it.
    ///
    /// # Errors
    /// The surface is unreachable or returned a non-404 error status.
    fn issue_title(&self, number: u64) -> Result<Option<String>, GithubError>;

    /// Find the comment on `issue_number` whose marker carries `key`, if any.
    /// The projection's idempotency lookup: a match with the desired digest is
    /// a no-op, a mismatch an update, `None` a create.
    ///
    /// # Errors
    /// The projection surface is unreachable or returned an error status — a
    /// `Status { status: 404, .. }` is the object being absent, which the
    /// projection records and skips rather than re-driving.
    fn find_comment(&self, issue_number: u64, key: &str) -> Result<Option<Comment>, GithubError>;

    /// Add a comment to an issue.
    ///
    /// # Errors
    /// The projection surface is unreachable or returned an error status.
    fn create_comment(&self, new: &NewComment) -> Result<Comment, GithubError>;

    /// Overwrite a comment's body.
    ///
    /// # Errors
    /// The projection surface is unreachable or returned an error status.
    fn update_comment(&self, comment_id: u64, body: &str) -> Result<(), GithubError>;
}

/// The issue-state surface the land reactor drives after a bloom lands.
///
/// A sibling of [`GithubApi`] rather than an extension of it: the projection
/// must not close anything (ADR-0149 §The write surface), and that bound holds
/// by absence — this verb lives here so nothing reachable from a projection
/// can issue it. The land reactor is the caller: GitHub closing keywords only
/// fire on a default-branch merge, so a day-branch land has to close the
/// member's source issue itself.
pub trait IssueStateApi {
    /// Close issue `number`. An already-closed issue is a success.
    ///
    /// # Errors
    /// The surface is unreachable, the issue is absent, or the write was refused.
    fn close_issue(&self, number: u64) -> Result<(), GithubError>;
}

/// The Git Data REST surface the source port drives (blob/tree/commit/ref over
/// HTTP, no working copy on disk — ADR-0149's git source port, [#3465]). A
/// sibling of [`GithubApi`] rather than an extension of it: the projection has
/// no use for refs and the source port has no use for issues, so segregating
/// the two keeps each backend generic over only the surface it touches. Both
/// ReqwestGithub and the test `FakeGithub` implement it, so the source
/// backend is exercised with no token or network.
///
/// Ref names are the short `heads/…` form (no leading `refs/`); the client
/// prepends `refs/` only where the create endpoint requires the full form.
///
/// [#3465]: https://github.com/iamacoffeepot/aether/issues/3465
pub trait GitDataApi {
    /// Read the ref named `name` (`heads/…` form), or `None` if it does not
    /// exist — a clean 404 is `Ok(None)`, not an error.
    ///
    /// # Errors
    /// A transport fault or a non-404 error status.
    fn get_ref(&self, name: &str) -> Result<Option<GitRef>, GithubError>;

    /// Create ref `name` at `sha`.
    ///
    /// # Errors
    /// A transport fault or an error status (e.g. the ref already exists).
    fn create_ref(&self, name: &str, sha: &str) -> Result<GitRef, GithubError>;

    /// Move ref `name` to `sha`. With `force` false the update is
    /// fast-forward-only — GitHub rejects a non-fast-forward with a 422, the
    /// compare-and-swap guard the source port's `land` and `integrate` rely on.
    ///
    /// # Errors
    /// A transport fault or an error status — a `Status { status: 422, .. }`
    /// is the non-fast-forward refusal a caller maps to its CAS-lost outcome.
    fn update_ref(&self, name: &str, sha: &str, force: bool) -> Result<GitRef, GithubError>;

    /// Delete ref `name` (`heads/…` short form). A ref that is already gone — a
    /// 404 or the 422 GitHub answers for a non-existent ref — is the clean
    /// idempotent `Ok(())`, not a fault: release's name-only cleanup delete runs
    /// after a tombstone CAS and an acquire's rollback re-deletes freely.
    ///
    /// # Errors
    /// A transport fault or an error status other than the already-gone 404/422.
    fn delete_ref(&self, name: &str) -> Result<(), GithubError>;

    /// List every ref under `prefix` (`heads/…` form) — the enumeration a
    /// successor bloom walks to reuse checkpoints drift did not invalidate.
    ///
    /// # Errors
    /// A transport fault or an error status.
    fn list_matching_refs(&self, prefix: &str) -> Result<Vec<GitRef>, GithubError>;

    /// Read commit object `sha` (for its tree).
    ///
    /// # Errors
    /// A transport fault or an error status.
    fn get_commit(&self, sha: &str) -> Result<GitCommit, GithubError>;

    /// Create a commit carrying `tree` with `parents`.
    ///
    /// # Errors
    /// A transport fault or an error status.
    fn create_commit(&self, message: &str, tree: &str, parents: &[String]) -> Result<GitCommit, GithubError>;

    /// Whether `ancestor` is reachable from `commit` — the ancestry an
    /// observation uses to refuse a stale or sideways mainline head (#4938).
    ///
    /// Equal shas are ancestors of themselves. A missing object or a
    /// transport fault is an error, not a silent `false`: the caller would
    /// otherwise treat an unreachable compare as a divergence and refuse a
    /// head it could not actually classify.
    ///
    /// # Errors
    /// A transport fault or an error status (including a 404 for an unknown sha).
    fn is_ancestor(&self, ancestor: &str, commit: &str) -> Result<bool, GithubError>;

    /// Merge commit `head` into branch `base` server-side, both in the short
    /// `heads/…` / branch-name form the merge endpoint takes.
    ///
    /// The composing counterpart to [`create_commit`](Self::create_commit): that
    /// one *states* a tree, this one *combines* two histories. A candidate built
    /// against one base and a branch that has moved past it have no common tree
    /// to state, only a common ancestor to merge from — so folding several
    /// members, or catching a bloom up to a mainline that advanced, has to go
    /// through here. Stating the candidate's tree onto a moved branch would
    /// produce a clean commit that silently reverts everything the branch gained
    /// in between.
    ///
    /// A conflict is an [`Ok`] outcome, not an error: it is a fact about the two
    /// histories that a caller parks on or routes back for repair, not a
    /// transport fault to retry.
    ///
    /// # Errors
    /// A transport fault, a missing base or head, or a non-conflict error status.
    fn merge(&self, base: &str, head: &str, message: &str) -> Result<MergeResult, GithubError>;
}

/// What a server-side merge did.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MergeResult {
    /// The histories combined into a new merge commit.
    Merged(GitCommit),
    /// `base` already contained `head`, so there was nothing to merge. Distinct
    /// from [`Merged`](Self::Merged) because no commit was created — a fold that
    /// treated this as a failure would stall on an already-folded member, and
    /// one that treated it as a merge would invent a head that does not exist.
    AlreadyUpToDate,
    /// The two histories touch the same lines and git cannot combine them
    /// unattended. Carries the merge's own report of what collided; the caller
    /// decides whether that is an owner decision or repair work.
    Conflict {
        /// The endpoint's description of the collision.
        detail: String,
        /// Paths the merge named as colliding. Empty when neither the merge
        /// nor a follow-up compare named any.
        paths: Vec<String>,
        /// Unified diff of `head` against `base`. Empty when the compare
        /// produced none.
        patch: String,
    },
}

/// What asking the source to merge a pull request did.
///
/// A refusal is an outcome here rather than a [`GithubError`], because the two
/// statuses this folds are decisions and not faults: nothing about them gets
/// better by re-driving the same request, and the caller has to journal them
/// rather than retry them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PullMergeResult {
    /// Merged. The base branch became `merge_commit_sha` — under a squash that
    /// is a commit no branch previously carried.
    Merged {
        /// The commit the base branch became.
        merge_commit_sha: String,
    },
    /// Refused. GitHub answers 405 when the pull request is not in a mergeable
    /// state (a conflict with the base, a protection rule) and 409 when the
    /// head has moved off the sha the merge was guarded by.
    Refused {
        /// The refusing status, so a reader can tell the two apart.
        status: u16,
        /// The refusal body, kept verbatim for the journal.
        detail: String,
    },
}

/// The pull-request surface the land path drives — Bloomery proposes a
/// resolved bloom by opening one and merges the proposal it opened once the
/// structural gates hold (ADR-0149 §The bloom, ADR-0186, issue #4953).
///
/// A third sibling of [`GithubApi`] and [`GitDataApi`] for the reason those two
/// are separate: the projection has no use for pull requests and the land path
/// has no use for issues, so each backend stays generic over only the surface
/// it touches. Both ReqwestGithub and the test `FakeGithub` implement it,
/// so the land path is exercised with no token and no network.
pub trait PullRequestApi {
    /// Open a pull request.
    ///
    /// # Errors
    /// A transport fault or an error status — a `Status { status: 422, .. }`
    /// is GitHub's refusal for a duplicate head or an empty diff, which the
    /// land path distinguishes by looking the existing one up.
    fn create_pull_request(&self, new: &NewPullRequest) -> Result<PullRequest, GithubError>;

    /// Read pull request `number`, or `None` if it does not exist — a clean 404
    /// is `Ok(None)`, not an error, matching [`GitDataApi::get_ref`].
    ///
    /// # Errors
    /// A transport fault or a non-404 error status.
    fn get_pull_request(&self, number: u64) -> Result<Option<PullRequest>, GithubError>;

    /// Find the most recent pull request proposing `head`, **in any state**, if
    /// one exists. What makes opening a landing idempotent: a re-drained land
    /// entry adopts the pull request it already opened instead of opening a
    /// second one.
    ///
    /// Any state, not just open, and that is load-bearing. A landing branch is
    /// per-bloom and Bloomery-owned, so whatever pull request sits on it *is*
    /// that bloom's landing proposal whatever has happened to it — and the
    /// states a watch most needs to find it in are exactly the ones it is no
    /// longer open in. Filtering to open would make a merged proposal invisible
    /// to the next poll, which would then open a fresh one instead of observing
    /// the landing that already happened.
    ///
    /// # Errors
    /// A transport fault or an error status.
    fn find_pull_request_for_head(&self, head: &str) -> Result<Option<PullRequest>, GithubError>;

    /// How the checks on commit `sha` stand.
    ///
    /// # Errors
    /// The source surface is unreachable or returned an error status.
    fn checks_for_ref(&self, sha: &str) -> Result<ChecksState, GithubError>;

    /// Squash-merge pull request `number`, guarded by `expected_head_sha`.
    ///
    /// The guard is the merge endpoint's own `sha` parameter, which GitHub
    /// refuses with a 409 when the head has moved off it — a compare-and-swap
    /// on the proposing branch, decided by the server between the caller's read
    /// and its write rather than by the caller's own earlier look.
    ///
    /// Squash because that is what this repository's mainline takes: the
    /// proposal's title becomes the commit subject, which is why the landing
    /// assembly authors one at all. No commit title is sent, so the subject is
    /// the one GitHub composes from the proposal — byte for byte what a person
    /// pressing the button produces.
    ///
    /// # Errors
    /// A transport fault or an error status other than the 405 / 409 refusals,
    /// which are [`PullMergeResult::Refused`].
    fn squash_merge_pull_request(&self, number: u64, expected_head_sha: &str) -> Result<PullMergeResult, GithubError>;
}

/// A client or transport failure. A clean not-found is `Ok(None)` at the API
/// layer, not an error; this type is a genuine transport fault or a non-2xx
/// status.
#[derive(Debug)]
pub enum GithubError {
    /// A non-2xx response.
    Status {
        /// The HTTP status code.
        status: u16,
        /// The response body (truncated by GitHub, kept for diagnostics).
        body: String,
    },
    /// The transport itself failed (DNS, connect, TLS, timeout).
    Transport(String),
    /// A 2xx response whose body did not decode as expected.
    Decode(String),
    /// A paginated list walk hit the `MAX_LIST_PAGES` cap without reaching a
    /// short (final) page, so the enumeration is incomplete. Surfaced as an
    /// error rather than folded into a `Ok(None)` / silently truncated `Ok`,
    /// which would misreport a not-yet-searched item as absent.
    PaginationExhausted {
        /// What was being listed (for diagnostics).
        what: String,
    },
}

impl fmt::Display for GithubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status { status, body } => write!(f, "github returned status {status}: {body}"),
            Self::PaginationExhausted { what } => {
                write!(f, "github pagination exhausted listing {what} without a final page")
            }
            Self::Transport(msg) => write!(f, "github transport error: {msg}"),
            Self::Decode(msg) => write!(f, "github response decode error: {msg}"),
        }
    }
}

impl Error for GithubError {}

/// A workflow run's lifecycle state, folded from GitHub's `status` field. The
/// pre-run states (queued / waiting / requested / pending) all collapse to
/// [`RunStatus::Queued`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunStatus {
    /// Dispatched but not yet started.
    Queued,
    /// In progress.
    InProgress,
    /// Finished — carries a [`RunConclusion`].
    Completed,
}

/// A completed run's conclusion, folded from GitHub's `conclusion` field onto
/// the four the executor distinguishes (the failure-shaped conclusions —
/// `timed_out` / `stale` / `startup_failure` — collapse to
/// [`RunConclusion::Failure`], the neither-shaped — `skipped` /
/// `action_required` — to [`RunConclusion::Neutral`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunConclusion {
    /// The run succeeded.
    Success,
    /// The run failed.
    Failure,
    /// Neither pass nor fail.
    Neutral,
    /// The run was cancelled.
    Cancelled,
}

/// A workflow run as the executor reads it: its id, the display title (the
/// wrapper's `run-name`, carrying the nonce), and its folded lifecycle state.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorkflowRun {
    /// The run's native id — the handle `get_run` / `cancel_run` /
    /// `list_run_artifacts` take.
    pub id: u64,
    /// The run's display title (the wrapper sets `run-name` to embed the
    /// nonce, so `find_run` matches against this).
    pub display_title: String,
    /// The folded lifecycle state.
    pub status: RunStatus,
    /// The folded conclusion, present once `status` is [`RunStatus::Completed`].
    pub conclusion: Option<RunConclusion>,
}

/// One artifact a run uploaded: its id, name, and size.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Artifact {
    /// The artifact's native id, for a later fetch.
    pub id: u64,
    /// The artifact name (carries the nonce so `stream_evidence` can filter).
    pub name: String,
    /// The artifact's size in bytes.
    pub size_bytes: u64,
}

/// Does `name` carry `nonce` as a delimiter-bounded segment? The wrapper embeds
/// the nonce in a run's display title and in each artifact's name between
/// non-alphanumeric delimiters (or at a name edge, e.g. `evidence-{nonce}-log`).
/// A raw `contains` would let a nonce that is a prefix of a longer one (`n-1`
/// inside `n-12`) resolve an unrelated concern's run or pull its evidence, so a
/// match counts only when the character on each side of the occurrence is a
/// boundary — a non-alphanumeric character or the string's edge. The nonce
/// itself may contain `-`, so a split-on-delimiter test would over-segment it;
/// bounding each occurrence is the delimiter-safe form. The one nonce-matching
/// predicate both the run resolution (`find_run`) and the artifact filter
/// (`stream_evidence`) share, so the two sides cannot drift (#3662).
pub fn name_carries_nonce(name: &str, nonce: &str) -> bool {
    if nonce.is_empty() {
        return false;
    }
    name.match_indices(nonce).any(|(start, matched)| {
        let before_is_boundary = name[..start].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric());
        let after_is_boundary = name[start + matched.len()..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric());
        before_is_boundary && after_is_boundary
    })
}

/// The GitHub Actions REST surface the executor port drives — `workflow_dispatch`
/// plus the run + artifacts API. A sibling of [`GithubApi`] / [`GitDataApi`]
/// (each backend is generic over only the surface it touches), implemented by
/// both ReqwestGithub and the test `FakeGithub` so the executor is
/// exercised with no token or network.
///
/// `workflow_dispatch` answers `204 No Content` with no run id, so there is no
/// "return the dispatched run" method: the executor resolves nonce → run
/// through [`find_run`](ActionsApi::find_run), matching the nonce the wrapper
/// embeds in the run's name.
pub trait ActionsApi {
    /// Dispatch `workflow_file` at `git_ref` with the string `inputs` — the
    /// `POST …/actions/workflows/{file}/dispatches` that answers `204`.
    ///
    /// # Errors
    /// A transport fault or an error status (e.g. the ref or workflow is
    /// unknown).
    fn dispatch_workflow(
        &self,
        workflow_file: &str,
        git_ref: &str,
        inputs: &BTreeMap<String, String>,
    ) -> Result<(), GithubError>;

    /// Find the most recent run of `workflow_file` whose name embeds `nonce`,
    /// or `None` if none has appeared yet — the nonce → run resolution the
    /// nonce-as-handle design rests on.
    ///
    /// # Errors
    /// A transport fault or an error status.
    fn find_run(&self, workflow_file: &str, nonce: &str) -> Result<Option<WorkflowRun>, GithubError>;

    /// Read run `run_id` (for its status + conclusion).
    ///
    /// # Errors
    /// A transport fault or an error status.
    fn get_run(&self, run_id: u64) -> Result<WorkflowRun, GithubError>;

    /// Cancel run `run_id` — the `POST …/runs/{id}/cancel`.
    ///
    /// # Errors
    /// A transport fault or an error status.
    fn cancel_run(&self, run_id: u64) -> Result<(), GithubError>;

    /// List run `run_id`'s uploaded artifacts.
    ///
    /// # Errors
    /// A transport fault or an error status.
    fn list_run_artifacts(&self, run_id: u64) -> Result<Vec<Artifact>, GithubError>;
}

/// Drop a ref prefix, leaving the bare branch name the repository-level
/// endpoints take. Accepts both the fully-qualified `refs/heads/…` a push
/// target carries and this trait's `heads/…` shorthand, because callers hold
/// one or the other depending on which side of the port they came from — and a
/// prefix left on does not fail loudly, it addresses a branch that does not
/// exist. A name carrying neither (a raw commit sha, which the merge endpoint
/// also accepts as a head) passes through.
pub fn strip_heads(name: &str) -> &str {
    name.strip_prefix("refs/heads/").or_else(|| name.strip_prefix("heads/")).unwrap_or(name)
}
