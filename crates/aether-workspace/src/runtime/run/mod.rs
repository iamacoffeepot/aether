//! The run sequence (ADR-0237 decisions 2, 4, and 8, as amended): steps over
//! a stored tree in a stored environment, each in its own container, run on
//! the actor's worker thread.
//!
//! 1. [`resolve`] loads the environment and every input from one artifact
//!    batch, checks the tree's `rust-toolchain.toml` against what the
//!    environment provides, resolves each step's tool to an executable in the
//!    root, and compares the environment's platform with the daemon's.
//! 2. [`environment`] makes sure the daemon holds the environment's image,
//!    importing the root as a filesystem tar when it does not.
//! 3. [`volumes`] creates the `/work` volume and one volume per mount, and
//!    writes each mount's tree through a helper container that never starts.
//! 4. [`step`] runs each step in its own container over the shared `/work`
//!    volume, under the sandbox pins and the run's [`Allotment`], which the
//!    actor's provisioning chose; the run tree is written into the first
//!    step's container before it starts. Each step's peak memory is sampled
//!    while it runs. The steps stop after the first non-zero exit.
//! 5. [`output`] decodes the last container's `/work` into a tree, minus
//!    `scratch`.
//! 6. [`cleanup`] removes every container and volume on every path; then the
//!    batch commits, before the reply, only for an `Ok`.
//!
//! [`answer`] is the one place a sequence's end becomes a [`RunResult`]: an
//! executor failure after the request was accepted is `Failed { detail }`,
//! never a refusal. The detail is [`RunError::cause`]: the failed call or the
//! in-tree path and the class of failure, never a host path, a socket, or the
//! daemon's words, because the driver records it; the log keeps the full
//! text. Beside the result, [`Runner::answer`] hands back what it
//! observed of the run — its peak memory and wall time — for the estimate,
//! which never reaches the result.

mod cleanup;
mod environment;
mod output;
mod resolve;
mod step;
mod volumes;

#[cfg(all(test, unix))]
mod tests;

use std::error::Error;
use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use aether_bloomery_journal::{AppendError, ArtifactBatch, ArtifactStore, GetError, JournalError};
use aether_bloomery_kinds::{Detail, Ref, Tree};
use aether_bloomery_tar::{DecodeError, EncodeError, Limits, encode};

use super::engine::{ContainerId, Engine, EngineError, UploadError};
use super::journal::{JournalSource, SourceError};
use super::provision::CpuSet;
use crate::{Outcome, Refusal, Resource, Run, RunResult};
use cleanup::{Cleanup, CleanupError};
use resolve::Resolved;
use volumes::Volumes;

/// The deadline a configured allotment too large for the clock stands in
/// for: a century.
const FAR_FUTURE: Duration = Duration::from_hours(100 * 365 * 24);

/// What one run is given: the cores its containers are pinned to, each
/// step's memory, and the deadline its steps share. Chosen per run at
/// admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allotment {
    /// The pinned cores (`CpusetCpus`, and `NanoCpus` as their count).
    pub(crate) cpus: CpuSet,
    /// Each step's memory, swap included.
    pub(crate) memory_bytes: u64,
    /// How long a run's steps may take in total, from the first step's
    /// container create to the last step's exit.
    pub(crate) deadline: Duration,
}

/// Everything one run needs, cloned onto the worker thread per request, and
/// the limits that are the same for every run.
#[derive(Clone)]
pub struct Runner {
    pub engine: Engine,
    pub artifacts: ArtifactStore,
    /// Each step's process and thread count.
    pub pids: u32,
    /// The bounds the output `/work` decodes under.
    pub output: Limits,
}

/// A run's answer and what was observed of it.
#[derive(Debug, Clone)]
pub struct Ran {
    pub result: RunResult,
    pub observed: Observed,
}

/// What a run was seen to use, for the estimate only. Each is `None` when
/// it was not observed: the run stopped before its steps finished, or
/// sampling failed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Observed {
    /// The most any step's container held, less its reclaimable file cache.
    pub peak_memory_bytes: Option<u64>,
    /// From the first step's container create to the last step's exit: the
    /// window the deadline covers.
    pub wall: Option<Duration>,
}

impl Runner {
    /// Run the sequence under `allotment` and answer it.
    pub fn answer(&self, run: &Run, allotment: &Allotment) -> Ran {
        match self.sequence(run, allotment) {
            Ok((outcome, observed)) => Ran { result: answer(Ok(outcome)), observed },
            Err(stop) => Ran { result: answer(Err(stop)), observed: Observed::default() },
        }
    }

    /// Resolve, run in the daemon, clean up, then commit.
    fn sequence(&self, run: &Run, allotment: &Allotment) -> Result<(Outcome, Observed), Stop> {
        let mut batch =
            self.artifacts.batch().map_err(|error| RunError::Journal { during: "opening a batch", error })?;
        let resolved = resolve::resolve(&batch, run)?;
        resolve::platform(&self.engine, &resolved.environment.platform)?;

        let mut cleanup = Cleanup::new(&self.engine);
        let ran = self.in_daemon(&mut batch, run, &resolved, allotment, &mut cleanup);
        let finished = settle(ran, cleanup.finish())?;
        batch.commit().map_err(RunError::Commit)?;
        Ok(finished)
    }

    /// Everything that creates daemon objects, each registered with
    /// `cleanup` as soon as it exists. Beside the outcome, what was observed
    /// of the steps that ran.
    fn in_daemon(
        &self,
        batch: &mut ArtifactBatch,
        run: &Run,
        resolved: &Resolved,
        allotment: &Allotment,
        cleanup: &mut Cleanup<'_>,
    ) -> Result<(Outcome, Observed), Stop> {
        let image = environment::ensure(&self.engine, batch, &run.environment, &resolved.environment.root)?;
        let volumes = volumes::prepare(&self.engine, cleanup, batch, &image, &run.mounts)?;
        let sandbox = step::Sandbox {
            image: &image,
            volumes: &volumes,
            scratch: &run.scratch,
            network: run.network,
            allotment,
            pids: self.pids,
            base_env: &resolved.environment.env,
        };

        let started = Instant::now();
        let deadline = started.checked_add(allotment.deadline).unwrap_or(started + FAR_FUTURE);
        let mut steps = Vec::with_capacity(run.steps.as_slice().len());
        let mut last: Option<ContainerId> = None;
        let mut peak_memory_bytes = None;
        let mut exited = started;
        for (step, tool) in run.steps.as_slice().iter().zip(&resolved.tools) {
            let container = step::create(&self.engine, cleanup, &sandbox, tool, step)?;
            if last.is_none() {
                write_tree(&self.engine, batch, &container, Volumes::WORK_PATH, &run.tree)?;
            }
            let ran = step::run(&self.engine, batch, &container, step, tool, deadline)?;
            let exited_zero = ran.outcome.exit_code == Some(0);
            peak_memory_bytes = peak_memory_bytes.max(ran.peak_memory_bytes);
            exited = ran.exited;
            steps.push(ran.outcome);
            last = Some(container);
            if !exited_zero {
                break;
            }
        }
        let last = last.ok_or_else(|| RunError::Shape("a run with no steps reached the daemon".to_owned()))?;
        let observed = Observed { peak_memory_bytes, wall: Some(exited.saturating_duration_since(started)) };

        let tree = output::collect(&self.engine, batch, &last, &run.scratch, self.output)?;
        Ok((Outcome { steps, tree }, observed))
    }
}

/// Map a sequence's end onto the reply. The only place a [`RunError`] becomes
/// `Failed`: the reply carries its host-free [`RunError::cause`], and its full
/// text is logged.
fn answer(ended: Result<Outcome, Stop>) -> RunResult {
    match ended {
        Ok(outcome) => {
            tracing::info!(target: "aether_workspace", tree = %outcome.tree.digest(), steps = outcome.steps.len(), "ran");
            RunResult::Ok(outcome)
        }
        Err(Stop::Refused(refusal)) => {
            tracing::info!(target: "aether_workspace", ?refusal, "run refused");
            RunResult::Refused(*refusal)
        }
        Err(Stop::Exhausted(resource)) => {
            tracing::info!(target: "aether_workspace", ?resource, "run exhausted its allotment");
            RunResult::Exhausted(resource)
        }
        Err(Stop::Failed(error)) => {
            tracing::warn!(target: "aether_workspace", %error, "run failed");
            RunResult::Failed { detail: RunError::cause(&error) }
        }
    }
}

/// Fold the cleanup's result into the run's: a failed removal fails the run,
/// naming what came before it, so every answer but `Failed` means nothing
/// was left behind as far as the daemon reports.
fn settle<T>(ran: Result<T, Stop>, removed: Result<(), CleanupError>) -> Result<T, Stop> {
    match (ran, removed) {
        (ran, Ok(())) => ran,
        (Ok(_), Err(error)) => Err(Stop::failed(RunError::Cleanup { error, after: None })),
        (Err(stop), Err(error)) => Err(Stop::failed(RunError::Cleanup { error, after: Some(Box::new(stop)) })),
    }
}

/// Stream the stored tree `tree` into the container at the absolute `path`.
fn write_tree(
    engine: &Engine,
    batch: &ArtifactBatch,
    container: &ContainerId,
    path: &str,
    tree: &Ref<Tree>,
) -> Result<(), Stop> {
    engine
        .put_archive(container, path, |out| encode(tree, &mut JournalSource::new(batch), out))
        .map_err(|error| upload_stop(format!("writing {path} into container {container}"), error))
}

/// Why a run ended without an [`Outcome`]. Both boxed arms are cold paths.
#[derive(Debug)]
pub enum Stop {
    Refused(Box<Refusal>),
    Exhausted(Resource),
    Failed(Box<RunError>),
}

impl Stop {
    fn refused(refusal: Refusal) -> Self {
        Self::Refused(Box::new(refusal))
    }

    fn failed(error: RunError) -> Self {
        Self::Failed(Box::new(error))
    }

    /// How the run ended, with an executor failure rendered as its
    /// [`RunError::cause`] rather than its full text.
    fn cause(&self) -> String {
        match self {
            Self::Refused(_) | Self::Exhausted(_) => self.to_string(),
            Self::Failed(error) => error.cause_text(),
        }
    }
}

impl From<RunError> for Stop {
    fn from(error: RunError) -> Self {
        Self::failed(error)
    }
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => write!(f, "the run was refused ({refusal:?})"),
            Self::Exhausted(Resource::Time) => f.write_str("the run passed its deadline"),
            Self::Exhausted(Resource::Memory) => f.write_str("a step ran out of memory"),
            Self::Failed(error) => error.fmt(f),
        }
    }
}

/// An executor failure during a run: the reason a run answers `Failed`. Its
/// text is the reply's [`Detail`], naming the call or the path.
#[derive(Debug)]
pub enum RunError {
    /// An Engine API call failed; `call` names it.
    Engine { call: String, error: EngineError },
    /// Streaming a stored tree into the daemon failed for a reason other than
    /// a missing artifact.
    Upload { call: String, error: EncodeError<SourceError> },
    /// A journal batch operation failed.
    Journal { during: &'static str, error: JournalError },
    /// A stored artifact did not load.
    Load { during: String, error: GetError },
    /// A stored blob did not read back whole.
    Read { during: String, error: io::Error },
    /// Reading a step's log stream failed.
    Logs { call: String, error: io::Error },
    /// The output `/work` is not a tree under the canonical rules and the
    /// output bounds, or did not store.
    Output(DecodeError<JournalError>),
    /// Something the daemon or a tree answered is not the shape the run
    /// needs.
    Shape(String),
    /// The batch did not commit.
    Commit(AppendError),
    /// A container or volume could not be removed; `after` is how the run
    /// ended before that, if it had already ended.
    Cleanup { error: CleanupError, after: Option<Box<Stop>> },
}

impl RunError {
    /// The reply's detail: the failed call or step and the in-tree path, with
    /// each wrapped error replaced by its class. It never carries a host
    /// path, a socket, a host name, or the daemon's words, because the driver
    /// records it; [`fmt::Display`] keeps the full text for the log.
    pub fn cause(&self) -> Detail {
        Detail::new(self.cause_text())
    }

    fn cause_text(&self) -> String {
        match self {
            Self::Engine { call, error } => format!("{call}: {}", error.cause()),
            Self::Upload { call, error } => format!("{call}: {}", upload_cause(error)),
            Self::Journal { during, .. } => format!("{during}: the journal failed"),
            Self::Load { during, error } => format!("{during}: {}", load_cause(error)),
            Self::Read { during, error } => format!("{during}: the stored blob did not read back ({})", error.kind()),
            Self::Logs { call, error } => format!("{call}: the log stream failed ({})", error.kind()),
            Self::Output(error) => format!("decoding the output /work: {}", output_cause(error)),
            Self::Shape(detail) => detail.clone(),
            Self::Commit(_) => "committing the run's artifacts: the journal failed".to_owned(),
            Self::Cleanup { error, after: None } => format!("cleaning up: {}", error.cause()),
            Self::Cleanup { error, after: Some(after) } => {
                format!("{}; cleaning up also failed: {}", after.cause(), error.cause())
            }
        }
    }
}

/// A failed tree upload's class: which side broke, never the journal's or the
/// transport's words.
fn upload_cause(error: &EncodeError<SourceError>) -> String {
    match error {
        EncodeError::Source(SourceError::Missing(digest)) => format!("the journal stores no artifact {digest}"),
        EncodeError::Source(SourceError::Get(_)) => "loading a tree: the journal failed".to_owned(),
        EncodeError::Source(SourceError::Journal(_)) => "opening a blob: the journal failed".to_owned(),
        EncodeError::Write(error) => format!("writing the archive failed ({})", error.kind()),
        EncodeError::BlobRead(error) => format!("reading a blob failed ({})", error.kind()),
        EncodeError::BlobLength { .. } | EncodeError::TooDeep => error.to_string(),
    }
}

/// A failed artifact load's class.
fn load_cause(error: &GetError) -> &'static str {
    match error {
        GetError::Journal(_) => "the journal failed",
        GetError::Decode(_) => "the stored artifact did not decode",
        GetError::PrefixMismatch { .. } => "the stored artifact is another kind",
    }
}

/// A failed output decode's class. A refused entry keeps its in-tree path and
/// the tree rule it broke.
fn output_cause(error: &DecodeError<JournalError>) -> String {
    match error {
        DecodeError::Sink(_) => "storing it in the journal failed".to_owned(),
        DecodeError::Read(error) => format!("reading the archive failed ({})", error.kind()),
        DecodeError::Refused { .. } => error.to_string(),
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine { call, error } => write!(f, "{call}: {error}"),
            Self::Upload { call, error } => write!(f, "{call}: {error}"),
            Self::Journal { during, error } => write!(f, "{during}: {error}"),
            Self::Load { during, error } => write!(f, "{during}: {error}"),
            Self::Read { during, error } => write!(f, "{during}: {error}"),
            Self::Logs { call, error } => write!(f, "{call}: {error}"),
            Self::Output(error) => write!(f, "decoding the output /work: {error}"),
            Self::Shape(detail) => f.write_str(detail),
            Self::Commit(error) => write!(f, "committing the run's artifacts: {error}"),
            Self::Cleanup { error, after: None } => write!(f, "cleaning up: {error}"),
            Self::Cleanup { error, after: Some(after) } => write!(f, "{after}; cleaning up also failed: {error}"),
        }
    }
}

impl Error for RunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Engine { error, .. } => Some(error),
            Self::Upload { error, .. } => Some(error),
            Self::Journal { error, .. } => Some(error),
            Self::Load { error, .. } => Some(error),
            Self::Read { error, .. } | Self::Logs { error, .. } => Some(error),
            Self::Output(error) => Some(error),
            Self::Commit(error) => Some(error),
            Self::Cleanup { error, .. } => Some(error),
            Self::Shape(_) => None,
        }
    }
}

/// Name the failed Engine API call.
fn engine_failed(call: impl fmt::Display) -> impl FnOnce(EngineError) -> RunError {
    move |error| RunError::Engine { call: call.to_string(), error }
}

/// Map a failed tree upload: an artifact the journal lacks is the input's
/// fault (`InputMissing`); anything else is the executor's.
fn upload_stop(call: String, error: UploadError<EncodeError<SourceError>>) -> Stop {
    match error {
        UploadError::Engine(error) => Stop::failed(RunError::Engine { call, error }),
        UploadError::Body(EncodeError::Source(SourceError::Missing(digest))) => {
            Stop::refused(Refusal::InputMissing(digest))
        }
        UploadError::Body(error) => Stop::failed(RunError::Upload { call, error }),
    }
}
