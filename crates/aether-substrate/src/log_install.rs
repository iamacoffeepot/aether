//! Issue #581 substrate-side install for the actor-aware logging
//! path. Replaces ADR-0023's `log_capture` module: the ring + flush
//! thread + sync `flush_now` retired; the cap is the egress owner
//! now.
//!
//! Two entry points:
//!   - [`init_subscriber`] — called from `SubstrateBoot::build`
//!     before any cap boots. Installs `EnvFilter` +
//!     `tsfmt::Layer` + [`ActorAwareLayer`] as `tracing`'s global
//!     default. With no log target registered yet, the
//!     [`ActorAwareLayer`] drops the mail-egress half of host-branch
//!     events; `tsfmt::Layer` keeps stderr live so operators don't
//!     lose early-boot diagnostics.
//!   - [`install_log_target_if_registered`] — called from
//!     [`crate::chassis_builder::Builder::build`] after the cap
//!     chain has booted. Looks up the well-known `"aether.log"`
//!     mailbox; if present, registers the [`MailerHostDispatch`]
//!     so the host branch starts shipping single-entry batches to
//!     the cap.

use std::sync::Arc;

use aether_actor::Local;
use aether_actor::log::{LogBuffer, MailDispatch, drain_buffer, encode_event, ship_host_event};
use aether_data::{KindId, MailboxId};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, fmt as tsfmt};

use crate::mail::Mail;
use crate::mailer::Mailer;
use crate::registry::Registry;

/// Tracing layer that splits events between the in-actor and host
/// paths. In-actor: push the event into the per-actor [`LogBuffer`];
/// priority-flush at `WARN`/`ERROR`. Host code (no actor stamped):
/// ship a single-entry batch through `aether-actor::log`'s
/// registered host dispatch.
pub struct ActorAwareLayer;

impl<S> Layer<S> for ActorAwareLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // Re-entry guard: events emitted from inside
        // `drain_buffer` / `ship_host_event` (e.g. the
        // `capability mailbox sender dropped` warn fired during
        // shutdown) would otherwise loop the pipeline. Stderr fmt
        // still receives the event via the registered fmt::Layer.
        if aether_actor::log::is_in_pipeline() {
            return;
        }
        let entry = encode_event(event);
        let level = entry.level;
        let entry_for_host = entry.clone();
        let buffered = LogBuffer::try_with_mut(|b| b.0.push(entry)).is_some();
        if buffered {
            if level >= 3 {
                drain_buffer();
            }
        } else {
            ship_host_event(entry_for_host);
        }
    }
}

/// `MailDispatch` impl that routes a single host-branch `LogBatch`
/// mail through the substrate's process-global `Mailer`. Lives behind
/// `aether-actor::log`'s registered host slot; reached when
/// [`ActorAwareLayer`] sees a `tracing::*` event outside any actor's
/// dispatch.
pub(crate) struct MailerHostDispatch {
    mailer: Arc<Mailer>,
}

impl MailDispatch for MailerHostDispatch {
    fn send(&self, mailbox: MailboxId, kind: KindId, payload: &[u8]) {
        let mail = Mail::new(mailbox, kind, payload.to_vec(), 1);
        self.mailer.push(mail);
    }
}

const FILTER_ENV: &str = "AETHER_LOG_FILTER";

/// Install the tracing subscriber stack: `EnvFilter` (reads
/// `AETHER_LOG_FILTER`, default `info`) + `tsfmt::Layer` to stderr +
/// [`ActorAwareLayer`]. Called from `SubstrateBoot::build` before
/// any cap is booted; idempotent (later calls no-op via `try_init`).
pub fn init_subscriber() {
    let filter = EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tsfmt::layer().with_writer(std::io::stderr))
        .with(ActorAwareLayer)
        .try_init();
}

/// Wire `aether-actor::log`'s host-branch dispatch + log mailbox id.
/// Called after `Builder::build` has booted every cap. Looks up
/// `"aether.log"` in the registry; if `LogCapability` registered it,
/// installs a [`MailerHostDispatch`] over the substrate's `Mailer`.
/// No-op if the mailbox isn't claimed (chassis that intentionally
/// skip `LogCapability`).
///
/// Idempotent — `aether-actor::log::install_log_target` itself
/// honours "first call wins."
pub fn install_log_target_if_registered(mailer: Arc<Mailer>, registry: &Registry) {
    let Some(log_mailbox) = registry.lookup("aether.log") else {
        return;
    };
    let dispatch: &'static dyn MailDispatch = Box::leak(Box::new(MailerHostDispatch { mailer }));
    aether_actor::log::install_log_target(dispatch, log_mailbox);
}
