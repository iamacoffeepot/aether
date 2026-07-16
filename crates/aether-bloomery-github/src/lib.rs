//! aether-bloomery-github: the outward projection mirror (ADR-0149 slice 4,
//! [#3459], as amended by [#3460]).
//!
//! The only crate in the workspace permitted to name a GitHub type. It is a
//! plain adapter rlib (no cdylib, no wasm actors, no `aether-data` kinds),
//! statically linked into `aether-bloomery-host` per ADR-0149 §Packaging, and
//! it depends *inward* on the control core ([`aether_bloomery`]) so the
//! static-link DAG stays cycle-free.
//!
//! # Direction is outward
//!
//! GitHub is a **shadow copy of Bloomery's internals** — a carbon-copy
//! tracking surface, never a source of intent. The adapter consumes a
//! self-contained [`ViewDocument`] (the pure projection of the journal that
//! [`aether_bloomery::view_of`] assembles) and projects it: each workpiece to
//! an issue, each bloom to its aggregate umbrella issue, and each piece of
//! evidence to a check-run or comment. Every projection carries the internal
//! Bloomery id plus a content digest in stable metadata (an HTML-comment
//! [`marker`] in issue/comment bodies, the native `external_id` on
//! check-runs), so the projection is **idempotent** — reconciling the same
//! document twice is a no-op — and **rebuildable from the journal** after a
//! deletion: a deleted projection leaves no marker to find, so the reconcile
//! recreates it. The projector reads only its own markers; it never
//! interprets free-form platform content as intent.
//!
//! # Scope of this slice
//!
//! The **git source port** (snapshot / branch namespace / integrate / CAS
//! land) is a **separate sibling slice** — ADR-0149's amendment [#3460] split
//! it out and this crate is the outward projection mirror only. What ships
//! here is the projection backend, its stable-metadata marker, the inward
//! stage-result normalizer (port *shape* only, no ingestion wiring), the thin
//! GitHub HTTP client the projection drives, and a fake GitHub double for
//! token- and network-free tests.
//!
//! [#3459]: https://github.com/iamacoffeepot/aether/issues/3459
//! [#3460]: https://github.com/iamacoffeepot/aether/issues/3460

mod client;
mod config;
mod inward;
mod marker;
mod projection;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use client::{
    CheckConclusion, CheckRun, Comment, GithubApi, GithubError, Issue, NewCheckRun, NewComment, NewIssue, ReqwestGithub,
};
pub use config::GithubConfig;
pub use inward::{InwardError, StageResult, StageVerdict, normalize_stage_result};
pub use marker::{Marker, check_run_external_id, parse_check_run_external_id, parse_marker, render_marker};
pub use projection::GithubProjection;
