//! Typed outbox scoping for the host reactors.
//!
//! [`StoreBackend`]'s outbox methods take `&str` because the store's mail
//! surface is open — any caller-defined topic enqueues, drains, and acks by
//! name. The reactors that consume the *reducer's* topics scope through this
//! extension instead: the [`Topic`]-to-string
//! conversion happens here, once, so a reactor call site cannot scope by an
//! arbitrary string — the hole the `Topic` type exists to close.

use aether_bloomery::Topic;

use crate::store::{OutboxEntry, StoreBackend};

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
}
