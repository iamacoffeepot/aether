// Shared engine registry. Keyed by hub-assigned `EngineId`; entries
// carry display metadata and a mail channel the Claude-facing tools
// will push `HubToEngine::Mail` frames into (PR 4).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aether_hub_protocol::{EngineId, HubToEngine};
use tokio::sync::mpsc;

/// One entry in the hub's engine table. The `mail_tx` side is how any
/// other task (including the MCP tool handlers, once they land) pushes
/// frames at this engine — the per-connection writer task drains the
/// receiver.
#[derive(Clone, Debug)]
pub struct EngineRecord {
    pub id: EngineId,
    pub name: String,
    pub pid: u32,
    pub version: String,
    pub mail_tx: mpsc::Sender<HubToEngine>,
}

/// Thread-safe map of live engines. Cheap to clone; all clones share
/// the same underlying table.
#[derive(Clone, Default)]
pub struct EngineRegistry {
    inner: Arc<Mutex<HashMap<EngineId, EngineRecord>>>,
}

impl EngineRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, record: EngineRecord) {
        self.inner.lock().unwrap().insert(record.id, record);
    }

    pub fn remove(&self, id: &EngineId) {
        self.inner.lock().unwrap().remove(id);
    }

    pub fn list(&self) -> Vec<EngineRecord> {
        self.inner.lock().unwrap().values().cloned().collect()
    }

    pub fn get(&self, id: &EngineId) -> Option<EngineRecord> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }
}
