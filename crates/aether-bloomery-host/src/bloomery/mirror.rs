//! The outward-mirror cap shell (#3459 step 7).
//!
//! The host mounts the `aether-bloomery-github` projection backend behind a
//! thin shell holding it as an `Arc<dyn ProjectionBackend>`. No GitHub type
//! crosses into a core module: the shell is the boundary, and only it and the
//! adapter name a github-crate type (ADR-0149 §The boundary, the "no core
//! module names a GitHub type" clause).
//!
//! The connection knobs ride the ADR-0090 derive-`Config` path
//! ([`GithubMirrorConfig`]) — token, owner/name, API base, and the CAS-land
//! enable flag (carried for the git source-port sibling slice, unused here) —
//! resolved argv > env > default like every other chassis knob, never a naked
//! env read.
//!
//! This slice ships the shell and the demo that drives a synthetic bloom
//! through it (see `tests/mirror_demo.rs`). Wiring the shell into the chassis
//! boot as an outbox-driven capability lands with the migration step 2
//! executor/review bridge, when the outbox republish that feeds it exists.

use std::sync::Arc;

use aether_bloomery::{LandingReceipt, ProjectionBackend, ViewDocument};
use aether_bloomery_github::{GithubConfig, GithubError, GithubProjection, ReqwestGithub};

/// The GitHub outward-mirror connection knobs (ADR-0090 derive-`Config`).
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_GITHUB", cli_prefix = "github")]
pub struct GithubMirrorConfig {
    /// The bearer token the mirror authenticates with. Pinned to the
    /// conventional unprefixed `GITHUB_TOKEN`; empty means unconfigured.
    #[config(env = "GITHUB_TOKEN", default = "")]
    pub token: String,
    /// The repository owner (user or org) the projections live under.
    #[config(default = "")]
    pub owner: String,
    /// The repository name.
    #[config(default = "")]
    pub repo: String,
    /// The REST API base — `https://api.github.com`, or a GitHub Enterprise
    /// base. No trailing slash.
    #[config(default = "https://api.github.com")]
    pub api_base: String,
    /// Whether compare-and-swap mainline landing is permitted. Off by default
    /// (ADR-0149 gates it to migration step 3); consumed by the git source
    /// port, a separate slice, so carried-but-unused here.
    #[config(default = false)]
    pub cas_land_enabled: bool,
}

impl Default for GithubMirrorConfig {
    fn default() -> Self {
        Self {
            token: String::new(),
            owner: String::new(),
            repo: String::new(),
            api_base: "https://api.github.com".to_owned(),
            cas_land_enabled: false,
        }
    }
}

impl GithubMirrorConfig {
    /// Project the host-resolved knobs into the adapter's plain config.
    #[must_use]
    pub fn to_github_config(&self) -> GithubConfig {
        GithubConfig {
            token: self.token.clone(),
            owner: self.owner.clone(),
            repo: self.repo.clone(),
            api_base: self.api_base.clone(),
            cas_land_enabled: self.cas_land_enabled,
        }
    }
}

/// The projection cap shell: the outward mirror behind an `Arc<dyn …>`, so no
/// core module ever names the concrete github-crate type.
#[derive(Clone)]
pub struct ProjectionShell {
    backend: Arc<dyn ProjectionBackend<Error = GithubError> + Send + Sync>,
}

impl ProjectionShell {
    /// Mount an arbitrary projection backend — the demo mounts a fake-backed
    /// one, production a `ReqwestGithub`-backed one.
    #[must_use]
    pub fn new(backend: Arc<dyn ProjectionBackend<Error = GithubError> + Send + Sync>) -> Self {
        Self { backend }
    }

    /// Connect a live GitHub-backed mirror from resolved config.
    ///
    /// # Errors
    /// The underlying `reqwest` client could not be constructed.
    pub fn connect(config: &GithubMirrorConfig) -> Result<Self, GithubError> {
        let client = ReqwestGithub::new(&config.to_github_config())?;
        Ok(Self::new(Arc::new(GithubProjection::new(client))))
    }

    /// Reconcile the outward mirror to `view`.
    ///
    /// # Errors
    /// The projection surface is unreachable or returned an error status.
    pub fn reconcile_view(&self, view: &ViewDocument) -> Result<(), GithubError> {
        self.backend.reconcile_view(view)
    }

    /// Project a landing receipt outward.
    ///
    /// # Errors
    /// The projection surface is unreachable or returned an error status.
    pub fn project_receipt(&self, receipt: &LandingReceipt) -> Result<(), GithubError> {
        self.backend.project_receipt(receipt)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::GithubMirrorConfig;

    #[test]
    fn config_projects_into_the_adapter_config() {
        // The knobs round-trip into the adapter's plain config unchanged — the
        // one bit of logic the shell owns.
        let config = GithubMirrorConfig {
            token: "t".into(),
            owner: "octo".into(),
            repo: "shadow".into(),
            api_base: "https://ghe.example/api/v3".into(),
            cas_land_enabled: true,
        };
        let projected = config.to_github_config();
        assert_eq!(projected.repo_path(), "octo/shadow");
        assert_eq!(projected.api_base, "https://ghe.example/api/v3");
        assert!(projected.cas_land_enabled);
    }
}
