//! The runtime for the notification reactor capability (#5166).
//!
//! Two handlers, one ledger, and a seeding pass on first mount:
//!
//! 1. **Ask.** The poll tick sends a detached
//!    [`Query`] to the control core — the same live
//!    read `GET /view` serves — and returns. Nothing is computed on the tick
//!    itself, so a slow endpoint cannot hold the dispatcher through a store
//!    read as well.
//! 2. **Answer.** [`QueryResult::Document`] decodes back into a
//!    [`ViewDocument`], [`loud_events`] turns it into a keyed set, and
//!    [`deliver`] differences that against the `notification_sent` ledger.
//!    An empty ledger is a first mount: every current key plus a reserved
//!    seed marker is recorded without posting, so the standing set is adopted
//!    in silence. Every later pass posts keys that are new and forgets keys
//!    that have gone quiet; the marker stays, so a ledger whose conditions
//!    have all cleared is never mistaken for a first mount.
//!
//! The record-after-post order is the whole crash story. An unrecorded key is
//! a message still owed, so a coordinator killed between the POST and the
//! record re-sends one message; a coordinator killed before the POST sends it
//! on the next tick. Recording first would have inverted that into silent
//! loss, which for an alert channel is the bad half of the trade.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_actor::Addressable;
use aether_actor::runtime;
use aether_bloomery::{Query, QueryResult, QuerySelector, ViewDocument};
use aether_bloomery_github::WebhookSink;
use aether_data::wire::from_bytes;
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::taxonomy::loud_events;
use super::{NotifyReactorCapability, NotifyReactorSetup};

use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::control::ControlCore;
use crate::store::{SqliteStore, StoreBackend};

/// The self-addressed wake the poll timer fires each interval; its handler asks
/// the control core for the live document. Zero-field — the timer carries only
/// the schedule.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.notify.notify_tick")]
pub struct NotifyTick {}

/// Runtime state for [`NotifyReactorCapability`]. The sink + store are `Some`
/// only when configured; a disabled reactor holds neither and spawns no timer.
pub struct NotifyReactorState {
    sink: Option<Arc<dyn WebhookSink>>,
    store: Option<SqliteStore>,
    control_mailbox: MailboxId,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    // The poll timer sidecar; `None` when disabled. Held for its `Drop`, which
    // stops + joins the thread on teardown.
    _timer: Option<TimerHandle>,
}

impl NotifyReactorState {
    /// Build state over an explicit sink + store — the seam the runtime tests
    /// drive with a recording sink and an in-memory store, bypassing `init`
    /// (which needs config and a real file read). Spawns no timer; a test
    /// drives the loop by handing a document straight to [`deliver`].
    #[must_use]
    pub fn with_parts(
        sink: Option<Arc<dyn WebhookSink>>,
        store: Option<SqliteStore>,
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
    ) -> Self {
        Self {
            sink,
            store,
            control_mailbox: <ControlCore as Addressable>::resolve(0, ()),
            mailer,
            self_mailbox,
            _timer: None,
        }
    }
}

/// What one pass over a document did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Delivered {
    /// Messages posted and recorded.
    pub posted: u32,
    /// Keys whose condition has cleared, dropped from the ledger.
    pub forgotten: u32,
    /// Keys recorded without posting because this was the first mount.
    pub seeded: u32,
    /// Whether the pass stopped early on a failing endpoint. The unposted keys
    /// stay unrecorded, so the next tick re-derives and retries them.
    pub stalled: bool,
}

/// Reserved ledger key that names no condition. Its dotted spelling cannot be
/// produced by any [`loud_events`] format (`<word>:<parts>`), and its presence
/// is what distinguishes a never-mounted ledger from one whose conditions have
/// all cleared.
pub const SEED_MARKER_KEY: &str = "aether.bloomery.notify.seeded";

/// Post every loud condition in `view` the ledger has not already reported,
/// and forget every recorded key whose condition has cleared.
///
/// Stops at the first failing POST rather than running the whole set: a
/// refusing or rate-limited endpoint refuses the next message too, and the
/// difference is recomputed from the same document next tick, so pressing on
/// buys nothing and costs one request per remaining event. The same ack-prefix
/// discipline every other reactor uses on its topic.
///
/// An empty ledger is a first mount, not a quiet day: every current event key
/// is recorded without posting, plus a reserved seed marker whose dotted
/// spelling no [`loud_events`] format produces. The marker is what keeps the
/// ledger non-empty after every condition has cleared, so a later genuinely
/// new wedge is a transition rather than a second seed. The forget sweep
/// skips the marker; the post loop never sees it, because it is not a
/// [`loud_events`] key.
///
/// The factored-out network side, unit-testable against a `SqliteStore` and a
/// recording sink without the mail harness.
pub fn deliver(
    store: &mut dyn StoreBackend,
    sink: &dyn WebhookSink,
    view: &ViewDocument,
    now_unix_millis: u64,
) -> rusqlite::Result<Delivered> {
    let events = loud_events(view);
    let recorded = store.list_notifications()?;
    if recorded.is_empty() {
        for event in &events {
            store.record_notification(&event.key, now_unix_millis)?;
        }
        store.record_notification(SEED_MARKER_KEY, now_unix_millis)?;
        return Ok(Delivered { seeded: u32::try_from(events.len()).unwrap_or(u32::MAX), ..Delivered::default() });
    }

    let mut report = Delivered::default();

    for key in recorded.iter().filter(|key| *key != SEED_MARKER_KEY && !events.iter().any(|event| &&event.key == key)) {
        store.forget_notification(key)?;
        report.forgotten += 1;
    }

    for event in events.iter().filter(|event| !recorded.iter().any(|key| key == &event.key)) {
        if let Err(error) = sink.post(&event.message) {
            // The error carries a status or a transport class and never the
            // endpoint URL — see `aether_bloomery_github::WebhookError`.
            tracing::warn!(
                target: "aether_chassis_bloomery::notify",
                key = %event.key,
                %error,
                "notification POST failed; leaving the key unrecorded so the next poll retries it",
            );
            report.stalled = true;
            break;
        }
        store.record_notification(&event.key, now_unix_millis)?;
        report.posted += 1;
    }

    Ok(report)
}

/// The host clock, in unix milliseconds — the ledger's `posted_unix_millis`.
fn now_unix_millis() -> u64 {
    #[allow(clippy::disallowed_methods, reason = "host wall clock, not capability configuration")]
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

#[runtime]
impl NativeActor for NotifyReactorCapability {
    type State = NotifyReactorState;
    type Config = ();
    type Params = NotifyReactorSetup;

    const NAMESPACE: &'static str = "aether.bloomery.notify";

    fn init((): (), config: NotifyReactorSetup, ctx: &mut NativeInitCtx<'_>) -> Result<NotifyReactorState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let control_mailbox = <ControlCore as Addressable>::resolve(0, ());

        // Unconfigured → disabled: no sink, no store, no timer. Exactly the
        // mirror's posture with no token — a coordinator with nowhere to shout
        // is not a broken coordinator, and the ledger simply stays empty.
        let Some(sink) = config.sink else {
            tracing::info!(
                target: "aether_chassis_bloomery::notify",
                "notification reactor mounted disabled (no webhook file configured or readable)",
            );
            return Ok(NotifyReactorState {
                sink: None,
                store: None,
                control_mailbox,
                mailer,
                self_mailbox,
                _timer: None,
            });
        };

        let store = SqliteStore::open(&config.store_path).map_err(|error| BootError::Other(Box::new(error)))?;
        let interval = Duration::from_secs(config.poll_interval_secs.max(1));
        let timer = spawn_timer(
            Arc::clone(&mailer),
            self_mailbox,
            NotifyTick::ID,
            NotifyTick::default().encode_into_bytes(),
            "aether-bloomery-notify",
            interval,
        );
        tracing::info!(
            target: "aether_chassis_bloomery::notify",
            poll_interval_secs = config.poll_interval_secs,
            "notification reactor mounted; posting loud transitions to the configured webhook",
        );
        Ok(NotifyReactorState {
            sink: Some(sink),
            store: Some(store),
            control_mailbox,
            mailer,
            self_mailbox,
            _timer: Some(timer),
        })
    }

    /// Fire an immediate boot tick so a condition that went loud while the
    /// coordinator was down is reported without waiting a full poll interval.
    /// Disabled reactors push nothing.
    fn wire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        if state.sink.is_some() {
            let wake = NotifyTick::default().encode_into_bytes();
            state.mailer.push(Mail::new(state.self_mailbox, NotifyTick::ID, wake, 1));
        }
    }

    /// Poll wake: ask the control core for the live document. The answer
    /// arrives as [`QueryResult`] on this mailbox.
    #[handler::single]
    fn on_notify_tick(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: NotifyTick) {
        if state.sink.is_none() {
            return;
        }
        let query = Query { selector: QuerySelector::Document };
        // Fire-and-forget: a dropped read costs one poll interval, and the
        // ledger makes the next pass produce exactly the same difference.
        let _ = ctx.send_envelope_detached(state.control_mailbox, Query::ID, &query.encode_into_bytes());
    }

    /// The control core's answer: difference the document's loud set against
    /// the ledger and post what is new.
    #[handler::single]
    fn on_query_result(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: QueryResult) {
        let Some(sink) = state.sink.clone() else {
            return;
        };
        let Some(store) = state.store.as_mut() else {
            return;
        };
        // Only the whole-document read is ours. A `Bloom` / `Release` /
        // `NotFound` reply belongs to a query this reactor never sent, and an
        // `Err` is the control core saying it could not encode the projection
        // — neither is a loud set, and neither is worth a ledger write.
        let document = match mail {
            QueryResult::Document { document } => document,
            QueryResult::Err { error } => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::notify",
                    %error,
                    "control core could not project the view document; nothing posted this poll",
                );
                return;
            }
            _ => return,
        };
        let Ok(view) = from_bytes::<ViewDocument>(&document) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::notify",
                "view document did not decode; nothing posted this poll",
            );
            return;
        };

        match deliver(store, sink.as_ref(), &view, now_unix_millis()) {
            Ok(report) if report.posted > 0 || report.forgotten > 0 || report.seeded > 0 => tracing::info!(
                target: "aether_chassis_bloomery::notify",
                posted = report.posted,
                forgotten = report.forgotten,
                seeded = report.seeded,
                stalled = report.stalled,
                "notification pass complete",
            ),
            Ok(_) => {}
            Err(error) => tracing::warn!(
                target: "aether_chassis_bloomery::notify",
                %error,
                "notification ledger read/write failed; the pass is retried on the next poll",
            ),
        }
    }
}
