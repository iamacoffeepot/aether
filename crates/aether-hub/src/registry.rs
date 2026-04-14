// Shared engine registry. Keyed by hub-assigned `EngineId`; entries
// carry display metadata and a mail channel the Claude-facing tools
// push `HubToEngine::Mail` frames into.
//
// For engines spawned by the hub (ADR-0009), the registry also owns a
// `tokio::process::Child` handle in a side map keyed by the same
// `EngineId`. Removing the engine drops the child, which — with
// `kill_on_drop(true)` — reaps the process. Externally connected
// engines have no entry in the side map; their lifecycle is owned by
// whoever launched them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aether_hub_protocol::{EngineId, HubToEngine, KindDescriptor};
use tokio::process::Child;
use tokio::sync::mpsc;

/// One entry in the hub's engine table. `mail_tx` is how any other
/// task (including MCP tool handlers) pushes frames at this engine —
/// the per-connection writer task drains the receiver.
#[derive(Clone, Debug)]
pub struct EngineRecord {
    pub id: EngineId,
    pub name: String,
    pub pid: u32,
    pub version: String,
    /// Kind vocabulary the engine declared at Hello. Used by the MCP
    /// tool surface for `describe_kinds` and for schema-driven encoding
    /// on `send_mail`.
    pub kinds: Vec<KindDescriptor>,
    pub mail_tx: mpsc::Sender<HubToEngine>,
    /// `true` if this engine was spawned by the hub (ADR-0009).
    /// `false` for externally connected substrates. Purely informational
    /// — set by the engine handshake when a pending spawn is matched.
    pub spawned: bool,
}

/// Thread-safe map of live engines. Cheap to clone; all clones share
/// the same underlying table.
#[derive(Clone, Default)]
pub struct EngineRegistry {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    records: HashMap<EngineId, EngineRecord>,
    /// Child processes the hub owns. Entry lifetime matches the
    /// corresponding record — `remove` drops both together, which
    /// kills the child via `kill_on_drop`.
    spawned_children: HashMap<EngineId, Child>,
}

impl EngineRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, record: EngineRecord) {
        self.inner.lock().unwrap().records.insert(record.id, record);
    }

    /// Attach a spawned `Child` to an already-registered engine. Called
    /// by `spawn_substrate` after handshake correlation completes.
    /// Silently replaces any prior child for the same id (which will
    /// never happen in practice but keeps the API total).
    pub fn adopt_child(&self, id: EngineId, child: Child) {
        self.inner
            .lock()
            .unwrap()
            .spawned_children
            .insert(id, child);
    }

    pub fn remove(&self, id: &EngineId) {
        let mut inner = self.inner.lock().unwrap();
        inner.records.remove(id);
        // Drop any adopted child alongside the record. `kill_on_drop`
        // takes care of reaping if the process is still running.
        inner.spawned_children.remove(id);
    }

    pub fn list(&self) -> Vec<EngineRecord> {
        self.inner
            .lock()
            .unwrap()
            .records
            .values()
            .cloned()
            .collect()
    }

    pub fn get(&self, id: &EngineId) -> Option<EngineRecord> {
        self.inner.lock().unwrap().records.get(id).cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().records.is_empty()
    }

    /// Test-only inspection of the child-handle side map. The production
    /// flow never needs to know whether a child is adopted separately
    /// from the `spawned: bool` on the record.
    #[cfg(test)]
    pub fn has_child(&self, id: &EngineId) -> bool {
        self.inner.lock().unwrap().spawned_children.contains_key(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_hub_protocol::Uuid;

    fn record(id_u128: u128) -> EngineRecord {
        let (tx, _rx) = mpsc::channel(1);
        EngineRecord {
            id: EngineId(Uuid::from_u128(id_u128)),
            name: "e".into(),
            pid: 1,
            version: "0".into(),
            kinds: vec![],
            mail_tx: tx,
            spawned: false,
        }
    }

    #[test]
    fn insert_and_remove_roundtrip() {
        let reg = EngineRegistry::new();
        let r = record(1);
        let id = r.id;
        reg.insert(r);
        assert!(reg.get(&id).is_some());
        reg.remove(&id);
        assert!(reg.get(&id).is_none());
    }

    #[test]
    fn remove_without_child_is_harmless() {
        let reg = EngineRegistry::new();
        let r = record(2);
        let id = r.id;
        reg.insert(r);
        assert!(!reg.has_child(&id));
        reg.remove(&id);
        assert!(reg.get(&id).is_none());
    }
}
