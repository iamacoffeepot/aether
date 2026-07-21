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

use super::now_unix_millis;
use std::io;
use std::sync::OnceLock;
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::reload;
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
        let timestamp = now_unix_millis();
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
    let timestamp = now_unix_millis();
    let target = target.to_owned();
    let message = message.to_owned();
    let level_u8 = level.min(4) as u8;
    let _ = ActorLogRing::try_with_mut(|ring| {
        ring.push(level_u8, target, message, timestamp);
    });
}

const FILTER_ENV: &str = "AETHER_LOG_FILTER";

/// Reload handle for the installed [`EnvFilter`] layer, boxed behind a
/// closure so callers never name the layered-subscriber generic. Set once
/// by [`init_subscriber`] when it wins `try_init`; [`apply_filter`] uses it
/// to swap the filter after full config resolution. `AETHER_LOG_FILTER` moved
/// off `RUNTIME_KNOBS` onto the chassis-declared `RuntimeConfig` derive-`Config`
/// member (ADR-0156 §6); the boot-time install still reads env directly (it
/// runs before the config file loads), and the chassis re-applies the resolved
/// directive through this handle.
type FilterReload = Box<dyn Fn(EnvFilter) + Send + Sync>;
static FILTER_RELOAD: OnceLock<FilterReload> = OnceLock::new();

/// Install the tracing subscriber stack: a reloadable `EnvFilter` (reads
/// `AETHER_LOG_FILTER`, default `info`) + `tsfmt::Layer` to stderr +
/// [`ActorAwareLayer`]. Called from `SubstrateBoot::build`; idempotent (later
/// calls no-op via `try_init`). The filter rides a [`reload::Layer`] so
/// [`apply_filter`] can re-apply the fully-resolved directive (which may pick
/// up a `[runtime]` config-file value the env-only boot install couldn't see).
pub fn init_subscriber() {
    let filter = EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new("info"));
    let (filter_layer, handle) = reload::Layer::new(filter);
    let installed = tracing_subscriber::registry()
        .with(filter_layer)
        .with(tsfmt::layer().with_writer(io::stderr))
        .with(ActorAwareLayer)
        .try_init()
        .is_ok();
    // Only publish the handle when *this* call installed the stack — otherwise
    // it points at a subscriber that never became global (another `try_init`
    // won, e.g. a test's own subscriber), and re-applying through it is a no-op
    // at best. `OnceLock::set` on a later call fails silently, keeping the
    // first installer's handle.
    if installed {
        let _ = FILTER_RELOAD.set(Box::new(move |filter| {
            let _ = handle.reload(filter);
        }));
    }
}

/// Re-apply a fully-resolved `EnvFilter` directive after config resolution
/// (ADR-0156 §6). [`init_subscriber`] installs the env-or-`info` filter at
/// boot, before the chassis config file is loaded; the chassis resolves
/// `RuntimeConfig` (env > `[runtime]` file section > `info`) and calls this so
/// a directive set only in the config file takes effect. A malformed directive
/// warns and keeps the installed filter; a no-op when this process's subscriber
/// isn't the one [`init_subscriber`] installed.
pub fn apply_filter(directive: &str) {
    let Some(reload) = FILTER_RELOAD.get() else {
        return;
    };
    match EnvFilter::try_new(directive) {
        Ok(filter) => reload(filter),
        Err(error) => tracing::warn!(
            target: "aether_substrate::boot",
            directive,
            %error,
            "resolved AETHER_LOG_FILTER directive is invalid — keeping the boot-time filter",
        ),
    }
}
