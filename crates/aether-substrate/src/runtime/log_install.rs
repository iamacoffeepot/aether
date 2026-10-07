//! ADR-0081 substrate-side install for the per-actor log path.
//!
//! Two surfaces:
//!   - [`init_subscriber`] — called from `SubstrateBoot::build`.
//!     Installs `EnvFilter` + `tsfmt::Layer` + [`ActorAwareLayer`]
//!     as `tracing`'s global default. Idempotent.
//!   - [`emit_host_event`] — host-side bridge the wasm `log_event_p32`
//!     host fn calls to dispatch one guest `tracing::*` event on the
//!     trampoline's dispatcher thread, where the filter, the stderr
//!     layer, and the `ActorAwareLayer` each see it once, in the order
//!     a native event meets them (ADR-0081 §6, §7).
//!
//! Host-target events emitted outside any actor stamp (substrate
//! boot, scheduler thread, panic hook) hit stderr via the registered
//! `tsfmt::Layer` for operator visibility but do not enter any
//! actor's ring — there is no longer a centralized store for them
//! to land in. ADR-0081 §5; matches the post-#601 disposition.
//!
//! The filter check for a guest event goes through the same
//! `reload::Layer` read guard every native event takes. Any thread that
//! logs takes that guard: actor dispatcher threads for native and guest
//! events alike, and the boot, scheduler, and panic-hook threads. It is
//! write-locked only when [`apply_filter`] swaps the directive once at
//! boot, so a guest event adds no contention a native event does not
//! already carry.

use aether_actor::Local;
use aether_actor::log::{ActorLogRing, render_event};

use super::now_unix_millis;
use std::fmt::Debug;
use std::io;
use std::sync::OnceLock;
use tracing::callsite::{Callsite, Identifier};
use tracing::dispatcher::get_default;
use tracing::field::{Field, FieldSet, Value, Visit};
use tracing::level_filters::LevelFilter;
use tracing::metadata::Kind;
use tracing::subscriber::Interest;
use tracing::{Dispatch, Event, Level, Metadata, Subscriber};
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
        let metadata = event.metadata();
        let guest = is_guest_event(metadata);
        let (level, target, message) = if guest {
            guest_entry(event)
        } else {
            render_event(event)
        };
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

/// Fixed host target every guest event is dispatched under; the guest's own
/// target rides in the [`GUEST_TARGET_FIELD`] field.
const GUEST_TARGET: &str = "aether_substrate::guest";
const MESSAGE_FIELD: &str = "message";
const GUEST_TARGET_FIELD: &str = "guest.target";
const GUEST_FIELDS: &[&str] = &[MESSAGE_FIELD, GUEST_TARGET_FIELD];

/// A `tracing` event callsite that is never registered with the callsite
/// registry: the guest path decides `enabled` itself, so interest is unused.
struct GuestCallsite {
    metadata: &'static Metadata<'static>,
}

impl Callsite for GuestCallsite {
    fn set_interest(&self, _interest: Interest) {}

    fn metadata(&self) -> &Metadata<'_> {
        self.metadata
    }
}

macro_rules! guest_callsite {
    ($callsite:ident, $metadata:ident, $level:expr) => {
        static $callsite: GuestCallsite = GuestCallsite { metadata: &$metadata };
        static $metadata: Metadata<'static> = Metadata::new(
            "guest event",
            GUEST_TARGET,
            $level,
            None,
            None,
            None,
            FieldSet::new(GUEST_FIELDS, Identifier(&$callsite)),
            Kind::EVENT,
        );
    };
}

guest_callsite!(TRACE_CALLSITE, TRACE_METADATA, Level::TRACE);
guest_callsite!(DEBUG_CALLSITE, DEBUG_METADATA, Level::DEBUG);
guest_callsite!(INFO_CALLSITE, INFO_METADATA, Level::INFO);
guest_callsite!(WARN_CALLSITE, WARN_METADATA, Level::WARN);
guest_callsite!(ERROR_CALLSITE, ERROR_METADATA, Level::ERROR);

/// The callsites by guest level, `0 = trace .. 4 = error`.
static GUEST_CALLSITES: [&GuestCallsite; 5] =
    [&TRACE_CALLSITE, &DEBUG_CALLSITE, &INFO_CALLSITE, &WARN_CALLSITE, &ERROR_CALLSITE];

/// The callsite for a guest level, clamped to `error` as a guest level always was.
fn guest_callsite_for(level: u32) -> &'static GuestCallsite {
    let index = level.min(4) as usize;
    GUEST_CALLSITES[index]
}

/// Whether `metadata` belongs to one of the five guest callsites.
fn is_guest_event(metadata: &Metadata<'_>) -> bool {
    let callsite = metadata.callsite();
    GUEST_CALLSITES.iter().any(|guest| guest.metadata.callsite() == callsite)
}

/// Whether a guest event at `level` clears the global maximum level. This is
/// the first check every `tracing` macro makes, and the host function makes
/// it before copying anything out of guest memory.
#[must_use]
pub fn guest_level_enabled(level: u32) -> bool {
    let level = *guest_callsite_for(level).metadata.level();
    level <= LevelFilter::current()
}

/// The guest's own target and rendered message, read back off a guest event.
#[derive(Default)]
struct GuestFields {
    target: String,
    message: String,
}

impl Visit for GuestFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            MESSAGE_FIELD => value.clone_into(&mut self.message),
            GUEST_TARGET_FIELD => value.clone_into(&mut self.target),
            _ => {}
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn Debug) {}
}

/// The ring entry fields for a guest event: its level from the callsite, its
/// own target and message from the two fields.
fn guest_entry(event: &Event<'_>) -> (u8, String, String) {
    let level = match *event.metadata().level() {
        Level::TRACE => 0,
        Level::DEBUG => 1,
        Level::INFO => 2,
        Level::WARN => 3,
        Level::ERROR => 4,
    };
    let mut fields = GuestFields::default();
    event.record(&mut fields);

    (level, fields.target, fields.message)
}

/// Dispatch one guest `tracing::*` event on the host's subscriber stack.
/// Called from the wasm `log_event_p32` host fn with `target` + `message`
/// borrowed from guest memory. Runs on the trampoline's dispatcher thread
/// (the same thread that invoked the guest), so the `ActorAwareLayer`'s
/// `try_with_mut` lookup hits the trampoline's `ActorSlots` and the entry
/// lands in its `ActorLogRing` — ADR-0081 §7.
///
/// The filter decides on a `Metadata` that carries the guest's own target, so
/// a directive naming a component matches it. An accepted event goes through
/// the level's static callsite under the host target, which `Dispatch::event`
/// does not filter again; the guest's target travels as a field.
pub fn emit_host_event(level: u32, target: &str, message: &str) {
    let callsite = guest_callsite_for(level);
    let host = callsite.metadata;
    let borrowed = Metadata::new(
        host.name(),
        target,
        *host.level(),
        None,
        None,
        None,
        FieldSet::new(GUEST_FIELDS, host.callsite()),
        Kind::EVENT,
    );
    get_default(|dispatch: &Dispatch| {
        if !dispatch.enabled(&borrowed) {
            return;
        }
        let fields = host.fields();
        let (Some(message_field), Some(target_field)) = (fields.field(MESSAGE_FIELD), fields.field(GUEST_TARGET_FIELD))
        else {
            return;
        };
        let values = [(&message_field, Some(&message as &dyn Value)), (&target_field, Some(&target as &dyn Value))];
        let value_set = fields.value_set(&values);
        dispatch.event(&Event::new(host, &value_set));
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

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use aether_actor::local::ActorSlots;
    use tracing::subscriber::with_default;

    use super::*;
    use crate::actor::native::local::with_stamped;

    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Ok(mut captured) = self.0.lock() {
                captured.extend_from_slice(bytes);
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // Catches: a guest event that lands in the ring or on stderr below the
    // filter; a directive naming the guest's target matched against the host
    // callsite's target; a ring entry that carries the host callsite's target;
    // an accepted guest line that never reaches the formatting layer.
    #[test]
    fn a_guest_event_is_filtered_printed_and_recorded_as_a_native_one_is() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&captured);
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("info,loud=trace"))
            .with(tsfmt::layer().with_ansi(false).with_writer(move || Capture(Arc::clone(&writer))))
            .with(ActorAwareLayer);
        let slots = ActorSlots::new();

        with_default(subscriber, || {
            with_stamped(&slots, || {
                let events = [
                    (0, "quiet", "below the filter"),
                    (0, "loud", "named by a directive"),
                    (2, "quiet", "at the filter"),
                ];
                for (level, target, message) in events {
                    if guest_level_enabled(level) {
                        emit_host_event(level, target, message);
                    }
                }
            });
        });

        let entries = with_stamped(&slots, || ActorLogRing::with(ActorLogRing::snapshot));
        let recorded: Vec<_> =
            entries.iter().map(|entry| (entry.level, entry.target.as_str(), entry.message.as_str())).collect();
        assert_eq!(recorded, [(0, "loud", "named by a directive"), (2, "quiet", "at the filter")]);

        let printed = captured.lock().map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).unwrap_or_default();
        assert!(printed.contains("named by a directive"), "stderr: {printed}");
        assert!(printed.contains("at the filter"), "stderr: {printed}");
        assert!(!printed.contains("below the filter"), "stderr: {printed}");
    }
}
