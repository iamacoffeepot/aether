//! The thin GitHub REST client the projection drives (#3459 step 2).
//!
//! The adapter owns this client directly — it is host-side native code, not a
//! wasm guest, so it does not route through the guest-facing `aether.http`
//! egress cap (a blocking `ureq` hop behind a host allowlist with URL-only
//! reply correlation, a poor fit for many correlated writes). The landed
//! ports are synchronous, so the client is `reqwest::blocking`; correlation is
//! per request/response.
//!
//! # Endpoint surface
//!
//! This is the **outward projection mirror** slice, so the client wraps only
//! the endpoints a projection-only reconcile touches: issues (create / update
//! / find) and issue comments (create / update / find). Check-runs and the Git
//! Data blob/tree/commit/ref surface belong to the **git source port** — a
//! separate sibling slice (ADR-0149 amendment [#3460]) — and are intentionally
//! absent: a check-run cannot attach without a commit the source port
//! produces, so shipping it here would be an endpoint that cannot work
//! projection-only.
//!
//! # Testability
//!
//! [`ReqwestGithub`] is generic over a small [`HttpTransport`] seam, so the
//! request-shaping (URL / headers / body) and error-mapping logic is unit
//! tested against a recording fake with no network. End-to-end projection
//! logic is tested against the higher-level `FakeGithub`,
//! which models the object store rather than the HTTP transport.
//!
//! [#3460]: https://github.com/iamacoffeepot/aether/issues/3460

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use reqwest::Method as ReqwestMethod;
use reqwest::blocking::Client as BlockingClient;
use serde::Deserialize;

use crate::marker::{Marker, parse_marker};

/// An issue projection: its number, current title/body, and the parsed marker
/// (`None` when the body carries no well-formed marker — a deleted-and-
/// recreated or hand-authored issue).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Issue {
    /// The issue number.
    pub number: u64,
    /// The current title.
    pub title: String,
    /// The current body (contains the marker when projected).
    pub body: String,
    /// The parsed marker, if the body carries one.
    pub marker: Option<Marker>,
}

/// The fields to open a new issue with. `body` already carries its rendered
/// marker.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NewIssue {
    /// The issue title.
    pub title: String,
    /// The issue body, marker included.
    pub body: String,
}

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
/// [`crate::normalize_stage_result`]). Kept here because it is the shape the
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

/// A git ref as the source port reads and writes it: its short name (the
/// `heads/…` form, no leading `refs/`) and the 40/64-hex object sha it targets.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitRef {
    /// The ref name in `heads/…` form (no `refs/` prefix).
    pub name: String,
    /// The object sha the ref points at.
    pub sha: String,
}

/// A git commit object: its own sha and the tree sha it carries. The source
/// port reads a commit to derive a snapshot's tree, and creates one to advance
/// an integration branch to a candidate tree.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitCommit {
    /// The commit object sha.
    pub sha: String,
    /// The tree sha this commit points at.
    pub tree: String,
}

/// The GitHub client contract the projection depends on. Both the real
/// [`ReqwestGithub`] and the test `FakeGithub` implement it,
/// so the projection logic is exercised without a token or network.
pub trait GithubApi {
    /// Find the issue whose marker carries `key`, if any. The projection's
    /// idempotency lookup: a match with the desired digest is a no-op, a
    /// mismatch an update, `None` a create.
    ///
    /// # Errors
    /// The projection surface is unreachable or returned an error status.
    fn find_issue(&self, key: &str) -> Result<Option<Issue>, GithubError>;

    /// Open a new issue.
    ///
    /// # Errors
    /// The projection surface is unreachable or returned an error status.
    fn create_issue(&self, new: &NewIssue) -> Result<Issue, GithubError>;

    /// Overwrite an issue's title and body.
    ///
    /// # Errors
    /// The projection surface is unreachable or returned an error status.
    fn update_issue(&self, number: u64, title: &str, body: &str) -> Result<(), GithubError>;

    /// Find the comment on `issue_number` whose marker carries `key`, if any.
    ///
    /// # Errors
    /// The projection surface is unreachable or returned an error status.
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

/// The Git Data REST surface the source port drives (blob/tree/commit/ref over
/// HTTP, no working copy on disk — ADR-0149's git source port, [#3465]). A
/// sibling of [`GithubApi`] rather than an extension of it: the projection has
/// no use for refs and the source port has no use for issues, so segregating
/// the two keeps each backend generic over only the surface it touches. Both
/// [`ReqwestGithub`] and the test `FakeGithub` implement it, so the source
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
}

impl fmt::Display for GithubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status { status, body } => write!(f, "github returned status {status}: {body}"),
            Self::Transport(msg) => write!(f, "github transport error: {msg}"),
            Self::Decode(msg) => write!(f, "github response decode error: {msg}"),
        }
    }
}

impl Error for GithubError {}

/// The HTTP verb an adapter request uses. Only the three the projection needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PATCH`.
    Patch,
}

/// One outbound request the transport executes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HttpRequest {
    /// The verb.
    pub method: Method,
    /// The absolute URL.
    pub url: String,
    /// Extra headers beyond the auth/accept/user-agent set the transport adds.
    pub headers: Vec<(String, String)>,
    /// The JSON body, if any.
    pub body: Option<String>,
}

/// One inbound response.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The body text.
    pub body: String,
}

/// The transport seam [`ReqwestGithub`] shapes requests against. The real
/// implementation is [`ReqwestTransport`]; tests inject a recording double.
pub trait HttpTransport {
    /// Execute `request`, returning the raw response.
    ///
    /// # Errors
    /// A transport-level failure (DNS, connect, TLS, timeout).
    fn execute(&self, request: HttpRequest) -> Result<HttpResponse, GithubError>;
}

/// The production transport: a `reqwest::blocking` client.
pub struct ReqwestTransport {
    client: BlockingClient,
    token: String,
}

impl ReqwestTransport {
    /// Build a transport bearing `token`.
    ///
    /// # Errors
    /// The `reqwest` client could not be constructed.
    pub fn new(token: String) -> Result<Self, GithubError> {
        let client = BlockingClient::builder()
            .user_agent("aether-bloomery-github")
            .build()
            .map_err(|error| GithubError::Transport(error.to_string()))?;
        Ok(Self { client, token })
    }
}

impl HttpTransport for ReqwestTransport {
    fn execute(&self, request: HttpRequest) -> Result<HttpResponse, GithubError> {
        let method = match request.method {
            Method::Get => ReqwestMethod::GET,
            Method::Post => ReqwestMethod::POST,
            Method::Patch => ReqwestMethod::PATCH,
        };
        let mut builder = self
            .client
            .request(method, &request.url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = request.body {
            builder = builder.header("Content-Type", "application/json").body(body);
        }
        let response = builder.send().map_err(|error| GithubError::Transport(error.to_string()))?;
        let status = response.status().as_u16();
        let body = response.text().map_err(|error| GithubError::Transport(error.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

/// The real GitHub client. Shapes REST requests over a [`HttpTransport`] and
/// maps responses into the projection's models.
pub struct ReqwestGithub<T: HttpTransport = ReqwestTransport> {
    transport: T,
    api_base: String,
    repo_path: String,
}

/// The bound on how many list pages `find_*` walks — a shadow repo stays far
/// under this, and it caps an otherwise-unbounded pagination loop rather than
/// spinning forever on a misbehaving server.
const MAX_LIST_PAGES: u32 = 100;
const PER_PAGE: u32 = 100;

impl<T: HttpTransport> ReqwestGithub<T> {
    /// Build a client over `transport`, rooted at `api_base` (no trailing
    /// slash) for `owner/repo`.
    pub fn with_transport(transport: T, api_base: impl Into<String>, repo_path: impl Into<String>) -> Self {
        Self { transport, api_base: api_base.into(), repo_path: repo_path.into() }
    }

    fn issues_url(&self) -> String {
        format!("{}/repos/{}/issues", self.api_base, self.repo_path)
    }

    fn request(&self, method: Method, url: String, body: Option<String>) -> Result<HttpResponse, GithubError> {
        let response = self.transport.execute(HttpRequest { method, url, headers: Vec::new(), body })?;
        if (200..300).contains(&response.status) {
            Ok(response)
        } else {
            Err(GithubError::Status { status: response.status, body: response.body })
        }
    }

    // Like `request`, but a 404 is the clean "not present" answer (`Ok(None)`)
    // rather than an error — a ref lookup that misses is not a fault.
    fn request_opt(&self, method: Method, url: String) -> Result<Option<HttpResponse>, GithubError> {
        let response = self.transport.execute(HttpRequest { method, url, headers: Vec::new(), body: None })?;
        if (200..300).contains(&response.status) {
            Ok(Some(response))
        } else if response.status == 404 {
            Ok(None)
        } else {
            Err(GithubError::Status { status: response.status, body: response.body })
        }
    }

    fn git_url(&self, suffix: &str) -> String {
        format!("{}/repos/{}/git/{suffix}", self.api_base, self.repo_path)
    }

    fn actions_url(&self, suffix: &str) -> String {
        format!("{}/repos/{}/actions/{suffix}", self.api_base, self.repo_path)
    }
}

impl ReqwestGithub<ReqwestTransport> {
    /// Build a client over the production `reqwest::blocking` transport.
    ///
    /// # Errors
    /// The `reqwest` client could not be constructed.
    pub fn new(config: &crate::GithubConfig) -> Result<Self, GithubError> {
        let transport = ReqwestTransport::new(config.token.clone())?;
        Ok(Self::with_transport(transport, config.api_base.clone(), config.repo_path()))
    }
}

#[derive(Deserialize)]
struct GhIssue {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct GhComment {
    id: u64,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Deserialize)]
struct GhRefObject {
    sha: String,
}

#[derive(Deserialize)]
struct GhRef {
    #[serde(rename = "ref")]
    ref_name: String,
    object: GhRefObject,
}

impl GhRef {
    // The API returns the fully-qualified `refs/heads/…`; the source port works
    // in the short `heads/…` form, so strip the `refs/` the create endpoint
    // required.
    fn into_git_ref(self) -> GitRef {
        let name = self.ref_name.strip_prefix("refs/").unwrap_or(&self.ref_name).to_owned();
        GitRef { name, sha: self.object.sha }
    }
}

#[derive(Deserialize)]
struct GhTreeRef {
    sha: String,
}

#[derive(Deserialize)]
struct GhCommit {
    sha: String,
    tree: GhTreeRef,
}

impl GhCommit {
    fn into_git_commit(self) -> GitCommit {
        GitCommit { sha: self.sha, tree: self.tree.sha }
    }
}

#[derive(Deserialize)]
struct GhRun {
    id: u64,
    #[serde(default)]
    display_title: String,
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
}

impl GhRun {
    // Fold GitHub's richer `status` × `conclusion` string sets into the port's
    // three-way lifecycle. The github-string knowledge lives here (the only
    // crate permitted to name GitHub); the executor maps the resulting typed
    // enums onto the port's `ExecutionStatus`.
    fn into_workflow_run(self) -> WorkflowRun {
        let status = match self.status.as_str() {
            "completed" => RunStatus::Completed,
            "in_progress" => RunStatus::InProgress,
            // queued / waiting / requested / pending — all pre-run.
            _ => RunStatus::Queued,
        };
        let conclusion = self.conclusion.as_deref().map(|c| match c {
            "success" => RunConclusion::Success,
            "cancelled" => RunConclusion::Cancelled,
            "neutral" | "skipped" | "action_required" => RunConclusion::Neutral,
            // failure / timed_out / stale / startup_failure — all a failed run.
            _ => RunConclusion::Failure,
        });
        WorkflowRun { id: self.id, display_title: self.display_title, status, conclusion }
    }
}

#[derive(Deserialize)]
struct GhRunList {
    #[serde(default)]
    workflow_runs: Vec<GhRun>,
}

#[derive(Deserialize)]
struct GhArtifact {
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    size_in_bytes: u64,
}

impl GhArtifact {
    fn into_artifact(self) -> Artifact {
        Artifact { id: self.id, name: self.name, size_bytes: self.size_in_bytes }
    }
}

#[derive(Deserialize)]
struct GhArtifactList {
    #[serde(default)]
    artifacts: Vec<GhArtifact>,
}

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

/// The GitHub Actions REST surface the executor port drives — `workflow_dispatch`
/// plus the run + artifacts API. A sibling of [`GithubApi`] / [`GitDataApi`]
/// (each backend is generic over only the surface it touches), implemented by
/// both [`ReqwestGithub`] and the test `FakeGithub` so the executor is
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

fn decode<D: for<'de> Deserialize<'de>>(response: &HttpResponse) -> Result<D, GithubError> {
    serde_json::from_str(&response.body).map_err(|error| GithubError::Decode(error.to_string()))
}

impl<T: HttpTransport> GithubApi for ReqwestGithub<T> {
    fn find_issue(&self, key: &str) -> Result<Option<Issue>, GithubError> {
        // No search-by-external-metadata endpoint exists, so list-and-scan.
        // The shadow repo's issue set is small; the page cap bounds the walk.
        for page in 1..=MAX_LIST_PAGES {
            let url = format!("{}?state=all&per_page={PER_PAGE}&page={page}", self.issues_url());
            let response = self.request(Method::Get, url, None)?;
            let issues: Vec<GhIssue> = decode(&response)?;
            let count = issues.len();
            for gh in issues {
                if gh.pull_request.is_some() {
                    continue; // the issues endpoint also returns PRs.
                }
                let body = gh.body.unwrap_or_default();
                let marker = parse_marker(&body);
                if marker.as_ref().is_some_and(|m| m.key == key) {
                    return Ok(Some(Issue { number: gh.number, title: gh.title, body, marker }));
                }
            }
            if count < PER_PAGE as usize {
                break;
            }
        }
        Ok(None)
    }

    fn create_issue(&self, new: &NewIssue) -> Result<Issue, GithubError> {
        let body = serde_json::json!({ "title": new.title, "body": new.body }).to_string();
        let response = self.request(Method::Post, self.issues_url(), Some(body))?;
        let gh: GhIssue = decode(&response)?;
        let issue_body = gh.body.unwrap_or_else(|| new.body.clone());
        let marker = parse_marker(&issue_body);
        Ok(Issue { number: gh.number, title: gh.title, body: issue_body, marker })
    }

    fn update_issue(&self, number: u64, title: &str, body: &str) -> Result<(), GithubError> {
        let payload = serde_json::json!({ "title": title, "body": body }).to_string();
        self.request(Method::Patch, format!("{}/{number}", self.issues_url()), Some(payload))?;
        Ok(())
    }

    fn find_comment(&self, issue_number: u64, key: &str) -> Result<Option<Comment>, GithubError> {
        for page in 1..=MAX_LIST_PAGES {
            let url = format!("{}/{issue_number}/comments?per_page={PER_PAGE}&page={page}", self.issues_url());
            let response = self.request(Method::Get, url, None)?;
            let comments: Vec<GhComment> = decode(&response)?;
            let count = comments.len();
            for gh in comments {
                let body = gh.body.unwrap_or_default();
                let marker = parse_marker(&body);
                if marker.as_ref().is_some_and(|m| m.key == key) {
                    return Ok(Some(Comment { id: gh.id, body, marker }));
                }
            }
            if count < PER_PAGE as usize {
                break;
            }
        }
        Ok(None)
    }

    fn create_comment(&self, new: &NewComment) -> Result<Comment, GithubError> {
        let payload = serde_json::json!({ "body": new.body }).to_string();
        let url = format!("{}/{}/comments", self.issues_url(), new.issue_number);
        let response = self.request(Method::Post, url, Some(payload))?;
        let gh: GhComment = decode(&response)?;
        let body = gh.body.unwrap_or_else(|| new.body.clone());
        let marker = parse_marker(&body);
        Ok(Comment { id: gh.id, body, marker })
    }

    fn update_comment(&self, comment_id: u64, body: &str) -> Result<(), GithubError> {
        let payload = serde_json::json!({ "body": body }).to_string();
        let url = format!("{}/repos/{}/issues/comments/{comment_id}", self.api_base, self.repo_path);
        self.request(Method::Patch, url, Some(payload))?;
        Ok(())
    }
}

impl<T: HttpTransport> GitDataApi for ReqwestGithub<T> {
    fn get_ref(&self, name: &str) -> Result<Option<GitRef>, GithubError> {
        let Some(response) = self.request_opt(Method::Get, self.git_url(&format!("ref/{name}")))? else {
            return Ok(None);
        };
        let gh: GhRef = decode(&response)?;
        Ok(Some(gh.into_git_ref()))
    }

    fn create_ref(&self, name: &str, sha: &str) -> Result<GitRef, GithubError> {
        let payload = serde_json::json!({ "ref": format!("refs/{name}"), "sha": sha }).to_string();
        let response = self.request(Method::Post, self.git_url("refs"), Some(payload))?;
        let gh: GhRef = decode(&response)?;
        Ok(gh.into_git_ref())
    }

    fn update_ref(&self, name: &str, sha: &str, force: bool) -> Result<GitRef, GithubError> {
        let payload = serde_json::json!({ "sha": sha, "force": force }).to_string();
        let response = self.request(Method::Patch, self.git_url(&format!("refs/{name}")), Some(payload))?;
        let gh: GhRef = decode(&response)?;
        Ok(gh.into_git_ref())
    }

    fn list_matching_refs(&self, prefix: &str) -> Result<Vec<GitRef>, GithubError> {
        // matching-refs is paginated (100/page), so a bloom with more than one
        // page of checkpoints must be walked to the end — a single GET would
        // silently truncate the enumeration and drop reusable checkpoints. Same
        // page-walk as find_issue / find_comment: stop when a page is short.
        let base = self.git_url(&format!("matching-refs/{prefix}"));
        let mut out = Vec::new();
        for page in 1..=MAX_LIST_PAGES {
            let response = self.request(Method::Get, format!("{base}?per_page={PER_PAGE}&page={page}"), None)?;
            let refs: Vec<GhRef> = decode(&response)?;
            let count = refs.len();
            out.extend(refs.into_iter().map(GhRef::into_git_ref));
            if count < PER_PAGE as usize {
                break;
            }
        }
        Ok(out)
    }

    fn get_commit(&self, sha: &str) -> Result<GitCommit, GithubError> {
        let response = self.request(Method::Get, self.git_url(&format!("commits/{sha}")), None)?;
        let gh: GhCommit = decode(&response)?;
        Ok(gh.into_git_commit())
    }

    fn create_commit(&self, message: &str, tree: &str, parents: &[String]) -> Result<GitCommit, GithubError> {
        let payload = serde_json::json!({ "message": message, "tree": tree, "parents": parents }).to_string();
        let response = self.request(Method::Post, self.git_url("commits"), Some(payload))?;
        let gh: GhCommit = decode(&response)?;
        Ok(gh.into_git_commit())
    }
}

impl<T: HttpTransport> ActionsApi for ReqwestGithub<T> {
    fn dispatch_workflow(
        &self,
        workflow_file: &str,
        git_ref: &str,
        inputs: &BTreeMap<String, String>,
    ) -> Result<(), GithubError> {
        let payload = serde_json::json!({ "ref": git_ref, "inputs": inputs }).to_string();
        // `workflow_dispatch` answers 204 No Content with no run id; `request`
        // accepts the empty 2xx body and the executor resolves the run by nonce.
        self.request(Method::Post, self.actions_url(&format!("workflows/{workflow_file}/dispatches")), Some(payload))?;
        Ok(())
    }

    fn find_run(&self, workflow_file: &str, nonce: &str) -> Result<Option<WorkflowRun>, GithubError> {
        // The runs list is newest-first, so the first name-embedding-nonce match
        // is the run this nonce dispatched. Page-walk like the other list ops.
        let base = self.actions_url(&format!("workflows/{workflow_file}/runs"));
        for page in 1..=MAX_LIST_PAGES {
            let response = self.request(Method::Get, format!("{base}?per_page={PER_PAGE}&page={page}"), None)?;
            let list: GhRunList = decode(&response)?;
            let count = list.workflow_runs.len();
            for gh in list.workflow_runs {
                if gh.display_title.contains(nonce) {
                    return Ok(Some(gh.into_workflow_run()));
                }
            }
            if count < PER_PAGE as usize {
                break;
            }
        }
        Ok(None)
    }

    fn get_run(&self, run_id: u64) -> Result<WorkflowRun, GithubError> {
        let response = self.request(Method::Get, self.actions_url(&format!("runs/{run_id}")), None)?;
        let gh: GhRun = decode(&response)?;
        Ok(gh.into_workflow_run())
    }

    fn cancel_run(&self, run_id: u64) -> Result<(), GithubError> {
        self.request(Method::Post, self.actions_url(&format!("runs/{run_id}/cancel")), None)?;
        Ok(())
    }

    fn list_run_artifacts(&self, run_id: u64) -> Result<Vec<Artifact>, GithubError> {
        let base = self.actions_url(&format!("runs/{run_id}/artifacts"));
        let mut out = Vec::new();
        for page in 1..=MAX_LIST_PAGES {
            let response = self.request(Method::Get, format!("{base}?per_page={PER_PAGE}&page={page}"), None)?;
            let list: GhArtifactList = decode(&response)?;
            let count = list.artifacts.len();
            out.extend(list.artifacts.into_iter().map(GhArtifact::into_artifact));
            if count < PER_PAGE as usize {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::cell::RefCell;

    use super::{GithubApi, GithubError, HttpRequest, HttpResponse, HttpTransport, Method, NewIssue, ReqwestGithub};

    // Records the last request and replays a queued response — the seam that
    // lets us assert URL/method/body shaping and status→error mapping with no
    // network.
    struct RecordingTransport {
        last: RefCell<Option<HttpRequest>>,
        response: HttpResponse,
    }

    impl RecordingTransport {
        fn new(status: u16, body: &str) -> Self {
            Self { last: RefCell::new(None), response: HttpResponse { status, body: body.to_owned() } }
        }
    }

    impl HttpTransport for RecordingTransport {
        fn execute(&self, request: HttpRequest) -> Result<HttpResponse, GithubError> {
            *self.last.borrow_mut() = Some(request);
            Ok(self.response.clone())
        }
    }

    fn client(status: u16, body: &str) -> ReqwestGithub<RecordingTransport> {
        ReqwestGithub::with_transport(RecordingTransport::new(status, body), "https://api.github.com", "octo/shadow")
    }

    #[test]
    fn create_issue_shapes_a_post_to_the_repo_issues_route() {
        let github = client(201, r#"{"number":42,"title":"t","body":"b"}"#);
        let issue = github.create_issue(&NewIssue { title: "t".into(), body: "b".into() }).expect("2xx create decodes");

        assert_eq!(issue.number, 42);
        let request = github.transport.last.borrow().clone().expect("a request was sent");
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/issues");
        // The body carries the title and body fields.
        let sent: serde_json::Value = serde_json::from_str(&request.body.unwrap()).unwrap();
        assert_eq!(sent["title"], "t");
        assert_eq!(sent["body"], "b");
    }

    #[test]
    fn update_issue_patches_the_numbered_route() {
        let github = client(200, "{}");
        github.update_issue(42, "nt", "nb").expect("2xx patch");
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.method, Method::Patch);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/issues/42");
    }

    #[test]
    fn non_2xx_maps_to_a_status_error() {
        // Tripwire: a 422 must surface as `Status`, never a silent success or a
        // decode of the error body into a model.
        let github = client(422, r#"{"message":"Validation Failed"}"#);
        let error = github.create_issue(&NewIssue { title: "t".into(), body: "b".into() }).unwrap_err();
        match error {
            GithubError::Status { status, body } => {
                assert_eq!(status, 422);
                assert!(body.contains("Validation Failed"));
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    use super::GitDataApi;

    #[test]
    fn get_ref_maps_404_to_none() {
        // Tripwire: a missing ref is the clean `Ok(None)` the source port reads
        // as "namespace not yet created", never a `Status` error.
        let github = client(404, r#"{"message":"Not Found"}"#);
        assert_eq!(github.get_ref("heads/bloom/x/integration").expect("404 is Ok(None)"), None);
    }

    #[test]
    fn get_ref_strips_the_refs_prefix_and_targets_the_ref_route() {
        let github = client(200, r#"{"ref":"refs/heads/bloom/x/integration","object":{"sha":"abc"}}"#);
        let git_ref = github.get_ref("heads/bloom/x/integration").expect("2xx decodes").expect("present");
        assert_eq!(git_ref.name, "heads/bloom/x/integration");
        assert_eq!(git_ref.sha, "abc");
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/git/ref/heads/bloom/x/integration");
    }

    #[test]
    fn create_ref_posts_the_fully_qualified_ref() {
        let github = client(201, r#"{"ref":"refs/heads/bloom/x/checkpoint/aa","object":{"sha":"c0ffee"}}"#);
        github.create_ref("heads/bloom/x/checkpoint/aa", "c0ffee").expect("2xx create decodes");
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/git/refs");
        let sent: serde_json::Value = serde_json::from_str(&request.body.unwrap()).unwrap();
        // The create endpoint takes the full `refs/…` form, not the short one.
        assert_eq!(sent["ref"], "refs/heads/bloom/x/checkpoint/aa");
        assert_eq!(sent["sha"], "c0ffee");
    }

    #[test]
    fn update_ref_carries_the_force_flag() {
        // Tripwire: the CAS guard is `force:false` in the body — a slip to
        // `true` would silently clobber a concurrent advance.
        let github = client(200, r#"{"ref":"refs/heads/main","object":{"sha":"new"}}"#);
        github.update_ref("heads/main", "new", false).expect("2xx patch");
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.method, Method::Patch);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/git/refs/heads/main");
        let sent: serde_json::Value = serde_json::from_str(&request.body.unwrap()).unwrap();
        assert_eq!(sent["sha"], "new");
        assert_eq!(sent["force"], false);
    }

    #[test]
    fn create_commit_posts_tree_and_parents() {
        let github = client(201, r#"{"sha":"newcommit","tree":{"sha":"treesha"}}"#);
        let commit = github.create_commit("checkpoint", "treesha", &["parentsha".to_owned()]).expect("2xx decodes");
        assert_eq!(commit.sha, "newcommit");
        assert_eq!(commit.tree, "treesha");
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/git/commits");
        let sent: serde_json::Value = serde_json::from_str(&request.body.unwrap()).unwrap();
        assert_eq!(sent["tree"], "treesha");
        assert_eq!(sent["parents"][0], "parentsha");
    }

    use super::{ActionsApi, RunConclusion, RunStatus};
    use std::collections::BTreeMap;

    #[test]
    fn dispatch_workflow_posts_ref_and_inputs_and_tolerates_204() {
        // Tripwire: the dispatch route is `workflows/{file}/dispatches` and the
        // body carries `ref` + `inputs`; a 204 No Content (empty body) is a
        // success, never a decode attempt.
        let github = client(204, "");
        let mut inputs = BTreeMap::new();
        inputs.insert("command".to_owned(), "verify.clippy".to_owned());
        inputs.insert("nonce".to_owned(), "n-abc".to_owned());
        github.dispatch_workflow("bloomery-transform.yml", "refs/heads/main", &inputs).expect("204 is success");

        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.method, Method::Post);
        assert_eq!(
            request.url,
            "https://api.github.com/repos/octo/shadow/actions/workflows/bloomery-transform.yml/dispatches"
        );
        let sent: serde_json::Value = serde_json::from_str(&request.body.unwrap()).unwrap();
        assert_eq!(sent["ref"], "refs/heads/main");
        assert_eq!(sent["inputs"]["command"], "verify.clippy");
        assert_eq!(sent["inputs"]["nonce"], "n-abc");
    }

    #[test]
    fn find_run_scans_the_runs_route_and_matches_the_nonce_in_the_title() {
        let body = r#"{"workflow_runs":[
            {"id":7,"display_title":"transform verify.clippy n-abc","status":"in_progress","conclusion":null},
            {"id":8,"display_title":"transform verify.clippy n-xyz","status":"completed","conclusion":"success"}
        ]}"#;
        let github = client(200, body);
        let run = github.find_run("bloomery-transform.yml", "n-abc").expect("2xx decodes").expect("a match");
        assert_eq!(run.id, 7);
        assert_eq!(run.status, RunStatus::InProgress);
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(
            request.url,
            "https://api.github.com/repos/octo/shadow/actions/workflows/bloomery-transform.yml/runs?per_page=100&page=1"
        );
    }

    #[test]
    fn find_run_returns_none_when_no_title_embeds_the_nonce() {
        let body =
            r#"{"workflow_runs":[{"id":8,"display_title":"transform x n-other","status":"queued","conclusion":null}]}"#;
        let github = client(200, body);
        assert_eq!(github.find_run("bloomery-transform.yml", "n-abc").expect("2xx decodes"), None);
    }

    #[test]
    fn get_run_folds_a_completed_success() {
        let github = client(200, r#"{"id":9,"display_title":"t","status":"completed","conclusion":"success"}"#);
        let run = github.get_run(9).expect("2xx decodes");
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.conclusion, Some(RunConclusion::Success));
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/actions/runs/9");
    }

    #[test]
    fn get_run_folds_timed_out_to_failure_and_skipped_to_neutral() {
        // Tripwire: the failure-shaped conclusions collapse to Failure, the
        // neither-shaped to Neutral — a slip would misreport a run's verdict.
        let timed = client(200, r#"{"id":1,"display_title":"t","status":"completed","conclusion":"timed_out"}"#);
        assert_eq!(timed.get_run(1).unwrap().conclusion, Some(RunConclusion::Failure));
        let skipped = client(200, r#"{"id":2,"display_title":"t","status":"completed","conclusion":"skipped"}"#);
        assert_eq!(skipped.get_run(2).unwrap().conclusion, Some(RunConclusion::Neutral));
    }

    #[test]
    fn cancel_run_posts_the_cancel_route() {
        let github = client(202, "");
        github.cancel_run(9).expect("202 is success");
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/actions/runs/9/cancel");
    }

    #[test]
    fn list_run_artifacts_decodes_the_artifacts_route() {
        let github = client(200, r#"{"artifacts":[{"id":5,"name":"evidence-n-abc","size_in_bytes":128}]}"#);
        let artifacts = github.list_run_artifacts(9).expect("2xx decodes");
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].id, 5);
        assert_eq!(artifacts[0].name, "evidence-n-abc");
        assert_eq!(artifacts[0].size_bytes, 128);
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(
            request.url,
            "https://api.github.com/repos/octo/shadow/actions/runs/9/artifacts?per_page=100&page=1"
        );
    }

    #[test]
    fn list_matching_refs_paginates_the_prefix_route() {
        // A short first page (1 < PER_PAGE) ends the walk after one request; the
        // URL carries the pagination query so a >100-checkpoint bloom is walked
        // to the end rather than silently truncated to the first page.
        let github = client(200, r#"[{"ref":"refs/heads/bloom/x/checkpoint/aa","object":{"sha":"s"}}]"#);
        let refs = github.list_matching_refs("heads/bloom/x/checkpoint/").expect("2xx decodes");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "heads/bloom/x/checkpoint/aa");
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(
            request.url,
            "https://api.github.com/repos/octo/shadow/git/matching-refs/heads/bloom/x/checkpoint/?per_page=100&page=1"
        );
    }
}
