//! Bloomery coordinator and GitHub adapter configuration boundaries.

use std::env;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

#[cfg(feature = "github")]
use std::fs;
#[cfg(feature = "github")]
use std::sync::Arc;
#[cfg(all(feature = "github", any(test, feature = "testing")))]
use std::sync::OnceLock;

#[cfg(all(feature = "github", any(test, feature = "testing")))]
use aether_bloomery::Digest;
#[cfg(all(feature = "github", any(test, feature = "testing")))]
use aether_bloomery::SharedCorrespondence;
#[cfg(feature = "github")]
use aether_bloomery_github::{AppTokenSource, GithubConfig, GithubError, MainlineRef, ReqwestGithub};
#[cfg(all(feature = "github", any(test, feature = "testing")))]
use aether_bloomery_github::{GitSource, GithubLanding, testing::FakeGithub};
use aether_substrate::config::ConfigError;

const DEFAULT_LANE_PROGRAM: &str = "cargo xtask transform";
#[cfg(all(feature = "github", any(test, feature = "testing")))]
use super::source::SourceShell;

#[cfg(feature = "github")]
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
    #[cfg(all(feature = "github", any(test, feature = "testing")))]
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
    #[cfg(all(feature = "github", any(test, feature = "testing")))]
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
    /// The same journal [`StoreConfig`](crate::store::StoreConfig) opens:
    /// `--github-store-path` is an alias of `--store-path`, and both read
    /// `AETHER_STORE_PATH`. [`BloomeryEnv::resolve`](super::BloomeryEnv::resolve)
    /// collapses the two overlays onto one path so reactors and the store
    /// capability cannot open different files from one command line.
    #[config(env = "AETHER_STORE_PATH", default = ":memory:")]
    pub store_path: String,
    /// The in-repo tier-policy artifact the pre-seal approve gate
    /// ([`super::Gate`] / [`super::ApprovalPolicy`], ADR-0149 §The line /
    /// ADR-0151) resolves a workpiece's declared surface against — the
    /// Bloomery-owned `approval-policy.toml` at the repository root. A
    /// repository-relative path read host-side, like the connection config's
    /// `executor_workflow_file`, names an in-repo artifact. Tier policy (*what*
    /// tier) is a **distinct reader** from the signing capability's key policy
    /// (*who* may sign) — the two are never folded (ADR-0151).
    #[config(env = "AETHER_APPROVAL_POLICY_FILE", default = "approval-policy.toml")]
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
    /// How many local lane children the executor backend may run at once.
    ///
    /// Each construct or verify lane is a whole cargo build with its own throwaway
    /// target dir, and a seal fans out one dispatch per member, so an uncapped
    /// backend turns member count directly into simultaneous builds racing the same
    /// CPU and disk. Dispatches past the ceiling wait in submission order and start
    /// as running lanes finish — a queue, never a refusal: every dispatch acks as
    /// submitted either way, so the reducer's view of one is the same whether it
    /// waited or not.
    ///
    /// Per backend rather than per bloom: member lanes, aggregate lanes, and the
    /// runs re-adopted at boot all count against the same slots. `0` resolves to
    /// `1`, since a ceiling of zero would start nothing at all.
    ///
    /// Named `AETHER_BLOOMERY_MAX_CONCURRENT_LANES` rather than under this struct's
    /// `AETHER_GITHUB` prefix, for the reason the lane program and operator knobs
    /// are: how much a host runs at once is a property of the machine, not of the
    /// GitHub connection.
    #[config(env = "AETHER_BLOOMERY_MAX_CONCURRENT_LANES", default = 3)]
    pub max_concurrent_lanes: usize,
    /// Where the per-slot cargo target directories live — one `slot-<index>-target`
    /// under this root, handed to the lane that holds that slot as its
    /// `CARGO_TARGET_DIR` (#4912).
    ///
    /// Empty (the default) puts them beside the slot checkouts, under
    /// [`local_worktree_base`](Self::local_worktree_base). A deployment whose
    /// scratch root sits on a small volume points this at a roomier one instead:
    /// a build tree is tens of gigabytes per slot and is pure cache, so it need
    /// not share a volume with the checkouts it is taken over.
    ///
    /// What it may **not** be is a path inside a slot's checkout. Every dispatch
    /// resets its slot with `git clean --force --force -d -x`, which removes
    /// ignored files — a target directory in there would be deleted once per
    /// dispatch, turning the warm dependency tree the slot layout exists for into
    /// a cold build every lap. A base that resolves inside one is refused in
    /// favour of the default rather than honoured.
    ///
    /// Named `AETHER_BLOOMERY_LANE_TARGET_BASE` rather than under this struct's
    /// `AETHER_GITHUB` prefix, for the reason the lane program and operator knobs
    /// are: which volume a host builds on is a property of the machine, not of
    /// the GitHub connection.
    #[config(env = "AETHER_BLOOMERY_LANE_TARGET_BASE", default = "")]
    pub lane_target_base: String,
    /// Combined size ceiling, in bytes, across every per-slot cargo target
    /// directory (`<base>/slot-<index>-target`). The janitor sweeps those dirs
    /// only when this total is crossed *and* no lane is running — a cold rebuild
    /// is minutes; an overflow wedge is a member. `0` means "keep nothing": an
    /// idle host reclaims every slot target dir.
    ///
    /// Named `AETHER_BLOOMERY_LANE_TARGET_BUDGET_BYTES` rather than under this
    /// struct's `AETHER_GITHUB` prefix, for the reason the other host-resource
    /// knobs are: the disk a build cache may fill is a property of the machine.
    #[config(env = "AETHER_BLOOMERY_LANE_TARGET_BUDGET_BYTES", default = 68_719_476_736u64)]
    pub lane_target_budget_bytes: u64,
    /// How many days a consumed evidence directory of a terminal bloom is kept
    /// after that bloom lands or is superseded. Evidence feeds intake and then
    /// serves forensics and the calibration ledger (ADR-0184); this is the
    /// stated retention window, not a silent default-delete. `0` reclaims as
    /// soon as the owning bloom is terminal and the evidence is no longer
    /// outstanding. Live blooms' evidence is never deleted.
    ///
    /// Named `AETHER_BLOOMERY_EVIDENCE_RETENTION_DAYS` rather than under this
    /// struct's `AETHER_GITHUB` prefix: retention is a coordinator policy, not
    /// a GitHub-connection property.
    #[config(env = "AETHER_BLOOMERY_EVIDENCE_RETENTION_DAYS", default = 7)]
    pub evidence_retention_days: u64,
    /// How many build jobs one lane's cargo invocations may run at once — the
    /// `CARGO_BUILD_JOBS` every dispatch and the verify gates inside it run under
    /// (#4912).
    ///
    /// The default is eight because that is where the measurement landed
    /// (`spike/build-concurrency`, 2026-08-13): `-j32` beat `-j8` by 18% on a solo
    /// cold build — the crate graph's critical path dominates, not the core count
    /// — while a `-j8` build peaks around 5 GiB. Capping each lane is therefore
    /// nearly free on latency and is what lets several lanes coexist in one
    /// host's memory instead of racing it to the out-of-memory killer.
    ///
    /// `0` resolves to cargo's own default (unset), which is one job per core.
    #[config(env = "AETHER_BLOOMERY_LANE_BUILD_JOBS", default = 8)]
    pub lane_build_jobs: usize,
    /// How long (in seconds) a tracked dispatch may stay unresolved before the
    /// executor reactor logs a `warn` naming the wedge ([`super::ExecutorReactorCapability`],
    /// #3635) — observability only, never a behavior change to admission or
    /// re-drive. `0` disables the warn.
    #[config(default = 1800)]
    pub stale_warn_after_secs: u64,
    /// How long (in seconds) a local model lane may stay silent — no advance of
    /// its streamed `transcript.jsonl` — before the executor reactor cancels it
    /// as a host-observed machinery failure (ADR-0195 §8). The sealed wall clock
    /// is still the outer bound; this only answers how long this host will trust
    /// a process that has stopped producing its declared liveness signal.
    ///
    /// Ten minutes by default: long enough that a slow compile is not silence,
    /// short enough that a dead child does not consume the sealed hour. `0` is
    /// refused at startup rather than meaning unbounded silence — a lane with
    /// no trustworthy heartbeat stays deadline-only because its backend reports
    /// none, not because this knob is off.
    ///
    /// Named `AETHER_BLOOMERY_HEARTBEAT_SILENCE_SECS` rather than under this
    /// struct's `AETHER_GITHUB` prefix: how long a host trusts a silent local
    /// process is a property of the machine, not of the GitHub connection.
    #[config(env = "AETHER_BLOOMERY_HEARTBEAT_SILENCE_SECS", default = 600)]
    pub heartbeat_silence_secs: u64,
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
    /// The ref bloomery treats as mainline: what it observes, what a bloom's
    /// sealed base is compared against on the compare-and-swap land, and what a
    /// landing proposal opens onto (ADR-0186).
    ///
    /// Boot configuration rather than a constant because bloomery operates on a
    /// branch cut per day and repoints at the roll. Resolved rather than sealed:
    /// a sealed base already pins the exact commit a bloom builds on, so sealing
    /// the ref name would only freeze the roll, and the journal records observed
    /// heads rather than ref names, so a repoint needs no migration.
    ///
    /// The default is the repository's own default branch — the ref the pipeline
    /// ran on before the day branch existed, so an unconfigured deployment
    /// behaves exactly as it did. Accepted in any of the three spellings
    /// (`refs/heads/x`, `heads/x`, `x`); an empty value resolves to the default.
    ///
    /// Named `AETHER_BLOOMERY_MAINLINE_REF` rather than under this struct's
    /// `AETHER_GITHUB` prefix, for the reason the operator and lane knobs are:
    /// which branch bloomery integrates on is a property of how it is being
    /// operated, not of the GitHub connection.
    #[config(env = "AETHER_BLOOMERY_MAINLINE_REF", default = "refs/heads/main")]
    pub mainline_ref: String,
    /// Which git-data backend the source port talks to: `github` (the default,
    /// the GitHub REST adapter remains authoritative) or `local` (an absolute
    /// bare-repository path via [`authority_repo`](Self::authority_repo)).
    ///
    /// Named `AETHER_BLOOMERY_AUTHORITY_BACKEND` rather than under this
    /// struct's `AETHER_GITHUB` prefix: which store owns the refs is how the
    /// coordinator is being operated, not a GitHub-connection property.
    #[config(env = "AETHER_BLOOMERY_AUTHORITY_BACKEND", default = "github")]
    pub authority_backend: String,
    /// Absolute filesystem path to the bare repository used when
    /// [`authority_backend`](Self::authority_backend) is `local`. Empty (the
    /// default) is only valid for the GitHub backend. No `file://` prefix.
    #[config(env = "AETHER_BLOOMERY_AUTHORITY_REPO", default = "")]
    pub authority_repo: String,
    /// How long a running batch gate stays young enough that arriving work
    /// restarts it (ADR-0200 §The batch gate). The eight-finish-then-twenty-four-more
    /// case restarts because the first eight's build is still young when the
    /// rest arrive. The default is the observed one-minute window: long enough
    /// that a burst of resolves joins one prove, short enough that a mature
    /// compile is not thrown away for a late single member.
    ///
    /// Named `AETHER_BLOOMERY_BATCH_RESTART_YOUNG_SECS` rather than under this
    /// struct's `AETHER_GITHUB` prefix: when a host preempts a prove is a
    /// property of the machine, not of the GitHub connection.
    #[config(env = "AETHER_BLOOMERY_BATCH_RESTART_YOUNG_SECS", default = 60)]
    pub batch_restart_young_secs: u64,
    /// How many newly-resolved members restart even a mature batch gate.
    /// The observed twenty-four-more case is the default: a large arrival
    /// pays for the restart; a single late member waits for the next take.
    ///
    /// Named `AETHER_BLOOMERY_BATCH_RESTART_ADDITION` rather than under this
    /// struct's `AETHER_GITHUB` prefix, for the reason the young-window knob
    /// is: accumulation policy is coordinator-owned.
    #[config(env = "AETHER_BLOOMERY_BATCH_RESTART_ADDITION", default = 24)]
    pub batch_restart_addition: usize,
    /// Absolute path to the single-writer marker file this coordinator must
    /// present before it will push source refs to GitHub (ADR-0199). Empty
    /// means no writer claim. A local-authority boot that has a GitHub replica
    /// configured refuses to start unless this path names an existing file,
    /// so two writers cannot be enabled by accident.
    ///
    /// Named `AETHER_BLOOMERY_SINGLE_WRITER_MARKER` rather than under this
    /// struct's `AETHER_GITHUB` prefix: which host may write the replica is
    /// how the coordinator is being operated.
    #[config(env = "AETHER_BLOOMERY_SINGLE_WRITER_MARKER", default = "")]
    pub single_writer_marker: String,
    /// Bearer token the commission control-API routes require. Empty (the
    /// default) refuses every commission request — fail-closed rather than an
    /// unauthenticated surface that can approve work. Named
    /// `AETHER_HTTP_CONTROL_TOKEN` so it sits with the REST ingress, not the
    /// GitHub prefix this struct otherwise uses.
    #[config(env = "AETHER_HTTP_CONTROL_TOKEN", default = "")]
    pub http_control_token: String,
}

#[cfg(feature = "github")]
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
            #[cfg(all(feature = "github", any(test, feature = "testing")))]
            github_backend: "github".to_owned(),
            #[cfg(all(feature = "github", any(test, feature = "testing")))]
            fixture_base_sha: String::new(),
        }
    }
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 5,
            store_path: ":memory:".to_owned(),
            approval_policy_file: "approval-policy.toml".to_owned(),
            local_lane_enabled: true,
            local_lane_commands: "construct.,review.".to_owned(),
            local_worktree_base: ".bloomery/local-worktrees".to_owned(),
            local_lane_program: DEFAULT_LANE_PROGRAM.to_owned(),
            max_concurrent_lanes: 3,
            lane_target_base: String::new(),
            lane_target_budget_bytes: 68_719_476_736,
            evidence_retention_days: 7,
            lane_build_jobs: 8,
            stale_warn_after_secs: 1800,
            heartbeat_silence_secs: 600,
            artifacts_root: None,
            operator_name: String::new(),
            operator_email: String::new(),
            mainline_ref: "refs/heads/main".to_owned(),
            authority_backend: "github".to_owned(),
            authority_repo: String::new(),
            batch_restart_young_secs: 60,
            batch_restart_addition: 24,
            single_writer_marker: String::new(),
            http_control_token: String::new(),
        }
    }
}

impl CoordinatorConfig {
    /// The resolved mainline ref, normalized into the forms the source port
    /// addresses it by (ADR-0186).
    #[cfg(feature = "github")]
    #[must_use]
    pub fn mainline(&self) -> MainlineRef {
        MainlineRef::new(&self.mainline_ref)
    }

    /// Whether the source port should open a fleet-local git-data backend.
    #[must_use]
    pub fn uses_local_authority(&self) -> bool {
        self.authority_backend == "local"
    }

    /// The git repository the transform runner materializes worktrees from.
    ///
    /// A local authority names its absolute path (`authority_repo`); GitHub
    /// still runs from the process's current directory, captured once here so
    /// the runner never re-reads `"."`.
    #[must_use]
    pub fn lane_repository(&self) -> PathBuf {
        if self.uses_local_authority() && !self.authority_repo.is_empty() {
            PathBuf::from(&self.authority_repo)
        } else {
            env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        }
    }

    /// Where missing order objects are fetched from, and where an admitted
    /// capture is published: the local authority path, or the `origin` remote
    /// when GitHub is authoritative. An absolute path, never a `file://` URL.
    #[must_use]
    pub fn candidate_remote(&self) -> String {
        if self.uses_local_authority() && !self.authority_repo.is_empty() {
            self.authority_repo.clone()
        } else {
            "origin".to_owned()
        }
    }

    #[must_use]
    pub fn local_lane_prefixes(&self) -> Vec<String> {
        self.local_lane_commands
            .split(',')
            .map(str::trim)
            .filter(|prefix| !prefix.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// The host heartbeat-silence allowance, or a boot fault when the knob is
    /// zero. Zero is not "unbounded": a lane without a trustworthy progress
    /// signal stays deadline-only because its backend reports none.
    pub fn heartbeat_silence_secs(&self) -> Result<u64, ConfigError> {
        if self.heartbeat_silence_secs == 0 {
            return Err(ConfigError::unparseable("AETHER_BLOOMERY_HEARTBEAT_SILENCE_SECS", "0", HeartbeatSilenceZero));
        }
        Ok(self.heartbeat_silence_secs)
    }

    /// Whether this boot should push allowlisted refs to the GitHub replica.
    ///
    /// Local authority plus a fully-configured GitHub connection — not a
    /// fixture, which has no git remote.
    #[cfg(feature = "github")]
    #[must_use]
    pub fn source_replica_enabled(&self, github: &GithubConnectionConfig) -> bool {
        self.uses_local_authority()
            && !self.authority_repo.is_empty()
            && github.missing_connection_knobs().is_empty()
            && !github.uses_fixture()
    }

    /// Refuse a source-replica boot that does not present the single-writer
    /// marker, so two writers cannot be enabled by accident.
    ///
    /// # Errors
    /// Source replication is enabled and the marker path is empty or is not
    /// an existing file.
    #[cfg(feature = "github")]
    pub fn require_single_writer_marker(&self, github: &GithubConnectionConfig) -> Result<(), MissingWriterMarker> {
        if !self.source_replica_enabled(github) {
            return Ok(());
        }
        if super::replica::writer_marker_present(&self.single_writer_marker) {
            return Ok(());
        }
        Err(MissingWriterMarker { path: self.single_writer_marker.clone() })
    }
}

/// Why a zero heartbeat-silence setting is refused rather than treated as
/// "never reap on silence".
#[derive(Debug)]
struct HeartbeatSilenceZero;

/// Why a source-replica boot without the single-writer marker is refused.
#[derive(Debug)]
pub struct MissingWriterMarker {
    /// The configured marker path, empty when the knob was left unset.
    pub path: String,
}

impl fmt::Display for HeartbeatSilenceZero {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("must be nonzero; zero is not unbounded silence")
    }
}

impl Error for HeartbeatSilenceZero {}

impl fmt::Display for MissingWriterMarker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(
                "source replica requires AETHER_BLOOMERY_SINGLE_WRITER_MARKER to name an existing file so two writers cannot be enabled by accident",
            )
        } else {
            write!(
                f,
                "source replica requires the single-writer marker at '{}' to exist so two writers cannot be enabled by accident",
                self.path
            )
        }
    }
}

impl Error for MissingWriterMarker {}

#[cfg(feature = "github")]
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

    /// Build the client the port shells authenticate with: a minted
    /// installation-token client when App-auth is configured, else the
    /// backward-compatible static-PAT one.
    ///
    /// The host's whole share of App-auth is here — resolve the key path and
    /// read the bytes (ADR-0150 keeps that custody host-local, so the path never
    /// crosses into the adapter), then hand plain values to the adapter's
    /// minter. A missing or malformed key is a boot fault, never a silent
    /// fallback to the ambient static token.
    pub fn connect_client(&self) -> Result<ReqwestGithub, GithubError> {
        if self.app_auth_configured() {
            let pem = fs::read(&self.app_private_key_path).map_err(|error| {
                GithubError::Transport(format!(
                    "reading GitHub App private key '{}': {error}",
                    self.app_private_key_path
                ))
            })?;
            let source = Arc::new(AppTokenSource::new(
                self.app_id,
                self.app_installation_id,
                &pem,
                self.app_token_skew_secs,
                self.api_base.clone(),
            )?);
            ReqwestGithub::with_token_source(source, self.api_base.clone(), self.to_github_config().repo_path())
        } else {
            ReqwestGithub::new(&self.to_github_config())
        }
    }

    #[cfg(all(feature = "github", any(test, feature = "testing")))]
    #[must_use]
    pub fn uses_fixture(&self) -> bool {
        self.github_backend == "fixture" || self.github_backend == "fake"
    }

    #[cfg(not(any(test, feature = "testing")))]
    #[must_use]
    pub fn uses_fixture(&self) -> bool {
        false
    }

    #[cfg(all(feature = "github", any(test, feature = "testing")))]
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

    #[cfg(all(feature = "github", any(test, feature = "testing")))]
    #[must_use]
    pub fn fixture_source(&self, mainline: MainlineRef, correspondence: SharedCorrespondence) -> SourceShell {
        let fake = self.shared_fixture();
        // The fixture seeds the default branch. A repointed coordinator reads its
        // own ref, so point that one at the same seeded commit — otherwise the
        // double has no mainline at all and every fixture-backed run wedges on
        // the repoint the knob exists to allow.
        if let Some(seeded) = fake.ref_target("heads/main") {
            fake.seed_ref(mainline.git_ref(), &seeded);
        }
        let source = GitSource::new(fake, Arc::clone(&correspondence), self.cas_land_enabled, mainline);
        SourceShell::new_with_correspondence(Arc::new(source), correspondence)
    }

    #[cfg(all(feature = "github", any(test, feature = "testing")))]
    #[must_use]
    pub fn fixture_landing(
        &self,
        mainline: MainlineRef,
        correspondence: SharedCorrespondence,
    ) -> Arc<dyn aether_bloomery_github::LandingSource> {
        let fake = self.shared_fixture();
        if let Some(seeded) = fake.ref_target("heads/main") {
            fake.seed_ref(mainline.git_ref(), &seeded);
        }
        let source = GitSource::new(fake, correspondence, self.cas_land_enabled, mainline);
        Arc::new(GithubLanding::new(source))
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{CoordinatorConfig, GithubConnectionConfig};
    use crate::bloomery::BloomeryCli;

    // A throwaway 2048-bit RSA keypair (never a real credential) — the fixture
    // the App branch parses when it is pointed at a key that exists.
    const TEST_PRIVATE_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEpQIBAAKCAQEAv2JzVEAUCyxtdyoUeFfFzydL9W9BOwO5W1fKkGhQ9dfgid5c
1dJwUR/jWb5KHXZ2cAvZ5j6wK8PaKG5WSxtSqO5ingxHJA7SzxX6kQseXLUHamAv
OZ6i3iiNDY3xuO9MdEd8BVT0iNpSm3eQN10+Ug1kXfw9rnIqNp7g/xwSYllKG9o8
rISa26Huo/WQ+PB/aKRHgVQ3Un8ajKnIc0UBTUa95t1kg1a1S5w2meBLb/sh2y3n
Wy1V4yMjVO70x2sWgoVTn2s7PAyzTpc0CmQ78/5Y/XxEZlK27VFOP3W4xdlPWC0S
pggjNDzSt+Q7bur8qzf20HeZOpeTyVCN3croVwIDAQABAoIBAD89sQ5t/jGTBLkT
1p/NoTfKrHb1xIBTwrREVlNRpS8XnsLwD404dJTaDK5jCuqhcpGj2OUUYfKUTUp+
61T2OmJII55GQFvR6ic0BBBZtDa+Oy0Ti4dmvDrc+383IGET8heaZ4j7gbKXMiTd
ZXJmBWnnsvq7l0ZFw105MvAZvplwhHczkq+CjfwypM0VRV9XKxUfuQdiFG377jjZ
LARbtN7kWxtm2iwL2ZtfKQPbYjxCUrYWVN1q9e3kcL+bITf3GwK34k7umuYy1+rw
zanpQ0L+F5sU6XCu+3G3XNhM76kKXoi9SCwlhLp4r6T+Wkk6yvBl2nBRtB90qeHE
ONM1FYECgYEA+sieSVEvvRPc7oH4na/P6JVKdoxCtf4oHHHm5uAltmjF30169R2V
a2mdQ8pPlLx2WeN9tzM/dEuDCR1f0Gb/04IZBJBlirj1G1yG85GxQqB34C246MlP
+nlc/wCgZi8I79RZ7Hp8OSY86M+h0gZWc6njWzEhyQ+BO4h9KB21cU8CgYEAw12K
tG53ml3wTTXSvkWrS07rkePR6MmyTBFn8TU3xYTI0Do5a6zM2pViqub3cFh9IhZp
Odox7onKS+mut/aXfDHZScba0+s9cZOUc/rW8Z2m4Os6vvCa8cguxUW3P/uZ5/Ey
XrSzJnwb+dKINnC/a6ag/JPwGSmQm4LDYnJ3hnkCgYEA6HZAazvLYZvg5mEp4JlQ
wopoTL01NVfTPJLEc2yA6LXz/Urn2ABFOhzbPzRwUjHkDuyV4tSpVBaO70sAPsDL
EPb+U8G5rj5GTceV/H8nbdgrZm1bgsTg0w/eiS2+gRnGUfFoLZFYRu1P9opIuNNR
HcPz0NsZMzOhGlspkJ8BSncCgYEAv8nHzgOYNJm9uv54qcPZOi/6wJjHS+EdwOFh
igD1hFkrjodqMVNNM9RtLVtaVBb6mQkpOdsDI6pvRwDcPcq9wfVp26x0zI/mHOaF
WSpJ8p4S4kDqxeGMKombqJwdHpnP6Ev3Z9O6/6/dAu50PAWJVZQZ/Hr6vKj6RkAj
sTSwM/kCgYEA+J08Bt+2+HDSw8Grsc3WOiPJTuIMaX3uhEjxwlozq36GPah6T8+d
q9nQWTzvE1G118enh8FoJE0/v3x+IGXpLXoseASCSkOuJvIZB4LIuz/sndc6QcDX
xAtw6HCuoUIzjbWZe1H+wS8KmJmYkTvf8f70x0/jMYRUyvMQy3beUUQ=
-----END RSA PRIVATE KEY-----";

    fn configured(app_id: u64, key_path: &str, installation_id: u64) -> GithubConnectionConfig {
        GithubConnectionConfig {
            app_id,
            app_private_key_path: key_path.to_owned(),
            app_installation_id: installation_id,
            ..GithubConnectionConfig::default()
        }
    }

    #[test]
    fn app_auth_configured_requires_all_three_knobs() {
        // The default (empty) config is the static-PAT path — App-auth off.
        assert!(!GithubConnectionConfig::default().app_auth_configured());
        // All three present → on.
        assert!(configured(12345, "/keys/app.pem", 42).app_auth_configured());
        // Any one missing → off (a partial config never silently half-enables).
        assert!(!configured(0, "/keys/app.pem", 42).app_auth_configured());
        assert!(!configured(12345, "", 42).app_auth_configured());
        assert!(!configured(12345, "/keys/app.pem", 0).app_auth_configured());
    }

    #[test]
    fn connect_client_takes_the_app_branch_when_configured_and_the_static_branch_otherwise() {
        // The host wiring under test: `connect_client` branches on
        // `app_auth_configured`. The App branch reads the host-local key and builds a
        // minted-token client; the static branch builds the backward-compatible
        // PAT client. Assert the branch is taken by its distinguishing behavior — a
        // configured-but-missing-key config errors (only the App branch reads a key),
        // while the same knobs pointed at a real key, and an unconfigured config,
        // both construct a client.
        use std::io::Write as _;

        // Unconfigured → the static-PAT branch constructs a client (no key read).
        assert!(GithubConnectionConfig::default().connect_client().is_ok(), "static-PAT path builds a client");

        // Configured with an absent key → the App branch is taken and fails fast
        // (the static branch would have succeeded, so the error proves the branch).
        // ADR-0150: an absent key is a boot fault, never a silent fallback to an
        // ambient secret.
        let missing = configured(12345, "/nonexistent/does-not-exist.pem", 42);
        assert!(missing.connect_client().is_err(), "App path reads the key and fails fast when it is absent");

        // Configured with a real key on disk → the App branch constructs a
        // minted-token client.
        let mut key_file = tempfile::NamedTempFile::new().expect("a temp key file is creatable");
        key_file.write_all(TEST_PRIVATE_KEY.as_bytes()).expect("the fixture key writes");
        let path = key_file.path().to_str().expect("the temp path is UTF-8").to_owned();
        let with_key = configured(12345, &path, 42);
        assert!(with_key.connect_client().is_ok(), "App path builds a client from a present key");
    }

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
        assert_eq!(coordinator.heartbeat_silence_secs, 600);
        assert_eq!(coordinator.heartbeat_silence_secs().expect("the default is nonzero"), 600);
        assert_eq!(coordinator.batch_restart_young_secs, 60);
        assert_eq!(coordinator.batch_restart_addition, 24);
        assert_eq!(coordinator.authority_backend, "github");
        assert!(!coordinator.uses_local_authority());
        assert!(coordinator.single_writer_marker.is_empty());
    }

    #[test]
    fn a_source_replica_boot_refuses_to_start_without_the_writer_marker() {
        // Tripwire: two local-authority coordinators that both have GitHub
        // credentials would both push. The marker is the operator-created file
        // that makes the writer claim explicit; an empty or missing path must
        // refuse the boot rather than start a second writer.
        use std::fs;

        let github = GithubConnectionConfig {
            token: "t".into(),
            owner: "octo".into(),
            repo: "shadow".into(),
            ..GithubConnectionConfig::default()
        };
        let local = CoordinatorConfig {
            authority_backend: "local".into(),
            authority_repo: "/tmp/authority.git".into(),
            ..CoordinatorConfig::default()
        };
        let error = local.require_single_writer_marker(&github).expect_err("no marker");
        let message = error.to_string();
        assert!(message.contains("SINGLE_WRITER_MARKER"), "{message}");
        assert!(message.contains("two writers"), "{message}");

        let dir = tempfile::tempdir().expect("writer-marker fixture dir");
        let writer_file = dir.path().join("writer");
        fs::write(&writer_file, "this host writes the replica\n").expect("writer marker writes");
        let claimed = CoordinatorConfig { single_writer_marker: writer_file.to_string_lossy().into_owned(), ..local };
        claimed.require_single_writer_marker(&github).expect("a present marker is the writer claim");

        assert!(
            CoordinatorConfig::default().require_single_writer_marker(&GithubConnectionConfig::default()).is_ok(),
            "a GitHub-authority boot does not need the marker",
        );
    }

    #[test]
    fn a_zero_heartbeat_silence_is_refused_rather_than_meaning_unbounded() {
        // The plausible bug: treating `0` like `stale_warn_after_secs`, where
        // zero disables the sweep. Silence has no "off" — a missing heartbeat
        // is already deadline-only — so zero would silently disable the
        // recovery this knob exists to perform.
        let coordinator = CoordinatorConfig { heartbeat_silence_secs: 0, ..CoordinatorConfig::default() };
        let error = coordinator.heartbeat_silence_secs().expect_err("zero must not resolve");
        let message = error.to_string();
        assert!(message.contains("AETHER_BLOOMERY_HEARTBEAT_SILENCE_SECS"), "{message}");
        assert!(message.contains("nonzero"), "{message}");
    }
}
