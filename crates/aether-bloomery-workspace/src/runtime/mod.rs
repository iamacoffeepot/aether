//! The `aether.bloomery.workspace` runtime half (ADR-0122 split), compiled only under
//! `feature = "runtime"`.
//!
//! Each `Import` and each `Run` first proves its `source` live, answering a
//! source that is not before anything is queued. It then runs on a worker
//! thread as a staged task (ADR-0243 §9): the handler holds the caller's
//! reply, stages the whole sequence, and returns at once, the caller's
//! settlement chain stays held until the `#[handler(task)]` completion answers
//! the result, and a request that cannot start yet queues rather than being
//! dropped. Imports are bounded by a cap-level [`TaskQueue`] counting
//! requests; runs are provisioned (ADR-0237 decision 9) and admitted in FIFO
//! order against the host budget by [`provision::RunQueue`].
//!
//! Every read and stage a task makes goes through its source as mail the
//! actor sends for its worker ([`storage`], ADR-0240 D7), so the workspace
//! holds no store and serves every unit's journal alike. No dispatcher thread
//! ever blocks on the daemon, the source, or the budget.

mod engine;
mod import;
mod provision;
mod run;
mod storage;

#[cfg(all(unix, any(test, feature = "test-support")))]
pub mod testing;

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

use aether_actor::{PathRefused, runtime};
use aether_bloomery_kinds::{ClosureLimit, ReadArtifactResult, ReadClosureResult, StageResult};
use aether_bloomery_tar::{Limits, LimitsError, Rules};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, Pending, TaskDone, TaskQueue};
pub use aether_substrate::chassis::error::BootError;

use crate::{Import, ImportResult, Run, RunResult, StorageWake, WorkspaceCapability, WorkspaceConfig};
use engine::{Endpoint, Engine};
use import::Importer;
use provision::{Amounts, Budget, CpuSet, Estimates, Headroom, RunQueue};
use run::{Ran, Runner};
use storage::{StorageAnswer, StorageDesk, StorageTicket};

/// `aether.bloomery.workspace` runtime state: the importer each import clones onto its
/// worker and the queue that bounds how many imports talk to the daemon at
/// once, the run queue that provisions and admits every run, and the desk
/// that carries every task's storage requests.
pub struct WorkspaceCapabilityState {
    importer: Importer,
    imports: TaskQueue<ImportResult>,
    runs: RunQueue,
    desk: StorageDesk,
}

#[runtime]
impl NativeActor for WorkspaceCapability {
    type State = WorkspaceCapabilityState;

    type Config = WorkspaceConfig;

    const NAMESPACE: &'static str = "aether.bloomery.workspace";

    /// Check the endpoint, the import bounds, the host budget, the default
    /// allotment, the fixed limits, and the prefetch budget, and mint the
    /// wake the storage desk's workers send. A `tcp://` endpoint's TLS files
    /// are read here, once. Nothing dials the daemon here, so an engine boots
    /// without one.
    fn init(config: WorkspaceConfig, ctx: &mut NativeInitCtx<'_>) -> Result<WorkspaceCapabilityState, BootError> {
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
        let prefetch = ClosureLimit::new(config.prefetch_bytes).map_err(|error| {
            boot_error(&format!(
                "AETHER_WORKSPACE_PREFETCH_BYTES={} is refused: {error}; it must be between {} and {}",
                config.prefetch_bytes,
                ClosureLimit::MIN_BYTES,
                ClosureLimit::MAX_BYTES
            ))
        })?;

        tracing::info!(
            target: "aether_bloomery_workspace",
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
            prefetch_bytes = config.prefetch_bytes,
            "workspace actor configured",
        );
        let engine = Engine::new(endpoint);
        Ok(WorkspaceCapabilityState {
            importer: Importer { engine: engine.clone(), rules: Rules::userland(import_limits) },
            imports: TaskQueue::new(config.max_in_flight),
            runs: RunQueue::new(
                Runner { engine, pids, output },
                Budget::new(&cpuset, budget_memory_bytes),
                Estimates::new(defaults, headroom, ceiling),
            ),
            desk: StorageDesk::new(ctx.self_wake(), prefetch),
        })
    }

    /// Import a digest-pinned image into a tree staged to its source.
    ///
    /// # Agent
    /// Reply: `import_result`. Answers `Err(Source(..))` at once when
    /// `source` did not prove or is not live. Otherwise pulls the image
    /// through the Docker Engine API, decodes its exported filesystem under
    /// the userland rules, stages the tree to `source` as it decodes, and
    /// answers `Ok { tree }` once every stage is answered, or
    /// `Err(Failed { detail })` with no container
    /// left behind; what a failed import staged is cited by nothing. The
    /// reply lands when the whole import is done.
    #[handler::request]
    fn on_import(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: Import) -> Pending<ImportResult> {
        let Import { image, source } = mail;
        match ctx.resolve(&source) {
            Ok(source) => {
                let session = state.desk.open(source, ctx.blob_check_in());
                let importer = state.importer.clone();
                state.imports.submit(ctx, move || importer.answer(&image, session))
            }
            Err(error) => {
                tracing::warn!(target: "aether_bloomery_workspace", image = image.as_str(), %error, "import source is not live");
                let (pending, held) = ctx.hold::<ImportResult>();
                held.answer(ctx, &ImportResult::from(PathRefused::from(error)));
                pending
            }
        }
    }

    /// Completion of an import: the queue answers the original caller with
    /// the worker's result, then starts the next queued import.
    #[handler(task)]
    fn on_import_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<ImportResult>) {
        state.imports.complete(ctx, done);
    }

    /// Run steps over a stored tree in a stored environment, reading and
    /// staging through the run's source.
    ///
    /// # Agent
    /// Reply: `run_result`. Answers `Err(Refused(SourceUnavailable(..)))` at
    /// once when `source` did not prove or is not live. The actor provisions the run itself: it picks
    /// the run's cores, memory, and deadline from its host budget and what
    /// it has seen of runs doing the same steps, and the run may wait, in
    /// arrival order, until they are free; it is never dropped. Runs each
    /// step in its own container over the tree at `/work`, under the sandbox
    /// pins and that allotment, and answers `Ok(Outcome)` with each step's
    /// exit code and stored stdout and stderr and the output tree minus
    /// `scratch`; `Err(Refused)` when the run cannot start as asked;
    /// `Err(Exhausted(Time | Memory))` when a step outran the allotment, after
    /// which a retry is given twice as much of it, up to the budget; or
    /// `Err(Failed { detail })` when the executor failed. No container or volume
    /// is left behind. Every input is read from `source` and every output
    /// staged to it; an `Ok` answers only once every stage is answered, and
    /// what a run that ends any other way staged is cited by nothing. The
    /// reply lands when the whole run is done.
    #[handler::request]
    fn on_run(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: Run) -> Pending<RunResult> {
        let Run { source, request } = mail;
        match ctx.resolve(&source) {
            Ok(source) => state.runs.submit(ctx, &mut state.desk, source, request),
            Err(error) => {
                tracing::info!(target: "aether_bloomery_workspace", %error, "run refused: its source is not live");
                let (pending, held) = ctx.hold::<RunResult>();
                held.answer(ctx, &RunResult::from(PathRefused::from(error)));
                pending
            }
        }
    }

    /// Completion of a run: learn from it, release its budget, answer its
    /// caller with its result, then admit the runs waiting at the front
    /// while they fit.
    #[handler(task)]
    fn on_run_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<Ran>) {
        state.runs.complete(ctx, &mut state.desk, done);
    }

    /// A worker queued storage requests: send each through its task's
    /// source.
    #[handler::tell]
    fn on_storage_wake(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _wake: StorageWake) {
        state.desk.drain(ctx);
    }

    /// A source's answer to a worker's `ReadArtifact`, handed to the worker.
    #[handler::response]
    fn on_read_artifact_result(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        result: ReadArtifactResult,
        ticket: StorageTicket,
    ) {
        state.desk.answer(ticket, StorageAnswer::Read(result));
    }

    /// A source's answer to a worker's `ReadClosure`, handed to the worker.
    #[handler::response]
    fn on_read_closure_result(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        result: ReadClosureResult,
        ticket: StorageTicket,
    ) {
        state.desk.answer(ticket, StorageAnswer::ReadClosure(result));
    }

    /// A source's answer to a worker's `Stage`, handed to the worker.
    #[handler::response]
    fn on_stage_result(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, result: StageResult, ticket: StorageTicket) {
        state.desk.answer(ticket, StorageAnswer::Stage(result));
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
