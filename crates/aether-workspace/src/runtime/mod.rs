//! The `aether.workspace` runtime half (ADR-0122 split), compiled only under
//! `feature = "runtime"`.
//!
//! Each `Import` and each `Run` runs on a worker thread through ADR-0093's
//! hold-until-resolve dispatch, bounded by one cap-level [`TaskQueue`]: the
//! handler submits the whole sequence and returns at once, the caller's
//! settlement chain stays held until the `#[handler(task)]` completion
//! re-replies the result, and a request past the bound queues rather than
//! being dropped. No dispatcher thread ever blocks on the daemon or the
//! journal.

mod engine;
mod import;
mod journal;
mod run;

#[cfg(all(unix, any(test, feature = "test-support")))]
pub mod testing;

use std::io;
use std::time::Duration;

use aether_actor::runtime;
use aether_bloomery_journal::ArtifactStore;
use aether_bloomery_tar::{Limits, LimitsError, Rules};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, Pending, TaskDone, TaskQueue};
pub use aether_substrate::chassis::error::BootError;

use crate::{DEFAULT_ENDPOINT, Import, ImportResult, Run, RunResult, WorkspaceCapability, WorkspaceConfig};
use engine::{Endpoint, Engine};
use import::Importer;
use run::{Allotment, Runner};

/// Composer-supplied construction params (ADR-0156 §3): the live artifact
/// store of the journal the chassis opened. It is a value the composer holds,
/// not operator-typed config, so it rides `Params`.
///
/// It is `None` only where a chassis is composed to be described and never
/// booted (`--describe` / `--print-config`, ADR-0155); `init` refuses `None`.
pub struct WorkspaceParams {
    pub artifacts: Option<ArtifactStore>,
}

/// `aether.workspace` runtime state: the importer and the runner each request
/// clones onto its worker, and the queue that bounds how many requests talk
/// to the daemon at once.
pub struct WorkspaceCapabilityState {
    importer: Importer,
    runner: Runner,
    tasks: TaskQueue,
}

#[runtime]
impl NativeActor for WorkspaceCapability {
    type State = WorkspaceCapabilityState;

    type Config = WorkspaceConfig;
    type Params = WorkspaceParams;

    const NAMESPACE: &'static str = "aether.workspace";

    /// Check the endpoint, the import bounds, and the run allotment, and take
    /// the journal's store. Nothing dials the daemon here, so an engine boots
    /// without one.
    fn init(
        config: WorkspaceConfig,
        params: WorkspaceParams,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<WorkspaceCapabilityState, BootError> {
        let artifacts = params.artifacts.ok_or_else(|| {
            boot_error("the aether.workspace actor needs the artifact store of the journal the chassis opened")
        })?;
        let endpoint = Endpoint::parse(config.endpoint.as_deref().unwrap_or(DEFAULT_ENDPOINT))
            .map_err(|error| BootError::Other(Box::new(error)))?;
        let import_limits = limits(config.import_max_entries, config.import_max_bytes, "IMPORT")?;
        let allotment = Allotment {
            deadline: Duration::from_millis(at_least_one(config.run_deadline_millis, "RUN_DEADLINE_MILLIS")?),
            memory_bytes: at_least_one(config.memory_limit_bytes, "MEMORY_LIMIT_BYTES")?,
            pids: at_least_one(config.pids_limit, "PIDS_LIMIT")?,
            output: limits(config.output_max_entries, config.output_max_bytes, "OUTPUT")?,
        };

        tracing::info!(
            target: "aether_workspace",
            %endpoint,
            max_in_flight = config.max_in_flight,
            import_max_entries = config.import_max_entries,
            import_max_bytes = config.import_max_bytes,
            run_deadline_millis = config.run_deadline_millis,
            memory_limit_bytes = config.memory_limit_bytes,
            pids_limit = config.pids_limit,
            output_max_entries = config.output_max_entries,
            output_max_bytes = config.output_max_bytes,
            "workspace actor configured",
        );
        let engine = Engine::new(endpoint);
        Ok(WorkspaceCapabilityState {
            importer: Importer {
                engine: engine.clone(),
                artifacts: artifacts.clone(),
                rules: Rules::userland(import_limits),
            },
            runner: Runner { engine, artifacts, allotment },
            tasks: TaskQueue::new(config.max_in_flight),
        })
    }

    /// Import a digest-pinned image into a tree in the journal.
    ///
    /// # Agent
    /// Reply: `import_result`. Pulls the image through the Docker Engine API,
    /// decodes its exported filesystem under the userland rules, and answers
    /// `Ok { tree }`, or `Failed { detail }` with no rows committed and no
    /// container left behind. The reply lands when the whole import is done.
    #[handler::single]
    fn on_import(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: Import) -> Pending<ImportResult> {
        let importer = state.importer.clone();
        state.tasks.submit(ctx, move || importer.answer(&mail.image))
    }

    /// ADR-0093 completion: re-reply the worker's result to the original
    /// caller, then free the slot for the next queued request.
    #[handler(task)]
    fn on_import_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<ImportResult>) {
        done.resolve(ctx);
        state.tasks.on_complete(ctx);
    }

    /// Run steps over a stored tree in a stored environment.
    ///
    /// # Agent
    /// Reply: `run_result`. Runs each step in its own container over the
    /// tree at `/work`, under the sandbox pins and the fixed allotment, and
    /// answers `Ok(Outcome)` with each step's exit code and stored stdout and
    /// stderr and the output tree minus `scratch`; `Refused` when the run
    /// cannot start as asked; `Exhausted(Time | Memory)` when a step outran
    /// the allotment; or `Failed { detail }` when the executor failed. No
    /// container or volume is left behind, and rows commit only for `Ok`.
    /// The reply lands when the whole run is done.
    #[handler::single]
    fn on_run(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: Run) -> Pending<RunResult> {
        let runner = state.runner.clone();
        state.tasks.submit(ctx, move || runner.answer(&mail))
    }

    /// ADR-0093 completion: re-reply the run's result to the original caller,
    /// then free the slot for the next queued request.
    #[handler(task)]
    fn on_run_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<RunResult>) {
        done.resolve(ctx);
        state.tasks.on_complete(ctx);
    }
}

/// Decode bounds from a pair of knobs, refusing boot naming a zero one.
fn limits(entries: u32, bytes: u64, knob: &str) -> Result<Limits, BootError> {
    Limits::new(entries, bytes).map_err(|error| {
        let key = match error {
            LimitsError::ZeroEntries => "MAX_ENTRIES",
            LimitsError::ZeroBytes => "MAX_BYTES",
        };
        boot_error(&format!("AETHER_WORKSPACE_{knob}_{key} must be at least 1: {error}"))
    })
}

/// A knob that must not be zero, refusing boot naming it.
fn at_least_one<T: PartialEq + Default>(value: T, knob: &str) -> Result<T, BootError> {
    if value == T::default() {
        Err(boot_error(&format!("AETHER_WORKSPACE_{knob} must be at least 1")))
    } else {
        Ok(value)
    }
}

fn boot_error(message: &str) -> BootError {
    BootError::Other(Box::new(io::Error::other(message.to_owned())))
}
