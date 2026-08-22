//! The outward GitHub projection shell (ADR-0149).

use std::sync::Arc;

use aether_bloomery::{CommissionProjection, ProjectedReceipt, ProjectionBackend, ViewDocument};
use aether_bloomery_github::{GithubError, GithubProjection};

use super::GithubConnectionConfig;

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
    pub fn connect(config: &GithubConnectionConfig) -> Result<Self, GithubError> {
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

    /// Project a landing receipt outward, onto the objects its membership
    /// reaches.
    ///
    /// # Errors
    /// The projection surface is unreachable or returned an error status.
    pub fn project_receipt(&self, receipt: &ProjectedReceipt) -> Result<(), GithubError> {
        self.backend.project_receipt(receipt)
    }

    /// Project one commission. `Some(number)` is an issue this projector owns
    /// and may retitle; `None` is a commission whose workpiece already names
    /// an object it must not own.
    ///
    /// # Errors
    /// The projection surface is unreachable or returned an error status.
    pub fn project_commission(&self, projection: &CommissionProjection) -> Result<Option<u64>, GithubError> {
        self.backend.project_commission(projection)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::GithubConnectionConfig;
    use crate::bloomery::CoordinatorConfig;

    #[test]
    fn config_projects_into_the_adapter_config() {
        // The knobs round-trip into the adapter's plain config unchanged — the
        // one bit of logic the shell owns.
        let config = GithubConnectionConfig {
            token: "t".into(),
            owner: "octo".into(),
            repo: "shadow".into(),
            api_base: "https://ghe.example/api/v3".into(),
            cas_land_enabled: true,
            ..GithubConnectionConfig::default()
        };
        let projected = config.to_github_config();
        assert_eq!(projected.repo_path(), "octo/shadow");
        assert_eq!(projected.api_base, "https://ghe.example/api/v3");
        assert!(projected.cas_land_enabled);
    }

    #[test]
    fn local_lane_prefixes_parses_the_comma_list() {
        // The one bit of logic the config owns for the router: split, trim, drop
        // empties — a whitespace-padded or trailing-comma value still parses clean.
        let config = CoordinatorConfig { local_lane_commands: " construct. , verify. ,".into(), ..Default::default() };
        assert_eq!(config.local_lane_prefixes(), vec!["construct.".to_owned(), "verify.".to_owned()]);

        // The default routes the model-driven lanes local — construct/refine,
        // the review critic, and the scoper — each forks an agent CLI under an
        // ambient credential the zero-secret runner deliberately lacks.
        assert_eq!(
            CoordinatorConfig::default().local_lane_prefixes(),
            vec!["construct.".to_owned(), "review.".to_owned(), "scope.".to_owned()]
        );
    }
}
