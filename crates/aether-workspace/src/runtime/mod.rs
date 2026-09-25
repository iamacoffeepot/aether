//! The `aether.workspace` runtime half (ADR-0122 split), compiled only under
//! `feature = "runtime"`.
//!
//! Each `Import` runs on a worker thread through ADR-0093's hold-until-resolve
//! dispatch, bounded by a cap-level [`TaskQueue`]: the handler submits the
//! whole import sequence and returns at once, the caller's settlement chain
//! stays held until the `#[handler(task)]` completion re-replies the result,
//! and a request past the bound queues rather than being dropped. No
//! dispatcher thread ever blocks on the daemon or the journal.

mod engine;
mod import;
mod journal;

#[cfg(all(unix, any(test, feature = "test-support")))]
pub mod testing;

use std::io;

use aether_actor::runtime;
use aether_bloomery_journal::ArtifactStore;
use aether_bloomery_tar::{Limits, LimitsError, Rules};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, Pending, TaskDone, TaskQueue};
pub use aether_substrate::chassis::error::BootError;

use crate::{DEFAULT_ENDPOINT, Import, ImportResult, WorkspaceCapability, WorkspaceConfig};
use engine::{Endpoint, Engine};
use import::Importer;

/// Composer-supplied construction params (ADR-0156 §3): the live artifact
/// store of the journal the chassis opened. It is a value the composer holds,
/// not operator-typed config, so it rides `Params`.
///
/// It is `None` only where a chassis is composed to be described and never
/// booted (`--describe` / `--print-config`, ADR-0155); `init` refuses `None`.
pub struct WorkspaceParams {
    pub artifacts: Option<ArtifactStore>,
}

/// `aether.workspace` runtime state: the importer each request clones onto
/// its worker, and the queue that bounds how many run at once.
pub struct WorkspaceCapabilityState {
    importer: Importer,
    tasks: TaskQueue,
}

#[runtime]
impl NativeActor for WorkspaceCapability {
    type State = WorkspaceCapabilityState;

    type Config = WorkspaceConfig;
    type Params = WorkspaceParams;

    const NAMESPACE: &'static str = "aether.workspace";

    /// Check the endpoint and the import bounds and take the journal's store.
    /// Nothing dials the daemon here, so an engine boots without one.
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
        let limits = Limits::new(config.import_max_entries, config.import_max_bytes).map_err(|error| {
            let key = match error {
                LimitsError::ZeroEntries => "AETHER_WORKSPACE_IMPORT_MAX_ENTRIES",
                LimitsError::ZeroBytes => "AETHER_WORKSPACE_IMPORT_MAX_BYTES",
            };
            boot_error(&format!("{key} must be at least 1: {error}"))
        })?;

        tracing::info!(
            target: "aether_workspace",
            %endpoint,
            max_in_flight = config.max_in_flight,
            import_max_entries = config.import_max_entries,
            import_max_bytes = config.import_max_bytes,
            "workspace actor configured",
        );
        Ok(WorkspaceCapabilityState {
            importer: Importer { engine: Engine::new(endpoint), artifacts, rules: Rules::userland(limits) },
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
    /// caller, then free the slot for the next queued import.
    #[handler(task)]
    fn on_import_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<ImportResult>) {
        done.resolve(ctx);
        state.tasks.on_complete(ctx);
    }
}

fn boot_error(message: &str) -> BootError {
    BootError::Other(Box::new(io::Error::other(message.to_owned())))
}
