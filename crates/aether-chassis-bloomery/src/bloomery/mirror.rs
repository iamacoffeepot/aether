#![allow(clippy::absolute_paths)]
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

#[cfg(any(test, feature = "testing"))]
use aether_bloomery_github::testing::FakeGithub;
use std::sync::{Arc, OnceLock};

use aether_bloomery::{Digest, LandingReceipt, ProjectionBackend, ViewDocument};
use aether_bloomery_github::{GithubConfig, GithubError, GithubProjection, ReqwestGithub, SharedCorrespondence};

use super::executor::DEFAULT_LANE_PROGRAM;
use crate::app_auth::AppTokenSource;
use crate::store::SqliteCorrespondence;

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
    /// Whether compare-and-swap mainline landing is permitted. On by default
    /// (ADR-0149 migration step 3 moved landing authority to the source port —
    /// the CAS `land` is the landing of record); consumed by the git source
    /// port to gate [`GitSource::land`](aether_bloomery_github::GitSource) and by
    /// the land reactor ([`super::LandReactorCapability`]) that drives it.
    #[config(default = true)]
    pub cas_land_enabled: bool,
    /// How often the mirror reactor ([`super::MirrorReactorCapability`])
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
    /// repository-relative path read host-side
    /// like [`executor_workflow_file`](Self::executor_workflow_file) names an
    /// in-repo workflow file; carried on this shared config the same way, so one
    /// Bloomery-host configuration serves the mirror, source, executor, and
    /// approve-gate readers rather than duplicating the knob. Tier policy (*what*
    /// tier) is a **distinct reader** from the signing capability's key policy
    /// (*who* may sign) — the two are never folded (ADR-0151).
    #[config(env = "AETHER_APPROVAL_POLICY_FILE", default = "approval-policy.yml")]
    pub approval_policy_file: String,
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
    /// `AETHER_GITHUB` prefix, the way [`token`](Self::token) pins the
    /// conventional `GITHUB_TOKEN`: the operator is not a property of the GitHub
    /// connection — it holds whether or not a remote is configured at all.
    #[config(env = "AETHER_BLOOMERY_OPERATOR_NAME", default = "")]
    pub operator_name: String,
    /// The email half of the operator identity; see
    /// [`operator_name`](Self::operator_name). Both halves are required
    /// together — a configured name beside an inherited email is a
    /// misconfiguration, not a request to blend the two.
    #[config(env = "AETHER_BLOOMERY_OPERATOR_EMAIL", default = "")]
    pub operator_email: String,
    /// Which GitHub implementation to use — `github` (default, real network) or
    /// `fake` (in-memory double for the lane-boundary harness, #4732). Only
    /// compiled in `cfg(test)` so prod's `config_manifest()` never advertises
    /// `AETHER_GITHUB_BACKEND` and a prod binary never links `FakeGithub`.
    #[cfg(any(test, feature = "testing"))]
    #[config(env = "AETHER_GITHUB_BACKEND", default = "github")]
    pub github_backend: String,
}

impl Default for GithubMirrorConfig {
    fn default() -> Self {
        Self {
            token: String::new(),
            owner: String::new(),
            repo: String::new(),
            api_base: "https://api.github.com".to_owned(),
            cas_land_enabled: true,
            poll_interval_secs: 5,
            executor_workflow_file: "transform.yml".to_owned(),
            executor_model_workflow_file: "transform-model.yml".to_owned(),
            executor_dispatch_ref: "refs/heads/main".to_owned(),
            store_path: ":memory:".to_owned(),
            approval_policy_file: "approval-policy.yml".to_owned(),
            app_id: 0,
            app_private_key_path: String::new(),
            app_installation_id: 0,
            app_token_skew_secs: 300,
            local_lane_enabled: true,
            local_lane_commands: "construct.,review.".to_owned(),
            local_worktree_base: ".bloomery/local-worktrees".to_owned(),
            local_lane_program: DEFAULT_LANE_PROGRAM.to_owned(),
            stale_warn_after_secs: 1800,
            artifacts_root: None,
            operator_name: String::new(),
            operator_email: String::new(),
            #[cfg(any(test, feature = "testing"))]
            github_backend: "github".to_owned(),
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

    /// Whether this config selects the in-memory fixture — `AETHER_GITHUB_BACKEND=fixture` (alias `fake`).
    /// Always false in prod so prod never links `FakeGithub`.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn uses_fixture(&self) -> bool {
        self.github_backend == "fixture" || self.github_backend == "fake"
    }
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    #[deprecated(note = "use uses_fixture")]
    pub fn is_fake(&self) -> bool {
        self.uses_fixture()
    }
    #[cfg(not(any(test, feature = "testing")))]
    #[must_use]
    pub fn uses_fixture(&self) -> bool {
        false
    }

    /// Shared in-memory GitHub double for `fixture` backends (#4732). Only
    /// available in `cfg(any(test, feature = "testing"))` so prod never links `FakeGithub`.
    #[cfg(any(test, feature = "testing"))]
    #[allow(clippy::absolute_paths)]
    #[must_use]
    pub fn shared_fixture(&self) -> FakeGithub {
        static FAKE: OnceLock<FakeGithub> = OnceLock::new();
        FAKE.get_or_init(|| {
            let fake = FakeGithub::new();
            let base = Digest::from_bytes([0xB0; 32]);
            // Lane harness runs over a real scratch repo whose HEAD the harness
            // records in SQLite; the forked coordinator's in-memory fixture must
            // resolve the same 0xB0 digest to that real object so `git fetch`
            // finds it. The harness passes the sha via env.
            #[allow(clippy::disallowed_methods)]
            if let Ok(sha) = std::env::var("AETHER_GITHUB_FIXTURE_BASE_SHA") {
                fake.seed_correspondence(&base, &sha);
                fake.seed_ref("heads/main", &sha);
                return fake;
            }
            let base_commit = fake.seed_base_commit(&base);
            fake.seed_ref_at("heads/main", &base_commit);
            fake
        })
        .clone()
    }
    #[cfg(any(test, feature = "testing"))]
    #[allow(clippy::absolute_paths)]
    #[deprecated(note = "use shared_fixture")]
    #[must_use]
    pub fn shared_fake(&self) -> FakeGithub {
        self.shared_fixture()
    }

    /// The connection knobs this config leaves empty, in the order an operator
    /// would set them. Empty when GitHub is reachable.
    ///
    /// Names rather than a bare bool so a caller can say *which* knob is missing
    /// (#4625) — the disabled-mount warning and the local-only shell's submit
    /// refusal both do. `token` reads the conventional unprefixed `GITHUB_TOKEN`,
    /// unlike its `AETHER_GITHUB_`-prefixed siblings, so it is the one most often
    /// left empty by accident.
    #[must_use]
    pub fn missing_connection_knobs(&self) -> Vec<&'static str> {
        [("GITHUB_TOKEN", &self.token), ("AETHER_GITHUB_OWNER", &self.owner), ("AETHER_GITHUB_REPO", &self.repo)]
            .into_iter()
            .filter_map(|(name, value)| value.is_empty().then_some(name))
            .collect()
    }

    /// The local-lane command-id prefixes, parsed from the comma-separated
    /// [`local_lane_commands`](Self::local_lane_commands) knob: split on commas,
    /// trimmed, empties dropped. An order whose command starts with any of these
    /// routes to the local backend.
    #[must_use]
    pub fn local_lane_prefixes(&self) -> Vec<String> {
        self.local_lane_commands
            .split(',')
            .map(str::trim)
            .filter(|prefix| !prefix.is_empty())
            .map(str::to_owned)
            .collect()
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

    /// Open the persisted git-object↔bloom-digest correspondence the source and
    /// executor ports resolve real git shas through (ADR-0150), over the **same**
    /// `SQLite` [`store_path`](Self::store_path) the `StoreCapability` owns — one
    /// persistence layer serves the journal and the correspondence. Every port
    /// shell that resolves a digest to a git object opens its own connection to
    /// that file (WAL serializes the rare concurrent write), so one config serves
    /// the mirror, source, and executor caps.
    ///
    /// # Errors
    /// The `SQLite` connection could not be opened or its migration failed.
    pub fn connect_correspondence(&self) -> Result<SharedCorrespondence, GithubError> {
        let store = SqliteCorrespondence::open(&self.store_path)
            // The correspondence store faulting at open is a boot-time
            // configuration failure; surface it through the shell's `GithubError`
            // bound (a store fault, mapped onto the transport arm) rather than
            // widening every shell's `connect` signature.
            .map_err(|error| GithubError::Transport(format!("correspondence store: {error}")))?;
        Ok(Arc::new(store))
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

    #[test]
    fn local_lane_prefixes_parses_the_comma_list() {
        // The one bit of logic the config owns for the router: split, trim, drop
        // empties — a whitespace-padded or trailing-comma value still parses clean.
        let config = GithubMirrorConfig { local_lane_commands: " construct. , verify. ,".into(), ..Default::default() };
        assert_eq!(config.local_lane_prefixes(), vec!["construct.".to_owned(), "verify.".to_owned()]);

        // The default routes both model-driven lanes local — construct/refine
        // and the review critic, which needs the model API the zero-secret
        // runner deliberately lacks.
        assert_eq!(
            GithubMirrorConfig::default().local_lane_prefixes(),
            vec!["construct.".to_owned(), "review.".to_owned()]
        );
    }
}
