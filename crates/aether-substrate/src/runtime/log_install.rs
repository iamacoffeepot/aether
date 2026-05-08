//! Issue #601 substrate-side install for the actor-aware logging
//! path. The host branch retired alongside `PROCESS` /
//! `install_log_target` / `ship_host_event` from `aether-actor::log`:
//! `tracing::*` events emitted outside any actor stamp (substrate
//! boot, scheduler thread, panic hook) hit stderr via the registered
//! fmt::Layer for operator visibility but do not enter the mail
//! system. Until those code paths run as actors, their events stay
//! out of `engine_logs`. The chassis-pushed `ConfigureLogDrain` mail
//! and per-actor [`aether_actor::log::LogDrainSlot`] handle every
//! actor-bound case.
//!
//! Single entry point:
//!   - [`init_subscriber`] — called from `SubstrateBoot::build`.
//!     Installs `EnvFilter` + `tsfmt::Layer` + [`ActorAwareLayer`]
//!     as `tracing`'s global default. Idempotent (later calls
//!     no-op via `try_init`).

use aether_actor::Local;
use aether_actor::log::{LogBuffer, drain_buffer, encode_event};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, fmt as tsfmt};

/// Tracing layer that routes in-actor events into the per-actor
/// [`LogBuffer`] for the chassis-installed drain to ship as
/// [`aether_kinds::LogBatch`] mail. Out-of-actor events drop here —
/// stderr fmt::Layer, registered alongside in [`init_subscriber`],
/// keeps them visible to operators. Issue #601 retired the
/// host-branch shortcut that previously routed out-of-actor events
/// through a process-global egress; the actor model's invariant is
/// that engine logic eventually runs as an actor, and code that
/// hasn't been migrated yet stays out of `engine_logs`.
pub struct ActorAwareLayer;

impl<S> Layer<S> for ActorAwareLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // Re-entry guard: events emitted from inside `drain_buffer`
        // (e.g. the `capability mailbox sender dropped` warn fired
        // during shutdown) would otherwise loop the pipeline. Stderr
        // fmt still receives the event via the registered fmt::Layer.
        if aether_actor::log::is_in_pipeline() {
            return;
        }
        let entry = encode_event(event);
        let level = entry.level;
        // `try_with_mut` returns `Some` only when the chassis
        // dispatcher has stamped an actor's slots (in-actor branch).
        // Out-of-actor events drop here and leave `engine_logs`
        // unchanged.
        if LogBuffer::try_with_mut(|b| b.0.push(entry)).is_some() && level >= 3 {
            drain_buffer();
        }
    }
}

const FILTER_ENV: &str = "AETHER_LOG_FILTER";

/// Install the tracing subscriber stack: `EnvFilter` (reads
/// `AETHER_LOG_FILTER`, default `info`) + `tsfmt::Layer` to stderr +
/// [`ActorAwareLayer`]. Called from `SubstrateBoot::build`;
/// idempotent (later calls no-op via `try_init`).
pub fn init_subscriber() {
    let filter = EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tsfmt::layer().with_writer(std::io::stderr))
        .with(ActorAwareLayer)
        .try_init();
}
