//! Request-context table shared by wasm guests and native actors.
//!
//! The table is keyed by the reply correlation id minted for an outbound
//! request. Context values are ordinary `Kind`s, so the stored bytes carry a
//! schema-derived `KindId` and can be restored across guest replacement.
//!
//! Request ids are monotonic per mailbox (ADR-0139 §3): a native actor mints
//! them from its binding's counter, and a wasm guest's counter carries across
//! `replace_component`. An id never repeats within a mailbox's life, so a
//! reply that arrives after its context was taken finds no entry, never a
//! newer request's context.
//!
//! The table never drops a stored context, because a context can hold the
//! caller's reply target. It reserves room on its first insert, reuses the
//! room a take frees, and grows when more requests are in flight than that
//! room holds. Each time the live count passes a new high-water mark, the
//! table's owner logs a warning, so a peer that never replies shows up as
//! warnings and memory growth, never as a lost reply.
//!
//! The snapshot keeps the layout older SDKs wrote: `next_seq`, `count`, then
//! per entry `request`, `kind`, `insert_seq`, `len`, `bytes`. The table does
//! not track insertion sequence, so restore ignores both sequence fields.
//! The writer sets `next_seq` to the entry count and each `insert_seq` to the
//! entry's position in id order, which an older reader takes as the same age
//! order. Keeping the layout, rather than bumping the envelope version, lets
//! snapshots cross in both directions: an older reader treats an unknown
//! version as plain user state.

use alloc::vec::Vec;
use core::hash::{BuildHasher, Hasher};

use aether_data::{Kind, KindId, RequestId, Source};
use hashbrown::HashMap;

/// Room for request contexts each actor reserves on its first insert. A take
/// frees its room for the next insert; the table grows past this when more
/// requests are in flight, and each high-water mark it passes (this count,
/// then each doubling) logs a warning.
const PREALLOCATED_REQUEST_CONTEXTS: usize = 256;

/// The smallest snapshot entry: `request`, `kind` and `insert_seq` at 8 bytes
/// each plus a 4-byte payload length. Restore bounds a claimed count by it
/// before reserving anything.
const SNAPSHOT_ENTRY_MIN_BYTES: usize = 28;

const ENVELOPE_VERSION: u32 = 0xAEC0_0001;
const ENVELOPE_MAGIC: &[u8; 8] = b"AECTX001";

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestContextEntry {
    kind: KindId,
    bytes: Vec<u8>,
}

/// Fixed multiplicative hash over a request id. The actor mints its own
/// request ids, so nothing needs a seeded hash.
#[derive(Clone, Copy, Default, Debug)]
struct RequestIdHasher(u64);

impl BuildHasher for RequestIdHasher {
    type Hasher = Self;

    fn build_hasher(&self) -> Self {
        Self(0)
    }
}

impl Hasher for RequestIdHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0 ^ value).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Per-actor request-context table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestContextTable {
    entries: HashMap<RequestId, RequestContextEntry, RequestIdHasher>,
    preallocated: usize,
    next_warning: usize,
}

impl RequestContextTable {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: HashMap::with_hasher(RequestIdHasher(0)),
            preallocated: PREALLOCATED_REQUEST_CONTEXTS,
            next_warning: PREALLOCATED_REQUEST_CONTEXTS,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The kind of every stored context, one per entry. The host's replace
    /// check reads it to refuse a replacement that cannot take a carried
    /// context (#6429).
    pub fn kinds(&self) -> impl Iterator<Item = KindId> + '_ {
        self.entries.values().map(|entry| entry.kind)
    }

    /// Store a context under `request`, replacing any older entry for the same
    /// correlation id. A no-correlation request is ignored because no reply can
    /// recover it exactly. A new request never displaces a stored one: the
    /// first insert reserves the preallocated room, and the table grows when
    /// that room is full.
    pub fn insert<C: Kind>(&mut self, request: RequestId, context: &C) {
        if request.0 == Source::NO_CORRELATION {
            tracing::warn!(kind = C::NAME, "request context not stored: request has no correlation id",);
            return;
        }

        if self.entries.capacity() == 0 {
            self.entries.reserve(self.preallocated);
        }
        self.entries.insert(request, RequestContextEntry { kind: C::ID, bytes: context.encode_into_bytes() });
    }

    /// The live count, once each time it passes the next high-water mark; the
    /// mark starts at the preallocated room and doubles past each count it
    /// reports, so a burst warns a few times and a leak keeps warning as it
    /// grows. The two table owners call it after an insert and log the
    /// warning.
    pub fn high_water(&mut self) -> Option<usize> {
        let live = self.entries.len();
        if live <= self.next_warning {
            return None;
        }

        while self.next_warning < live {
            self.next_warning = self.next_warning.saturating_mul(2).max(1);
        }
        Some(live)
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
        let decoded = C::decode_from_bytes(&entry.bytes);
        if decoded.is_none() {
            tracing::warn!(request = request.0, kind = C::ID.0, "request context decode failed",);
        }
        decoded
    }

    #[must_use]
    pub fn snapshot_bytes(&self) -> Vec<u8> {
        let mut entries: Vec<_> = self.entries.iter().collect();
        entries.sort_unstable_by_key(|(request, _)| **request);

        let mut out = Vec::new();
        push_u64(&mut out, entries.len() as u64);
        push_len(&mut out, entries.len());
        for (position, (request, entry)) in (0_u64..).zip(entries) {
            push_u64(&mut out, request.0);
            push_u64(&mut out, entry.kind.0);
            push_u64(&mut out, position);
            push_len(&mut out, entry.bytes.len());
            out.extend_from_slice(&entry.bytes);
        }
        out
    }

    pub fn restore_snapshot_bytes(&mut self, bytes: &[u8]) -> bool {
        let mut cursor = bytes;
        let (Some(_next_seq), Some(count)) = (take_u64(&mut cursor), take_u32(&mut cursor)) else {
            return false;
        };
        let count = count as usize;
        if count > cursor.len() / SNAPSHOT_ENTRY_MIN_BYTES {
            return false;
        }

        let mut entries = HashMap::with_capacity_and_hasher(count.max(self.preallocated), RequestIdHasher(0));
        for _ in 0..count {
            let (Some(request), Some(kind), Some(_insert_seq), Some(len)) =
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

            let entry = RequestContextEntry { kind: KindId(kind), bytes: payload.to_vec() };
            if entries.insert(RequestId(request), entry).is_some() {
                return false;
            }
        }
        if !cursor.is_empty() {
            return false;
        }

        self.entries = entries;
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
pub(crate) fn compose_state_envelope(
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

    /// A new request on a table past its preallocated room must grow the
    /// table, never evict or overwrite a stored context: a context can hold
    /// the reply its caller is owed.
    #[test]
    fn a_full_table_grows_and_keeps_every_context() {
        let mut table = RequestContextTable { preallocated: 3, ..RequestContextTable::new() };
        for value in 0..7 {
            table.insert(RequestId(100 + u64::from(value)), &TestContext { value });
        }

        for value in 0..7 {
            assert_eq!(table.take::<TestContext>(RequestId(100 + u64::from(value))), Some(TestContext { value }));
        }
    }

    /// The high-water warning fires once per doubling past the preallocated
    /// room: not on every insert (a log flood under a burst), not never (a
    /// leak stays invisible), and not again after a drain re-arms it.
    #[test]
    fn high_water_warns_once_per_doubling() {
        let mut table = RequestContextTable { preallocated: 2, next_warning: 2, ..RequestContextTable::new() };
        let marks: Vec<_> = (1..=5)
            .map(|request| {
                table.insert(RequestId(request), &TestContext { value: 0 });
                table.high_water()
            })
            .collect();
        assert_eq!(marks, [None, None, Some(3), None, Some(5)]);

        for request in 1..=5 {
            assert!(table.take::<TestContext>(RequestId(request)).is_some());
        }
        let refilled: Vec<_> = (6..=10)
            .map(|request| {
                table.insert(RequestId(request), &TestContext { value: 0 });
                table.high_water()
            })
            .collect();
        assert_eq!(refilled, [None; 5]);
    }

    /// An older SDK wrote entries in insertion order with its own sequence
    /// numbers; a guest on this SDK must still restore that snapshot, whatever
    /// the sequence fields hold.
    #[test]
    fn older_sdk_snapshot_restores_whatever_its_sequence_fields_hold() {
        let mut bytes = Vec::new();
        push_u64(&mut bytes, 5);
        push_len(&mut bytes, 2);
        for (request, value, insert_seq) in [(9, 1, 40), (7, 2, 12)] {
            let payload = TestContext { value }.encode_into_bytes();
            push_u64(&mut bytes, request);
            push_u64(&mut bytes, TestContext::ID.0);
            push_u64(&mut bytes, insert_seq);
            push_len(&mut bytes, payload.len());
            bytes.extend_from_slice(&payload);
        }

        let mut restored = RequestContextTable::new();
        assert!(restored.restore_snapshot_bytes(&bytes));
        assert_eq!(restored.take::<TestContext>(RequestId(9)), Some(TestContext { value: 1 }));
        assert_eq!(restored.take::<TestContext>(RequestId(7)), Some(TestContext { value: 2 }));
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

    /// A grown table carries across replace: a snapshot larger than the
    /// successor's preallocated room restores in full.
    #[test]
    fn restore_accepts_a_snapshot_larger_than_the_reservation() {
        let mut table = RequestContextTable::new();
        for value in 1..=4 {
            table.insert(RequestId(u64::from(value)), &TestContext { value });
        }

        let mut restored = RequestContextTable { preallocated: 3, ..RequestContextTable::new() };
        assert!(restored.restore_snapshot_bytes(&table.snapshot_bytes()));
        for value in 1..=4 {
            assert_eq!(restored.take::<TestContext>(RequestId(u64::from(value))), Some(TestContext { value }));
        }
    }

    /// A corrupt count cannot force a huge reservation: a header claiming more
    /// entries than its payload could hold is refused before anything is
    /// reserved, and the existing table is left untouched.
    #[test]
    fn restore_rejects_a_count_the_payload_cannot_hold() {
        let mut table = RequestContextTable::new();
        table.insert(RequestId(7), &TestContext { value: 42 });
        let before = table.clone();
        let mut bytes = Vec::new();
        push_u64(&mut bytes, 9);
        push_u32(&mut bytes, u32::MAX);

        assert!(!table.restore_snapshot_bytes(&bytes));
        assert_eq!(table, before);
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
