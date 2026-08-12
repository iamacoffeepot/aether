//! Bloomery coordinator and GitHub adapter configuration boundaries.

use std::sync::Arc;
#[cfg(any(test, feature = "testing"))]
use std::sync::OnceLock;

#[cfg(any(test, feature = "testing"))]
use aether_bloomery::Digest;
#[cfg(any(test, feature = "testing"))]
use aether_bloomery::SharedCorrespondence;
#[cfg(any(test, feature = "testing"))]
use aether_bloomery_github::{GitSource, testing::FakeGithub};
use aether_bloomery_github::{GithubConfig, GithubError, ReqwestGithub};

use super::executor::DEFAULT_LANE_PROGRAM;
#[cfg(any(test, feature = "testing"))]
use super::source::SourceShell;
use crate::app_auth::AppTokenSource;

#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_GITHUB", cli_prefix = "github")]
pub struct GithubConnectionConfig {
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
    /// Whether compare-and-swap mainline landing is permitted. On by default
    /// (ADR-0149 migration step 3 moved landing authority to the source port —
    /// the CAS `land` is the landing of record); consumed by the git source
    /// port to gate [`GitSource::land`](aether_bloomery_github::GitSource) and by
    /// the land reactor ([`super::LandReactorCapability`]) that drives it.
    #[config(default = true)]
    pub cas_land_enabled: bool,
    /// The wrapper workflow file the executor port dispatches (ADR-0149
    /// §Execution on Actions, [#3500]). Carried on this shared GitHub-connection
    /// config the same way `cas_land_enabled` is — one config serves the mirror,
    /// source, and executor caps rather than duplicating the connection knobs.
    /// The default must name the wrapper workflow that actually exists at
    /// `.github/workflows/transform.yml` ([#3501]); the two must not drift.
    ///
    /// [#3500]: https://github.com/iamacoffeepot/aether/issues/3500
    /// [#3501]: https://github.com/iamacoffeepot/aether/issues/3501
    #[config(default = "transform.yml")]
    pub executor_workflow_file: String,
    /// The wrapper workflow the executor port dispatches a **model** lane at —
    /// the credential-bearing sibling of
    /// [`executor_workflow_file`](Self::executor_workflow_file) (ADR-0149
    /// §Execution on Actions). Two knobs rather than one because the two
    /// wrappers are not interchangeable: `transform.yml` runs zero-secret on an
    /// untrusted lane, `transform-model.yml` holds a Claude credential, and a
    /// lane that needs a model needs the latter. Which of the two an order fires
    /// is never this config's call — the executor reads it off the sealed
    /// command (`is_model_lane`), so pointing either knob at a different file
    /// moves where a lane runs and never which lane may hold a secret. The
    /// default must name the wrapper that actually exists at
    /// `.github/workflows/transform-model.yml`.
    #[config(default = "transform-model.yml")]
    pub executor_model_workflow_file: String,
    /// The protected git ref the executor pins the wrapper dispatch at.
    #[config(default = "refs/heads/main")]
    pub executor_dispatch_ref: String,
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
    /// Which GitHub implementation to mount — `github` (the default, the real
    /// network) or `fixture` (the in-memory double the lane-boundary harness
    /// drives, #4732). Compiled only under `cfg(any(test, feature =
    /// "testing"))`, so a production `config_manifest()` never advertises
    /// `AETHER_GITHUB_BACKEND` and a production binary never links
    /// `FakeGithub`.
    #[cfg(any(test, feature = "testing"))]
    #[config(env = "AETHER_GITHUB_BACKEND", default = "github")]
    pub github_backend: String,
    /// The commit the fixture's base digest names, as a git sha.
    ///
    /// The harness seals against a repository the coordinator can actually
    /// check out, and records that correspondence in the store it hands over;
    /// the fixture has to agree, or the first dispatch resolves its checkout to
    /// an object no `git worktree add` can find. Set it and the fixture also
    /// mints its own commits in that repository — see
    /// [`shared_fixture`](Self::shared_fixture).
    ///
    /// Cfg-gated beside [`github_backend`](Self::github_backend), for the same
    /// reason.
    #[cfg(any(test, feature = "testing"))]
    #[config(env = "AETHER_GITHUB_FIXTURE_BASE_SHA", default = "")]
    pub fixture_base_sha: String,
}

#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_GITHUB", cli_prefix = "github")]
pub struct CoordinatorConfig {
    /// How often the mirror reactor ([`super::MirrorReactorCapability`])
    /// polls the store outbox for undelivered projection entries, in seconds.
    /// The land, integrate, and executor reactors use the same backend-neutral
    /// coordinator cadence.
    #[config(default = 5)]
    pub poll_interval_secs: u64,
    /// The `SQLite` store path the executor dispatch reactor ([`super::ExecutorReactorCapability`])
    /// opens its own connection to, to drive the intake registry directly (#3505).
    /// Reads the **same** `AETHER_STORE_PATH` env the [`StoreConfig`](crate::store::StoreConfig)
    /// resolves, so the reactor's connection targets the store the `StoreCapability`
    /// owns; carried on this shared config the same way the executor knobs are (one
    /// config serves the mirror, source, and executor caps).
    #[config(env = "AETHER_STORE_PATH", default = ":memory:")]
    pub store_path: String,
    /// The in-repo tier-policy artifact the pre-seal approve gate
    /// ([`super::Gate`] / [`super::ApprovalPolicy`], ADR-0149 §The line /
    /// ADR-0151) resolves a workpiece's declared surface against — the
    /// Bloomery-owned `approval-policy.yml` at the repository root. A
    /// repository-relative path read host-side, like the connection config's
    /// `executor_workflow_file`, names an in-repo artifact. Tier policy (*what*
    /// tier) is a **distinct reader** from the signing capability's key policy
    /// (*who* may sign) — the two are never folded (ADR-0151).
    #[config(env = "AETHER_APPROVAL_POLICY_FILE", default = "approval-policy.yml")]
    pub approval_policy_file: String,
    /// Whether the executor mounts the local-process backend for the model lane
    /// (ADR-0150, #3586). On by default: the `construct.*` lanes route to a local
    /// process under ambient `claude` auth rather than a shared-runner wrapper,
    /// so no Claude credential is ever staged into a GitHub secret. Off falls back
    /// to the bare Actions backend (every lane on shared runners).
    #[config(default = true)]
    pub local_lane_enabled: bool,
    /// The comma-separated command-id prefixes the executor routes to the local
    /// backend (the rest go to Actions). The default routes both model-driven
    /// lanes local — `construct.` (construct/refine) and `review.` (the critic,
    /// which needs the model API the zero-secret runner deliberately lacks);
    /// adding e.g. `verify.` is the release valve that flips a heavy mechanical
    /// lane local (Actions outage, quota, offline work). Parsed by
    /// [`local_lane_prefixes`](Self::local_lane_prefixes).
    #[config(default = "construct.,review.")]
    pub local_lane_commands: String,
    /// The scratch-worktree base dir the local backend checks each order's subject
    /// into (keyed by nonce). Should be absolute in production so the checkout
    /// resolves regardless of the coordinator's cwd.
    #[config(default = ".bloomery/local-worktrees")]
    pub local_worktree_base: String,
    /// The program a local lane dispatch spawns in the scratch worktree, as a
    /// whole invocation — the program and the arguments that precede the
    /// transform's own argv (#4727). The default is the portable entrypoint the
    /// wrapper workflows run.
    ///
    /// Resolvable rather than hardcoded so a test can drive the *whole* dispatch
    /// — the `git worktree add`, the environment scrub, the child, its exit
    /// status, the `evidence.json` — against a stand-in that finishes in
    /// milliseconds. Everything above the program is where the failures have
    /// actually been, and a double mounted at the runner seam skips all of it.
    ///
    /// Named `AETHER_BLOOMERY_LANE_PROGRAM` rather than under this struct's
    /// `AETHER_GITHUB` prefix, for the same reason the operator knobs are: which
    /// program a lane runs is not a property of the GitHub connection, and holds
    /// whether or not a remote is configured at all.
    #[config(env = "AETHER_BLOOMERY_LANE_PROGRAM", default = "cargo xtask transform")]
    pub local_lane_program: String,
    /// How long (in seconds) a tracked dispatch may stay unresolved before the
    /// executor reactor logs a `warn` naming the wedge ([`super::ExecutorReactorCapability`],
    /// #3635) — observability only, never a behavior change to admission or
    /// re-drive. `0` disables the warn.
    #[config(default = 1800)]
    pub stale_warn_after_secs: u64,
    /// Where the executor reactor puts an admitted attempt's study record
    /// (#4679) — the artifacts content store's root.
    ///
    /// Named by the **same** environment variable the artifacts capability
    /// resolves its own root from, rather than a second knob of its own. The
    /// reactor opens its own handle on that store the way it opens its own
    /// `SqliteStore` on the shared journal, and two handles are only the same
    /// store if they resolve the same path — a private knob would let a
    /// deployment configure one and not the other, and the failure is silent:
    /// study records land in a directory nothing else reads while the index
    /// rows point at them from the journal.
    #[config(env = "AETHER_ARTIFACTS_ROOT")]
    pub artifacts_root: Option<String>,
    /// Who this coordinator runs on behalf of — the name a candidate capture is
    /// authored under (#4630). A bloom is that person's work delegated to a
    /// machine, not a separate contributor, so the history should read that way.
    ///
    /// Empty (the default) inherits the host's ambient git identity, which
    /// attributes a bloom to whoever runs the coordinator with no configuration
    /// at all. Set alongside [`operator_email`](Self::operator_email) for a
    /// deployment that wants a distinct, stable identity.
    ///
    /// Named `AETHER_BLOOMERY_OPERATOR_*` rather than under this struct's
    /// `AETHER_GITHUB` prefix: the operator is not a property of the GitHub
    /// connection and exists whether or not a remote is configured.
    #[config(env = "AETHER_BLOOMERY_OPERATOR_NAME", default = "")]
    pub operator_name: String,
    /// The email half of the operator identity; see
    /// [`operator_name`](Self::operator_name). Both halves are required
    /// together — a configured name beside an inherited email is a
    /// misconfiguration, not a request to blend the two.
    #[config(env = "AETHER_BLOOMERY_OPERATOR_EMAIL", default = "")]
    pub operator_email: String,
}

impl Default for GithubConnectionConfig {
    fn default() -> Self {
        Self {
            token: String::new(),
            owner: String::new(),
            repo: String::new(),
            api_base: "https://api.github.com".to_owned(),
            cas_land_enabled: true,
            executor_workflow_file: "transform.yml".to_owned(),
            executor_model_workflow_file: "transform-model.yml".to_owned(),
            executor_dispatch_ref: "refs/heads/main".to_owned(),
            app_id: 0,
            app_private_key_path: String::new(),
            app_installation_id: 0,
            app_token_skew_secs: 300,
            #[cfg(any(test, feature = "testing"))]
            github_backend: "github".to_owned(),
            #[cfg(any(test, feature = "testing"))]
            fixture_base_sha: String::new(),
        }
    }
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 5,
            store_path: ":memory:".to_owned(),
            approval_policy_file: "approval-policy.yml".to_owned(),
            local_lane_enabled: true,
            local_lane_commands: "construct.,review.".to_owned(),
            local_worktree_base: ".bloomery/local-worktrees".to_owned(),
            local_lane_program: DEFAULT_LANE_PROGRAM.to_owned(),
            stale_warn_after_secs: 1800,
            artifacts_root: None,
            operator_name: String::new(),
            operator_email: String::new(),
        }
    }
}

impl CoordinatorConfig {
    #[must_use]
    pub fn local_lane_prefixes(&self) -> Vec<String> {
        self.local_lane_commands
            .split(',')
            .map(str::trim)
            .filter(|prefix| !prefix.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

impl GithubConnectionConfig {
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

    #[must_use]
    pub fn missing_connection_knobs(&self) -> Vec<&'static str> {
        [("GITHUB_TOKEN", &self.token), ("AETHER_GITHUB_OWNER", &self.owner), ("AETHER_GITHUB_REPO", &self.repo)]
            .into_iter()
            .filter_map(|(name, value)| value.is_empty().then_some(name))
            .collect()
    }

    #[must_use]
    pub fn app_auth_configured(&self) -> bool {
        self.app_id != 0 && !self.app_private_key_path.is_empty() && self.app_installation_id != 0
    }

    pub fn connect_client(&self) -> Result<ReqwestGithub, GithubError> {
        if self.app_auth_configured() {
            let source = Arc::new(AppTokenSource::from_config(self)?);
            ReqwestGithub::with_token_source(source, self.api_base.clone(), self.to_github_config().repo_path())
        } else {
            ReqwestGithub::new(&self.to_github_config())
        }
    }

    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn uses_fixture(&self) -> bool {
        self.github_backend == "fixture" || self.github_backend == "fake"
    }

    #[cfg(not(any(test, feature = "testing")))]
    #[must_use]
    pub fn uses_fixture(&self) -> bool {
        false
    }

    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn shared_fixture(&self) -> FakeGithub {
        static FAKE: OnceLock<FakeGithub> = OnceLock::new();
        const COORDINATOR_REPO: &str = ".";
        FAKE.get_or_init(|| {
            let base = Digest::from_bytes([0xB0; 32]);
            if self.fixture_base_sha.is_empty() {
                let fake = FakeGithub::new();
                let base_commit = fake.seed_base_commit(&base);
                fake.seed_ref_at("heads/main", &base_commit);
                return fake;
            }
            let fake = FakeGithub::new().with_object_repo(COORDINATOR_REPO);
            fake.seed_correspondence(&base, &self.fixture_base_sha);
            fake.seed_ref("heads/main", &self.fixture_base_sha);
            fake
        })
        .clone()
    }

    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn fixture_source(&self, correspondence: SharedCorrespondence) -> SourceShell {
        let fake = self.shared_fixture();
        let source = GitSource::new(fake, Arc::clone(&correspondence), self.cas_land_enabled);
        SourceShell::new_with_correspondence(Arc::new(source), correspondence)
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{CoordinatorConfig, GithubConnectionConfig};
    use crate::bloomery::BloomeryCli;

    #[test]
    fn coordinator_and_connection_overlays_resolve_independently() {
        let cli = BloomeryCli::try_parse_from([
            "bloomery",
            "--github-owner",
            "octo",
            "--github-poll-interval-secs",
            "17",
            "--github-local-lane-enabled=false",
        ])
        .expect("split overlays accept the preserved flag spellings");

        let connection = GithubConnectionConfig::try_from_argv_then_env(cli.github.into_layer())
            .expect("connection overlay resolves");
        let coordinator = CoordinatorConfig::try_from_argv_then_env(cli.coordinator.into_layer())
            .expect("coordinator overlay resolves");

        assert_eq!(connection.owner, "octo");
        assert_eq!(coordinator.poll_interval_secs, 17);
        assert!(!coordinator.local_lane_enabled);
    }

    #[test]
    fn split_defaults_preserve_the_combined_boundary_values() {
        let connection = GithubConnectionConfig::default();
        let coordinator = CoordinatorConfig::default();

        assert_eq!(connection.executor_workflow_file, "transform.yml");
        assert_eq!(connection.executor_model_workflow_file, "transform-model.yml");
        assert_eq!(coordinator.poll_interval_secs, 5);
        assert_eq!(coordinator.local_lane_prefixes(), ["construct.", "review."]);
        assert_eq!(coordinator.store_path, ":memory:");
    }
}
