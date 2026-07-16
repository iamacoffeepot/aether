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
//! logic is tested against the higher-level [`crate::testing::FakeGithub`],
//! which models the object store rather than the HTTP transport.
//!
//! [#3460]: https://github.com/iamacoffeepot/aether/issues/3460

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

/// The GitHub client contract the projection depends on. Both the real
/// [`ReqwestGithub`] and the test [`crate::testing::FakeGithub`] implement it,
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
}
