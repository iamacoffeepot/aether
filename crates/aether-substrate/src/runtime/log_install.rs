//! ADR-0081 substrate-side install for the per-actor log path.
//!
//! Two surfaces:
//!   - [`init_subscriber`] — called from `SubstrateBoot::build`.
//!     Installs `EnvFilter` + `tsfmt::Layer` + [`ActorAwareLayer`]
//!     as `tracing`'s global default. Idempotent.
//!   - [`emit_host_event`] — host-side bridge the wasm `log_event_p32`
//!     host fn calls to re-fire one guest `tracing::*` event on the
//!     trampoline's dispatcher thread, where the `ActorAwareLayer`
//!     lands it in the trampoline's [`ActorLogRing`] (ADR-0081 §7).
//!
//! Host-target events emitted outside any actor stamp (substrate
//! boot, scheduler thread, panic hook) hit stderr via the registered
//! `tsfmt::Layer` for operator visibility but do not enter any
//! actor's ring — there is no longer a centralized store for them
//! to land in. ADR-0081 §5; matches the post-#601 disposition.

use aether_actor::Local;
use aether_actor::log::{ActorLogRing, render_event};
use std::time::{SystemTime, UNIX_EPOCH};

use std::io;
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, fmt as tsfmt};

/// Tracing layer that routes in-actor events into the per-actor
/// [`ActorLogRing`]. Out-of-actor events drop here — the registered
/// `tsfmt::Layer` (stderr) keeps them visible to operators.
/// ADR-0081 §1.
pub struct ActorAwareLayer;

impl<S> Layer<S> for ActorAwareLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let (level, target, message) = render_event(event);
        let timestamp = now_unix_ms();
        // `try_with_mut` returns `Some` only when the chassis
        // dispatcher has stamped an actor's slots (in-actor branch).
        // Out-of-actor events drop here and leave `engine_logs`
        // unchanged.
        let _ = ActorLogRing::try_with_mut(|ring| {
            ring.push(level, target, message, timestamp);
        });
    }
}

/// Re-fire one guest `tracing::*` event on the host's subscriber
/// stack. Called from the wasm `log_event_p32` host fn after copying
/// `target` + `message` out of guest memory. Runs on the
/// trampoline's dispatcher thread (the same thread that invoked the
/// guest), so the `ActorAwareLayer`'s `try_with_mut` lookup hits the
/// trampoline's `ActorSlots` and the entry lands in the trampoline's
/// `ActorLogRing` — ADR-0081 §7.
pub fn emit_host_event(level: u32, target: &str, message: &str) {
    // `tracing::event!` requires a literal target + level; the
    // dynamic path uses the `event_enabled!` + low-level dispatch
    // trick, but for ADR-0081 the simplest sufficient path is to
    // skip the macro entirely and push directly to the actor's ring.
    // `EnvFilter` matched against the *host* target, not the guest's,
    // would otherwise drop the guest event on its way through.
    let timestamp = now_unix_ms();
    let target = target.to_owned();
    let message = message.to_owned();
    let level_u8 = level.min(4) as u8;
    let _ = ActorLogRing::try_with_mut(|ring| {
        ring.push(level_u8, target, message, timestamp);
    });
}

fn now_unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
        #[allow(clippy::cast_possible_truncation)]
        let ms = d.as_millis() as u64;
        ms
    })
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
        .with(tsfmt::layer().with_writer(io::stderr))
        .with(ActorAwareLayer)
        .try_init();
}
