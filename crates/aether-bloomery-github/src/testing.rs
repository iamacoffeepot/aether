//! An in-process fake GitHub (#3459 step 4).
//!
//! Models the projection's object store — the objects a repository already
//! holds and the comments hanging off them — with enough fidelity to drive the
//! projection end-to-end with no token and no network: [`seed_issue`] and
//! [`seed_pull_request`] present pre-existing targets (the projection opens
//! none), find scans marker keys exactly as the real client does, a comment on
//! an unknown number answers the same 404-shaped refusal the real surface
//! does, and [`delete_comment`] models an operator deleting a projection so the
//! rebuild property (delete → reappear) is exercisable.
//!
//! Compiled for this crate's own tests unconditionally and, behind the
//! `testing` feature, exported so the host demo (#3459 step 7) drives the same
//! double.
//!
//! [`seed_issue`]: FakeGithub::seed_issue
//! [`seed_pull_request`]: FakeGithub::seed_pull_request
//! [`delete_comment`]: FakeGithub::delete_comment

// The fake holds its `Mutex` guard to the end of each short method rather than
// dropping it a line early: this is an in-memory test double with no
// contention, so the nursery lint's early-drop rewrite buys nothing and only
// clutters the store methods.
#![allow(clippy::significant_drop_tightening)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use aether_bloomery::{BackendObjectId, BloomId, Correspondence, CorrespondenceError, Digest};
use sha2::{Digest as _, Sha256};

use crate::client::{
    ActionsApi, Artifact, ChecksState, Comment, GitCommit, GitDataApi, GitRef, GithubApi, GithubError, IssueStateApi,
    MergeResult, NewComment, NewPullRequest, PullRequest, PullRequestApi, PullRequestState, RunConclusion, RunStatus,
    WorkflowRun, strip_heads,
};
use crate::correspondence::GitObjectId;
use crate::executor::INPUT_NONCE;
use crate::marker::parse_marker;
use crate::source::{EMPTY_TREE, digest_from_hex, render_claim_message, render_tombstone_message, to_hex};

#[derive(Clone)]
struct StoredIssue {
    number: u64,
    /// The object's own title — human-authored, and never written by anything
    /// here. Read by the landing assembly as the fallback for a member whose
    /// lane named no commit message.
    title: String,
    /// The object's own body — human-authored, and never written by the
    /// projection. Held so a test can prove it comes back unchanged.
    body: String,
    /// Closed by the land reactor after a bloom lands. Seeded open.
    closed: bool,
}

#[derive(Clone)]
struct StoredComment {
    id: u64,
    issue_number: u64,
    body: String,
}

#[derive(Clone)]
struct StoredDispatch {
    workflow_file: String,
    git_ref: String,
    nonce: String,
    inputs: BTreeMap<String, String>,
}

#[derive(Clone)]
struct StoredRun {
    id: u64,
    nonce: String,
    display_title: String,
    status: RunStatus,
    conclusion: Option<RunConclusion>,
    artifacts: Vec<Artifact>,
}

#[derive(Clone)]
struct StoredCommit {
    tree: String,
    message: String,
    // The commit graph, needed only so `merge` can answer "already contains"
    // by ancestry rather than by tree equality.
    parents: Vec<String>,
}

#[derive(Clone)]
struct StoredPullRequest {
    number: u64,
    /// The title and body the proposal was opened with. Neither crosses the
    /// [`PullRequest`] projection — the land watch reads a proposal's state, not
    /// its prose — so they are held here for the assembly assertions.
    title: String,
    body: String,
    head: String,
    head_sha: String,
    base: String,
    state: PullRequestState,
    merged: bool,
    merge_commit_sha: Option<String>,
}

impl StoredPullRequest {
    fn project(&self) -> PullRequest {
        PullRequest {
            number: self.number,
            head_sha: self.head_sha.clone(),
            base: self.base.clone(),
            state: self.state,
            merged: self.merged,
            merge_commit_sha: self.merge_commit_sha.clone(),
        }
    }
}

#[derive(Default)]
struct State {
    next_comment: u64,
    issues: Vec<StoredIssue>,
    comments: Vec<StoredComment>,
    // The git object store the source port drives: ref name (`heads/…`) → sha,
    // and commit sha → tree sha. Trees themselves are opaque shas the port
    // treats as digest-addressed handles, so they need no separate store.
    refs: HashMap<String, String>,
    commits: HashMap<String, StoredCommit>,
    // Merges armed to conflict, as (base, head) bare branch names. A fake holds
    // no real file content, so a collision cannot arise from what is seeded and
    // has to be stated outright.
    merge_conflicts: HashSet<(String, String)>,
    // The Actions surface the executor port drives: recorded dispatches (a
    // `workflow_dispatch` creates no resolvable run synchronously, so a
    // dispatched-but-unseeded nonce inspects `Unknown`), and the runs a test
    // seeds — keyed by the nonce the wrapper would embed in the run name.
    next_run: u64,
    dispatches: Vec<StoredDispatch>,
    runs: Vec<StoredRun>,
    // The checks a landing watch reads, keyed by head sha. Unseeded means no
    // check reported, which the fold reads as `Absent` — the same thing the
    // real listing returns for a commit no workflow has picked up yet.
    checks: HashMap<String, ChecksState>,
    // The pull-request surface the land path drives: opened proposals, keyed
    // for lookup by head branch the way the real list endpoint filters.
    next_pull_request: u64,
    pull_requests: Vec<StoredPullRequest>,
    // The git-object↔bloom-digest correspondence the source/executor ports
    // resolve real git shas through (ADR-0150), keyed forward by the 32-byte
    // digest; the reverse direction scans for the matching object (a test store
    // is small).
    correspondence: HashMap<[u8; 32], BackendObjectId>,
    // The repository commit objects are minted in and read from, when the fake
    // is backed by a real one (`with_object_repo`). `None` keeps the synthetic
    // in-memory shas.
    object_repo: Option<PathBuf>,
}

/// An in-memory GitHub double implementing [`GithubApi`].
///
/// Cloning shares one backing store (an `Arc<Mutex<…>>`), so a demo can hand a
/// clone to a [`GithubProjection`](crate::GithubProjection) and keep another to
/// introspect what the projection wrote.
#[derive(Default, Clone)]
pub struct FakeGithub {
    state: Arc<Mutex<State>>,
}

impl FakeGithub {
    /// A fresh, empty fake.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint and read commit objects in the real repository at `repo`, instead
    /// of the synthetic in-memory shas.
    ///
    /// What the fake stands in for is **GitHub**, not git. Refs, pull requests,
    /// workflow runs and the correspondence are the parts a token buys, and they
    /// stay in memory. Commit and tree objects are not: a caller that checks a
    /// fold out with a real `git worktree add` needs the object to be genuinely
    /// present in a genuine object database, and a synthetic sha256 sha is a
    /// name git will never resolve. The lane-boundary harness (#4732) is exactly
    /// that caller — its aggregate lanes check out the fold this fake produces —
    /// so pointing the fake at its scratch repository is what closes the loop
    /// between the two.
    ///
    /// Off by default: an in-memory-only test wants neither the shell-outs nor a
    /// repository to point at, and the synthetic shas round-trip through
    /// `crate::source::digest_from_hex` exactly as before.
    #[must_use]
    pub fn with_object_repo(self, repo: impl Into<PathBuf>) -> Self {
        self.lock().object_repo = Some(repo.into());
        self
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The repository backing commit objects, if any. Cloned out rather than
    /// borrowed so the caller shells git without holding the state lock.
    fn object_repo(&self) -> Option<PathBuf> {
        self.lock().object_repo.clone()
    }

    /// Present an issue the repository already holds, numbered `number` and
    /// carrying the human-authored `body` — the target a workpiece named
    /// `issue-<number>` addresses. The projection opens no object, so a test
    /// that wants one places it here.
    pub fn seed_issue(&self, number: u64, body: &str) {
        self.lock().issues.push(StoredIssue { number, title: String::new(), body: body.to_owned(), closed: false });
    }

    /// Present an issue carrying a human-authored `title` as well as its body —
    /// what the landing assembly falls back to when a member's lane named no
    /// commit message.
    pub fn seed_issue_with_title(&self, number: u64, title: &str, body: &str) {
        self.lock().issues.push(StoredIssue { number, title: title.to_owned(), body: body.to_owned(), closed: false });
    }

    /// Present a pull request the repository already holds, numbered `number`
    /// and proposing `head` (the short `bloom/…` branch form). Findable by head
    /// like an opened one, and commentable like an issue — GitHub numbers both
    /// from one sequence and shares the comment route.
    pub fn seed_pull_request(&self, number: u64, head: &str) {
        let mut state = self.lock();
        // Keep the generator ahead of anything seeded, so a later
        // `create_pull_request` cannot mint a number that already exists.
        state.next_pull_request = state.next_pull_request.max(number);
        state.pull_requests.push(StoredPullRequest {
            number,
            title: String::new(),
            body: String::new(),
            head: head.to_owned(),
            head_sha: String::new(),
            base: "main".to_owned(),
            state: PullRequestState::Open,
            merged: false,
            merge_commit_sha: None,
        });
    }

    /// How many issues currently exist — what a projection must never move.
    #[must_use]
    pub fn issue_count(&self) -> usize {
        self.lock().issues.len()
    }

    /// How many comments currently exist across all issues.
    #[must_use]
    pub fn comment_count(&self) -> usize {
        self.lock().comments.len()
    }

    /// The current issue numbers, ascending.
    #[must_use]
    pub fn issue_numbers(&self) -> Vec<u64> {
        let mut numbers: Vec<u64> = self.lock().issues.iter().map(|issue| issue.number).collect();
        numbers.sort_unstable();
        numbers
    }

    /// The current body of issue `number`, if it exists.
    #[must_use]
    pub fn issue_body(&self, number: u64) -> Option<String> {
        self.lock().issues.iter().find(|issue| issue.number == number).map(|issue| issue.body.clone())
    }

    /// Whether issue `number` is closed, or `None` when the repository holds no
    /// such object — what a land-path test asserts after a bloom lands.
    #[must_use]
    pub fn issue_is_closed(&self, number: u64) -> Option<bool> {
        self.lock().issues.iter().find(|issue| issue.number == number).map(|issue| issue.closed)
    }

    /// The bodies of the comments on `number`, in creation order.
    #[must_use]
    pub fn comments_on(&self, number: u64) -> Vec<String> {
        self.lock()
            .comments
            .iter()
            .filter(|comment| comment.issue_number == number)
            .map(|comment| comment.body.clone())
            .collect()
    }

    /// The ids of the comments on `number`, in creation order — what a delete →
    /// reappear test names its victim by.
    #[must_use]
    pub fn comment_ids_on(&self, number: u64) -> Vec<u64> {
        self.lock().comments.iter().filter(|comment| comment.issue_number == number).map(|comment| comment.id).collect()
    }

    /// Delete comment `id` — an operator removing a projection. The next
    /// reconcile finds no marker and writes it again.
    pub fn delete_comment(&self, id: u64) {
        self.lock().comments.retain(|comment| comment.id != id);
    }

    /// Delete issue `number` and its comments — the object going away entirely,
    /// which the projection reads as a target it must skip rather than
    /// recreate.
    pub fn delete_issue(&self, number: u64) {
        let mut state = self.lock();
        state.issues.retain(|issue| issue.number != number);
        state.comments.retain(|comment| comment.issue_number != number);
    }

    /// Merge pull request `number` at `merge_commit_sha` — the fake's stand-in
    /// for the person who merges a landing proposal. Bloomery never merges (the
    /// client has no verb for it), so a land-watch test cannot reach this state
    /// through the port and drives it from here instead.
    ///
    /// The sha is the caller's to choose precisely because a squash merge — what
    /// this repository requires — produces a mainline commit that is *not* the
    /// head Bloomery proposed. A test that passes the proposed head back would
    /// quietly assert away the one distinction the land watch has to get right.
    pub fn merge_pull_request(&self, number: u64, merge_commit_sha: &str) {
        let mut state = self.lock();
        if let Some(pull) = state.pull_requests.iter_mut().find(|pull| pull.number == number) {
            pull.state = PullRequestState::Closed;
            pull.merged = true;
            pull.merge_commit_sha = Some(merge_commit_sha.to_owned());
        }
    }

    /// Close pull request `number` without merging — the operator declining a
    /// landing proposal, the other terminal a land watch must recognize.
    pub fn close_pull_request(&self, number: u64) {
        let mut state = self.lock();
        if let Some(pull) = state.pull_requests.iter_mut().find(|pull| pull.number == number) {
            pull.state = PullRequestState::Closed;
        }
    }

    /// Seed a commit object carrying `tree_sha` (no parents) and return its
    /// sha — a source-port test's way to place a base commit the port can
    /// snapshot or a namespace can be created on.
    #[must_use]
    pub fn seed_commit(&self, tree_sha: &str) -> String {
        self.seed_commit_with_message("seed", tree_sha)
    }

    /// Seed a commit object carrying `tree_sha` and an explicit `message` (no
    /// parents) and return its sha — a claim-registry test's way to place a
    /// commit directly at the empty-tree + `Bloom-Id` message convention,
    /// sidestepping `claim_seal`.
    #[must_use]
    pub fn seed_commit_with_message(&self, message: &str, tree_sha: &str) -> String {
        let sha = commit_sha(message, tree_sha, &[]);
        self.lock().commits.insert(
            sha.clone(),
            StoredCommit { tree: tree_sha.to_owned(), message: message.to_owned(), parents: Vec::new() },
        );
        sha
    }

    /// Seed a commit at an explicit `sha` (for `b0b0…` base bootstrapping, #4732).
    pub fn seed_commit_at(&self, sha: &str, tree_hex: &str) {
        self.lock().commits.insert(
            sha.to_owned(),
            StoredCommit { tree: tree_hex.to_owned(), message: "seed".to_owned(), parents: Vec::new() },
        );
    }

    /// Seed a ref (`heads/…` form) pointing at `sha`.
    pub fn seed_ref(&self, name: &str, sha: &str) {
        self.lock().refs.insert(name.to_owned(), sha.to_owned());
    }

    /// The head sha of pull request `number` — what a landing watch reads its
    /// checks against.
    #[must_use]
    pub fn pull_request_head_sha(&self, number: u64) -> Option<String> {
        self.lock().pull_requests.iter().find(|pull| pull.number == number).map(|pull| pull.head_sha.clone())
    }

    /// The title and body pull request `number` was opened with — the landing
    /// proposal's assembled prose, which nothing on the read path projects.
    #[must_use]
    pub fn pull_request_proposal(&self, number: u64) -> Option<(String, String)> {
        self.lock()
            .pull_requests
            .iter()
            .find(|pull| pull.number == number)
            .map(|pull| (pull.title.clone(), pull.body.clone()))
    }

    /// Seed how the checks on commit `sha` stand — what a landing watch reads
    /// to tell a proposal that is still running from one that cannot merge.
    pub fn seed_checks(&self, sha: &str, checks: ChecksState) {
        self.lock().checks.insert(sha.to_owned(), checks);
    }

    /// Whether ref `name` (`heads/…` form) currently exists.
    #[must_use]
    pub fn ref_exists(&self, name: &str) -> bool {
        self.lock().refs.contains_key(name)
    }

    /// The sha ref `name` points at, if it exists.
    #[must_use]
    pub fn ref_target(&self, name: &str) -> Option<String> {
        self.lock().refs.get(name).cloned()
    }

    /// Seed a base commit carrying `tree` and return the commit's digest — the
    /// digest-typed form a source-port demo works in, hiding the hex-of-digest
    /// object-sha encoding the port owns.
    ///
    /// # Panics
    /// Never in practice — the seeded commit sha is always the 64-hex a digest
    /// round-trips through; a panic here is a broken invariant in the fake.
    #[must_use]
    pub fn seed_base_commit(&self, tree: &Digest) -> Digest {
        let tree_sha = to_hex(tree);
        let commit_sha = self.seed_commit(&tree_sha);
        let base = digest_from_hex(&commit_sha).expect("a seeded commit sha is 64-hex");
        // Record the correspondences the mainline paths resolve through: the base
        // head digest ↔ its real commit object, and the tree digest ↔ the commit's
        // real tree object, so `snapshot` / `create_namespace` / `land` resolve.
        self.seed_correspondence(&base, &commit_sha);
        self.seed_correspondence(tree, &tree_sha);
        base
    }

    /// Seed a ref (`heads/…` form) pointing at the commit `target`.
    pub fn seed_ref_at(&self, name: &str, target: &Digest) {
        self.seed_ref(name, &to_hex(target));
    }

    /// Point claim ref `name` at a fresh commit carrying `holder`'s id on the
    /// claim registry's `Bloom-Id` message convention (empty tree, ADR-0150
    /// amendment #3598) — a claim-registry consumer's way to stage another
    /// instance's live hold directly, sidestepping `claim_seal`.
    pub fn seed_claim_hold(&self, name: &str, holder: &BloomId) {
        let sha = self.seed_commit_with_message(&render_claim_message(holder), EMPTY_TREE);
        self.seed_ref(name, &sha);
    }

    /// Point claim ref `name` at a fresh tombstone commit (empty tree +
    /// `Bloom-Id: tombstone`) — the ref state an interrupted `release_seal`
    /// leaves after its CAS-to-tombstone linearized but its name-only cleanup
    /// delete never ran.
    pub fn seed_claim_tombstone(&self, name: &str) {
        let sha = self.seed_commit_with_message(&render_tombstone_message(), EMPTY_TREE);
        self.seed_ref(name, &sha);
    }

    /// The commit digest ref `name` points at, if it exists — the digest-typed
    /// [`ref_target`](Self::ref_target).
    #[must_use]
    pub fn ref_digest(&self, name: &str) -> Option<Digest> {
        self.ref_target(name).and_then(|sha| digest_from_hex(&sha))
    }

    /// Record a git-object↔bloom-digest correspondence directly (a source /
    /// executor test's way to stage the mapping the mainline paths resolve
    /// through). `sha` is a real git object sha (sha1/40-hex or sha256/64-hex).
    ///
    /// # Panics
    /// If `sha` is not a well-formed git object sha — a test-setup bug.
    pub fn seed_correspondence(&self, digest: &Digest, sha: &str) {
        let git = GitObjectId::from_hex(sha).expect("seed_correspondence: sha must be 40/64-hex");
        self.record_correspondence(digest, BackendObjectId::from(git));
    }

    /// Record a correspondence for `digest` against a synthetic git object sha
    /// derived from the digest's own hex — a source / executor test's convenient
    /// way to stage a resolvable digest (a candidate tree, a land head) without a
    /// matching git object in the store. The synthetic sha is `to_hex(digest)`, so
    /// a test that points a ref at it uses [`seed_ref_at`](Self::seed_ref_at) with
    /// the same digest.
    pub fn seed_git_object(&self, digest: &Digest) {
        self.seed_correspondence(digest, &to_hex(digest));
    }

    // Keep the fake faithful to the durable store's two-axis uniqueness: a new
    // digest for an existing backend object retires the old forward mapping,
    // while HashMap insertion retires the old reverse mapping for a digest.
    fn record_correspondence(&self, digest: &Digest, object: BackendObjectId) {
        let mut state = self.lock();
        state
            .correspondence
            .retain(|stored_digest, stored_object| stored_digest == digest.as_bytes() || stored_object != &object);
        state.correspondence.insert(*digest.as_bytes(), object);
    }

    /// Arm a merge of `head` into `base` (bare branch names) to answer with a
    /// conflict — the cross-member collision a fold parks on, which no amount of
    /// seeded content can otherwise provoke from a fake with no real diffs.
    pub fn seed_merge_conflict(&self, base: &str, head: &str) {
        self.lock().merge_conflicts.insert((base.to_owned(), head.to_owned()));
    }

    /// The commit sha `name` resolves to — a branch (bare name) through its ref,
    /// or a commit sha directly, mirroring what the merge endpoint accepts as a
    /// head.
    fn commit_at(&self, name: &str) -> Result<String, GithubError> {
        let (sha, repo) = {
            let state = self.lock();
            let sha = state.refs.get(&format!("heads/{name}")).map_or(name, String::as_str).to_owned();
            if state.commits.contains_key(&sha) {
                return Ok(sha);
            }
            (sha, state.object_repo.clone())
        };
        // A ref standing at a commit the fake did not mint — the repository's
        // own base, which its object database answers for.
        if repo.is_some_and(|repo| real_commit_tree(&repo, &sha).is_ok()) {
            return Ok(sha);
        }
        Err(GithubError::Status { status: 404, body: format!("no commit or branch {name}") })
    }

    /// The tree sha at `name`.
    fn tree_at(&self, name: &str) -> Result<String, GithubError> {
        let sha = self.commit_at(name)?;
        GitDataApi::get_commit(self, &sha).map(|commit| commit.tree)
    }

    /// Whether commit `ancestor` is reachable from `commit` — the ancestry a
    /// merge reads to answer "nothing to do". Iterative over an explicit stack:
    /// a history is caller-shaped and a recursive walk would be bounded only by
    /// how deep a test happens to build.
    fn contains(&self, commit: &str, ancestor: &str) -> bool {
        let state = self.lock();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut pending = vec![commit];
        while let Some(sha) = pending.pop() {
            if sha == ancestor {
                return true;
            }
            if !seen.insert(sha) {
                continue;
            }
            if let Some((stored_sha, stored)) = state.commits.get_key_value(sha) {
                debug_assert_eq!(stored_sha, sha);
                pending.extend(stored.parents.iter().map(String::as_str));
            }
        }
        false
    }

    /// The nonces of the dispatches recorded so far, in dispatch order — an
    /// executor test's way to assert a `submit` reached the Actions surface.
    #[must_use]
    pub fn dispatched_nonces(&self) -> Vec<String> {
        self.lock().dispatches.iter().map(|d| d.nonce.clone()).collect()
    }

    /// The `ref` a dispatch was posted against (the `workflow_dispatch` body
    /// ref — the protected pinned ref), for the dispatch carrying `nonce`.
    #[must_use]
    pub fn dispatched_ref(&self, nonce: &str) -> Option<String> {
        self.lock().dispatches.iter().find(|d| d.nonce == nonce).map(|d| d.git_ref.clone())
    }

    /// The workflow file a dispatch targeted, for the dispatch carrying `nonce`.
    #[must_use]
    pub fn dispatched_workflow(&self, nonce: &str) -> Option<String> {
        self.lock().dispatches.iter().find(|d| d.nonce == nonce).map(|d| d.workflow_file.clone())
    }

    /// The full input map a dispatch carried, for the dispatch carrying
    /// `nonce` — so a test can assert the `command` / `ref` / `nonce` shaping.
    #[must_use]
    pub fn dispatched_inputs(&self, nonce: &str) -> Option<BTreeMap<String, String>> {
        self.lock().dispatches.iter().find(|d| d.nonce == nonce).map(|d| d.inputs.clone())
    }

    /// Seed (or update) a resolvable run for `nonce` with the given lifecycle
    /// state, returning its run id. The display title embeds the nonce exactly
    /// as the wrapper's `run-name` would, so `find_run` resolves it.
    #[must_use]
    pub fn seed_run(&self, nonce: &str, status: RunStatus, conclusion: Option<RunConclusion>) -> u64 {
        let mut state = self.lock();
        if let Some(run) = state.runs.iter_mut().find(|r| r.nonce == nonce) {
            run.status = status;
            run.conclusion = conclusion;
            return run.id;
        }
        state.next_run += 1;
        let id = state.next_run;
        state.runs.push(StoredRun {
            id,
            nonce: nonce.to_owned(),
            display_title: format!("transform {nonce}"),
            status,
            conclusion,
            artifacts: Vec::new(),
        });
        id
    }

    /// Attach `artifacts` to the run with id `run_id` — seed a run first.
    ///
    /// # Panics
    /// If `run_id` names no seeded run — an unknown id is a test-setup bug, so
    /// it panics rather than silently no-op'ing, matching the file's other
    /// id-lookup mutators (`update_comment` / `cancel_run` return `Err(404)`).
    pub fn seed_run_artifacts(&self, run_id: u64, artifacts: Vec<Artifact>) {
        let mut state = self.lock();
        let run = state
            .runs
            .iter_mut()
            .find(|r| r.id == run_id)
            .expect("seed_run_artifacts: unknown run_id — seed a run first");
        run.artifacts = artifacts;
    }
}

// A deterministic commit sha: sha256 over the message, tree, and parents,
// rendered as 64 lowercase hex so it round-trips through the port's
// `digest_from_hex`. Deterministic so a re-seed of the same content is stable.
fn commit_sha(message: &str, tree: &str, parents: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(message.as_bytes());
    hasher.update([0u8]);
    hasher.update(tree.as_bytes());
    for parent in parents {
        hasher.update([0u8]);
        hasher.update(parent.as_bytes());
    }
    hex_of(&hasher.finalize().into())
}

// 64 lowercase hex — the sha form the port's `digest_from_hex` round-trips.
fn hex_of(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

impl GitDataApi for FakeGithub {
    fn get_ref(&self, name: &str) -> Result<Option<GitRef>, GithubError> {
        let state = self.lock();
        Ok(state.refs.get(name).map(|sha| GitRef { name: name.to_owned(), sha: sha.clone() }))
    }

    fn create_ref(&self, name: &str, sha: &str) -> Result<GitRef, GithubError> {
        let mut state = self.lock();
        if state.refs.contains_key(name) {
            return Err(GithubError::Status { status: 422, body: format!("ref {name} already exists") });
        }
        state.refs.insert(name.to_owned(), sha.to_owned());
        Ok(GitRef { name: name.to_owned(), sha: sha.to_owned() })
    }

    fn update_ref(&self, name: &str, sha: &str, _force: bool) -> Result<GitRef, GithubError> {
        // The fake does not model commit ancestry, so it cannot enforce the
        // fast-forward-only meaning of `force:false`; the source backend does
        // its compare-and-swap by reading and comparing before it updates, so a
        // plain set here is faithful to how the port drives the store.
        let mut state = self.lock();
        if !state.refs.contains_key(name) {
            return Err(GithubError::Status { status: 404, body: format!("no ref {name}") });
        }
        state.refs.insert(name.to_owned(), sha.to_owned());
        Ok(GitRef { name: name.to_owned(), sha: sha.to_owned() })
    }

    fn delete_ref(&self, name: &str) -> Result<(), GithubError> {
        // Name-only: an absent ref is the clean idempotent Ok — the fake models
        // GitHub's already-gone tolerance the source port's cleanup delete relies
        // on.
        self.lock().refs.remove(name);
        Ok(())
    }

    fn list_matching_refs(&self, prefix: &str) -> Result<Vec<GitRef>, GithubError> {
        let state = self.lock();
        let mut refs: Vec<GitRef> = state
            .refs
            .iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .map(|(name, sha)| GitRef { name: name.clone(), sha: sha.clone() })
            .collect();
        refs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(refs)
    }

    fn get_commit(&self, sha: &str) -> Result<GitCommit, GithubError> {
        if let Some(stored) = self.lock().commits.get(sha) {
            return Ok(GitCommit { sha: sha.to_owned(), tree: stored.tree.clone(), message: stored.message.clone() });
        }
        // A commit the fake did not mint. Against a real repository that is the
        // ordinary case rather than a miss — the base the harness seeds is a
        // commit the repository already held — so the object database answers
        // it. The message is not read back through this port, so the empty one
        // costs a second `cat-file` nothing.
        let repo =
            self.object_repo().ok_or_else(|| GithubError::Status { status: 404, body: format!("no commit {sha}") })?;
        Ok(GitCommit { sha: sha.to_owned(), tree: real_commit_tree(&repo, sha)?, message: String::new() })
    }

    fn create_commit(&self, message: &str, tree: &str, parents: &[String]) -> Result<GitCommit, GithubError> {
        let sha = mint_commit(self.object_repo().as_deref(), message, tree, parents)?;
        self.lock().commits.insert(
            sha.clone(),
            StoredCommit { tree: tree.to_owned(), message: message.to_owned(), parents: parents.to_vec() },
        );
        Ok(GitCommit { sha, tree: tree.to_owned(), message: message.to_owned() })
    }

    fn merge(&self, base: &str, head: &str, message: &str) -> Result<MergeResult, GithubError> {
        let (base, head) = (strip_heads(base), strip_heads(head));
        let (base_sha, head_sha) = (self.commit_at(base)?, self.commit_at(head)?);

        // "Already up to date" is an ancestry question, not a content one: the
        // base has nothing to take from a head it already contains. Comparing
        // trees instead would answer `false` for every re-merge, because a merge
        // commit's tree equals neither side — so a fold resuming after a restart
        // would re-merge every member it had already folded.
        let repo = self.object_repo();
        let collided =
            || MergeResult::Conflict { detail: format!("{{\"message\":\"Merge conflict\"}} ({head} into {base})") };
        let already = repo
            .as_ref()
            .map_or_else(|| self.contains(&base_sha, &head_sha), |repo| real_is_ancestor(repo, &head_sha, &base_sha));
        if already {
            return Ok(MergeResult::AlreadyUpToDate);
        }
        if self.lock().merge_conflicts.contains(&(base.to_owned(), head.to_owned())) {
            return Ok(collided());
        }

        // The one property worth modelling: a merge's tree is a function of
        // *both* sides. A fake that echoed the head's tree would let a caller
        // that actually tree-replaces pass — the precise bug merging exists to
        // prevent — so the combined tree must differ from either input. Backed
        // by a repository the real three-way merge answers that outright, and
        // answers the collision the armed set can only state.
        let tree = match repo.as_deref() {
            Some(repo) => match real_merge_tree(repo, &base_sha, &head_sha)? {
                Some(tree) => tree,
                None => return Ok(collided()),
            },
            None => merged_tree(&self.tree_at(base)?, &self.tree_at(head)?),
        };
        let parents = vec![base_sha, head_sha];
        let sha = mint_commit(repo.as_deref(), message, &tree, &parents)?;
        let mut state = self.lock();
        state.commits.insert(sha.clone(), StoredCommit { tree: tree.clone(), message: message.to_owned(), parents });
        // A real merge commits onto the base branch, so the ref advances.
        if let Some(target) = state.refs.get_mut(&format!("heads/{base}")) {
            target.clone_from(&sha);
        }
        Ok(MergeResult::Merged(GitCommit { sha, tree, message: message.to_owned() }))
    }
}

/// Run `git` in `repo`, returning its trimmed stdout.
///
/// A git that could not be spawned is [`Transport`](GithubError::Transport) —
/// the environment failed, not the request. A git that ran and refused is a
/// [`Status`](GithubError::Status), so a caller distinguishes the two the same
/// way it does against the real client.
fn git_in(repo: &Path, args: &[&str]) -> Result<String, GithubError> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .map_err(|error| GithubError::Transport(format!("git {args:?} in {}: {error}", repo.display())))?;
    if !output.status.success() {
        return Err(GithubError::Status {
            status: 422,
            body: format!("git {args:?}: {}", String::from_utf8_lossy(&output.stderr).trim()),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The identity and timestamps every minted commit carries.
///
/// Pinned rather than ambient so the same `(message, tree, parents)` hashes to
/// the same commit on every call. `GitSource::integrate` relies on precisely
/// that: a fault between its commit and its ref update is recoverable only
/// because the retry "re-creates a byte-identical commit ... so git hands back
/// the same sha". An ambient committer date would mint a second commit instead
/// and strand the first.
const COMMIT_IDENTITY: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "bloomery fixture"),
    ("GIT_AUTHOR_EMAIL", "fixture@bloomery.invalid"),
    ("GIT_AUTHOR_DATE", "@0 +0000"),
    ("GIT_COMMITTER_NAME", "bloomery fixture"),
    ("GIT_COMMITTER_EMAIL", "fixture@bloomery.invalid"),
    ("GIT_COMMITTER_DATE", "@0 +0000"),
];

/// The sha a commit over `tree` with `parents` takes: a real object in `repo`
/// when the fake is backed by one, the synthetic hash otherwise. The one place
/// the two modes part company, so `create_commit` and `merge` cannot drift.
fn mint_commit(repo: Option<&Path>, message: &str, tree: &str, parents: &[String]) -> Result<String, GithubError> {
    repo.map_or_else(|| Ok(commit_sha(message, tree, parents)), |repo| real_commit(repo, message, tree, parents))
}

/// Mint `message` over `tree` with `parents` as a real commit object in `repo`,
/// returning its git sha.
fn real_commit(repo: &Path, message: &str, tree: &str, parents: &[String]) -> Result<String, GithubError> {
    let mut args = vec!["commit-tree".to_owned(), tree.to_owned()];
    for parent in parents {
        args.push("-p".to_owned());
        args.push(parent.clone());
    }
    args.extend(["-m".to_owned(), message.to_owned()]);

    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = Command::new("git")
        .current_dir(repo)
        .envs(COMMIT_IDENTITY)
        .args(&borrowed)
        .output()
        .map_err(|error| GithubError::Transport(format!("git commit-tree in {}: {error}", repo.display())))?;
    if !output.status.success() {
        return Err(GithubError::Status {
            status: 422,
            body: format!("git commit-tree {tree}: {}", String::from_utf8_lossy(&output.stderr).trim()),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The tree commit `sha` carries, read from `repo` — the real-object answer to
/// the question the in-memory `commits` map answers for a minted commit.
fn real_commit_tree(repo: &Path, sha: &str) -> Result<String, GithubError> {
    git_in(repo, &["rev-parse", "--verify", "--quiet", &format!("{sha}^{{tree}}")])
        .map_err(|_| GithubError::Status { status: 404, body: format!("no commit {sha}") })
}

/// The tree combining `base` and `head` in `repo`, or `None` when the two
/// histories collide — a real three-way merge, so a conflict is a fact about
/// the content rather than something a test has to arm.
///
/// Read from the exit status rather than the output: `merge-tree` answers 0 for
/// a clean merge and 1 for a conflict, and prints a tree either way, so a caller
/// that judged by the presence of a tree would read every conflict as merged.
/// Any other status is a real refusal — a missing object, or a git too old for
/// `--write-tree` — and must not pass as a conflict.
fn real_merge_tree(repo: &Path, base: &str, head: &str) -> Result<Option<String>, GithubError> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["merge-tree", "--write-tree", base, head])
        .output()
        .map_err(|error| GithubError::Transport(format!("git merge-tree in {}: {error}", repo.display())))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    match output.status.code() {
        Some(0) => Ok(Some(stdout.lines().next().unwrap_or_default().trim().to_owned())),
        Some(1) => Ok(None),
        _ => Err(GithubError::Status {
            status: 422,
            body: format!("git merge-tree {base} {head}: {}", String::from_utf8_lossy(&output.stderr).trim()),
        }),
    }
}

/// Whether `ancestor` is reachable from `commit` in `repo` — the real-object
/// answer to the ancestry question a merge reads to say "nothing to do".
fn real_is_ancestor(repo: &Path, ancestor: &str, commit: &str) -> bool {
    git_in(repo, &["merge-base", "--is-ancestor", ancestor, commit]).is_ok()
}

/// The tree a merge of `base` and `head` lands on. Order-independent, so
/// merging the same two sides either way agrees, and distinct from both inputs.
fn merged_tree(base: &str, head: &str) -> String {
    let (first, second) = if base <= head {
        (base, head)
    } else {
        (head, base)
    };
    let mut hasher = Sha256::new();
    hasher.update(b"merged-tree");
    hasher.update([0u8]);
    hasher.update(first.as_bytes());
    hasher.update([0u8]);
    hasher.update(second.as_bytes());
    hex_of(&hasher.finalize().into())
}

impl Correspondence for FakeGithub {
    fn record(&self, digest: &Digest, object: &BackendObjectId) -> Result<(), CorrespondenceError> {
        self.record_correspondence(digest, object.clone());
        Ok(())
    }

    fn resolve_backend_object(&self, digest: &Digest) -> Result<Option<BackendObjectId>, CorrespondenceError> {
        Ok(self.lock().correspondence.get(digest.as_bytes()).cloned())
    }

    fn resolve_digest(&self, object: &BackendObjectId) -> Result<Option<Digest>, CorrespondenceError> {
        Ok(self
            .lock()
            .correspondence
            .iter()
            .find_map(|(digest, stored)| (stored == object).then(|| Digest::from_bytes(*digest))))
    }
}

// The nonce a run's stored `StoredRun` carries mirrors the wrapper's contract:
// the run name embeds the nonce. `find_run` resolves by that nonce key rather
// than scanning titles — the fake models the *resolution semantics*; the
// real client's title-scan is asserted in `client`'s RecordingTransport tests.
impl ActionsApi for FakeGithub {
    fn dispatch_workflow(
        &self,
        workflow_file: &str,
        git_ref: &str,
        inputs: &BTreeMap<String, String>,
    ) -> Result<(), GithubError> {
        let nonce = inputs.get(INPUT_NONCE).cloned().unwrap_or_default();
        self.lock().dispatches.push(StoredDispatch {
            workflow_file: workflow_file.to_owned(),
            git_ref: git_ref.to_owned(),
            nonce,
            inputs: inputs.clone(),
        });
        Ok(())
    }

    fn find_run(&self, _workflow_file: &str, nonce: &str) -> Result<Option<WorkflowRun>, GithubError> {
        Ok(self.lock().runs.iter().find(|r| r.nonce == nonce).map(StoredRun::to_workflow_run))
    }

    fn get_run(&self, run_id: u64) -> Result<WorkflowRun, GithubError> {
        self.lock()
            .runs
            .iter()
            .find(|r| r.id == run_id)
            .map(StoredRun::to_workflow_run)
            .ok_or_else(|| GithubError::Status { status: 404, body: format!("no run {run_id}") })
    }

    fn cancel_run(&self, run_id: u64) -> Result<(), GithubError> {
        let mut state = self.lock();
        let Some(run) = state.runs.iter_mut().find(|r| r.id == run_id) else {
            return Err(GithubError::Status { status: 404, body: format!("no run {run_id}") });
        };
        // A cancel drives the run to a completed/cancelled terminal state, so a
        // follow-up inspect reports `Cancelled` — the observable effect a test
        // asserts.
        run.status = RunStatus::Completed;
        run.conclusion = Some(RunConclusion::Cancelled);
        Ok(())
    }

    fn list_run_artifacts(&self, run_id: u64) -> Result<Vec<Artifact>, GithubError> {
        self.lock()
            .runs
            .iter()
            .find(|r| r.id == run_id)
            .map(|r| r.artifacts.clone())
            .ok_or_else(|| GithubError::Status { status: 404, body: format!("no run {run_id}") })
    }
}

impl StoredRun {
    fn to_workflow_run(&self) -> WorkflowRun {
        WorkflowRun {
            id: self.id,
            display_title: self.display_title.clone(),
            status: self.status,
            conclusion: self.conclusion,
        }
    }
}

impl PullRequestApi for FakeGithub {
    fn create_pull_request(&self, new: &NewPullRequest) -> Result<PullRequest, GithubError> {
        let mut state = self.lock();
        // GitHub refuses a second open pull request for the same head with a
        // 422. Modelling it is what lets a test prove the land path adopts the
        // existing proposal rather than relying on the refusal never happening.
        if state.pull_requests.iter().any(|pull| pull.head == new.head && pull.state == PullRequestState::Open) {
            return Err(GithubError::Status {
                status: 422,
                body: format!("a pull request already exists for {}", new.head),
            });
        }
        // The head sha comes from the ref the branch names, as it does on the
        // real surface: opening a pull request proposes whatever that ref
        // currently points at, not a sha the caller asserts.
        let head_sha = state.refs.get(&format!("heads/{}", new.head)).cloned().unwrap_or_default();
        state.next_pull_request += 1;
        let stored = StoredPullRequest {
            number: state.next_pull_request,
            title: new.title.clone(),
            body: new.body.clone(),
            head: new.head.clone(),
            head_sha,
            base: new.base.clone(),
            state: PullRequestState::Open,
            merged: false,
            merge_commit_sha: None,
        };
        state.pull_requests.push(stored.clone());
        Ok(stored.project())
    }

    fn get_pull_request(&self, number: u64) -> Result<Option<PullRequest>, GithubError> {
        let state = self.lock();
        Ok(state.pull_requests.iter().find(|pull| pull.number == number).map(StoredPullRequest::project))
    }

    fn find_pull_request_for_head(&self, head: &str) -> Result<Option<PullRequest>, GithubError> {
        // Any state, newest first — the real list endpoint's `state=all` +
        // `direction=desc`. A merged proposal stays findable, which is what lets
        // a land watch observe the landing instead of proposing again.
        let state = self.lock();
        Ok(state.pull_requests.iter().rev().find(|pull| pull.head == head).map(StoredPullRequest::project))
    }

    fn checks_for_ref(&self, sha: &str) -> Result<ChecksState, GithubError> {
        Ok(self.lock().checks.get(sha).cloned().unwrap_or(ChecksState::Absent))
    }
}

impl GithubApi for FakeGithub {
    fn issue_title(&self, number: u64) -> Result<Option<String>, GithubError> {
        Ok(self.lock().issues.iter().find(|issue| issue.number == number).map(|issue| issue.title.clone()))
    }

    fn find_comment(&self, issue_number: u64, key: &str) -> Result<Option<Comment>, GithubError> {
        let state = self.lock();
        if !comment_target_exists(&state, issue_number) {
            return Err(absent_target(issue_number));
        }
        Ok(state.comments.iter().filter(|comment| comment.issue_number == issue_number).find_map(|comment| {
            let marker = parse_marker(&comment.body);
            match &marker {
                Some(m) if m.key == key => Some(Comment { id: comment.id, body: comment.body.clone(), marker }),
                _ => None,
            }
        }))
    }

    fn create_comment(&self, new: &NewComment) -> Result<Comment, GithubError> {
        let mut state = self.lock();
        if !comment_target_exists(&state, new.issue_number) {
            return Err(absent_target(new.issue_number));
        }
        state.next_comment += 1;
        let id = state.next_comment;
        state.comments.push(StoredComment { id, issue_number: new.issue_number, body: new.body.clone() });
        Ok(Comment { id, body: new.body.clone(), marker: parse_marker(&new.body) })
    }

    fn update_comment(&self, comment_id: u64, body: &str) -> Result<(), GithubError> {
        let mut state = self.lock();
        let Some(comment) = state.comments.iter_mut().find(|comment| comment.id == comment_id) else {
            return Err(GithubError::Status { status: 404, body: format!("no comment {comment_id}") });
        };
        body.clone_into(&mut comment.body);
        Ok(())
    }
}

impl IssueStateApi for FakeGithub {
    fn close_issue(&self, number: u64) -> Result<(), GithubError> {
        let mut state = self.lock();
        let Some(issue) = state.issues.iter_mut().find(|issue| issue.number == number) else {
            return Err(absent_target(number));
        };
        issue.closed = true;
        Ok(())
    }
}

/// Whether `number` names an object that can carry a comment. Issues and pull
/// requests both can: GitHub numbers them from one sequence and the comment
/// route is shared, so a landing receipt reaches a pull request through exactly
/// the path a member issue is reached by.
fn comment_target_exists(state: &State, number: u64) -> bool {
    state.issues.iter().any(|issue| issue.number == number)
        || state.pull_requests.iter().any(|pull| pull.number == number)
}

/// The refusal the real surface answers for a comment on a number it does not
/// hold. Modelled because it is the condition the projection has to classify as
/// a skip rather than an error — an unmodelled 404 would let a silently-stalling
/// projection pass.
fn absent_target(number: u64) -> GithubError {
    GithubError::Status { status: 404, body: format!("no object {number}") }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! The fake's merge is the seam every fold test will stand on, and nothing
    //! else exercises it yet — so its contract is pinned here rather than
    //! discovered later through a port test that passes for the wrong reason.

    use aether_bloomery::{BackendObjectId, Correspondence, Digest};

    use super::{FakeGithub, GitDataApi, GitObjectId, MergeResult};

    // Seed `branch` at a commit carrying `tree`, and hand back that commit's sha.
    fn branch_at(fake: &FakeGithub, branch: &str, tree: &str) -> String {
        let commit = fake.create_commit(branch, tree, &[]).unwrap();
        fake.seed_ref(&format!("heads/{branch}"), &commit.sha);
        commit.sha
    }

    #[test]
    fn correspondence_object_reassignment_retires_the_old_digest() {
        let fake = FakeGithub::new();
        let old_digest = Digest::from_bytes([1; 32]);
        let new_digest = Digest::from_bytes([2; 32]);
        let sha = "3a3f8c0b9e1d2a4f6b8c0e2d4a6f8b0c1e3d5a7f";
        let object = BackendObjectId::from(GitObjectId::from_hex(sha).unwrap());

        fake.seed_correspondence(&old_digest, sha);
        fake.seed_correspondence(&new_digest, sha);

        assert_eq!(fake.resolve_backend_object(&old_digest).unwrap(), None);
        assert_eq!(fake.resolve_digest(&object).unwrap(), Some(new_digest));
    }

    #[test]
    fn correspondence_digest_reassignment_retires_the_old_object() {
        let fake = FakeGithub::new();
        let digest = Digest::from_bytes([1; 32]);
        let old_object = BackendObjectId::new(vec![2; 20]);
        let new_object = BackendObjectId::new(vec![3; 32]);

        fake.record(&digest, &old_object).unwrap();
        fake.record(&digest, &new_object).unwrap();

        assert_eq!(fake.resolve_digest(&old_object).unwrap(), None);
        assert_eq!(fake.resolve_backend_object(&digest).unwrap(), Some(new_object));
    }

    #[test]
    fn a_merge_combines_both_sides_rather_than_taking_the_heads_tree() {
        // Tripwire: the whole reason the fold moves off `create_commit` is that
        // stating the head's tree onto a moved base reverts what the base
        // gained. A fake that echoed the head's tree would let exactly that bug
        // pass its port test, so the combined tree must equal neither input.
        let fake = FakeGithub::new();
        branch_at(&fake, "integration", "base-tree");
        branch_at(&fake, "candidate", "head-tree");

        let MergeResult::Merged(commit) = fake.merge("heads/integration", "heads/candidate", "fold").unwrap() else {
            panic!("two divergent trees merge");
        };
        assert_ne!(commit.tree, "head-tree", "a merge is not a tree-replace");
        assert_ne!(commit.tree, "base-tree", "nor a no-op");

        // A real merge commits onto the base branch, so the fold's next read of
        // the integration branch must see the merge, not the pre-merge position.
        assert_eq!(fake.get_ref("heads/integration").unwrap().unwrap().sha, commit.sha, "the base branch advanced");
    }

    #[test]
    fn an_already_contained_head_reports_nothing_to_do_and_an_armed_pair_conflicts() {
        // "Already contained" is ancestry, not content. Two branches that happen
        // to carry the same tree are divergent histories and do merge; a branch
        // that has already merged the other genuinely has nothing left to take.
        // Answering this by tree comparison says `false` for every re-merge —
        // a merge commit's tree equals neither side — so a fold resuming after a
        // restart would re-merge every member it had already folded.
        let fake = FakeGithub::new();
        branch_at(&fake, "integration", "base-tree");
        branch_at(&fake, "candidate", "head-tree");
        assert!(
            matches!(fake.merge("heads/integration", "heads/candidate", "fold").unwrap(), MergeResult::Merged(_)),
            "the first fold of a member is a real merge",
        );
        assert_eq!(
            fake.merge("heads/integration", "heads/candidate", "fold").unwrap(),
            MergeResult::AlreadyUpToDate,
            "re-folding a member the branch already contains has nothing to do",
        );

        let fake = FakeGithub::new();
        branch_at(&fake, "integration", "base-tree");
        branch_at(&fake, "candidate", "head-tree");
        fake.seed_merge_conflict("integration", "candidate");
        assert!(
            matches!(fake.merge("heads/integration", "heads/candidate", "fold").unwrap(), MergeResult::Conflict { .. }),
            "an armed pair collides — the only way a contentless fake can model one",
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod object_repo_tests {
    //! The object-repo mode, where the fake stops inventing shas and mints real
    //! git objects instead. What it buys is a fold a caller can actually check
    //! out, so these pin the properties a checkout depends on.

    use std::path::Path;
    use std::process::Command;
    use std::{fs, slice};

    use super::{FakeGithub, GitDataApi, MergeResult};

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// A repository with one commit, and the fake that mints into it.
    fn repo_backed() -> (tempfile::TempDir, FakeGithub, String) {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet", "."]);
        git(dir.path(), &["config", "--local", "user.name", "fixture"]);
        git(dir.path(), &["config", "--local", "user.email", "fixture@example.test"]);
        fs::write(dir.path().join("subject.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "--all"]);
        git(dir.path(), &["commit", "--quiet", "--message", "base"]);

        let head = git(dir.path(), &["rev-parse", "HEAD"]);
        let fake = FakeGithub::new().with_object_repo(dir.path());
        (dir, fake, head)
    }

    #[test]
    fn a_minted_commit_is_an_object_the_repository_resolves() {
        // Tripwire: the whole point of the mode. A fold's consumer checks it out
        // with a real `git worktree add`, which resolves against the object
        // database and nothing else — so a synthetic sha is a name that can
        // never be found, and the bloom stalls one stage short of its landing
        // with no error to read. Ask git itself, because git is the only
        // authority on whether git can find it.
        let (dir, fake, head) = repo_backed();
        let tree = git(dir.path(), &["rev-parse", "HEAD:"]);

        let commit = fake.create_commit("bloomery integrate", &tree, slice::from_ref(&head)).unwrap();

        assert_eq!(git(dir.path(), &["cat-file", "-t", &commit.sha]), "commit", "the minted sha names a real object");
        assert_eq!(git(dir.path(), &["rev-parse", &format!("{}^", commit.sha)]), head, "over the parent it was given");
        // And a commit the fake never minted still reads, so a branch standing
        // at the repository's own base resolves its tree like any other.
        assert_eq!(fake.get_commit(&head).unwrap().tree, tree, "an unminted commit reads through to the repository");
    }

    #[test]
    fn re_minting_the_same_commit_returns_the_same_sha() {
        // Tripwire: `GitSource::integrate` recovers from a fault between its
        // commit and its ref update *only* because the retry re-creates a
        // byte-identical commit and git hands back the same sha. An ambient
        // committer date would mint a second commit instead and strand the
        // first, turning a recoverable fault into an absorbing one.
        let (dir, fake, head) = repo_backed();
        let tree = git(dir.path(), &["rev-parse", "HEAD:"]);

        let first = fake.create_commit("bloomery integrate", &tree, slice::from_ref(&head)).unwrap();
        let second = fake.create_commit("bloomery integrate", &tree, &[head]).unwrap();

        assert_eq!(first.sha, second.sha, "the same message, tree and parents mint the same commit");
    }

    #[test]
    fn a_real_collision_conflicts_without_being_armed() {
        // The in-memory fake has to be *told* two branches collide, because it
        // holds no content to collide. Backed by a repository the question has a
        // real answer, and a fold's conflict path is exercised by content rather
        // than by a seeded flag — so this is also what keeps that path honest
        // when the merge is a genuine three-way one.
        let (dir, fake, head) = repo_backed();
        for (branch, line) in [("integration", "theirs\n"), ("candidate", "ours\n")] {
            git(dir.path(), &["checkout", "--quiet", "--detach", &head]);
            fs::write(dir.path().join("subject.txt"), line).unwrap();
            git(dir.path(), &["add", "--all"]);
            git(dir.path(), &["commit", "--quiet", "--message", branch]);
            fake.seed_ref(&format!("heads/{branch}"), &git(dir.path(), &["rev-parse", "HEAD"]));
        }

        assert!(
            matches!(fake.merge("heads/integration", "heads/candidate", "fold").unwrap(), MergeResult::Conflict { .. }),
            "two edits of the same line collide on their own",
        );
    }
}
