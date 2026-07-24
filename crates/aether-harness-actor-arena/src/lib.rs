//! Disposable measurement harness for the namespace-owned actor arena spike.
//!
//! This crate intentionally does not participate in substrate runtime wiring.
//! It mirrors the relevant storage, route, run-token, and Wasm-instance shapes
//! so each proposed mechanism can be measured independently before a runtime
//! migration commits to them.

mod allocator;
mod churn;
mod metrics;
mod native;
mod preallocation;
mod trace;
mod wasm;

use std::time::Instant;

use anyhow::{Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

pub use allocator::{ActorCoordinate, HierarchicalBitmap};
pub use metrics::{AllocationSnapshot, begin_allocation_counting, peak_rss_bytes};
pub use preallocation::{
    HolePattern, PreallocationConfig, PreallocationReport, PreallocationTarget, SweepMode, run_preallocation_trial,
};

/// Experimental backend selected once, before the timed loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// `Arc<dyn handler>` route plus one boxed state and mutex per actor.
    BoxedCurrent,
    /// Current-shaped dynamic route whose target points into an arena page.
    ArenaState,
    /// Concrete route endpoint containing page, slot, and generation.
    ArenaEndpoint,
    /// Concrete endpoint plus one run token and drain batch per arena page.
    ArenaPage,
    /// One Wasmtime store and linear memory per actor.
    WasmDetached,
    /// One store, but a pointer table addresses scattered state records.
    WasmInline,
    /// One store with directly addressed contiguous arena slots.
    WasmArena,
    /// The contiguous Wasm arena with packed host-to-guest delivery batches.
    WasmBatch,
}

impl Backend {
    #[must_use]
    pub const fn is_wasm(self) -> bool {
        matches!(self, Self::WasmDetached | Self::WasmInline | Self::WasmArena | Self::WasmBatch)
    }
}

/// Deterministic activation locality used by both sides of a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum AccessPattern {
    /// Walk actor addresses in order.
    Sequential,
    /// Uniform deterministic pseudorandom actor choice.
    Random,
    /// Stay on one actor for a short run before moving.
    Clustered,
    /// Ninety percent of activations target ten percent of actors.
    HotCold,
}

/// Timed mechanism. Dispatch models mail delivery; lifecycle churn models
/// reserve, state initialization, and retirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Workload {
    Dispatch,
    LifecycleChurn,
    SceneSweep,
}

/// One fresh-process trial configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialConfig {
    pub backend: Backend,
    pub workload: Workload,
    pub actors: usize,
    pub mails: u64,
    pub mails_per_activation: usize,
    pub page_slots: usize,
    pub state_bytes: usize,
    pub pattern: AccessPattern,
    pub seed: u64,
    pub warmup_mails: u64,
    pub instrument_allocations: bool,
}

impl TrialConfig {
    pub fn validate(&self) -> Result<()> {
        if self.actors == 0 {
            bail!("actors must be greater than zero");
        }
        if self.mails == 0 {
            bail!("mails must be greater than zero");
        }
        if self.mails_per_activation == 0 {
            bail!("mails-per-activation must be greater than zero");
        }
        if self.page_slots == 0 || self.page_slots > 64 || !self.page_slots.is_power_of_two() {
            bail!("page-slots must be a power of two in 1..=64");
        }
        if self.state_bytes < 64 || !self.state_bytes.is_multiple_of(8) {
            bail!("state-bytes must be a multiple of 8 and at least 64");
        }
        if self.backend == Backend::WasmDetached && self.actors > 4_096 {
            bail!("wasm-detached is capped at 4096 instances to bound setup memory");
        }
        if self.workload == Workload::LifecycleChurn
            && !matches!(self.backend, Backend::BoxedCurrent | Backend::ArenaState)
        {
            bail!("lifecycle-churn supports boxed-current and arena-state");
        }
        if self.workload == Workload::SceneSweep {
            if self.backend.is_wasm() {
                bail!("scene-sweep currently measures the native actor arms");
            }
            if self.pattern != AccessPattern::Sequential || self.mails_per_activation != 1 {
                bail!("scene-sweep requires sequential access and one mail per activation");
            }
        }

        Ok(())
    }

    fn activations(&self) -> usize {
        if self.workload == Workload::LifecycleChurn {
            return usize::try_from(self.mails).expect("lifecycle operation count fits in usize");
        }
        usize::try_from(
            self.mails.div_ceil(u64::try_from(self.mails_per_activation).expect("activation batch fits in u64")),
        )
        .expect("activation count fits in usize")
    }

    fn warmup_activations(&self) -> usize {
        if self.workload == Workload::LifecycleChurn {
            return usize::try_from(self.warmup_mails).expect("warmup lifecycle operation count fits in usize");
        }
        usize::try_from(
            self.warmup_mails.div_ceil(u64::try_from(self.mails_per_activation).expect("activation batch fits in u64")),
        )
        .expect("warmup activation count fits in usize")
    }
}

/// Mechanism counters are kept out of the hot path where they can be derived
/// exactly from the deterministic trace.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MechanismCounters {
    pub route_lookups: u64,
    pub state_lock_acquisitions: u64,
    pub scheduled_items: u64,
    pub host_entries: u64,
    pub host_to_guest_bytes: u64,
    pub guest_linear_memory_bytes: u64,
    pub allocator_cas_retries: u64,
}

/// Machine-readable output emitted by one fresh trial process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialReport {
    pub schema: u32,
    pub config: TrialConfig,
    pub elapsed_nanos: u64,
    pub completed_mails: u64,
    pub nanos_per_mail: f64,
    pub mails_per_second: f64,
    pub checksum: String,
    pub peak_rss_bytes: u64,
    pub allocations: Option<AllocationSnapshot>,
    pub counters: MechanismCounters,
}

/// Run warmup, restore a deterministic initial state, and time only delivery.
#[allow(
    clippy::cast_precision_loss,
    reason = "sub-nanosecond reporting intentionally converts bounded trial totals to f64"
)]
pub fn run_trial(config: TrialConfig) -> Result<TrialReport> {
    config.validate()?;

    let trace = trace::ActivationTrace::new(
        config.actors,
        config.activations().max(config.warmup_activations()),
        config.pattern,
        config.seed,
    );
    let mut experiment: Experiment = if config.workload == Workload::LifecycleChurn {
        churn::ChurnExperiment::new(&config)?.into()
    } else if config.backend.is_wasm() {
        wasm::WasmExperiment::new(&config)?.into()
    } else {
        native::NativeExperiment::new(&config).into()
    };

    experiment.deliver(&config, trace.prefix(config.warmup_activations()))?;
    experiment.reset(&config)?;

    let allocation_guard = config.instrument_allocations.then(begin_allocation_counting);
    let started = Instant::now();
    let delivery = experiment.deliver(&config, trace.prefix(config.activations()))?;
    let elapsed = started.elapsed();
    let allocations = allocation_guard.map(metrics::AllocationGuard::finish);
    let checksum = experiment.checksum();

    let elapsed_nanos = elapsed.as_nanos().try_into().unwrap_or(u64::MAX);
    let completed_mails = delivery.completed_mails;
    let nanos_per_mail = elapsed_nanos as f64 / completed_mails as f64;

    Ok(TrialReport {
        schema: 1,
        config,
        elapsed_nanos,
        completed_mails,
        nanos_per_mail,
        mails_per_second: 1_000_000_000.0 / nanos_per_mail,
        checksum: format!("{checksum:016x}"),
        peak_rss_bytes: peak_rss_bytes(),
        allocations,
        counters: delivery.counters,
    })
}

#[derive(Debug)]
struct DeliveryOutcome {
    completed_mails: u64,
    counters: MechanismCounters,
}

enum Experiment {
    Churn(churn::ChurnExperiment),
    Native(native::NativeExperiment),
    Wasm(wasm::WasmExperiment),
}

impl From<native::NativeExperiment> for Experiment {
    fn from(value: native::NativeExperiment) -> Self {
        Self::Native(value)
    }
}

impl From<churn::ChurnExperiment> for Experiment {
    fn from(value: churn::ChurnExperiment) -> Self {
        Self::Churn(value)
    }
}

impl From<wasm::WasmExperiment> for Experiment {
    fn from(value: wasm::WasmExperiment) -> Self {
        Self::Wasm(value)
    }
}

impl Experiment {
    fn deliver(&mut self, config: &TrialConfig, trace: &[usize]) -> Result<DeliveryOutcome> {
        match self {
            Self::Churn(experiment) => Ok(experiment.deliver(config, trace)),
            Self::Native(experiment) => Ok(experiment.deliver(config, trace)),
            Self::Wasm(experiment) => experiment.deliver(config, trace),
        }
    }

    fn reset(&mut self, config: &TrialConfig) -> Result<()> {
        match self {
            Self::Churn(experiment) => {
                experiment.reset(config);
                Ok(())
            }
            Self::Native(experiment) => {
                experiment.reset(config);
                Ok(())
            }
            Self::Wasm(experiment) => experiment.reset(config),
        }
    }

    fn checksum(&mut self) -> u64 {
        match self {
            Self::Churn(experiment) => experiment.checksum(),
            Self::Native(experiment) => experiment.checksum(),
            Self::Wasm(experiment) => experiment.checksum(),
        }
    }
}
