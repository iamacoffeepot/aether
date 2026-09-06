//! The host-side source-replica shell (ADR-0199).
//!
//! Wraps a [`SourceReplica`] so the mirror reactor can push allowlisted refs
//! without naming the git crate at every call site. Credentials stay on this
//! process: each push resolves a bearer from a
//! [`TokenSource`] and the git backend
//! applies it as an HTTP header.

use std::path::Path;
use std::sync::Arc;

use aether_bloomery_git::replica::ReplicaTokenSource;
use aether_bloomery_github::{GitSourceReplica, MainlineRef, ReplicaError, SourceReplica, TokenSource};

/// The source-replica cap shell: a git push backend behind an `Arc<dyn …>`.
#[derive(Clone)]
pub struct SourceReplicaShell {
    backend: Arc<dyn SourceReplica>,
}

impl SourceReplicaShell {
    /// Mount an arbitrary replica backend — tests mount a recorder.
    #[must_use]
    pub fn new(backend: Arc<dyn SourceReplica>) -> Self {
        Self { backend }
    }

    /// Push from `authority` to the configured GitHub URL with a static PAT.
    #[must_use]
    pub fn connect(authority: &str, remote: &str, mainline: MainlineRef, token: &str) -> Self {
        Self::new(Arc::new(GitSourceReplica::new(authority, remote, mainline, token)))
    }

    /// Push from `authority` to the configured GitHub URL, resolving a bearer
    /// from `token` on every publish so App installation-token rotation stays
    /// valid.
    #[must_use]
    pub fn connect_with_token_source(
        authority: &str,
        remote: &str,
        mainline: MainlineRef,
        token: Arc<dyn TokenSource>,
    ) -> Self {
        Self::new(Arc::new(GitSourceReplica::with_token_source(
            authority,
            remote,
            mainline,
            Arc::new(GithubReplicaToken(token)),
        )))
    }

    /// Push the current allowlisted refs.
    ///
    /// # Errors
    /// Transient transport failure, a rejected mainline force-push, or a
    /// deterministic refusal.
    pub fn publish(&self) -> Result<(), ReplicaError> {
        self.backend.publish()
    }
}

/// Bridges the GitHub REST [`TokenSource`] into the git replica without the
/// git crate depending on the GitHub adapter. Minting failures and an empty
/// bearer fail closed; the mapped error never includes the token.
struct GithubReplicaToken(Arc<dyn TokenSource>);

impl ReplicaTokenSource for GithubReplicaToken {
    fn token(&self) -> Result<String, ReplicaError> {
        match self.0.token() {
            Ok(token) if token.is_empty() => {
                Err(ReplicaError::Deterministic("source replica token source returned no credential".into()))
            }
            Ok(token) => Ok(token),
            Err(_) => Err(ReplicaError::Deterministic("source replica could not mint a GitHub token".into())),
        }
    }
}

/// The git remote URL a GitHub connection pushes source refs to.
///
/// Derived from the REST `api_base` so github.com and a GHE host share one
/// spelling. The token is never interpolated here.
#[must_use]
pub fn github_push_url(api_base: &str, owner: &str, repo: &str) -> String {
    let host = git_host(api_base);
    format!("{host}/{owner}/{repo}.git")
}

fn git_host(api_base: &str) -> &str {
    let trimmed = api_base.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_suffix("/api/v3") {
        return rest;
    }
    if trimmed == "https://api.github.com" || trimmed.ends_with("://api.github.com") {
        return "https://github.com";
    }
    trimmed
}

/// Whether `path` is a present single-writer marker file.
#[must_use]
pub fn writer_marker_present(path: &str) -> bool {
    let path = path.trim();
    !path.is_empty() && Path::new(path).is_file()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use aether_bloomery_git::replica::ReplicaTokenSource;
    use aether_bloomery_github::{GithubError, StaticTokenSource, TokenSource};

    use super::{GithubReplicaToken, github_push_url, writer_marker_present};

    #[test]
    fn github_dot_com_api_base_projects_to_the_git_host() {
        assert_eq!(github_push_url("https://api.github.com", "octo", "shadow"), "https://github.com/octo/shadow.git");
        assert_eq!(
            github_push_url("https://ghe.example/api/v3", "octo", "shadow"),
            "https://ghe.example/octo/shadow.git"
        );
    }

    #[test]
    fn an_empty_or_missing_path_is_not_a_writer_marker() {
        assert!(!writer_marker_present(""));
        assert!(!writer_marker_present("/no/such/bloomery-writer"));
    }

    #[test]
    fn host_token_source_rotation_reaches_the_replica_bridge() {
        // Tripwire: App-only used to freeze `github.token` (empty) onto the
        // replica. The host bridge must hand the live TokenSource through so
        // each publish sees the current bearer, not the first or empty PAT (#5586).
        const FIRST: &str = "token-one";
        const SECOND: &str = "token-two";
        let source = Arc::new(MutableToken { value: Mutex::new(FIRST.to_owned()) });
        let bridge = GithubReplicaToken(source.clone());

        assert!(matches!(bridge.token(), Ok(token) if token == FIRST), "first resolve must be the live TokenSource");
        *source.value.lock().expect("token") = SECOND.to_owned();
        assert!(matches!(bridge.token(), Ok(token) if token == SECOND), "a rotated TokenSource must be re-read");
    }

    #[test]
    fn an_empty_or_failed_host_token_source_fails_closed() {
        const SECRET: &str = "gho_should-not-leak";
        let empty = GithubReplicaToken(Arc::new(StaticTokenSource::new(String::new())));
        let empty_error = empty.token().expect_err("empty PAT must not succeed as a replica credential");
        assert!(empty_error.to_string().contains("no credential"), "{empty_error}");

        let boom = GithubReplicaToken(Arc::new(BoomToken { secret: SECRET }));
        let boom_error = boom.token().expect_err("minting failure fails closed");
        let text = boom_error.to_string();
        assert!(text.contains("could not mint"), "{text}");
        assert!(!text.contains(SECRET), "mapped replica errors must not carry the GitHub error body");
    }

    struct MutableToken {
        value: Mutex<String>,
    }

    impl TokenSource for MutableToken {
        fn token(&self) -> Result<String, GithubError> {
            Ok(self.value.lock().expect("token").clone())
        }
    }

    struct BoomToken {
        secret: &'static str,
    }

    impl TokenSource for BoomToken {
        fn token(&self) -> Result<String, GithubError> {
            Err(GithubError::Transport(self.secret.to_owned()))
        }
    }
}
