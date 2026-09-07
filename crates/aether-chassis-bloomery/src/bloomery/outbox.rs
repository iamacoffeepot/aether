//! Typed outbox scoping for the host reactors.
//!
//! [`StoreBackend`]'s outbox methods take `&str` because the store's mail
//! surface is open — any caller-defined topic enqueues, drains, and acks by
//! name. The reactors that consume the *reducer's* topics scope through this
//! extension instead: the [`Topic`]-to-string
//! conversion happens here, once, so a reactor call site cannot scope by an
//! arbitrary string — the hole the `Topic` type exists to close.

use aether_bloomery::{Admit, Event, Topic};
use aether_data::wire::to_vec;
use rusqlite::ffi::{Error as SqliteFfiError, SQLITE_ERROR};
use std::slice::from_ref;

use crate::store::{OutboxEntry, StoreBackend};

/// How a drained outbox row's persisted result batch stands relative to the
/// control journal.
///
/// The journal is the acknowledgement oracle. [`Self::Journaled`] requires **every**
/// retained event key to be present — each key is asked through
/// [`StoreBackend::journal_holds_any`] on its own, AND-ed across the batch,
/// never OR-ed as a set. [`Self::Pending`] resends the exact first-completed batch.
/// [`Self::Unrecorded`] means this exact topic+sequence row has no receipt yet.
pub enum OutboxResultDelivery {
    /// The exact topic+sequence row exists and has no result receipt.
    Unrecorded,
    /// A nonempty receipt is stored and at least one event key is absent from
    /// the journal; the admits are the full retained batch in stored order.
    Pending(Vec<Admit>),
    /// Every retained event key is in the control journal.
    Journaled,
}

/// Reducer-[`Topic`]-scoped outbox access over a [`StoreBackend`] — the
/// reactors' typed edge above the open string surface.
pub trait TopicOutbox {
    /// Read `topic`'s undelivered entries, in sequence order.
    fn drain_topic(&mut self, topic: Topic) -> rusqlite::Result<Vec<OutboxEntry>>;
    /// Mark `topic`'s entries at or below `through_sequence` delivered;
    /// returns how many were newly acknowledged.
    fn ack_topic(&mut self, topic: Topic, through_sequence: u64) -> rusqlite::Result<u32>;
    /// Read `topic`'s acknowledged entries, in sequence order.
    fn delivered_topic(&mut self, topic: Topic) -> rusqlite::Result<Vec<OutboxEntry>>;
    /// Return one of `topic`'s acknowledged entries to the undelivered queue;
    /// `true` when a row moved.
    fn redeliver_topic(&mut self, topic: Topic, sequence: u64) -> rusqlite::Result<bool>;
    /// Enqueue `payload` under `topic`; returns its sequence. Test seeding —
    /// production enqueue rides the combined `Commit` via `OutboxPayload::new`.
    /// `payload_schema` is the writing-schema identity; `None` is positional.
    fn enqueue_topic(&mut self, topic: Topic, payload: &[u8], payload_schema: Option<&str>) -> rusqlite::Result<u64>;
    /// Persist `events` as `topic`'s result receipt at `sequence`.
    fn record_topic_results(&mut self, topic: Topic, sequence: u64, events: &[Event]) -> rusqlite::Result<()>;
    /// Replay `topic`'s result receipt at `sequence` against the control journal.
    ///
    /// Reads only. Does not write the journal or ack the row.
    fn replay_topic_results(&mut self, topic: Topic, sequence: u64) -> rusqlite::Result<OutboxResultDelivery>;
}

impl<S: StoreBackend + ?Sized> TopicOutbox for S {
    fn drain_topic(&mut self, topic: Topic) -> rusqlite::Result<Vec<OutboxEntry>> {
        self.drain_outbox(Some(topic.as_str()))
    }

    fn ack_topic(&mut self, topic: Topic, through_sequence: u64) -> rusqlite::Result<u32> {
        self.ack_outbox(Some(topic.as_str()), through_sequence)
    }

    fn delivered_topic(&mut self, topic: Topic) -> rusqlite::Result<Vec<OutboxEntry>> {
        self.delivered_outbox(topic.as_str())
    }

    fn redeliver_topic(&mut self, topic: Topic, sequence: u64) -> rusqlite::Result<bool> {
        self.redeliver_outbox(topic.as_str(), sequence)
    }

    fn enqueue_topic(&mut self, topic: Topic, payload: &[u8], payload_schema: Option<&str>) -> rusqlite::Result<u64> {
        self.enqueue_outbox(topic.as_str(), payload, payload_schema)
    }

    fn record_topic_results(&mut self, topic: Topic, sequence: u64, events: &[Event]) -> rusqlite::Result<()> {
        self.record_outbox_results(topic.as_str(), sequence, events)
    }

    fn replay_topic_results(&mut self, topic: Topic, sequence: u64) -> rusqlite::Result<OutboxResultDelivery> {
        let Some(events) = self.outbox_results(topic.as_str(), sequence)? else {
            return Ok(OutboxResultDelivery::Unrecorded);
        };
        if events.is_empty() {
            return Err(named_store_error(format!(
                "outbox results for {} sequence {sequence} are empty",
                topic.as_str()
            )));
        }
        let mut journaled = true;
        for event in &events {
            journaled &= self.journal_holds_any(from_ref(&event.idempotency_key.0))?;
        }
        if journaled {
            return Ok(OutboxResultDelivery::Journaled);
        }
        encode_result_admits(&events).map(OutboxResultDelivery::Pending)
    }
}

fn encode_result_admits(events: &[Event]) -> rusqlite::Result<Vec<Admit>> {
    events
        .iter()
        .map(|event| {
            to_vec(event)
                .map(|bytes| Admit { event: bytes })
                .map_err(|error| named_store_error(format!("outbox result event did not encode: {error}")))
        })
        .collect()
}

fn named_store_error(message: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(SqliteFfiError::new(SQLITE_ERROR), Some(message))
}
