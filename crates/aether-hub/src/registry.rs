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

use aether_data::KindDescriptor;

use crate::wire::{EngineId, HubToEngine};
use tokio::process::Child;
use tokio::sync::mpsc;

use crate::mcp::args::ComponentCapabilitiesWire;

/// Per-loaded-component metadata tracked by the hub (ADR-0033). Seeded
/// when `load_component` resolves, refreshed on `replace_component`,
/// removed on `drop_component`. Surfaced verbatim by the
/// `describe_component` MCP tool.
#[derive(Clone, Debug)]
pub struct ComponentRecord {
    pub name: String,
    pub capabilities: ComponentCapabilitiesWire,
}

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
    /// Per-mailbox component metadata (ADR-0033). Keyed on the
    /// substrate-assigned `mailbox_id`. Clonable as a snapshot; the
    /// authoritative write path goes through
    /// `EngineRegistry::upsert_component`.
    pub components: HashMap<aether_data::MailboxId, ComponentRecord>,
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

    /// Remove and return the `Child` for a spawned engine without
    /// touching the record. Callers (notably `terminate_substrate`)
    /// use this to take ownership of the child before signalling and
    /// awaiting its exit — leaving the record in place so the engine
    /// connection task can continue reading until the socket drops,
    /// at which point the standard `remove` path fires.
    ///
    /// Returns `None` for unknown or externally connected engines.
    pub fn take_child(&self, id: &EngineId) -> Option<Child> {
        self.inner.lock().unwrap().spawned_children.remove(id)
    }

    /// Drain every adopted child, returning them for orderly shutdown.
    /// Used by the hub's signal handler to explicitly terminate every
    /// spawned substrate before the process exits, so a SIGTERM on the
    /// hub doesn't orphan children into init. `take_child`'s per-id
    /// path covers normal `terminate_substrate` calls; this bulk
    /// variant exists for shutdown only.
    pub fn drain_spawned_children(&self) -> Vec<(EngineId, Child)> {
        self.inner
            .lock()
            .unwrap()
            .spawned_children
            .drain()
            .collect()
    }

    pub fn remove(&self, id: &EngineId) {
        let mut inner = self.inner.lock().unwrap();
        inner.records.remove(id);
        // Drop any adopted child alongside the record. `kill_on_drop`
        // takes care of reaping if the process is still running.
        inner.spawned_children.remove(id);
    }

    /// Replace the cached kind descriptors for an engine. Called when
    /// the substrate reports `EngineToHub::KindsChanged` post-load
    /// (ADR-0010 §4) so subsequent `describe_kinds` calls see the
    /// newly-registered vocabulary. No-op if the engine is unknown —
    /// the engine may have dropped concurrently.
    pub fn update_kinds(&self, id: &EngineId, kinds: Vec<KindDescriptor>) {
        if let Some(record) = self.inner.lock().unwrap().records.get_mut(id) {
            record.kinds = kinds;
        }
    }

    /// Insert or replace the component record for `(engine, mailbox)`.
    /// Called after a successful `load_component` or
    /// `replace_component` (ADR-0033). No-op if the engine is unknown.
    pub fn upsert_component(
        &self,
        id: &EngineId,
        mailbox: aether_data::MailboxId,
        record: ComponentRecord,
    ) {
        if let Some(engine) = self.inner.lock().unwrap().records.get_mut(id) {
            engine.components.insert(mailbox, record);
        }
    }

    /// Drop the component record for `(engine, mailbox)`. Called after
    /// a successful `drop_component`. No-op if the engine or mailbox
    /// is unknown.
    pub fn drop_component(&self, id: &EngineId, mailbox: aether_data::MailboxId) {
        if let Some(engine) = self.inner.lock().unwrap().records.get_mut(id) {
            engine.components.remove(&mailbox);
        }
    }

    /// Snapshot the component record for `(engine, mailbox)`. Used by
    /// the `describe_component` MCP tool. Returns `None` for unknown
    /// engine / mailbox pairs.
    pub fn get_component(
        &self,
        id: &EngineId,
        mailbox: aether_data::MailboxId,
    ) -> Option<ComponentRecord> {
        self.inner
            .lock()
            .unwrap()
            .records
            .get(id)?
            .components
            .get(&mailbox)
            .cloned()
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
    use crate::wire::Uuid;

    fn record(id_u128: u128) -> EngineRecord {
        let (tx, _rx) = mpsc::channel(1);
        EngineRecord {
            id: EngineId(Uuid::from_u128(id_u128)),
            name: "e".into(),
            pid: 1,
            version: "0".into(),
            kinds: vec![],
            components: HashMap::new(),
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

    #[test]
    fn update_kinds_replaces_cached_descriptors() {
        use aether_data::{KindDescriptor, SchemaType};
        let reg = EngineRegistry::new();
        let r = record(3);
        let id = r.id;
        reg.insert(r);
        assert!(reg.get(&id).unwrap().kinds.is_empty());

        let new_kinds = vec![KindDescriptor {
            name: "physics.contact".into(),
            schema: SchemaType::Bytes,
            is_stream: false,
        }];
        reg.update_kinds(&id, new_kinds.clone());
        assert_eq!(reg.get(&id).unwrap().kinds, new_kinds);
    }

    #[test]
    fn update_kinds_for_unknown_engine_is_noop() {
        let reg = EngineRegistry::new();
        let unknown = EngineId(Uuid::from_u128(999));
        reg.update_kinds(&unknown, vec![]);
    }
}
