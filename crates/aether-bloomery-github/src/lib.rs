//! aether-bloomery-github: the outward projection mirror (ADR-0149 slice 4,
//! [#3459], as amended by [#3460]).
//!
//! The only crate in the workspace permitted to name a GitHub type. It is a
//! plain adapter rlib (no cdylib, no wasm actors, no `aether-data` kinds),
//! statically linked into `aether-chassis-bloomery` per ADR-0149 §Packaging, and
//! it depends *inward* on the control core ([`aether_bloomery`]) so the
//! static-link DAG stays cycle-free.
//!
//! # Direction is outward
//!
//! GitHub is a **tracking surface for Bloomery's internals**, never a source of
//! intent. The adapter consumes a self-contained
//! [`ViewDocument`](aether_bloomery::ViewDocument) (the pure projection of the
//! journal that [`aether_bloomery::view_of`] assembles) and mirrors it onto the
//! objects the repository already holds: each member becomes one marker-keyed
//! comment on the issue its workpiece addresses, and a landing receipt becomes
//! one comment per member issue plus one on the landing pull request. No object
//! is ever opened, closed, retitled, or rewritten — the projection owns its own
//! comments and nothing else (ADR-0149, amended by [#4663]). Every projection
//! carries the internal Bloomery id plus a content digest in stable metadata (an
//! HTML-comment [`Marker`] in comment bodies, the native `external_id` on
//! check-runs), so the projection is **idempotent** — reconciling the same
//! document twice is a no-op — and **rebuildable from the journal** after a
//! deletion: a deleted comment leaves no marker to find, so the reconcile
//! recreates it. The projector reads only its own markers; it never
//! interprets free-form platform content as intent.
//!
//! # Scope of this slice
//!
//! What ships here has grown one port per sibling slice, each adapter over the
//! same thin HTTP client and fake double: the outward **projection mirror**
//! (marker-keyed comments, [#3459]) with its stable-metadata marker and the
//! inward stage-result normalizer; the **git source port** (snapshot / branch
//! namespace / integrate / CAS land, [#3465]) over the Git Data API; and the
//! **Actions executor port** ([`ActionsExecutor`], migration step 2 [#3500]) —
//! the four-message [`ExecutorBackend`](aether_bloomery::ExecutorBackend) that
//! dispatches a resolved work order by `workflow_dispatch` and maps
//! inspect / cancel / stream-evidence onto the Actions run + artifacts API. The
//! fake GitHub double models each surface for token- and network-free tests.
//!
//! Authentication is a [`TokenSource`] every port's client resolves its bearer
//! from, and both implementations live here because both speak GitHub: the
//! static PAT ([`StaticTokenSource`]) and the GitHub-App installation-token
//! minter ([`AppTokenSource`], migration step 3). The embedder reads the App's
//! host-local private key and hands the bytes in, so the key's custody stays on
//! the host (ADR-0150) while the JWT signing and token exchange stay here.
//!
//! [#3465]: https://github.com/iamacoffeepot/aether/issues/3465
//! [#3500]: https://github.com/iamacoffeepot/aether/issues/3500
//!
//! [#3459]: https://github.com/iamacoffeepot/aether/issues/3459
//! [#3460]: https://github.com/iamacoffeepot/aether/issues/3460
//! [#4663]: https://github.com/iamacoffeepot/aether/issues/4663

mod app_auth;
mod client;
mod config;
mod executor;
mod inward;
mod projection;

// Re-export the moved modules so in-crate paths (`crate::source::…`,
// `crate::correspondence::…`, `crate::marker::…`, `crate::mainline::…`) and
// the in-process fake stay stable after the extraction.
#[cfg(any(test, feature = "testing"))]
pub use aether_bloomery_git::testing;
pub use aether_bloomery_git::{correspondence, mainline, marker, source};

pub use aether_bloomery_git::{
    ActionsApi, Artifact, ChecksState, Comment, GitCommit, GitDataApi, GitObjectFormat, GitObjectId, GitRef, GitSource,
    GithubApi, GithubError, IssueStateApi, LandAcceptance, LandingProposal, LandingRefusal, LandingSource, MainlineRef,
    Marker, MergeResult, NewComment, NewPullRequest, PullMergeResult, PullRequest, PullRequestApi, PullRequestState,
    RunConclusion, RunStatus, SourceError, WorkflowRun, candidate_ref_name, check_run_external_id, landing_branch,
    landing_floor_title, member_checkpoint_ref_name, parse_check_run_external_id, parse_marker, render_marker,
    short_hex, strip_heads, to_hex,
};
pub use app_auth::{AppTokenSource, InstallationTokenExchange};
pub use client::{
    CheckConclusion, CheckRun, HttpRequest, HttpResponse, HttpTransport, InstallationToken, Method, NewCheckRun,
    ReqwestGithub, ReqwestTransport, StaticTokenSource, TokenSource,
};
pub use config::GithubConfig;
pub use executor::{ActionsExecutor, ExecutorError, LaneWorkflows};
pub use inward::{
    InwardError, StageResult, StageVerdict, StudyRecordError, StudyResult, normalize_stage_result,
    normalize_study_result, parse_study, parse_study_cost,
};
pub use projection::{GithubProjection, canonical_issue_number};
