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
//! GitHub is a **shadow copy of Bloomery's internals** — a carbon-copy
//! tracking surface, never a source of intent. The adapter consumes a
//! self-contained [`ViewDocument`](aether_bloomery::ViewDocument) (the pure
//! projection of the journal that [`aether_bloomery::view_of`] assembles) and
//! projects it: each workpiece to an issue, each bloom to its aggregate
//! umbrella issue, and evidence to comments. Every projection carries the internal
//! Bloomery id plus a content digest in stable metadata (an HTML-comment
//! [`Marker`] in issue/comment bodies, the native `external_id` on
//! check-runs), so the projection is **idempotent** — reconciling the same
//! document twice is a no-op — and **rebuildable from the journal** after a
//! deletion: a deleted projection leaves no marker to find, so the reconcile
//! recreates it. The projector reads only its own markers; it never
//! interprets free-form platform content as intent.
//!
//! # Scope of this slice
//!
//! What ships here has grown one port per sibling slice, each adapter over the
//! same thin HTTP client and fake double: the outward **projection mirror**
//! (issues / comments, [#3459]) with its stable-metadata marker and the inward
//! stage-result normalizer; the **git source port** (snapshot / branch
//! namespace / integrate / CAS land, [#3465]) over the Git Data API; and the
//! **Actions executor port** ([`ActionsExecutor`], migration step 2 [#3500]) —
//! the four-message [`ExecutorBackend`](aether_bloomery::ExecutorBackend) that
//! dispatches a resolved work order by `workflow_dispatch` and maps
//! inspect / cancel / stream-evidence onto the Actions run + artifacts API. The
//! fake GitHub double models each surface for token- and network-free tests.
//!
//! [#3465]: https://github.com/iamacoffeepot/aether/issues/3465
//! [#3500]: https://github.com/iamacoffeepot/aether/issues/3500
//!
//! [#3459]: https://github.com/iamacoffeepot/aether/issues/3459
//! [#3460]: https://github.com/iamacoffeepot/aether/issues/3460

mod client;
mod config;
mod correspondence;
mod executor;
mod inward;
mod marker;
mod projection;
mod source;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use client::{
    ActionsApi, Artifact, CheckConclusion, CheckRun, Comment, GitCommit, GitDataApi, GitRef, GithubApi, GithubError,
    HttpRequest, HttpResponse, HttpTransport, InstallationToken, Issue, Method, NewCheckRun, NewComment, NewIssue,
    ReqwestGithub, ReqwestTransport, RunConclusion, RunStatus, StaticTokenSource, TokenSource, WorkflowRun,
};
pub use config::GithubConfig;
pub use correspondence::{Correspondence, CorrespondenceError, GitObjectFormat, GitObjectId};
pub use executor::{ActionsExecutor, ExecutorError};
pub use inward::{
    InwardError, StageResult, StageVerdict, StudyRecordError, StudyResult, normalize_stage_result,
    normalize_study_result, parse_study_cost,
};
pub use marker::{Marker, check_run_external_id, parse_check_run_external_id, parse_marker, render_marker};
pub use projection::GithubProjection;
pub use source::{GitSource, SharedCorrespondence, SourceError};
