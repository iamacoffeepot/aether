//! The `aether.workspace` runtime half (ADR-0122 split), compiled only under
//! `feature = "runtime"`.
//!
//! Each `Import` and each `Run` runs on a worker thread through ADR-0093's
//! hold-until-resolve dispatch: the handler submits the whole sequence and
//! returns at once, the caller's settlement chain stays held until the
//! `#[handler(task)]` completion re-replies the result, and a request that
//! cannot start yet queues rather than being dropped. Imports are bounded by
//! a cap-level [`TaskQueue`] counting requests; runs are provisioned
//! (ADR-0237 decision 9) and admitted in FIFO order against the host budget
//! by [`provision::RunQueue`]. No dispatcher thread ever blocks on the
//! daemon, the journal, or the budget.

mod engine;
mod import;
mod journal;
mod provision;
mod run;

#[cfg(all(unix, any(test, feature = "test-support")))]
pub mod testing;

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

use aether_actor::runtime;
use aether_bloomery_journal::ArtifactStore;
use aether_bloomery_tar::{Limits, LimitsError, Rules};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, Pending, TaskDone, TaskQueue};
pub use aether_substrate::chassis::error::BootError;

use crate::{Import, ImportResult, Run, RunResult, WorkspaceCapability, WorkspaceConfig};
use engine::{Endpoint, Engine};
use import::Importer;
use provision::{Admitted, Amounts, Budget, CpuSet, Estimates, Headroom, RunQueue};
use run::{Ran, Runner};

/// Composer-supplied construction params (ADR-0156 §3): the live artifact
/// store of the journal the chassis opened. It is a value the composer holds,
/// not operator-typed config, so it rides `Params`.
///
/// It is `None` only where a chassis is composed to be described and never
/// booted (`--describe` / `--print-config`, ADR-0155); `init` refuses `None`.
pub struct WorkspaceParams {
    pub artifacts: Option<ArtifactStore>,
}

/// `aether.workspace` runtime state: the importer each import clones onto its
/// worker and the queue that bounds how many imports talk to the daemon at
/// once, and the run queue that provisions and admits every run.
pub struct WorkspaceCapabilityState {
    importer: Importer,
    imports: TaskQueue,
    runs: RunQueue,
}

#[runtime]
impl NativeActor for WorkspaceCapability {
    type State = WorkspaceCapabilityState;

    type Config = WorkspaceConfig;
    type Params = WorkspaceParams;

    const NAMESPACE: &'static str = "aether.workspace";

    /// Check the endpoint, the import bounds, the host budget, the default
    /// allotment, and the fixed limits, and take the journal's store. A
    /// `tcp://` endpoint's TLS files are read here, once. Nothing dials the
    /// daemon here, so an engine boots without one.
    fn init(
        config: WorkspaceConfig,
        params: WorkspaceParams,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<WorkspaceCapabilityState, BootError> {
        let artifacts = params.artifacts.ok_or_else(|| {
            boot_error("the aether.workspace actor needs the artifact store of the journal the chassis opened")
        })?;
        let endpoint = Endpoint::from_config(&config).map_err(|error| BootError::Other(Box::new(error)))?;
        let import_limits = limits(config.import_max_entries, config.import_max_bytes, "IMPORT")?;
        let cpuset = CpuSet::parse(&config.cpuset).map_err(|error| {
            boot_error(&format!("AETHER_WORKSPACE_CPUSET={} is not a cpuset list: {error}", config.cpuset))
        })?;
        let budget_memory_bytes = non_zero_bytes(config.budget_memory_bytes, "BUDGET_MEMORY_BYTES")?;
        let defaults = Amounts {
            cores: NonZeroU32::new(config.run_cores).ok_or_else(|| must_be_positive("RUN_CORES"))?,
            memory_bytes: non_zero_bytes(config.default_memory_bytes, "DEFAULT_MEMORY_BYTES")?,
            deadline: Duration::from_millis(at_least_one(config.default_deadline_millis, "DEFAULT_DEADLINE_MILLIS")?),
        };
        let ceiling = Amounts {
            cores: cpuset.count(),
            memory_bytes: budget_memory_bytes,
            deadline: Duration::from_millis(at_least_one(config.max_deadline_millis, "MAX_DEADLINE_MILLIS")?),
        };
        let headroom = Headroom::new(config.headroom_percent).map_err(|error| {
            boot_error(&format!("AETHER_WORKSPACE_HEADROOM_PERCENT={} is refused: {error}", config.headroom_percent))
        })?;
        let pids = at_least_one(config.pids_limit, "PIDS_LIMIT")?;
        let output = limits(config.output_max_entries, config.output_max_bytes, "OUTPUT")?;

        tracing::info!(
            target: "aether_workspace",
            %endpoint,
            tls = matches!(endpoint, Endpoint::Tcp(_)),
            tls_ca_file = config.tls_ca_file.as_deref(),
            tls_cert_file = config.tls_cert_file.as_deref(),
            tls_key_file = config.tls_key_file.as_deref(),
            max_in_flight = config.max_in_flight,
            import_max_entries = config.import_max_entries,
            import_max_bytes = config.import_max_bytes,
            %cpuset,
            budget_memory_bytes = config.budget_memory_bytes,
            run_cores = config.run_cores,
            default_memory_bytes = config.default_memory_bytes,
            default_deadline_millis = config.default_deadline_millis,
            max_deadline_millis = config.max_deadline_millis,
            headroom_percent = config.headroom_percent,
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
            imports: TaskQueue::new(config.max_in_flight),
            runs: RunQueue::new(
                Runner { engine, artifacts, pids, output },
                Budget::new(&cpuset, budget_memory_bytes),
                Estimates::new(defaults, headroom, ceiling),
            ),
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
        state.imports.submit(ctx, move || importer.answer(&mail.image))
    }

    /// ADR-0093 completion: re-reply the worker's result to the original
    /// caller, then free the slot for the next queued request.
    #[handler(task)]
    fn on_import_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<ImportResult>) {
        done.resolve(ctx);
        state.imports.on_complete(ctx);
    }

    /// Run steps over a stored tree in a stored environment.
    ///
    /// # Agent
    /// Reply: `run_result`. The actor provisions the run itself: it picks
    /// the run's cores, memory, and deadline from its host budget and what
    /// it has seen of runs doing the same steps, and the run may wait, in
    /// arrival order, until they are free; it is never dropped. Runs each
    /// step in its own container over the tree at `/work`, under the sandbox
    /// pins and that allotment, and answers `Ok(Outcome)` with each step's
    /// exit code and stored stdout and stderr and the output tree minus
    /// `scratch`; `Refused` when the run cannot start as asked;
    /// `Exhausted(Time | Memory)` when a step outran the allotment, after
    /// which a retry is given twice as much of it, up to the budget; or
    /// `Failed { detail }` when the executor failed. No container or volume
    /// is left behind, and rows commit only for `Ok`. The reply lands when
    /// the whole run is done.
    #[handler::single]
    fn on_run(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: Run) -> Pending<RunResult> {
        state.runs.submit(ctx, mail)
    }

    /// ADR-0093 completion: learn from the run, release its budget, re-reply
    /// its result to the original caller, then admit the runs waiting at the
    /// front while they fit.
    #[handler(task)]
    fn on_run_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<Ran, Admitted>) {
        state.runs.complete(ctx, done);
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
        Err(must_be_positive(knob))
    } else {
        Ok(value)
    }
}

/// A byte count that must not be zero, refusing boot naming its knob.
fn non_zero_bytes(value: u64, knob: &str) -> Result<NonZeroU64, BootError> {
    NonZeroU64::new(value).ok_or_else(|| must_be_positive(knob))
}

fn must_be_positive(knob: &str) -> BootError {
    boot_error(&format!("AETHER_WORKSPACE_{knob} must be at least 1"))
}

fn boot_error(message: &str) -> BootError {
    BootError::Other(Box::new(io::Error::other(message.to_owned())))
}
