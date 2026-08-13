//! The local-process executor-port backend (ADR-0149 §The boundary, ADR-0150
//! credentials-never-leave-the-machine — issue #3586).
//!
//! The Actions backend ([`ActionsExecutor`](aether_bloomery_github::ActionsExecutor))
//! dispatches a work order at a wrapper workflow on a shared runner. That is
//! correct for the zero-secret mechanical verify lanes, but the model-driven
//! `construct.implement` lane runs headless Claude, whose non-delegable
//! subscription credential ADR-0150 forbids from ever living in a GitHub secret
//! store. This backend reproduces on the operator's own machine exactly what the
//! wrapper does on a runner — materialize the order's checkout into a scratch
//! worktree and spawn the same `cargo xtask transform <command>` entrypoint under
//! ambient local `claude` auth — so the model lane needs no fork and no secret.
//!
//! # The nonce is the handle, the output dir is the evidence
//!
//! Like the Actions backend, `submit` returns the order's [`Nonce`](aether_bloomery::Nonce)
//! as the handle and the other three messages resolve the run from it — here
//! through an in-process registry the backend owns rather than a
//! `workflow_dispatch` resolution. Because the backend owns the run's output
//! directory directly, it synthesizes the nonce-tagged
//! [`EvidenceRef`](aether_bloomery::EvidenceRef) name from the run's
//! `evidence.json` itself (`attempt.<verdict>.<subject_hex>.<detail_hex>.<nonce>`,
//! the [`attempt_artifact_name`](crate::bloomery::intake::attempt_artifact_name)
//! contract the intake path's
//! [`NameEvidenceClaims`](crate::bloomery::intake::NameEvidenceClaims) decodes) —
//! no artifact-upload naming step to depend on.
//!
//! # The lane ceiling, and the slot a dispatch runs in
//!
//! A submit accepts a dispatch; it does not promise to spawn it immediately.
//! Each lane is a whole cargo build with its own throwaway target dir, and a seal
//! fans out one dispatch per member, so the backend runs at most
//! [`with_max_concurrent_lanes`](LocalExecutor::with_max_concurrent_lanes)
//! children at once and holds the rest in submission order, starting each as a
//! running lane finishes. Every dispatch is acked as submitted either way — the
//! ceiling is a queue, never a refusal, so nothing about it reaches the reducer.
//!
//! The ceiling is also what names the checkouts. A dispatch holds a **lane
//! slot** — the lowest index free when it starts — and builds in that slot's
//! canonical checkout, `<base>/slot-<index>`, which every later dispatch in the
//! same slot builds in too. That is the whole point of the arrangement (#4904):
//! `sccache` keys a compilation partly by the paths cargo names on the `rustc`
//! invocation, so a per-dispatch checkout misses the cache on its entire
//! dependency tree while a per-slot one hits everything an earlier lane in that
//! slot already compiled. A host's builds therefore live in `0..ceiling` paths
//! forever, and what a terminal run frees is the slot, not the tree: the next
//! dispatch to take it resets the checkout to its own subject before building.
//! Evidence stays per dispatch (`<base>/<nonce>-evidence`) — it is what one
//! attempt produced — and carries the slot the dispatch ran in, so a coordinator
//! restart can re-adopt a run without handing its build path to a stranger.
//!
//! The slot owns its cargo build directory too (`<base>/slot-<index>-target`,
//! #4912), exported to the lane and every verify gate under it as
//! `CARGO_TARGET_DIR`. One directory per slot rather than one for the host:
//! cargo locks a build directory exclusively, so lanes sharing one build in turn
//! however many slots are free, and its state is keyed by source path, so a
//! shared directory accumulates a fresh artifact set per checkout path and can
//! surface another slot's source as a compile failure. It is the checkout's
//! *sibling*, never a directory in it — the reset each dispatch runs removes
//! ignored files, and an in-tree build directory would be deleted every lap.
//!
//! # The spawn seam
//!
//! The git-checkout + `cargo xtask` shell-out is behind the [`TransformRunner`]
//! trait so the backend's registry / lifecycle / evidence-synthesis logic is
//! unit-testable against a stub that writes a canned output dir, without a real
//! git repo or a Claude credential. Production mounts [`ProcessTransformRunner`].
//!
//! The module splits along those seams: the [`error`] type, the [`runner`] spawn
//! contract, its production [`process_runner`] implementation, the [`lane_env`]
//! policy for what that spawn may hand down, the [`lane_program`] policy for
//! which program it spawns, the [`mock_lane`] stand-in a lane-boundary scenario
//! points that policy at, the [`orphan`] stand-in for a child inherited across a
//! coordinator restart, and the [`backend`] registry +
//! [`ExecutorBackend`](aether_bloomery::ExecutorBackend) impl over them.

mod backend;
mod error;
mod lane_env;
mod lane_program;
pub mod mock_lane;
mod orphan;
mod process_runner;
mod runner;

pub use backend::LocalExecutor;
pub use error::LocalExecutorError;
pub use lane_program::{DEFAULT_LANE_PROGRAM, LaneProgram};
pub use orphan::OrphanedRun;
pub use process_runner::{CaptureIdentity, ProcessTransformRunner};
pub use runner::{CapturedObjects, RunLifecycle, RunProcess, RunSpec, TransformRunner};

#[cfg(test)]
pub mod testing;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
