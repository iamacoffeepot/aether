//! Request-context table shared by wasm guests and native actors.
//!
//! The table is keyed by the reply correlation id minted for an outbound
//! request. Context values are ordinary `Kind`s, so the stored bytes carry a
//! schema-derived `KindId` and can be restored across guest replacement.
//!
//! Entries are found by key. Capacity eviction follows the table's own
//! `insert_seq`, kept in a separate age index, never the request id: request
//! ids are minted outside the table, so their order says nothing about when a
//! context was stored, while `insert_seq` is persisted in the snapshot and
//! continues from it after a restore.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use aether_data::{Kind, KindId, RequestId, Source};

/// Default per-actor cap on remembered request contexts.
pub const REQUEST_CONTEXT_CAPACITY: usize = 1024;

const ENVELOPE_VERSION: u32 = 0xAEC0_0001;
const ENVELOPE_MAGIC: &[u8; 8] = b"AECTX001";

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestContextEntry {
    kind: KindId,
    bytes: Vec<u8>,
    insert_seq: u64,
}

/// Per-actor request-context table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestContextTable {
    entries: BTreeMap<RequestId, RequestContextEntry>,
    by_age: BTreeMap<u64, RequestId>,
    next_seq: u64,
    capacity: usize,
}

impl RequestContextTable {
    #[must_use]
    pub const fn new() -> Self {
        Self { entries: BTreeMap::new(), by_age: BTreeMap::new(), next_seq: 0, capacity: REQUEST_CONTEXT_CAPACITY }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Store a context under `request`, replacing any older entry for the same
    /// correlation id. A no-correlation request is ignored because no reply can
    /// recover it exactly.
    pub fn insert<C: Kind>(&mut self, request: RequestId, context: &C) {
        if request.0 == Source::NO_CORRELATION {
            tracing::warn!(kind = C::NAME, "request context not stored: request has no correlation id",);
            return;
        }

        let insert_seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);

        if let Some(existing) = self.entries.get_mut(&request) {
            self.by_age.remove(&existing.insert_seq);
            existing.kind = C::ID;
            existing.bytes = context.encode_into_bytes();
            existing.insert_seq = insert_seq;
            self.by_age.insert(insert_seq, request);
            return;
        }

        if self.entries.len() >= self.capacity
            && let Some((dropped_seq, dropped_request)) = self.by_age.pop_first()
            && let Some(dropped) = self.entries.remove(&dropped_request)
        {
            tracing::warn!(
                request = dropped_request.0,
                kind = dropped.kind.0,
                age = insert_seq.saturating_sub(dropped_seq),
                "request context table full; dropped oldest context",
            );
        }

        self.entries
            .insert(request, RequestContextEntry { kind: C::ID, bytes: context.encode_into_bytes(), insert_seq });
        self.by_age.insert(insert_seq, request);
    }

    /// Remove and decode the context associated with `request`.
    ///
    /// A take of the wrong type returns `None` and leaves the entry stored, so
    /// a reply handler that serves several context kinds tries each type in
    /// turn. A matching kind removes the entry; if its bytes then fail to
    /// decode, the entry is consumed with a warning, since no other type could
    /// ever take it.
    pub fn take<C: Kind>(&mut self, request: RequestId) -> Option<C> {
        if self.entries.get(&request)?.kind != C::ID {
            return None;
        }

        let entry = self.entries.remove(&request)?;
        self.by_age.remove(&entry.insert_seq);

        let decoded = C::decode_from_bytes(&entry.bytes);
        if decoded.is_none() {
            tracing::warn!(request = request.0, kind = C::ID.0, "request context decode failed",);
        }
        decoded
    }

    #[must_use]
    pub fn snapshot_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        push_u64(&mut out, self.next_seq);
        push_len(&mut out, self.entries.len());
        for request in self.by_age.values() {
            let entry = &self.entries[request];
            push_u64(&mut out, request.0);
            push_u64(&mut out, entry.kind.0);
            push_u64(&mut out, entry.insert_seq);
            push_len(&mut out, entry.bytes.len());
            out.extend_from_slice(&entry.bytes);
        }
        out
    }

    pub fn restore_snapshot_bytes(&mut self, bytes: &[u8]) -> bool {
        let mut cursor = bytes;
        let Some(next_seq) = take_u64(&mut cursor) else {
            return false;
        };
        let Some(count) = take_u32(&mut cursor) else {
            return false;
        };
        if count as usize > self.capacity {
            return false;
        }

        let mut entries = BTreeMap::new();
        let mut by_age = BTreeMap::new();
        for _ in 0..count {
            let (Some(request), Some(kind), Some(insert_seq), Some(len)) =
                (take_u64(&mut cursor), take_u64(&mut cursor), take_u64(&mut cursor), take_u32(&mut cursor))
            else {
                return false;
            };
            let len = len as usize;
            if cursor.len() < len {
                return false;
            }
            let (payload, rest) = cursor.split_at(len);
            cursor = rest;

            // A sequence at or past `next_seq` would collide with the next
            // insert's age key; the writer never produces one.
            let request = RequestId(request);
            if insert_seq >= next_seq || by_age.insert(insert_seq, request).is_some() {
                return false;
            }
            let entry = RequestContextEntry { kind: KindId(kind), bytes: payload.to_vec(), insert_seq };
            if entries.insert(request, entry).is_some() {
                return false;
            }
        }
        if !cursor.is_empty() {
            return false;
        }

        self.entries = entries;
        self.by_age = by_age;
        self.next_seq = next_seq;
        true
    }
}

impl Default for RequestContextTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Compose the SDK request-context snapshot with the user/inline-child state
/// bundle that already occupies the single `save_state` slot.
#[must_use]
pub fn compose_state_envelope(
    table: &RequestContextTable,
    user_state: Option<(u32, Vec<u8>)>,
) -> Option<(u32, Vec<u8>)> {
    if table.is_empty() {
        return user_state;
    }

    let (user_version, user_bytes) = user_state.unwrap_or((0, Vec::new()));
    let table_bytes = table.snapshot_bytes();
    let mut out = Vec::new();
    out.extend_from_slice(ENVELOPE_MAGIC);
    push_len(&mut out, table_bytes.len());
    out.extend_from_slice(&table_bytes);
    push_u32(&mut out, user_version);
    push_len(&mut out, user_bytes.len());
    out.extend_from_slice(&user_bytes);
    Some((ENVELOPE_VERSION, out))
}

/// Split a prior-state bundle into the restored request-context snapshot and
/// the user/inline-child state to pass to existing rehydrate code. Old-format
/// state is returned unchanged with an empty table.
#[must_use]
pub fn split_state_envelope(version: u32, bytes: &[u8]) -> (RequestContextTable, u32, Vec<u8>) {
    if version != ENVELOPE_VERSION || !bytes.starts_with(ENVELOPE_MAGIC) {
        return (RequestContextTable::new(), version, bytes.to_vec());
    }

    let mut cursor = &bytes[ENVELOPE_MAGIC.len()..];
    let Some(table_len) = take_u32(&mut cursor) else {
        tracing::warn!("request context state envelope truncated before table length");
        return (RequestContextTable::new(), 0, Vec::new());
    };
    let table_len = table_len as usize;
    if cursor.len() < table_len {
        tracing::warn!("request context state envelope truncated in table payload");
        return (RequestContextTable::new(), 0, Vec::new());
    }
    let (table_payload, rest) = cursor.split_at(table_len);
    cursor = rest;
    let Some(user_version) = take_u32(&mut cursor) else {
        tracing::warn!("request context state envelope truncated before user version");
        return (RequestContextTable::new(), 0, Vec::new());
    };
    let Some(user_len) = take_u32(&mut cursor) else {
        tracing::warn!("request context state envelope truncated before user length");
        return (RequestContextTable::new(), 0, Vec::new());
    };
    let user_len = user_len as usize;
    if cursor.len() < user_len {
        tracing::warn!("request context state envelope truncated in user payload");
        return (RequestContextTable::new(), 0, Vec::new());
    }
    let (user_payload, rest) = cursor.split_at(user_len);
    if !rest.is_empty() {
        tracing::warn!("request context state envelope has trailing bytes");
        return (RequestContextTable::new(), 0, Vec::new());
    }

    let mut table = RequestContextTable::new();
    if !table.restore_snapshot_bytes(table_payload) {
        tracing::warn!("request context snapshot failed to decode");
        table = RequestContextTable::new();
    }
    (table, user_version, user_payload.to_vec())
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_len(out: &mut Vec<u8>, len: usize) {
    let len = u32::try_from(len).expect("request context state length exceeds u32");
    push_u32(out, len);
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn take_u32(cursor: &mut &[u8]) -> Option<u32> {
    if cursor.len() < 4 {
        return None;
    }
    let (head, rest) = cursor.split_at(4);
    *cursor = rest;
    Some(u32::from_le_bytes(head.try_into().ok()?))
}

fn take_u64(cursor: &mut &[u8]) -> Option<u64> {
    if cursor.len() < 8 {
        return None;
    }
    let (head, rest) = cursor.split_at(8);
    *cursor = rest;
    Some(u64::from_le_bytes(head.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::{MailboxId, Source, SourceAddr};

    #[aether_data::kind(name = "test.request_context", partial_eq)]
    struct TestContext {
        value: u32,
    }

    #[aether_data::kind(name = "test.other_request_context", partial_eq)]
    struct OtherContext {
        value: u32,
    }

    #[aether_data::kind(name = "test.source_request_context", partial_eq)]
    struct SourceContext {
        source: Source,
    }

    #[test]
    fn take_removes_entry_once() {
        let mut table = RequestContextTable::new();
        table.insert(RequestId(7), &TestContext { value: 42 });
        assert_eq!(table.take::<TestContext>(RequestId(7)), Some(TestContext { value: 42 }));
        assert_eq!(table.take::<TestContext>(RequestId(7)), None);
    }

    #[test]
    fn wrong_kind_take_leaves_entry() {
        let mut table = RequestContextTable::new();
        table.insert(RequestId(7), &TestContext { value: 42 });
        assert_eq!(table.take::<OtherContext>(RequestId(7)), None);
        assert_eq!(table.take::<TestContext>(RequestId(7)), Some(TestContext { value: 42 }));
    }

    /// A replaced guest mints request ids from a counter that need not
    /// continue the old guest's, so the smallest request id is not the oldest
    /// context. Eviction must follow the age index across an overwrite, a
    /// take, and a snapshot restore.
    #[test]
    fn eviction_drops_the_oldest_context_not_the_smallest_request_id() {
        let mut table = RequestContextTable { capacity: 3, ..RequestContextTable::new() };
        table.insert(RequestId(100), &TestContext { value: 0 });
        table.insert(RequestId(101), &TestContext { value: 1 });
        table.insert(RequestId(102), &TestContext { value: 2 });
        table.insert(RequestId(100), &TestContext { value: 3 });
        assert_eq!(table.take::<TestContext>(RequestId(101)), Some(TestContext { value: 1 }));

        let mut restored = RequestContextTable { capacity: 3, ..RequestContextTable::new() };
        assert!(restored.restore_snapshot_bytes(&table.snapshot_bytes()));
        restored.insert(RequestId(1), &TestContext { value: 4 });
        restored.insert(RequestId(2), &TestContext { value: 5 });

        assert_eq!(restored.take::<TestContext>(RequestId(102)), None, "the oldest context is evicted");
        assert_eq!(restored.take::<TestContext>(RequestId(100)), Some(TestContext { value: 3 }));
        assert_eq!(restored.take::<TestContext>(RequestId(1)), Some(TestContext { value: 4 }));
        assert_eq!(restored.take::<TestContext>(RequestId(2)), Some(TestContext { value: 5 }));
    }

    #[test]
    fn snapshot_round_trips_entries() {
        let mut table = RequestContextTable::new();
        table.insert(RequestId(7), &TestContext { value: 42 });
        let bytes = table.snapshot_bytes();
        let mut restored = RequestContextTable::new();
        assert!(restored.restore_snapshot_bytes(&bytes));
        assert_eq!(restored.take::<TestContext>(RequestId(7)), Some(TestContext { value: 42 }));
    }

    #[test]
    fn restore_snapshot_rejects_count_over_capacity_before_reserve() {
        let mut table = RequestContextTable::new();
        table.insert(RequestId(7), &TestContext { value: 42 });
        let before = table.clone();
        let mut bytes = Vec::new();
        push_u64(&mut bytes, 9);
        push_len(&mut bytes, table.capacity + 1);

        assert!(!table.restore_snapshot_bytes(&bytes));
        assert_eq!(table, before, "an over-capacity snapshot leaves existing contexts untouched");
    }

    #[test]
    fn context_can_carry_source() {
        let mut table = RequestContextTable::new();
        let context = SourceContext { source: Source::with_correlation(SourceAddr::Component(MailboxId(99)), 123) };
        table.insert(RequestId(10), &context);
        assert_eq!(table.take::<SourceContext>(RequestId(10)), Some(context));
    }

    #[test]
    fn envelope_preserves_user_state_and_table() {
        let mut table = RequestContextTable::new();
        table.insert(RequestId(9), &TestContext { value: 11 });
        let (version, bytes) = compose_state_envelope(&table, Some((3, alloc::vec![1, 2, 3]))).expect("envelope");
        let (mut restored, user_version, user_bytes) = split_state_envelope(version, &bytes);
        assert_eq!(user_version, 3);
        assert_eq!(user_bytes, alloc::vec![1, 2, 3]);
        assert_eq!(restored.take::<TestContext>(RequestId(9)), Some(TestContext { value: 11 }));
    }

    #[test]
    fn old_format_state_passes_through() {
        let (table, version, bytes) = split_state_envelope(4, &[5, 6, 7]);
        assert!(table.is_empty());
        assert_eq!(version, 4);
        assert_eq!(bytes, alloc::vec![5, 6, 7]);
    }
}
