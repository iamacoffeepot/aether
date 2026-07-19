//! Typed outbox scoping for the host drivers.
//!
//! [`StoreBackend`]'s outbox methods take `&str` because the store's mail
//! surface is open — any caller-defined topic enqueues, drains, and acks by
//! name. The drivers that consume the *reducer's* topics scope through this
//! extension instead: the [`Topic`](aether_bloomery::Topic)-to-string
//! conversion happens here, once, so a driver call site cannot scope by an
//! arbitrary string — the hole the `Topic` type exists to close.

use aether_bloomery::Topic;

use crate::store::{OutboxEntry, StoreBackend};

/// Reducer-[`Topic`]-scoped outbox access over a [`StoreBackend`] — the
/// drivers' typed edge above the open string surface.
pub trait TopicOutbox {
    /// Read `topic`'s undelivered entries, in sequence order.
    fn drain_topic(&mut self, topic: Topic) -> rusqlite::Result<Vec<OutboxEntry>>;
    /// Mark `topic`'s entries at or below `through_sequence` delivered;
    /// returns how many were newly acknowledged.
    fn ack_topic(&mut self, topic: Topic, through_sequence: u64) -> rusqlite::Result<u32>;
    /// Enqueue `payload` under `topic`; returns its sequence. Test seeding —
    /// production enqueue rides the combined `Commit` via `OutboxPayload::new`.
    fn enqueue_topic(&mut self, topic: Topic, payload: &[u8]) -> rusqlite::Result<u64>;
}

impl<S: StoreBackend + ?Sized> TopicOutbox for S {
    fn drain_topic(&mut self, topic: Topic) -> rusqlite::Result<Vec<OutboxEntry>> {
        self.drain_outbox(Some(topic.as_str()))
    }

    fn ack_topic(&mut self, topic: Topic, through_sequence: u64) -> rusqlite::Result<u32> {
        self.ack_outbox(Some(topic.as_str()), through_sequence)
    }

    fn enqueue_topic(&mut self, topic: Topic, payload: &[u8]) -> rusqlite::Result<u64> {
        self.enqueue_outbox(topic.as_str(), payload)
    }
}
