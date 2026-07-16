//! An in-process fake GitHub (#3459 step 4).
//!
//! Models the projection's object store — issues and their comments — with
//! enough fidelity to drive the projection end-to-end with no token and no
//! network: create returns a fresh number, find scans marker keys exactly as
//! the real client does, update overwrites in place, and [`delete_issue`]
//! models an operator deleting a projection so the rebuild property (delete →
//! reappear) is exercisable.
//!
//! Compiled for this crate's own tests unconditionally and, behind the
//! `testing` feature, exported so the host demo (#3459 step 7) drives the same
//! double.
//!
//! [`delete_issue`]: FakeGithub::delete_issue

// The fake holds its `Mutex` guard to the end of each short method rather than
// dropping it a line early: this is an in-memory test double with no
// contention, so the nursery lint's early-drop rewrite buys nothing and only
// clutters the store methods.
#![allow(clippy::significant_drop_tightening)]

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use aether_bloomery::Digest;
use sha2::{Digest as _, Sha256};

use crate::client::{
    ActionsApi, Artifact, Comment, GitCommit, GitDataApi, GitRef, GithubApi, GithubError, Issue, NewComment, NewIssue,
    RunConclusion, RunStatus, WorkflowRun,
};
use crate::marker::parse_marker;
use crate::source::{digest_from_hex, to_hex};

#[derive(Clone)]
struct StoredIssue {
    number: u64,
    title: String,
    body: String,
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

#[derive(Default)]
struct State {
    next_issue: u64,
    next_comment: u64,
    issues: Vec<StoredIssue>,
    comments: Vec<StoredComment>,
    // The git object store the source port drives: ref name (`heads/…`) → sha,
    // and commit sha → tree sha. Trees themselves are opaque shas the port
    // treats as digest-addressed handles, so they need no separate store.
    refs: HashMap<String, String>,
    commits: HashMap<String, String>,
    // The Actions surface the executor port drives: recorded dispatches (a
    // `workflow_dispatch` creates no resolvable run synchronously, so a
    // dispatched-but-unseeded nonce inspects `Unknown`), and the runs a test
    // seeds — keyed by the nonce the wrapper would embed in the run name.
    next_run: u64,
    dispatches: Vec<StoredDispatch>,
    runs: Vec<StoredRun>,
}

/// An in-memory GitHub double implementing [`GithubApi`].
///
/// Cloning shares one backing store (an `Arc<Mutex<…>>`), so a demo can hand a
/// clone to a [`GithubProjection`](crate::GithubProjection) and keep another to
/// introspect the objects the projection created.
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

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many issues currently exist — the carbon-copy count a demo asserts.
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

    /// Delete issue `number` and its comments — an operator removing a
    /// projection. The next reconcile finds no marker and recreates it.
    pub fn delete_issue(&self, number: u64) {
        let mut state = self.lock();
        state.issues.retain(|issue| issue.number != number);
        state.comments.retain(|comment| comment.issue_number != number);
    }

    /// Seed a commit object carrying `tree_sha` (no parents) and return its
    /// sha — a source-port test's way to place a base commit the port can
    /// snapshot or a namespace can be created on.
    #[must_use]
    pub fn seed_commit(&self, tree_sha: &str) -> String {
        let sha = commit_sha("seed", tree_sha, &[]);
        self.lock().commits.insert(sha.clone(), tree_sha.to_owned());
        sha
    }

    /// Seed a ref (`heads/…` form) pointing at `sha`.
    pub fn seed_ref(&self, name: &str, sha: &str) {
        self.lock().refs.insert(name.to_owned(), sha.to_owned());
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
        let sha = self.seed_commit(&to_hex(tree));
        digest_from_hex(&sha).expect("a seeded commit sha is 64-hex")
    }

    /// Seed a ref (`heads/…` form) pointing at the commit `target`.
    pub fn seed_ref_at(&self, name: &str, target: &Digest) {
        self.seed_ref(name, &to_hex(target));
    }

    /// The commit digest ref `name` points at, if it exists — the digest-typed
    /// [`ref_target`](Self::ref_target).
    #[must_use]
    pub fn ref_digest(&self, name: &str) -> Option<Digest> {
        self.ref_target(name).and_then(|sha| digest_from_hex(&sha))
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
    pub fn seed_run_artifacts(&self, run_id: u64, artifacts: Vec<Artifact>) {
        let mut state = self.lock();
        if let Some(run) = state.runs.iter_mut().find(|r| r.id == run_id) {
            run.artifacts = artifacts;
        }
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
    let bytes: [u8; 32] = hasher.finalize().into();
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

    fn delete_ref(&self, name: &str) -> Result<(), GithubError> {
        // Deleting an absent ref is the clean idempotent outcome (GitHub answers
        // 404/422, which the real client maps to Ok), so a release retried after
        // a crash is a no-op.
        self.lock().refs.remove(name);
        Ok(())
    }

    fn get_commit(&self, sha: &str) -> Result<GitCommit, GithubError> {
        self.lock()
            .commits
            .get(sha)
            .map(|tree| GitCommit { sha: sha.to_owned(), tree: tree.clone() })
            .ok_or_else(|| GithubError::Status { status: 404, body: format!("no commit {sha}") })
    }

    fn create_commit(&self, message: &str, tree: &str, parents: &[String]) -> Result<GitCommit, GithubError> {
        let sha = commit_sha(message, tree, parents);
        self.lock().commits.insert(sha.clone(), tree.to_owned());
        Ok(GitCommit { sha, tree: tree.to_owned() })
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
        let nonce = inputs.get("nonce").cloned().unwrap_or_default();
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

impl GithubApi for FakeGithub {
    fn find_issue(&self, key: &str) -> Result<Option<Issue>, GithubError> {
        let state = self.lock();
        Ok(state.issues.iter().find_map(|issue| {
            let marker = parse_marker(&issue.body);
            match &marker {
                Some(m) if m.key == key => {
                    Some(Issue { number: issue.number, title: issue.title.clone(), body: issue.body.clone(), marker })
                }
                _ => None,
            }
        }))
    }

    fn create_issue(&self, new: &NewIssue) -> Result<Issue, GithubError> {
        let mut state = self.lock();
        state.next_issue += 1;
        let number = state.next_issue;
        state.issues.push(StoredIssue { number, title: new.title.clone(), body: new.body.clone() });
        Ok(Issue { number, title: new.title.clone(), body: new.body.clone(), marker: parse_marker(&new.body) })
    }

    fn update_issue(&self, number: u64, title: &str, body: &str) -> Result<(), GithubError> {
        let mut state = self.lock();
        let Some(issue) = state.issues.iter_mut().find(|issue| issue.number == number) else {
            return Err(GithubError::Status { status: 404, body: format!("no issue {number}") });
        };
        title.clone_into(&mut issue.title);
        body.clone_into(&mut issue.body);
        Ok(())
    }

    fn find_comment(&self, issue_number: u64, key: &str) -> Result<Option<Comment>, GithubError> {
        let state = self.lock();
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
