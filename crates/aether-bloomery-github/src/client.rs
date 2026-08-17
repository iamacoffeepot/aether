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
//! the endpoints a projection-only reconcile touches: issue comments (create /
//! update / find on one named object). There is no issue create, overwrite, or
//! find-by-marker verb, and that absence is the projection's bound rather than
//! an omission — with no such method on [`GithubApi`], nothing reachable from a
//! projection can address a human-authored title or body (ADR-0149 §The write
//! surface). Check-runs and the Git Data blob/tree/commit/ref surface belong to
//! the **git source port** — a separate sibling slice (ADR-0149 amendment
//! [#3460]) — and are intentionally absent: a check-run cannot attach without a
//! commit the source port produces, so shipping it here would be an endpoint
//! that cannot work projection-only.
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
use std::sync::Arc;
use std::time::Duration;

use reqwest::Method as ReqwestMethod;
use reqwest::blocking::Client as BlockingClient;
use serde::Deserialize;

use crate::parse_marker;

pub use aether_bloomery_git::{
    ActionsApi, Artifact, ChecksState, Comment, GitCommit, GitDataApi, GitRef, GithubApi, GithubError, IssueStateApi,
    MergeResult, NewComment, NewPullRequest, PullMergeResult, PullRequest, PullRequestApi, PullRequestState,
    RunConclusion, RunStatus, WorkflowRun, name_carries_nonce, strip_heads,
};

/// The HTTP verb an adapter request uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
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

/// The source of the bearer token each request authenticates with. The client
/// resolves a token from this on every request rather than freezing one at
/// construction, so a rotating credential — a GitHub-App installation token the
/// host mints and refreshes (ADR-0149 §Migration step 3) — is picked up without
/// reconstructing the client. The backward-compatible static-PAT path is a
/// [`StaticTokenSource`].
///
/// `Send + Sync` because the client lives in a capability's runtime state, held
/// behind an `Arc` and driven from the actor's dispatch thread.
pub trait TokenSource: Send + Sync {
    /// The current bearer token.
    ///
    /// # Errors
    /// The token could not be produced — e.g. a minting source's exchange with
    /// the credential authority failed.
    fn token(&self) -> Result<String, GithubError>;
}

/// A fixed bearer token — the backward-compatible personal-access-token path.
/// `GithubConfig.token` becomes one of these, so the mirror / unconfigured /
/// test paths keep authenticating with a static string.
pub struct StaticTokenSource {
    token: String,
}

impl StaticTokenSource {
    /// Wrap a fixed bearer `token`.
    #[must_use]
    pub fn new(token: String) -> Self {
        Self { token }
    }
}

impl TokenSource for StaticTokenSource {
    fn token(&self) -> Result<String, GithubError> {
        Ok(self.token.clone())
    }
}

/// A minted GitHub-App installation token and the moment it expires (the
/// `expires_at` GitHub returns, an RFC3339 timestamp string kept for
/// diagnostics). The host's App-auth custody caches this and re-mints before
/// expiry.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InstallationToken {
    /// The installation access token — the bearer a
    /// [`TokenSource`] hands the client.
    pub token: String,
    /// The token's expiry as GitHub reports it (RFC3339), for diagnostics.
    pub expires_at: String,
}

/// The production transport: a `reqwest::blocking` client. The bearer token is
/// not held here — the client resolves it per request from a [`TokenSource`]
/// and passes it in the request's `Authorization` header, so the transport only
/// executes requests.
pub struct ReqwestTransport {
    client: BlockingClient,
}

/// Connect-phase bound for every GitHub HTTP hop through this transport.
///
/// Tied to the executor-driver wedge (#3640): `on_dispatch_tick` runs these
/// calls inline on the cooperative chassis dispatcher, so an unbounded
/// connect stalls the actor's `DispatcherSlot` forever.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Total request-phase bound (connect + send + response) for every GitHub
/// HTTP hop through this transport. See [`HTTP_CONNECT_TIMEOUT`].
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

impl ReqwestTransport {
    /// Build the transport's `reqwest::blocking` client with the production
    /// timeouts.
    ///
    /// # Errors
    /// The `reqwest` client could not be constructed.
    pub fn new() -> Result<Self, GithubError> {
        Self::with_timeouts(HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT)
    }

    /// Build the transport's `reqwest::blocking` client with explicit
    /// connect/total timeouts, so a caller (e.g. a regression test) can drive
    /// the real transport against a short deadline.
    ///
    /// # Errors
    /// The `reqwest` client could not be constructed.
    pub fn with_timeouts(connect: Duration, total: Duration) -> Result<Self, GithubError> {
        let client = BlockingClient::builder()
            .user_agent("aether-bloomery-github")
            .connect_timeout(connect)
            .timeout(total)
            .build()
            .map_err(|error| GithubError::Transport(error.to_string()))?;
        Ok(Self { client })
    }
}

impl HttpTransport for ReqwestTransport {
    fn execute(&self, request: HttpRequest) -> Result<HttpResponse, GithubError> {
        let method = match request.method {
            Method::Get => ReqwestMethod::GET,
            Method::Post => ReqwestMethod::POST,
            Method::Put => ReqwestMethod::PUT,
            Method::Patch => ReqwestMethod::PATCH,
            Method::Delete => ReqwestMethod::DELETE,
        };
        // The `Authorization` bearer rides in `request.headers` (the client
        // stamps it from its `TokenSource`), so the transport applies the
        // caller's headers uniformly and holds no token itself.
        let mut builder = self
            .client
            .request(method, &request.url)
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
    token_source: Arc<dyn TokenSource>,
    api_base: String,
    repo_path: String,
}

/// The bound on how many list pages `find_*` walks — a shadow repo stays far
/// under this, and it caps an otherwise-unbounded pagination loop rather than
/// spinning forever on a misbehaving server.
const MAX_LIST_PAGES: u32 = 100;
const PER_PAGE: u32 = 100;

impl<T: HttpTransport> ReqwestGithub<T> {
    /// Build a client over `transport` bearing tokens from `token_source`,
    /// rooted at `api_base` (no trailing slash) for `owner/repo`.
    pub fn with_transport(
        transport: T,
        token_source: Arc<dyn TokenSource>,
        api_base: impl Into<String>,
        repo_path: impl Into<String>,
    ) -> Self {
        Self { transport, token_source, api_base: api_base.into(), repo_path: repo_path.into() }
    }

    fn issues_url(&self) -> String {
        format!("{}/repos/{}/issues", self.api_base, self.repo_path)
    }

    // Resolve the current bearer from the token source and stamp it as the
    // request's `Authorization` header, then execute. Every request routes
    // through here so a rotating (App-minted) token is picked up per request
    // and the transport stays token-agnostic.
    fn dispatch(&self, method: Method, url: String, body: Option<String>) -> Result<HttpResponse, GithubError> {
        let token = self.token_source.token()?;
        let headers = vec![("Authorization".to_owned(), format!("Bearer {token}"))];
        self.transport.execute(HttpRequest { method, url, headers, body })
    }

    fn request(&self, method: Method, url: String, body: Option<String>) -> Result<HttpResponse, GithubError> {
        let response = self.dispatch(method, url, body)?;
        if (200..300).contains(&response.status) {
            Ok(response)
        } else {
            Err(GithubError::Status { status: response.status, body: response.body })
        }
    }

    // Like `request`, but a 404 is the clean "not present" answer (`Ok(None)`)
    // rather than an error — a ref lookup that misses is not a fault.
    fn request_opt(&self, method: Method, url: String) -> Result<Option<HttpResponse>, GithubError> {
        let response = self.dispatch(method, url, None)?;
        if (200..300).contains(&response.status) {
            Ok(Some(response))
        } else if response.status == 404 {
            Ok(None)
        } else {
            Err(GithubError::Status { status: response.status, body: response.body })
        }
    }

    /// Mint an installation access token for `installation_id` — the
    /// `POST /app/installations/{id}/access_tokens` exchange (ADR-0149
    /// §Migration step 3). The client's [`TokenSource`] must bear the App JWT
    /// for this call; the host's App-auth custody builds a JWT-bearing client
    /// solely to drive this exchange, then authenticates every other request
    /// with the returned installation token.
    ///
    /// # Errors
    /// The exchange surface is unreachable or returned an error status, or the
    /// 2xx body did not decode as an installation token.
    pub fn create_installation_token(&self, installation_id: u64) -> Result<InstallationToken, GithubError> {
        let url = format!("{}/app/installations/{installation_id}/access_tokens", self.api_base);
        let response = self.request(Method::Post, url, None)?;
        let gh: GhInstallationToken = decode(&response)?;
        Ok(InstallationToken { token: gh.token, expires_at: gh.expires_at })
    }

    fn git_url(&self, suffix: &str) -> String {
        format!("{}/repos/{}/git/{suffix}", self.api_base, self.repo_path)
    }

    fn actions_url(&self, suffix: &str) -> String {
        format!("{}/repos/{}/actions/{suffix}", self.api_base, self.repo_path)
    }

    fn pulls_url(&self, suffix: &str) -> String {
        format!("{}/repos/{}/pulls{suffix}", self.api_base, self.repo_path)
    }

    fn merges_url(&self) -> String {
        format!("{}/repos/{}/merges", self.api_base, self.repo_path)
    }

    fn compare_url(&self, base: &str, head: &str) -> String {
        format!("{}/repos/{}/compare/{}...{}", self.api_base, self.repo_path, strip_heads(base), strip_heads(head))
    }

    /// Files that differ between `base` and `head`, plus their concatenated
    /// patches. A compare fault returns empty rather than turning a known
    /// collision into a transport error — the merge already answered.
    fn compare_conflict(&self, base: &str, head: &str) -> (Vec<String>, String) {
        let Ok(response) = self.request(Method::Get, self.compare_url(base, head), None) else {
            return (Vec::new(), String::new());
        };
        let Ok(compared) = decode::<GhCompare>(&response) else {
            return (Vec::new(), String::new());
        };
        render_compare(compared.files)
    }
}

/// Turn GitHub compare-file rows into colliding paths and a unified patch
/// the reconcile overlay can quote. Binary files (no `patch`) still name
/// the path so the work order is not silent about them.
fn render_compare(files: Vec<GhCompareFile>) -> (Vec<String>, String) {
    use core::fmt::Write;
    let mut paths = Vec::with_capacity(files.len());
    let mut patch = String::new();
    for file in files {
        if file.filename.is_empty() {
            continue;
        }
        paths.push(file.filename.clone());
        let Some(hunk) = file.patch.filter(|hunk| !hunk.is_empty()) else {
            continue;
        };
        if !patch.is_empty() {
            patch.push('\n');
        }
        let _ = writeln!(patch, "diff --git a/{0} b/{0}", file.filename);
        patch.push_str(&hunk);
        if !hunk.ends_with('\n') {
            patch.push('\n');
        }
    }
    (paths, patch)
}

impl ReqwestGithub<ReqwestTransport> {
    /// Build a client over the production `reqwest::blocking` transport, bearing
    /// the static PAT from `config` — the backward-compatible path.
    ///
    /// # Errors
    /// The `reqwest` client could not be constructed.
    pub fn new(config: &crate::GithubConfig) -> Result<Self, GithubError> {
        let source = Arc::new(StaticTokenSource::new(config.token.clone()));
        Self::with_token_source(source, config.api_base.clone(), config.repo_path())
    }

    /// Build a client over the production `reqwest::blocking` transport, bearing
    /// tokens from `token_source` — the App-minted-token path (ADR-0149
    /// §Migration step 3), where the source is the host's cached-and-refreshing
    /// installation-token custody.
    ///
    /// # Errors
    /// The `reqwest` client could not be constructed.
    pub fn with_token_source(
        token_source: Arc<dyn TokenSource>,
        api_base: impl Into<String>,
        repo_path: impl Into<String>,
    ) -> Result<Self, GithubError> {
        let transport = ReqwestTransport::new()?;
        Ok(Self::with_transport(transport, token_source, api_base, repo_path))
    }
}

#[derive(Deserialize)]
struct GhInstallationToken {
    token: String,
    #[serde(default)]
    expires_at: String,
}

#[derive(Deserialize)]
struct GhComment {
    id: u64,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Deserialize)]
struct GhIssue {
    title: String,
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
struct GhPullRequestHead {
    sha: String,
    #[serde(rename = "ref")]
    ref_name: String,
}

#[derive(Deserialize)]
struct GhPullRequestBase {
    #[serde(rename = "ref")]
    ref_name: String,
}

#[derive(Deserialize)]
struct GhPullRequest {
    number: u64,
    #[serde(default)]
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    merge_commit_sha: Option<String>,
    head: GhPullRequestHead,
    base: GhPullRequestBase,
}

impl GhPullRequest {
    fn into_pull_request(self) -> PullRequest {
        // `merged` is absent from the list endpoint's objects (it is a detail-
        // view field), so a listed pull request decodes `merged: false` and the
        // closed state is what a caller reads. The land watch re-reads by
        // number through the detail route, where the field is present.
        let state = if self.state == "open" {
            PullRequestState::Open
        } else {
            PullRequestState::Closed
        };
        PullRequest {
            number: self.number,
            head_sha: self.head.sha,
            head_ref: self.head.ref_name,
            base: self.base.ref_name,
            state,
            merged: self.merged,
            // Blanked unless the pull request actually merged: GitHub populates
            // the field on an open one with a throwaway test-merge commit that
            // is on no branch, and admitting that as a landing would record a
            // mainline head that exists nowhere.
            merge_commit_sha: self.merged.then_some(self.merge_commit_sha).flatten(),
        }
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
    message: String,
}

impl GhCommit {
    fn into_git_commit(self) -> GitCommit {
        GitCommit { sha: self.sha, tree: self.tree.sha, message: self.message }
    }
}

/// `PUT /repos/{owner}/{repo}/pulls/{number}/merge` — the accepted reply. Its
/// `sha` is the commit the base branch became, which under a squash is a commit
/// neither branch previously carried.
#[derive(Deserialize)]
struct GhMerged {
    sha: String,
}

/// The merges endpoint's reply. It is a *repository* commit, which nests the
/// message and tree under `commit` — not the flat Git Data commit [`GhCommit`]
/// decodes. Same concept, different serialization, so it needs its own shape
/// rather than a reuse that would fail to decode against real GitHub.
#[derive(Deserialize)]
struct GhMergeCommit {
    sha: String,
    commit: GhMergeCommitDetail,
}

#[derive(Deserialize)]
struct GhMergeCommitDetail {
    tree: GhTreeRef,
    message: String,
}

impl GhMergeCommit {
    fn into_git_commit(self) -> GitCommit {
        GitCommit { sha: self.sha, tree: self.commit.tree.sha, message: self.commit.message }
    }
}

/// `GET /repos/{owner}/{repo}/compare/{base}...{head}` — the follow-up a
/// merge 409 needs, because that status names no files.
#[derive(Deserialize)]
struct GhCompare {
    #[serde(default)]
    status: String,
    #[serde(default)]
    files: Vec<GhCompareFile>,
}

#[derive(Deserialize)]
struct GhCompareFile {
    #[serde(default)]
    filename: String,
    #[serde(default)]
    patch: Option<String>,
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

/// The `GET /commits/{sha}/check-runs` listing.
#[derive(Deserialize)]
struct GhCheckRunList {
    check_runs: Vec<GhCheckRun>,
}

/// One row of that listing — the two fields the landing verdict turns on.
#[derive(Deserialize)]
struct GhCheckRun {
    name: String,
    status: String,
    conclusion: Option<String>,
}

/// Fold a commit's check runs into the landing gate's verdict.
///
/// Anything still queued or running makes the whole set pending, because a
/// later check can still fail and a bloom must not be judged on a partial gate.
/// A conclusion is failing unless it is one of the passing spellings — an
/// unknown conclusion reads as a failure rather than being skipped past, so a
/// vocabulary GitHub adds cannot silently pass the gate. `skipped` and
/// `neutral` do not fail it: they are the shapes a conditional job takes when
/// it correctly declines to run.
fn fold_checks(runs: &[GhCheckRun]) -> ChecksState {
    if runs.is_empty() {
        return ChecksState::Absent;
    }
    if runs.iter().any(|run| run.status != "completed") {
        return ChecksState::Pending;
    }

    let failing: Vec<String> = runs
        .iter()
        .filter(|run| !matches!(run.conclusion.as_deref(), Some("success" | "skipped" | "neutral")))
        .map(|run| run.name.clone())
        .collect();
    if failing.is_empty() {
        ChecksState::Passed
    } else {
        ChecksState::Failed { failing }
    }
}

fn decode<D: for<'de> Deserialize<'de>>(response: &HttpResponse) -> Result<D, GithubError> {
    serde_json::from_str(&response.body).map_err(|error| GithubError::Decode(error.to_string()))
}

impl<T: HttpTransport> GithubApi for ReqwestGithub<T> {
    fn issue_title(&self, number: u64) -> Result<Option<String>, GithubError> {
        let Some(response) = self.request_opt(Method::Get, format!("{}/{number}", self.issues_url()))? else {
            return Ok(None);
        };
        Ok(Some(decode::<GhIssue>(&response)?.title))
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
                return Ok(None);
            }
        }
        Err(GithubError::PaginationExhausted { what: "issue comments".to_owned() })
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

impl<T: HttpTransport> IssueStateApi for ReqwestGithub<T> {
    fn close_issue(&self, number: u64) -> Result<(), GithubError> {
        let payload = serde_json::json!({ "state": "closed" }).to_string();
        self.request(Method::Patch, format!("{}/{number}", self.issues_url()), Some(payload))?;
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

    fn delete_ref(&self, name: &str) -> Result<(), GithubError> {
        // Name-only DELETE on the qualified `refs/{name}` route. A 404/422 means
        // the ref is already gone — the idempotent outcome release's cleanup
        // delete and an acquire's rollback both rely on, not a fault.
        let response = self.dispatch(Method::Delete, self.git_url(&format!("refs/{name}")), None)?;
        if (200..300).contains(&response.status) || response.status == 404 || response.status == 422 {
            Ok(())
        } else {
            Err(GithubError::Status { status: response.status, body: response.body })
        }
    }

    fn list_matching_refs(&self, prefix: &str) -> Result<Vec<GitRef>, GithubError> {
        // matching-refs is paginated (100/page), so a bloom with more than one
        // page of checkpoints must be walked to the end — a single GET would
        // silently truncate the enumeration and drop reusable checkpoints. Same
        // page-walk as find_comment / find_run: stop when a page is short.
        let base = self.git_url(&format!("matching-refs/{prefix}"));
        let mut out = Vec::new();
        for page in 1..=MAX_LIST_PAGES {
            let response = self.request(Method::Get, format!("{base}?per_page={PER_PAGE}&page={page}"), None)?;
            let refs: Vec<GhRef> = decode(&response)?;
            let count = refs.len();
            out.extend(refs.into_iter().map(GhRef::into_git_ref));
            if count < PER_PAGE as usize {
                return Ok(out);
            }
        }
        // Falling off the page cap means the enumeration truncated — a silently
        // short ref list would drop reusable checkpoints, so surface it.
        Err(GithubError::PaginationExhausted { what: "matching refs".to_owned() })
    }

    fn get_commit(&self, sha: &str) -> Result<GitCommit, GithubError> {
        let response = self.request(Method::Get, self.git_url(&format!("commits/{sha}")), None)?;
        let gh: GhCommit = decode(&response)?;
        Ok(gh.into_git_commit())
    }

    fn is_ancestor(&self, ancestor: &str, commit: &str) -> Result<bool, GithubError> {
        if ancestor == commit {
            return Ok(true);
        }
        // `ahead` / `identical`: `commit` contains `ancestor`. `behind` is
        // the stale-ancestor case #4938 refuses. `diverged` is a rewrite of
        // the live ref; observation asks this both ways and follows it.
        let response = self.request(Method::Get, self.compare_url(ancestor, commit), None)?;
        let compared: GhCompare = decode(&response)?;
        Ok(matches!(compared.status.as_str(), "ahead" | "identical"))
    }

    fn create_commit(&self, message: &str, tree: &str, parents: &[String]) -> Result<GitCommit, GithubError> {
        let payload = serde_json::json!({ "message": message, "tree": tree, "parents": parents }).to_string();
        let response = self.request(Method::Post, self.git_url("commits"), Some(payload))?;
        let gh: GhCommit = decode(&response)?;
        Ok(gh.into_git_commit())
    }

    fn merge(&self, base: &str, head: &str, message: &str) -> Result<MergeResult, GithubError> {
        // The merges endpoint speaks branch names, not refs — it is a repository
        // operation rather than a Git Data one, so it takes neither the `refs/`
        // form nor this trait's `heads/` shorthand. Normalizing here keeps every
        // caller on one ref vocabulary, the way `create_ref` re-adds `refs/`.
        let payload = serde_json::json!({
            "base": strip_heads(base),
            "head": strip_heads(head),
            "commit_message": message,
        })
        .to_string();
        let response = self.dispatch(Method::Post, self.merges_url(), Some(payload))?;

        // 204 is "base already contains head" — a success with no body, so it
        // must be read before any decode. 409 is a conflict, which is an answer
        // about the two histories rather than a transport fault.
        match response.status {
            204 => Ok(MergeResult::AlreadyUpToDate),
            409 => {
                // GitHub's merge 409 body is a bare "Merge conflict" with no
                // file list. The compare of the same two refs names the files
                // that differ and carries their patches — the collision
                // evidence the reconcile overlay needs.
                let (paths, patch) = self.compare_conflict(base, head);
                Ok(MergeResult::Conflict { detail: response.body, paths, patch })
            }
            status if (200..300).contains(&status) => {
                Ok(MergeResult::Merged(decode::<GhMergeCommit>(&response)?.into_git_commit()))
            }
            status => Err(GithubError::Status { status, body: response.body }),
        }
    }
}

impl<T: HttpTransport> PullRequestApi for ReqwestGithub<T> {
    fn create_pull_request(&self, new: &NewPullRequest) -> Result<PullRequest, GithubError> {
        let payload = serde_json::json!({
            "title": new.title,
            "body": new.body,
            "head": new.head,
            "base": new.base,
        })
        .to_string();
        let response = self.request(Method::Post, self.pulls_url(""), Some(payload))?;
        let gh: GhPullRequest = decode(&response)?;
        Ok(gh.into_pull_request())
    }

    fn get_pull_request(&self, number: u64) -> Result<Option<PullRequest>, GithubError> {
        let Some(response) = self.request_opt(Method::Get, self.pulls_url(&format!("/{number}")))? else {
            return Ok(None);
        };
        let gh: GhPullRequest = decode(&response)?;
        Ok(Some(gh.into_pull_request()))
    }

    fn checks_for_ref(&self, sha: &str) -> Result<ChecksState, GithubError> {
        let url = format!("{}/repos/{}/commits/{sha}/check-runs", self.api_base, self.repo_path);
        let response = self.request(Method::Get, url, None)?;
        let listing: GhCheckRunList = decode(&response)?;
        Ok(fold_checks(&listing.check_runs))
    }

    fn squash_merge_pull_request(&self, number: u64, expected_head_sha: &str) -> Result<PullMergeResult, GithubError> {
        let payload = serde_json::json!({ "merge_method": "squash", "sha": expected_head_sha }).to_string();
        // Dispatched rather than `request`ed: the two refusing statuses are
        // outcomes this maps, so raising them to errors first and unwrapping
        // them back would put the classification in the error path.
        let response = self.dispatch(Method::Put, self.pulls_url(&format!("/{number}/merge")), Some(payload))?;
        match response.status {
            status if (200..300).contains(&status) => {
                Ok(PullMergeResult::Merged { merge_commit_sha: decode::<GhMerged>(&response)?.sha })
            }
            status @ (405 | 409) => Ok(PullMergeResult::Refused { status, detail: response.body }),
            status => Err(GithubError::Status { status, body: response.body }),
        }
    }

    fn find_pull_request_for_head(&self, head: &str) -> Result<Option<PullRequest>, GithubError> {
        // The list endpoint qualifies `head` as `owner:branch`. Same-repo pull
        // requests are all this path opens, so the owner is our own — taken
        // from the configured `owner/repo` rather than asked for separately.
        let owner = self.repo_path.split('/').next().unwrap_or(&self.repo_path);
        let url =
            self.pulls_url(&format!("?head={owner}:{head}&state=all&sort=created&direction=desc&per_page={PER_PAGE}"));
        let pulls: Vec<GhPullRequest> = decode(&self.request(Method::Get, url, None)?)?;
        // Newest first, so the first match is the current proposal; no page walk
        // is owed the way `list_matching_refs` owes one to an unbounded ref
        // enumeration, since only the most recent one is ever adopted.
        Ok(pulls.into_iter().next().map(GhPullRequest::into_pull_request))
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
        // The match is delimiter-bounded (`name_carries_nonce`), never a raw
        // `contains`: nonces are `dispatch-<int>`-shaped, so `dispatch-4` is a
        // prefix of `dispatch-42`, and the newer run's title would otherwise
        // shadow the older nonce's — mis-resolving inspect, cancelling the
        // wrong run, and returning silently empty evidence (#3662).
        let base = self.actions_url(&format!("workflows/{workflow_file}/runs"));
        for page in 1..=MAX_LIST_PAGES {
            let response = self.request(Method::Get, format!("{base}?per_page={PER_PAGE}&page={page}"), None)?;
            let list: GhRunList = decode(&response)?;
            let count = list.workflow_runs.len();
            for gh in list.workflow_runs {
                if name_carries_nonce(&gh.display_title, nonce) {
                    return Ok(Some(gh.into_workflow_run()));
                }
            }
            if count < PER_PAGE as usize {
                return Ok(None);
            }
        }
        Err(GithubError::PaginationExhausted { what: "workflow runs".to_owned() })
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
                return Ok(out);
            }
        }
        Err(GithubError::PaginationExhausted { what: "run artifacts".to_owned() })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::cell::RefCell;

    use std::sync::{Arc, Mutex};

    use super::{
        GitDataApi, GithubApi, GithubError, HttpRequest, HttpResponse, HttpTransport, IssueStateApi, MergeResult,
        Method, NewComment, ReqwestGithub, StaticTokenSource, TokenSource,
    };

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

    // A merge 409 is two requests (the merge, then the compare). One canned
    // body cannot answer both, so this walks a queue.
    struct QueuedTransport {
        last: RefCell<Option<HttpRequest>>,
        responses: RefCell<Vec<HttpResponse>>,
    }

    impl QueuedTransport {
        fn new(responses: Vec<HttpResponse>) -> Self {
            Self { last: RefCell::new(None), responses: RefCell::new(responses) }
        }
    }

    impl HttpTransport for QueuedTransport {
        fn execute(&self, request: HttpRequest) -> Result<HttpResponse, GithubError> {
            *self.last.borrow_mut() = Some(request);
            let mut responses = self.responses.borrow_mut();
            if responses.is_empty() {
                return Err(GithubError::Transport("queued transport exhausted".to_owned()));
            }
            Ok(responses.remove(0))
        }
    }

    // A token source whose value can change between requests — the seam for
    // asserting the client re-reads a rotating token per request.
    struct MutableTokenSource {
        value: Mutex<String>,
    }

    impl TokenSource for MutableTokenSource {
        fn token(&self) -> Result<String, GithubError> {
            Ok(self.value.lock().expect("token lock").clone())
        }
    }

    fn client(status: u16, body: &str) -> ReqwestGithub<RecordingTransport> {
        ReqwestGithub::with_transport(
            RecordingTransport::new(status, body),
            Arc::new(StaticTokenSource::new("t0ken".to_owned())),
            "https://api.github.com",
            "octo/shadow",
        )
    }

    // The `Authorization: Bearer …` header the client stamps from its source.
    fn bearer(request: &HttpRequest) -> String {
        request
            .headers
            .iter()
            .find(|(name, _)| name == "Authorization")
            .map(|(_, value)| value.clone())
            .expect("a request carries an Authorization header")
    }

    #[test]
    fn each_request_bearer_is_read_from_the_token_source() {
        // Tripwire: the client resolves the bearer per request from its source,
        // so a rotated token (an App re-mint) is picked up without rebuilding the
        // client — a slip back to a construction-frozen token would keep sending
        // the stale value.
        let source = Arc::new(MutableTokenSource { value: Mutex::new("first".to_owned()) });
        let github = ReqwestGithub::with_transport(
            RecordingTransport::new(200, "{}"),
            source.clone(),
            "https://api.github.com",
            "octo/shadow",
        );

        github.update_comment(1, "b").expect("2xx patch");
        assert_eq!(bearer(&github.transport.last.borrow().clone().unwrap()), "Bearer first");

        *source.value.lock().unwrap() = "second".to_owned();
        github.update_comment(1, "b").expect("2xx patch");
        assert_eq!(bearer(&github.transport.last.borrow().clone().unwrap()), "Bearer second");
    }

    #[test]
    fn create_installation_token_posts_the_app_route_and_decodes_token_and_expiry() {
        // Tripwire: the exchange is `POST /app/installations/{id}/access_tokens`
        // (App-level, not repo-scoped) and decodes both the token and its
        // expiry — the shape the host's App-auth custody caches.
        let github = client(201, r#"{"token":"ghs_minted","expires_at":"2026-07-17T13:00:00Z"}"#);
        let minted = github.create_installation_token(42).expect("2xx decodes");
        assert_eq!(minted.token, "ghs_minted");
        assert_eq!(minted.expires_at, "2026-07-17T13:00:00Z");
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://api.github.com/app/installations/42/access_tokens");
    }

    #[test]
    fn issue_title_gets_the_named_object_and_absorbs_a_404() {
        // Tripwire: a slip in this route reads as "the repository holds no such
        // object", which the landing assembly answers by dropping to the floor
        // title — a silent downgrade with nothing failing anywhere.
        let github = client(200, r#"{"number":7,"title":"fix(crate:aether-fs): reject a traversing path"}"#);

        assert_eq!(
            github.issue_title(7).expect("2xx decodes").as_deref(),
            Some("fix(crate:aether-fs): reject a traversing path")
        );
        let request = github.transport.last.borrow().clone().expect("a request was sent");
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/issues/7");
        assert!(request.body.is_none(), "a title read writes nothing");

        // An object the repository does not hold is the clean absence the
        // fallback expects, not an error that would stop the drain.
        assert_eq!(client(404, "{}").issue_title(7).expect("a 404 is the clean absence"), None);
    }

    #[test]
    fn create_comment_shapes_a_post_to_the_named_object_comments_route() {
        // Tripwire: the comment route is scoped to one object number. A slip to
        // the bare issues route would open an object instead of commenting on
        // one — the write the projection is bounded away from.
        let github = client(201, r#"{"id":42,"body":"b"}"#);
        let comment =
            github.create_comment(&NewComment { issue_number: 7, body: "b".into() }).expect("2xx create decodes");

        assert_eq!(comment.id, 42);
        let request = github.transport.last.borrow().clone().expect("a request was sent");
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/issues/7/comments");
        let sent: serde_json::Value = serde_json::from_str(&request.body.unwrap()).unwrap();
        assert_eq!(sent["body"], "b");
        assert!(sent.get("title").is_none(), "a comment write carries no title field");
    }

    #[test]
    fn update_comment_patches_the_repository_wide_comment_route() {
        // Tripwire: an issue comment is edited on `issues/comments/{id}` — the
        // repository-wide route with no object number in it — not under the
        // object it hangs off. The wrong route 404s only against real GitHub.
        let github = client(200, "{}");
        github.update_comment(42, "nb").expect("2xx patch");
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.method, Method::Patch);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/issues/comments/42");
    }

    #[test]
    fn close_issue_patches_the_named_object_to_closed() {
        // Tripwire: a slip to the comment route or a body/title write would
        // either 404 against real GitHub or rewrite the human-authored issue —
        // the write the land reactor is bounded away from.
        let github = client(200, r#"{"number":7,"state":"closed"}"#);
        github.close_issue(7).expect("2xx close");
        let request = github.transport.last.borrow().clone().expect("a request was sent");
        assert_eq!(request.method, Method::Patch);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/issues/7");
        let sent: serde_json::Value = serde_json::from_str(&request.body.unwrap()).unwrap();
        assert_eq!(sent["state"], "closed");
        assert!(sent.get("title").is_none(), "a close writes no title");
        assert!(sent.get("body").is_none(), "a close writes no body");
    }

    #[test]
    fn non_2xx_maps_to_a_status_error() {
        // Tripwire: a 422 must surface as `Status`, never a silent success or a
        // decode of the error body into a model.
        let github = client(422, r#"{"message":"Validation Failed"}"#);
        let error = github.create_comment(&NewComment { issue_number: 7, body: "b".into() }).unwrap_err();
        match error {
            GithubError::Status { status, body } => {
                assert_eq!(status, 422);
                assert!(body.contains("Validation Failed"));
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn find_comment_signals_pagination_exhaustion_rather_than_a_false_not_found() {
        // Tripwire: when every page is full to the cap and none matches, the walk
        // truncated — it must surface `PaginationExhausted`, never fold a
        // not-yet-searched comment into a `Ok(None)` "absent", which the
        // projection would answer by writing a duplicate.
        let full_page: Vec<String> = (0..100).map(|i| format!(r#"{{"id":{i},"body":"no marker"}}"#)).collect();
        let github = client(200, &format!("[{}]", full_page.join(",")));
        match github.find_comment(7, "never-present").unwrap_err() {
            GithubError::PaginationExhausted { what } => assert_eq!(what, "issue comments"),
            other => panic!("expected PaginationExhausted, got {other:?}"),
        }
    }

    #[test]
    fn merge_reads_the_repository_commit_shape_not_the_git_data_one() {
        // Tripwire: the merges endpoint answers with a *repository* commit,
        // nesting message and tree under `commit`, while every other commit this
        // client reads is the flat Git Data shape. Decoding the merge reply with
        // the flat struct compiles and passes against any fake — it only fails
        // against real GitHub, so the shape is pinned here.
        let github =
            client(201, r#"{"sha":"merge1","commit":{"message":"fold wp-a","tree":{"sha":"combined"}},"parents":[]}"#);
        let MergeResult::Merged(commit) = github.merge("heads/bloom/x/integration", "heads/cand", "fold wp-a").unwrap()
        else {
            panic!("a 201 is a completed merge");
        };
        assert_eq!(commit.sha, "merge1");
        assert_eq!(commit.tree, "combined", "the tree is read from under `commit`, not the top level");

        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/merges");
        let body: serde_json::Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["base"], "bloom/x/integration", "the endpoint takes bare branch names, not the heads/ form");
        assert_eq!(body["head"], "cand");
    }

    #[test]
    fn merge_separates_nothing_to_do_from_a_conflict_and_from_a_fault() {
        // The three non-201 answers carry three different meanings, and folding
        // any pair together breaks a fold: a 204 read as failure stalls on an
        // already-folded member, a 409 read as a fault gets retried forever
        // instead of parked, and a 404 read as a conflict parks an owner on a
        // missing branch.
        assert_eq!(
            client(204, "").merge("heads/base", "heads/head", "m").unwrap(),
            MergeResult::AlreadyUpToDate,
            "204 is success with no body — it must not reach the decoder",
        );
        let MergeResult::Conflict { detail, .. } =
            client(409, r#"{"message":"Merge conflict"}"#).merge("heads/base", "heads/head", "m").unwrap()
        else {
            panic!("409 is a conflict outcome, not an error");
        };
        assert!(detail.contains("Merge conflict"), "the conflict carries the endpoint's own report: {detail}");
        assert!(
            matches!(
                client(404, r#"{"message":"Not Found"}"#).merge("heads/base", "heads/gone", "m"),
                Err(GithubError::Status { status: 404, .. })
            ),
            "a missing base or head is a fault, not a conflict",
        );
    }

    #[test]
    fn a_merge_conflict_fills_paths_and_patch_from_the_compare() {
        // GitHub's merge 409 names no files. The follow-up compare is what
        // populates the collision evidence; without it a production fold
        // dispatches a reconcile order that cannot see the member's work.
        let transport = QueuedTransport::new(vec![
            HttpResponse { status: 409, body: r#"{"message":"Merge conflict"}"#.to_owned() },
            HttpResponse {
                status: 200,
                body: r#"{"files":[{"filename":"crates/overlap.rs","patch":"@@ -1 +1,2 @@\n keep\n+added"}]}"#
                    .to_owned(),
            },
        ]);
        let github = ReqwestGithub::with_transport(
            transport,
            Arc::new(StaticTokenSource::new("t0ken".to_owned())),
            "https://api.github.com",
            "octo/shadow",
        );
        let MergeResult::Conflict { paths, patch, .. } =
            github.merge("heads/bloom/x/integration", "heads/cand", "fold").unwrap()
        else {
            panic!("409 is a conflict outcome");
        };
        assert_eq!(paths, vec!["crates/overlap.rs"]);
        assert!(patch.contains("diff --git a/crates/overlap.rs b/crates/overlap.rs"), "{patch}");
        assert!(patch.contains("+added"), "{patch}");
        let last = github.transport.last.borrow().clone().unwrap();
        assert_eq!(last.method, Method::Get);
        assert_eq!(
            last.url, "https://api.github.com/repos/octo/shadow/compare/bloom/x/integration...cand",
            "the follow-up is the compare of the same two refs the merge named",
        );
    }

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
    fn delete_ref_deletes_the_qualified_ref_route() {
        let github = client(204, "");
        github.delete_ref("bloomery/claims/wp-1").expect("204 is success");
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.method, Method::Delete);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/git/refs/bloomery/claims/wp-1");
    }

    #[test]
    fn delete_ref_treats_already_gone_as_ok() {
        // Tripwire: a 404/422 (the ref is already gone) is the clean idempotent
        // outcome release's name-only cleanup delete and an acquire's rollback
        // depend on — never a `Status` error that would fail an interrupted
        // release's re-delete.
        let gone_404 = client(404, r#"{"message":"Not Found"}"#);
        gone_404.delete_ref("bloomery/claims/absent").expect("404 is Ok");
        let gone_422 = client(422, r#"{"message":"Reference does not exist"}"#);
        gone_422.delete_ref("bloomery/claims/absent").expect("422 is Ok");
    }

    #[test]
    fn create_commit_posts_tree_and_parents() {
        let github = client(201, r#"{"sha":"newcommit","tree":{"sha":"treesha"},"message":"checkpoint"}"#);
        let commit = github.create_commit("checkpoint", "treesha", &["parentsha".to_owned()]).expect("2xx decodes");
        assert_eq!(commit.sha, "newcommit");
        assert_eq!(commit.tree, "treesha");
        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/git/commits");
        let sent: serde_json::Value = serde_json::from_str(&request.body.unwrap()).unwrap();
        assert_eq!(sent["tree"], "treesha");
        assert_eq!(sent["parents"][0], "parentsha");
    }

    #[test]
    fn get_commit_decodes_the_message() {
        // Tripwire: the claim registry resolves a claim's holder from the
        // commit message, not the tree — a message decode regression here would
        // silently break every claim-holder read.
        let github = client(
            200,
            r#"{"sha":"commitsha","tree":{"sha":"treesha"},"message":"bloomery claim\n\nBloom-Id: sha256-ab"}"#,
        );
        let commit = github.get_commit("commitsha").expect("2xx decodes");
        assert_eq!(commit.message, "bloomery claim\n\nBloom-Id: sha256-ab");
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

    // #3662 — the nonce match is delimiter-bounded, never a raw `contains`.
    // Nonces are `dispatch-<int>`-shaped, so `dispatch-4` is a prefix of
    // `dispatch-42`; the newer superstring run sits first in the newest-first
    // list and a raw `contains` would resolve it for the older nonce —
    // mis-reporting status, cancelling the wrong run, and returning silently
    // empty evidence once the artifact filter drops everything.
    #[test]
    fn find_run_never_resolves_a_superstring_nonce() {
        let body = r#"{"workflow_runs":[
            {"id":42,"display_title":"transform verify.check dispatch-42","status":"in_progress","conclusion":null},
            {"id":4,"display_title":"transform verify.check dispatch-4","status":"completed","conclusion":"success"}
        ]}"#;
        let github = client(200, body);
        let run = github.find_run("bloomery-transform.yml", "dispatch-4").expect("2xx decodes").expect("a match");
        assert_eq!(run.id, 4, "the bounded match skips the superstring title");
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

    // Tripwire: the production GitHub transport must bound a stalled request.
    // Without a timeout this test hangs forever, which is the #3640 wedge —
    // an unbounded blocking call on the chassis dispatcher never returns, so
    // the actor's `DispatcherSlot` never goes back to `Idle`.
    #[test]
    fn stalled_connection_returns_a_bounded_transport_error() {
        use std::net::TcpListener;
        use std::thread;
        use std::time::{Duration, Instant};

        use super::ReqwestTransport;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        // A raw thread is fine here: it's a test-only stall server, not
        // engine actor work, so the settlement/trace umbrella doesn't apply.
        #[allow(clippy::disallowed_methods)]
        thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                thread::sleep(Duration::from_mins(1));
                drop(stream);
            }
        });

        let transport = ReqwestTransport::with_timeouts(Duration::from_millis(250), Duration::from_millis(250))
            .expect("client builds");
        let request = HttpRequest { method: Method::Get, url: format!("http://{addr}/"), headers: vec![], body: None };

        let started = Instant::now();
        let result = transport.execute(request);
        assert!(matches!(result, Err(GithubError::Transport(_))), "expected a transport error, got {result:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "transport did not bound the stalled request");
    }

    use super::{NewPullRequest, PullMergeResult, PullRequestApi, PullRequestState};

    #[test]
    fn create_pull_request_posts_the_repo_pulls_route_with_head_and_base() {
        let github = client(
            201,
            r#"{"number":7,"state":"open","merged":false,"merge_commit_sha":null,
                "head":{"sha":"deadbeef","ref":"bloom/abcd/landing"},"base":{"ref":"main"}}"#,
        );
        let pull = github
            .create_pull_request(&NewPullRequest {
                title: "bloomery: land".into(),
                body: "b".into(),
                head: "bloomery/land/abcd".into(),
                base: "main".into(),
            })
            .expect("2xx create decodes");

        assert_eq!(pull.number, 7);
        assert_eq!(pull.head_sha, "deadbeef");
        assert_eq!(pull.base, "main");
        assert_eq!(pull.state, PullRequestState::Open);

        let request = github.transport.last.borrow().clone().expect("a request was sent");
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/pulls");
        let sent: serde_json::Value = serde_json::from_str(&request.body.unwrap()).unwrap();
        assert_eq!(sent["head"], "bloomery/land/abcd");
        assert_eq!(sent["base"], "main");
    }

    // Tripwire: GitHub populates `merge_commit_sha` on an *open* pull request
    // with a throwaway test-merge commit that is on no branch. Admitting that as
    // a landing would record a mainline head that exists nowhere and seal the
    // next bloom on it, so the decode blanks the field until the pull request
    // actually merged.
    #[test]
    fn merge_commit_sha_is_blank_until_the_pull_request_actually_merged() {
        let open = client(
            200,
            r#"{"number":7,"state":"open","merged":false,"merge_commit_sha":"7e57e57e57",
                "head":{"sha":"deadbeef","ref":"bloom/abcd/landing"},"base":{"ref":"main"}}"#,
        );
        let pull = open.get_pull_request(7).expect("2xx decodes").expect("present");
        assert_eq!(pull.merge_commit_sha, None, "an open pull request's test-merge sha is not a landing");
        assert!(!pull.merged);

        let merged = client(
            200,
            r#"{"number":7,"state":"closed","merged":true,"merge_commit_sha":"5quash3d",
                "head":{"sha":"deadbeef","ref":"bloom/abcd/landing"},"base":{"ref":"main"}}"#,
        );
        let pull = merged.get_pull_request(7).expect("2xx decodes").expect("present");
        assert_eq!(pull.state, PullRequestState::Closed);
        assert!(pull.merged);
        // The squash commit, not the proposed head — what mainline became.
        assert_eq!(pull.merge_commit_sha.as_deref(), Some("5quash3d"));

        // A closed-unmerged pull request is the operator's rejection: closed,
        // not merged, and carrying no landing sha whatever the field held.
        let rejected = client(
            200,
            r#"{"number":7,"state":"closed","merged":false,"merge_commit_sha":"7e57e57e57",
                "head":{"sha":"deadbeef","ref":"bloom/abcd/landing"},"base":{"ref":"main"}}"#,
        );
        let pull = rejected.get_pull_request(7).expect("2xx decodes").expect("present");
        assert_eq!(pull.state, PullRequestState::Closed);
        assert!(!pull.merged);
        assert_eq!(pull.merge_commit_sha, None);
    }

    // Tripwire: the merge request carries `sha`. That parameter is the whole
    // compare-and-swap the coordinator's own acceptance rests on — GitHub
    // refuses a merge whose head has moved off it — so a request shaped without
    // it would merge whatever the branch had become between the caller's checks
    // and its write, and every test above the transport would still pass.
    #[test]
    fn squash_merging_puts_the_merge_route_guarded_by_the_head_sha() {
        let github = client(200, r#"{"sha":"5quash3d","merged":true,"message":"Pull Request successfully merged"}"#);

        assert_eq!(
            github.squash_merge_pull_request(7, "deadbeef").expect("2xx decodes"),
            PullMergeResult::Merged { merge_commit_sha: "5quash3d".to_owned() },
        );

        let request = github.transport.last.borrow().clone().expect("a request was sent");
        assert_eq!(request.method, Method::Put);
        assert_eq!(request.url, "https://api.github.com/repos/octo/shadow/pulls/7/merge");
        let sent: serde_json::Value = serde_json::from_str(&request.body.unwrap()).unwrap();
        assert_eq!(sent["sha"], "deadbeef", "the merge is guarded by the head the caller proved");
        assert_eq!(sent["merge_method"], "squash", "mainline takes squashes; the proposal's title is the subject");
        assert!(sent.get("commit_title").is_none(), "the subject is GitHub's from the proposal, as by hand");
    }

    // The two statuses that mean "no" are outcomes, not faults: a caller that
    // re-drove them would hammer a decision that cannot change. Everything else
    // stays an error, because everything else might.
    #[test]
    fn a_refused_merge_is_an_outcome_and_any_other_status_is_still_an_error() {
        for status in [405u16, 409] {
            match client(status, r#"{"message":"Pull Request is not mergeable"}"#).squash_merge_pull_request(7, "d") {
                Ok(PullMergeResult::Refused { status: got, detail }) => {
                    assert_eq!(got, status);
                    assert!(detail.contains("not mergeable"), "the refusal keeps its body: {detail}");
                }
                other => panic!("expected a refusal outcome for {status}, got {other:?}"),
            }
        }

        match client(500, r#"{"message":"boom"}"#).squash_merge_pull_request(7, "d") {
            Err(GithubError::Status { status: 500, .. }) => {}
            other => panic!("expected a 500 to stay an error, got {other:?}"),
        }
    }

    #[test]
    fn get_pull_request_maps_404_to_none() {
        let github = client(404, r#"{"message":"Not Found"}"#);
        assert_eq!(github.get_pull_request(7).expect("404 is Ok(None)"), None);
    }

    // Tripwire: the list endpoint only filters when `head` is qualified as
    // `owner:branch`. An unqualified value silently matches nothing, so the land
    // path would believe it had never proposed this bloom and open a fresh pull
    // request on every poll tick.
    #[test]
    fn find_pull_request_for_head_qualifies_the_branch_with_the_repo_owner() {
        let github = client(
            200,
            r#"[{"number":7,"state":"open","merged":false,"merge_commit_sha":null,
                 "head":{"sha":"deadbeef","ref":"bloom/abcd/landing"},"base":{"ref":"main"}}]"#,
        );
        let found = github.find_pull_request_for_head("bloomery/land/abcd").expect("2xx decodes").expect("present");
        assert_eq!(found.number, 7);

        let request = github.transport.last.borrow().clone().unwrap();
        assert_eq!(request.method, Method::Get);
        assert!(
            request.url.contains("head=octo:bloomery/land/abcd"),
            "the head filter must be owner-qualified, got {}",
            request.url,
        );
        // Tripwire: any state, newest first. Filtering to open would hide a
        // merged proposal from the poll that has to observe it, and the land
        // path would open a second one instead of admitting the landing.
        assert!(request.url.contains("state=all"), "a merged proposal must stay findable, got {}", request.url);
        assert!(request.url.contains("direction=desc"), "newest first, got {}", request.url);
    }

    #[test]
    fn find_pull_request_for_head_reports_none_when_the_branch_has_never_been_proposed() {
        let github = client(200, "[]");
        assert_eq!(github.find_pull_request_for_head("bloomery/land/abcd").expect("2xx decodes"), None);
    }
}
