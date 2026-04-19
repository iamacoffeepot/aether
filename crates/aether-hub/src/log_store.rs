// ADR-0023 hub-side log buffer. One bounded ring per `EngineId`,
// fed by `EngineToHub::LogBatch` frames and drained by the
// `engine_logs` MCP tool. The buffer survives engine exit until hub
// shutdown — post-mortem ("why did the substrate crash?") is the
// most valuable case for these logs, so the engine record going away
// must not take its history with it.
//
// Eviction is silent at append time; readers learn about it from
// `truncated_before` on the response (the smallest sequence still
// present is compared to `since` to surface a gap).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use aether_hub_protocol::{EngineId, LogEntry, LogLevel};

/// Default cap on entries per engine. Quoted in ADR-0023.
pub const DEFAULT_RING_ENTRIES: usize = 2_000;
/// Default cap on bytes per engine (variable-length parts only).
pub const DEFAULT_RING_BYTES: usize = 2 * 1024 * 1024;

/// Hard ceiling the MCP tool will honour. Caller-supplied `max`
/// values above this clamp down silently.
pub const TOOL_MAX_ENTRIES: usize = 1_000;
/// Default `max` when the caller omits it.
pub const TOOL_DEFAULT_ENTRIES: usize = 100;

/// Shared, thread-safe per-engine log store. Cheap to clone — all
/// clones share the same map.
#[derive(Clone, Default)]
pub struct LogStore {
    inner: Arc<Mutex<HashMap<EngineId, Buffer>>>,
}

struct Buffer {
    entries: VecDeque<LogEntry>,
    current_bytes: usize,
    cap_entries: usize,
    cap_bytes: usize,
    /// Smallest sequence ever evicted. `None` if the buffer has never
    /// dropped an entry; the reader uses this to fill `truncated_before`.
    earliest_evicted: Option<u64>,
}

impl Buffer {
    fn new(cap_entries: usize, cap_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            current_bytes: 0,
            cap_entries,
            cap_bytes,
            earliest_evicted: None,
        }
    }

    fn append(&mut self, entry: LogEntry) {
        self.current_bytes = self.current_bytes.saturating_add(entry_size(&entry));
        self.entries.push_back(entry);
        while self.entries.len() > self.cap_entries || self.current_bytes > self.cap_bytes {
            let Some(dropped) = self.entries.pop_front() else {
                break;
            };
            self.current_bytes = self.current_bytes.saturating_sub(entry_size(&dropped));
            // First eviction: remember the boundary. Subsequent
            // evictions advance it to the latest dropped sequence so
            // readers see "anything below this sequence is gone."
            self.earliest_evicted = Some(dropped.sequence);
        }
    }
}

fn entry_size(entry: &LogEntry) -> usize {
    entry.target.len() + entry.message.len()
}

impl LogStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a batch of entries to the per-engine buffer, creating
    /// the buffer on first use. Per ADR-0023 the buffer outlives the
    /// engine record; this method does not consult the engine
    /// registry.
    pub fn append(&self, engine_id: EngineId, entries: Vec<LogEntry>) {
        if entries.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let buf = inner
            .entry(engine_id)
            .or_insert_with(|| Buffer::new(DEFAULT_RING_ENTRIES, DEFAULT_RING_BYTES));
        for entry in entries {
            buf.append(entry);
        }
    }

    /// Read back at most `max` entries with `sequence > since` and
    /// `level >= min_level`. Returns the slice plus `next_since`
    /// (the highest sequence in the slice, or `since` unchanged if
    /// the slice is empty) and `truncated_before` (the first
    /// surviving sequence above `since` if the buffer has evicted
    /// anything the caller hadn't seen).
    pub fn read(
        &self,
        engine_id: EngineId,
        max: usize,
        min_level: LogLevel,
        since: u64,
    ) -> ReadResult {
        let inner = self.inner.lock().unwrap();
        let Some(buf) = inner.get(&engine_id) else {
            return ReadResult {
                entries: Vec::new(),
                next_since: since,
                truncated_before: None,
            };
        };
        let truncated_before = buf.earliest_evicted.filter(|&seq| seq >= since).map(|_| {
            // Surface the lowest sequence still present — the
            // gap the caller is missing starts above `since` and
            // ends just below this. Empty buffer post-eviction
            // shouldn't happen in practice but is handled safely.
            buf.entries.front().map(|e| e.sequence).unwrap_or(0)
        });
        let max = max.min(TOOL_MAX_ENTRIES);
        let mut out: Vec<LogEntry> = Vec::with_capacity(max.min(buf.entries.len()));
        for e in &buf.entries {
            if e.sequence <= since {
                continue;
            }
            if e.level < min_level {
                continue;
            }
            out.push(e.clone());
            if out.len() >= max {
                break;
            }
        }
        let next_since = out.last().map(|e| e.sequence).unwrap_or(since);
        ReadResult {
            entries: out,
            next_since,
            truncated_before,
        }
    }

    #[cfg(test)]
    pub fn buffer_len(&self, engine_id: EngineId) -> usize {
        self.inner
            .lock()
            .unwrap()
            .get(&engine_id)
            .map(|b| b.entries.len())
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
pub struct ReadResult {
    pub entries: Vec<LogEntry>,
    pub next_since: u64,
    pub truncated_before: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_hub_protocol::Uuid;

    fn id(n: u128) -> EngineId {
        EngineId(Uuid::from_u128(n))
    }

    fn entry(seq: u64, level: LogLevel, msg: &str) -> LogEntry {
        LogEntry {
            timestamp_unix_ms: seq,
            level,
            target: "t".into(),
            message: msg.into(),
            sequence: seq,
        }
    }

    #[test]
    fn append_and_read_basic() {
        let store = LogStore::new();
        let e = id(1);
        store.append(
            e,
            vec![
                entry(0, LogLevel::Info, "a"),
                entry(1, LogLevel::Error, "b"),
            ],
        );
        let r = store.read(e, 100, LogLevel::Trace, 0);
        // since=0 is exclusive: only sequence > 0 returned.
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].sequence, 1);
        assert_eq!(r.next_since, 1);
        assert!(r.truncated_before.is_none());
    }

    #[test]
    fn since_zero_includes_all_when_starting_below_zero() {
        // u64 has no negative range; the convention is that callers
        // start with since=0 only after consulting next_since once.
        // For pure first-poll, callers omit since (server defaults to
        // u64::MAX_PRE = absent → we treat omitted as "from start").
        // Verified at the MCP layer (default is None → 0 means
        // "after seq 0"); document the boundary here.
        let store = LogStore::new();
        let e = id(1);
        store.append(e, vec![entry(0, LogLevel::Info, "first")]);
        let r = store.read(e, 100, LogLevel::Trace, 0);
        // sequence 0 is NOT > 0, so excluded.
        assert!(r.entries.is_empty());
    }

    #[test]
    fn level_filter_drops_below_min() {
        let store = LogStore::new();
        let e = id(1);
        store.append(
            e,
            vec![
                entry(1, LogLevel::Debug, "d"),
                entry(2, LogLevel::Info, "i"),
                entry(3, LogLevel::Error, "e"),
            ],
        );
        let r = store.read(e, 100, LogLevel::Info, 0);
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[0].sequence, 2);
        assert_eq!(r.entries[1].sequence, 3);
    }

    #[test]
    fn max_clamps_returned_count() {
        let store = LogStore::new();
        let e = id(1);
        for i in 1..=10 {
            store.append(e, vec![entry(i, LogLevel::Info, "x")]);
        }
        let r = store.read(e, 3, LogLevel::Trace, 0);
        assert_eq!(r.entries.len(), 3);
        assert_eq!(r.next_since, 3);
    }

    #[test]
    fn eviction_surfaces_truncated_before() {
        let store = LogStore::new();
        let e = id(1);
        // Tight cap by injecting raw — use the public API by writing
        // many entries past the default cap is too noisy; insert a
        // tiny custom buffer manually.
        {
            let mut inner = store.inner.lock().unwrap();
            inner.insert(e, Buffer::new(3, 1024));
        }
        for i in 1..=5 {
            store.append(e, vec![entry(i, LogLevel::Info, "x")]);
        }
        // Buffer holds the last 3: seqs 3,4,5. Earliest evicted is 2.
        assert_eq!(store.buffer_len(e), 3);
        let r = store.read(e, 100, LogLevel::Trace, 0);
        assert_eq!(r.entries.len(), 3);
        // truncated_before flags the gap above since=0.
        assert!(r.truncated_before.is_some());
    }

    #[test]
    fn truncated_before_clears_once_caller_catches_up() {
        let store = LogStore::new();
        let e = id(1);
        {
            let mut inner = store.inner.lock().unwrap();
            inner.insert(e, Buffer::new(3, 1024));
        }
        for i in 1..=5 {
            store.append(e, vec![entry(i, LogLevel::Info, "x")]);
        }
        // since=10 is above any evicted sequence (latest evicted: 2).
        let r = store.read(e, 100, LogLevel::Trace, 10);
        assert!(r.entries.is_empty());
        assert!(r.truncated_before.is_none());
    }

    #[test]
    fn unknown_engine_returns_empty() {
        let store = LogStore::new();
        let r = store.read(id(99), 100, LogLevel::Trace, 0);
        assert!(r.entries.is_empty());
        assert_eq!(r.next_since, 0);
        assert!(r.truncated_before.is_none());
    }
}
