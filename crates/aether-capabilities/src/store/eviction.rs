//! LRU disk-budget eviction for [`super::ArtifactStore`].

use std::collections::HashSet;
use std::fs;

use super::ArtifactStore;
use super::TARGET;

impl ArtifactStore {
    /// Evict LRU entries that are neither pinned nor named until the disk
    /// ledger is back under budget (or no eligible candidate remains).
    pub(super) fn evict_if_needed(&mut self) {
        while self.total_bytes > self.disk_budget_bytes {
            // Snapshot the named set once (a name protects its target), then
            // pick the oldest eligible entry. Both reads borrow disjoint
            // fields; `victim` is owned, so the removal below is clear.
            let named: HashSet<&str> = self.names.values().map(String::as_str).collect();
            let victim = self
                .entries
                .iter()
                .filter(|(hash, entry)| !entry.pinned && !named.contains(hash.as_str()))
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(hash, _)| hash.clone());
            drop(named);
            let Some(hash) = victim else {
                // Everything left is protected; can't shrink further.
                break;
            };
            if let Some(entry) = self.entries.remove(&hash) {
                self.total_bytes = self.total_bytes.saturating_sub(entry.bytes_len);
                let (bytes_path, manifest_path) = self.entry_paths(&hash);
                let _ = fs::remove_file(&bytes_path);
                let _ = fs::remove_file(&manifest_path);
                tracing::info!(target: TARGET, hash = %hash, "binary store: evicted to hold disk budget");
            }
        }
    }
}
