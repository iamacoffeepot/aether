// ADR-0021 publish/subscribe routing for substrate input streams,
// ADR-0068 keying.
//
// `InputSubscribers` is the shared table between the platform thread
// (which publishes `aether.tick`, `aether.key`, `aether.mouse_move`,
// and `aether.mouse_button`) and the control-plane handler (which
// mutates subscriber sets on `subscribe_input` / `unsubscribe_input`
// and on component drop). Readers take a shared lock per published
// event; writers take an exclusive lock on subscribe / unsubscribe /
// drop. `BTreeSet` rather than `Vec` so subscriber order is
// deterministic across runs and duplicate subscribe is naturally
// idempotent.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

use aether_data::KindId;

use crate::mail::MailboxId;

/// Per-kind subscriber sets, keyed on the input kind's compile-time
/// `KindId` (ADR-0068). Absent entries are treated the same as empty
/// sets, which lets subscribe lazily initialise an entry without a
/// boot-time prepass.
pub type InputSubscribers = Arc<RwLock<HashMap<KindId, BTreeSet<MailboxId>>>>;

/// Build an empty subscriber table. Callers clone the returned `Arc`
/// into both the control plane (mutator) and the platform thread
/// (reader).
pub fn new_subscribers() -> InputSubscribers {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Snapshot of the subscribers for a single input kind, returned as a
/// `Vec` so the caller can drop the read lock before dispatching. The
/// platform thread calls this once per platform event; the copy is
/// cheap (small sets, integer elements).
pub fn subscribers_for(table: &InputSubscribers, kind: KindId) -> Vec<MailboxId> {
    table
        .read()
        .unwrap()
        .get(&kind)
        .map(|set| set.iter().copied().collect())
        .unwrap_or_default()
}

/// Remove `id` from every stream's subscriber set. Invoked by the
/// control plane on successful `drop_component` so the invariant
/// "every subscriber id references a live mailbox" holds.
pub fn remove_from_all(table: &InputSubscribers, id: MailboxId) {
    let mut guard = table.write().unwrap();
    for set in guard.values_mut() {
        set.remove(&id);
    }
}
