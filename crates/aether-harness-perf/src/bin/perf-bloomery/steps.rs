//! Step timing: a `tracing` layer that times each driver step span from its
//! creation to its close.
//!
//! The driver opens a step span when it sends a ticketed command and drops it
//! when the reply takes the ticket back, so new-to-close is the step's wall
//! time. The step key is the span's name, with `read_artifact` split by its
//! `purpose` field (`read_artifact.bundle`, `read_artifact.fetch`, …), since a
//! declare read and a fetch read cost different amounts.
//!
//! The bin installs this layer as the process's global subscriber before the
//! harness boots, so the substrate's own subscriber install finds one already
//! set and stands down. Its filters are per layer: the timer sees only the
//! step target at `DEBUG`, and a stderr layer shows warnings, so no other
//! callsite in the engine is enabled and stdout stays pure JSON.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::io;
use std::mem;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use aether_bloomery_driver::STEP_TARGET;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::subscriber::{self, SetGlobalDefaultError};
use tracing::{Level, Subscriber};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::{Registry, fmt};

/// The step spans open now, and every closed one's wall time by step.
#[derive(Default)]
struct Timings {
    open: HashMap<Id, (String, Instant)>,
    samples: BTreeMap<String, Vec<u64>>,
}

/// The layer timing step spans, and the handle the bin drains samples through.
#[derive(Clone, Default)]
pub struct StepTimer(Arc<Mutex<Timings>>);

impl StepTimer {
    /// Install the timer and a stderr warning layer as the global subscriber.
    pub fn install() -> Result<Self, SetGlobalDefaultError> {
        let timer = Self::default();
        let steps = Targets::new().with_target(STEP_TARGET, Level::DEBUG);
        subscriber::set_global_default(
            Registry::default()
                .with(timer.clone().with_filter(steps))
                .with(fmt::layer().with_writer(io::stderr).with_filter(LevelFilter::WARN)),
        )?;
        Ok(timer)
    }

    /// Take every sample recorded since the last drain, in nanoseconds by
    /// step.
    pub fn drain(&self) -> BTreeMap<String, Vec<u64>> {
        mem::take(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner).samples)
    }
}

impl<S: Subscriber> Layer<S> for StepTimer {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        let opened = Instant::now();
        let name = attrs.metadata().name();
        let step = if name == "read_artifact" {
            let mut purpose = Purpose(None);
            attrs.record(&mut purpose);
            purpose.0.map_or_else(|| name.to_owned(), |purpose| format!("{name}.{purpose}"))
        } else {
            name.to_owned()
        };
        self.0.lock().unwrap_or_else(PoisonError::into_inner).open.insert(id.clone(), (step, opened));
    }

    fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
        let closed = Instant::now();
        let mut timings = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((step, opened)) = timings.open.remove(&id) {
            let nanos = u64::try_from(closed.duration_since(opened).as_nanos()).unwrap_or(u64::MAX);
            timings.samples.entry(step).or_default().push(nanos);
        }
    }
}

/// The `purpose` field of a `read_artifact` span.
struct Purpose(Option<String>);

impl Visit for Purpose {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "purpose" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == "purpose" {
            self.0 = Some(format!("{value:?}"));
        }
    }
}
