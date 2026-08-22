//! The host-side source-replica shell (ADR-0199).
//!
//! Wraps a [`SourceReplica`] so the mirror reactor can push allowlisted refs
//! without naming the git crate at every call site. Credentials stay on this
//! process: the token is held here and applied as an HTTP header by the
//! backend.

use std::path::Path;
use std::sync::Arc;

use aether_bloomery_github::{GitSourceReplica, MainlineRef, ReplicaError, SourceReplica};

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

    /// Push from `authority` to the configured GitHub URL.
    #[must_use]
    pub fn connect(authority: &str, remote: &str, mainline: MainlineRef, token: &str) -> Self {
        Self::new(Arc::new(GitSourceReplica::new(authority, remote, mainline, token)))
    }

    /// Push the current allowlisted refs.
    ///
    /// # Errors
    /// Transient transport failure, a rejected mainline force-push, or a
    /// deterministic refusal.
    pub fn publish(&self) -> Result<(), ReplicaError> {
        self.backend.publish()
    }

    /// The mainline head this replica has not published yet, or `None` when
    /// nothing is owed (#5260) — the timer question that catches a ref advance
    /// no coordinator event announced.
    #[must_use]
    pub fn unpublished_head(&self) -> Option<String> {
        self.backend.unpublished_head()
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
    use super::{github_push_url, writer_marker_present};

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
}
