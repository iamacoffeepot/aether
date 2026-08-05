//! Request-context table shared by wasm guests and native actors.
//!
//! The table is keyed by the reply correlation id minted for an outbound
//! request. Context values are ordinary `Kind`s, so the stored bytes carry a
//! schema-derived `KindId` and can be restored across guest replacement.

use alloc::vec::Vec;

use aether_data::{Kind, KindId, RequestId, Source};

/// Default per-actor cap on remembered request contexts.
pub const REQUEST_CONTEXT_CAPACITY: usize = 1024;

const ENVELOPE_VERSION: u32 = 0xAEC0_0001;
const ENVELOPE_MAGIC: &[u8; 8] = b"AECTX001";

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestContextEntry {
    request: RequestId,
    kind: KindId,
    bytes: Vec<u8>,
    insert_seq: u64,
}

/// Per-actor request-context table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestContextTable {
    entries: Vec<RequestContextEntry>,
    next_seq: u64,
    capacity: usize,
}

impl RequestContextTable {
    #[must_use]
    pub const fn new() -> Self {
        Self { entries: Vec::new(), next_seq: 0, capacity: REQUEST_CONTEXT_CAPACITY }
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

        if let Some(existing) = self.entries.iter_mut().find(|entry| entry.request == request) {
            existing.kind = C::ID;
            existing.bytes = context.encode_into_bytes();
            existing.insert_seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            return;
        }

        if self.entries.len() >= self.capacity
            && let Some((oldest, _)) = self.entries.iter().enumerate().min_by_key(|(_, entry)| entry.insert_seq)
        {
            let dropped = self.entries.remove(oldest);
            tracing::warn!(
                request = dropped.request.0,
                kind = dropped.kind.0,
                age = self.next_seq.saturating_sub(dropped.insert_seq),
                "request context table full; dropped oldest context",
            );
        }

        self.entries.push(RequestContextEntry {
            request,
            kind: C::ID,
            bytes: context.encode_into_bytes(),
            insert_seq: self.next_seq,
        });
        self.next_seq = self.next_seq.wrapping_add(1);
    }

    /// Remove and decode the context associated with `request`.
    ///
    /// Wrong-kind and decode failures consume the stored entry. A reply is a
    /// one-shot event, and retaining a malformed or wrong-type context would
    /// make a later handler observe stale bookkeeping.
    pub fn take<C: Kind>(&mut self, request: RequestId) -> Option<C> {
        let index = self.entries.iter().position(|entry| entry.request == request)?;
        let entry = self.entries.remove(index);
        if entry.kind != C::ID {
            tracing::warn!(
                request = request.0,
                expected_kind = C::ID.0,
                actual_kind = entry.kind.0,
                "request context kind mismatch",
            );
            return None;
        }
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
        for entry in &self.entries {
            push_u64(&mut out, entry.request.0);
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
        let mut entries = Vec::with_capacity(count as usize);
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
            entries.push(RequestContextEntry {
                request: RequestId(request),
                kind: KindId(kind),
                bytes: payload.to_vec(),
                insert_seq,
            });
        }
        if !cursor.is_empty() {
            return false;
        }
        self.entries = entries;
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

    #[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
    #[kind(name = "test.request_context")]
    struct TestContext {
        value: u32,
    }

    #[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
    #[kind(name = "test.other_request_context")]
    struct OtherContext {
        value: u32,
    }

    #[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
    #[kind(name = "test.source_request_context")]
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
    fn wrong_kind_take_consumes_entry() {
        let mut table = RequestContextTable::new();
        table.insert(RequestId(7), &TestContext { value: 42 });
        assert_eq!(table.take::<OtherContext>(RequestId(7)), None);
        assert_eq!(table.take::<TestContext>(RequestId(7)), None);
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
