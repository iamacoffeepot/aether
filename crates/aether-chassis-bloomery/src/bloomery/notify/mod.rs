//! The notification reactor capability (#5166).
//!
//! Post-cutover the operator's ambient verification surface is gone: the
//! console covers attended watching, and nothing covers unattended. This
//! capability is the unattended half — one plain-text message per loud
//! transition, posted to a configured Discord-compatible webhook, best-effort
//! and never on the line's critical path.
//!
//! # It drains no topic
//!
//! Every other reactor in `super::reactor` is the executing half of a
//! reducer decision and drains the outbox topic that decision projects onto.
//! This one is not: "loud" is a **pure function of the current
//! [`ViewDocument`](aether_bloomery::ViewDocument)** — the same set the war
//! room computes for the board — not a stream of transitions the reducer
//! emits. So there is nothing for a producer to mint and nothing for a topic
//! to carry, and inventing one would put a second drainer's worth of
//! machinery behind an answer the projection already holds.
//!
//! Instead each tick asks the control core for the live document (the
//! `Query` → `QueryResult` pair the REST reads already use), turns it into a
//! keyed loud set, and differences that against a `notification_sent` ledger.
//! The difference **is** the transition stream, and it is reconstructible from
//! the projection at any moment — which is what makes the channel survive a
//! restart with no delivery bookkeeping of its own.
//!
//! # Delivery discipline
//!
//! A key is recorded only after its POST succeeds, so an unrecorded key is
//! exactly a message still owed and the next tick re-sends it: at-least-once,
//! idempotent by the key, and with no backoff state machine to get wrong. A
//! key whose condition has cleared is forgotten, so the same condition arising
//! again is a new transition. A failing endpoint costs one request per poll
//! interval and blocks nothing.
//!
//! # What is deliberately not here
//!
//! The daily digest the issue asks for — blooms landed, members shipped,
//! members superseded or ejected, lines changed, for the day just closed — is
//! still not sent. [`MetricDay`](aether_bloomery::MetricDay) now carries
//! landings, wedges, spend, and cycle time, so landed count and dollars are
//! no longer invented. Members shipped, superseded, or ejected, and lines
//! changed, still have no rollup; the digest would have to invent those.
//! Building that remainder is its own slice; this one ships the
//! per-transition stream, which is the half that has a source of truth.
//!
//! Mounted only in the GitHub branch of the chassis, because that is where the
//! outbound HTTP client lives ([`aether_bloomery_github::WebhookSink`]).
//! Identity/runtime split (ADR-0122): this ZST is the addressing identity, the
//! state-bearing logic is [`runtime`].

use aether_actor::actor;

// The handler kinds the `#[actor]` macro references when it emits this cap's
// `HandlesKind` markers must be in scope here: `NotifyTick` from the runtime
// module, `QueryResult` from the control core it reads the document from.
use aether_bloomery::QueryResult;

mod config;
pub use config::{NotifyConfig, NotifyOverlay};

mod taxonomy;
pub use taxonomy::{LoudEvent, loud_events};

mod runtime;
pub use runtime::{Delivered, NotifyReactorState, NotifyTick, deliver};

use aether_bloomery_github::{ReqwestWebhook, WebhookSink};
use std::fs;
use std::sync::Arc;

/// Read the webhook URL out of the file [`NotifyConfig`] names and build the
/// production sink, or `None` when there is nothing to build — no path
/// configured, an unreadable file, or a file whose contents are blank.
///
/// Every `None` path logs one line and none of them fail the boot: an operator
/// alert channel that refuses to start the coordinator would be a worse
/// failure than a coordinator that starts without one. The URL itself never
/// reaches a log line at any level — only the *path* does, which is the point
/// of taking a path.
#[must_use]
pub fn webhook_sink(config: &NotifyConfig) -> Option<Arc<dyn WebhookSink>> {
    let path = config.webhook_file.as_deref()?;
    let url = match fs::read_to_string(path) {
        Ok(contents) => contents.trim().to_owned(),
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::notify",
                %path,
                %error,
                "webhook file could not be read; the notification reactor mounts disabled",
            );
            return None;
        }
    };
    if url.is_empty() {
        tracing::warn!(
            target: "aether_chassis_bloomery::notify",
            %path,
            "webhook file is empty; the notification reactor mounts disabled",
        );
        return None;
    }
    match ReqwestWebhook::new(url) {
        Ok(sink) => Some(Arc::new(sink)),
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::notify",
                %error,
                "webhook client could not be built; the notification reactor mounts disabled",
            );
            None
        }
    }
}

/// Composer-supplied parts for the notification reactor.
pub struct NotifyReactorSetup {
    /// The endpoint to post to, or `None` for an unconfigured coordinator
    /// (which mounts the reactor disabled).
    pub sink: Option<Arc<dyn WebhookSink>>,
    /// The store the dedupe ledger lives in.
    pub store_path: String,
    /// How often to wake, read the document, and post what is new.
    pub poll_interval_secs: u64,
}

/// Addressing identity for the notification reactor capability.
#[actor(singleton, root)]
pub struct NotifyReactorCapability;

#[cfg(test)]
mod tests;
