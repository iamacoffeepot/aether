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

use crate::app_auth::AppTokenSource;

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
    /// How often the outbox-consumer mirror driver ([`super::MirrorDriverCapability`])
    /// polls the store outbox for undelivered projection entries, in seconds.
    /// The connection knobs and the poll cadence share this config because one
    /// GitHub-mirror configuration governs the whole outbound path; the source
    /// shell ignores it (`to_github_config` drops it).
    #[config(default = 5)]
    pub poll_interval_secs: u64,
    /// The wrapper workflow file the executor port dispatches (ADR-0149
    /// §Execution on Actions, [#3500]). Carried on this shared GitHub-connection
    /// config the same way `cas_land_enabled` is — one config serves the mirror,
    /// source, and executor caps rather than duplicating the connection knobs.
    ///
    /// [#3500]: https://github.com/iamacoffeepot/aether/issues/3500
    #[config(default = "bloomery-transform.yml")]
    pub executor_workflow_file: String,
    /// The protected git ref the executor pins the wrapper dispatch at.
    #[config(default = "refs/heads/main")]
    pub executor_dispatch_ref: String,
    /// The `SQLite` store path the executor dispatch driver ([`super::ExecutorDriverCapability`])
    /// opens its own connection to, to drive the intake registry directly (#3505).
    /// Reads the **same** `AETHER_STORE_PATH` env the [`StoreConfig`](crate::store::StoreConfig)
    /// resolves, so the driver's connection targets the store the `StoreCapability`
    /// owns; carried on this shared config the same way the executor knobs are (one
    /// config serves the mirror, source, and executor caps).
    #[config(env = "AETHER_STORE_PATH", default = ":memory:")]
    pub store_path: String,
    /// The GitHub App id whose minted installation token supersedes the static
    /// `token` when App-auth is configured (ADR-0149 §Migration step 3). `0`
    /// (the default) means App-auth is off and the static `token` authenticates.
    /// Carried on this shared config so the mirror, source, and executor caps
    /// all authenticate the same way.
    #[config(default = 0)]
    pub app_id: u64,
    /// Absolute path to the GitHub App's private-key PEM, held host-local
    /// (ADR-0150 — the key bytes never leave the machine, never cross into wasm
    /// or a config echo; the custody reads the file, mints a JWT, and discards
    /// the key). Empty (the default) means App-auth is off.
    #[config(default = "")]
    pub app_private_key_path: String,
    /// The installation id the App mints access tokens for. `0` (the default)
    /// means App-auth is off.
    #[config(default = 0)]
    pub app_installation_id: u64,
    /// Seconds before an installation token's expiry to re-mint it (the refresh
    /// skew). `0` resolves to the 300-second default.
    #[config(default = 300)]
    pub app_token_skew_secs: u64,
}

impl Default for GithubMirrorConfig {
    fn default() -> Self {
        Self {
            token: String::new(),
            owner: String::new(),
            repo: String::new(),
            api_base: "https://api.github.com".to_owned(),
            cas_land_enabled: false,
            poll_interval_secs: 5,
            executor_workflow_file: "bloomery-transform.yml".to_owned(),
            executor_dispatch_ref: "refs/heads/main".to_owned(),
            store_path: ":memory:".to_owned(),
            app_id: 0,
            app_private_key_path: String::new(),
            app_installation_id: 0,
            app_token_skew_secs: 300,
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

    /// Whether GitHub-App auth is configured — all three App knobs present. When
    /// off, the static `token` authenticates; when on, a minted installation
    /// token supersedes it (ADR-0149 §Migration step 3).
    #[must_use]
    pub fn app_auth_configured(&self) -> bool {
        self.app_id != 0 && !self.app_private_key_path.is_empty() && self.app_installation_id != 0
    }

    /// Build the adapter client the port shells drive: an App-minted-token
    /// client when [`app_auth_configured`](Self::app_auth_configured), else the
    /// backward-compatible static-PAT client. The App path reads the host-local
    /// private key and hands the client a cached-and-refreshing
    /// [`AppTokenSource`] (ADR-0150 custody); the key file's absence fails fast
    /// here rather than silently falling back to the static token. Each shell
    /// that calls this holds its own token cache — a handful of independent
    /// mint cadences at boot, never a per-request mail hop (the design's
    /// in-process `TokenSource` handle).
    ///
    /// # Errors
    /// The `reqwest` client could not be constructed, or (App path) the private
    /// key file is unreadable or not a valid RSA PEM.
    pub fn connect_client(&self) -> Result<ReqwestGithub, GithubError> {
        if self.app_auth_configured() {
            let source = Arc::new(AppTokenSource::from_config(self)?);
            ReqwestGithub::with_token_source(source, self.api_base.clone(), self.to_github_config().repo_path())
        } else {
            ReqwestGithub::new(&self.to_github_config())
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
        let client = config.connect_client()?;
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
            ..GithubMirrorConfig::default()
        };
        let projected = config.to_github_config();
        assert_eq!(projected.repo_path(), "octo/shadow");
        assert_eq!(projected.api_base, "https://ghe.example/api/v3");
        assert!(projected.cas_land_enabled);
    }
}
