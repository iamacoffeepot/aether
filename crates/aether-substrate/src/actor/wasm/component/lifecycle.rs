use std::mem;

use wasmtime::Store;

use super::instantiate::Placement;
use super::{Component, ComponentCtx, MAX_DELIVERABLE_MAIL_BYTES, PendingSpawn, SMALL_REGION_BYTES, StateBundle};
use crate::mail::MailboxId;
use crate::mail::registry::PreparedAliasRoute;

impl Component {
    /// Loudly log an init config rejected by [`Component::instantiate`] (ADR-0095)
    /// because it could not be delivered safely — either past the absolute
    /// ceiling, or to a guest with no allocator export. Mirrors the dispatch
    /// oversize log; the caller returns an `Err` that surfaces as
    /// `LoadResult::Err` rather than writing or trapping. Associated (no
    /// `&self`) because `instantiate` has no `Component` yet.
    pub(super) fn log_oversize_config(store: &Store<ComponentCtx>, config_bytes: usize, reason: &str) {
        tracing::error!(
            target: "aether_substrate::component",
            mailbox_id = store.data().sender.0,
            config_bytes,
            small_region_bytes = SMALL_REGION_BYTES,
            deliverable_cap_bytes = MAX_DELIVERABLE_MAIL_BYTES,
            reason,
            "rejecting init config; cannot deliver safely (see ADR-0095)",
        );
    }

    /// Issue 584 Phase 2b (ADR-0079 amended): pre-shutdown mail-allowed
    /// hook. Invoked by the trampoline before `on_dehydrate` on the
    /// dying instance, or before the `Component` value drops on a
    /// `DropComponent`. Same trap containment as the other hooks —
    /// a guest panic doesn't stall teardown.
    pub fn unwire(&mut self) {
        if let Some(f) = self.unwire.clone()
            && let Err(e) = f.call(&mut self.store, self.self_mailbox_id)
        {
            tracing::error!(target: "aether_substrate::component", error = %e, "unwire hook trapped");
        }
    }

    /// Invoke the guest's `on_dehydrate` hook if it exports one.
    /// Wasmtime traps (guest panics, unreachable) are caught and
    /// logged rather than propagated — per ADR-0015, a panicking
    /// hook must not stall teardown.
    pub fn on_dehydrate(&mut self) {
        if let Some(f) = self.on_dehydrate.clone()
            && let Err(e) = f.call(&mut self.store, ())
        {
            tracing::error!(target: "aether_substrate::component", error = %e, "on_dehydrate hook trapped");
        }
    }

    /// Extract the state bundle the guest deposited via `save_state`
    /// during `on_dehydrate`. Returns `None` if `save_state` was never
    /// called (component doesn't implement migration, or the hook is
    /// a no-op). Called by the control plane *after* `on_dehydrate`
    /// runs on the old instance — the bundle has to outlive the
    /// store.
    pub fn take_saved_state(&mut self) -> Option<StateBundle> {
        self.store.data_mut().saved_state.take()
    }

    /// ADR-0097: drain every sibling-spawn request the guest staged via
    /// the `spawn_sibling` host fn during the just-returned `receive`.
    /// The trampoline calls this after `deliver` and performs one
    /// `spawn_child::<WasmTrampoline>` per request. Destructive — empty
    /// once drained, and empty when the guest didn't spawn.
    pub fn drain_pending_spawns(&mut self) -> Vec<PendingSpawn> {
        mem::take(&mut self.store.data_mut().pending_spawns)
    }

    /// Drain logical inline-child aliases staged during the just-returned
    /// guest call. The trampoline publishes them through the registry owner
    /// before its handler-end buffered mail is routed.
    pub fn drain_pending_aliases(&mut self) -> Vec<PreparedAliasRoute> {
        self.store.data_mut().take_pending_aliases()
    }

    /// Drain the inline-child aliases the just-returned guest call despawned
    /// (#4228). The trampoline retires each route through the registry owner
    /// and notifies its watchers, the teardown mirror of
    /// [`Self::drain_pending_aliases`].
    pub fn drain_pending_alias_retirements(&mut self) -> Vec<MailboxId> {
        self.store.data_mut().take_pending_alias_retirements()
    }

    /// Extract a failure recorded by `save_state` (size cap, OOB).
    /// `None` on clean saves and on components that didn't attempt a
    /// save. Checked by the control plane to decide whether to abort
    /// the replace (ADR-0016 §4).
    pub fn take_save_error(&mut self) -> Option<String> {
        self.store.data_mut().save_state_error.take()
    }

    /// Write the prior-state bytes into a delivery region (ADR-0095, via
    /// `place`) and invoke `on_rehydrate(version, ptr, len)`. Returns
    /// `Ok(())` if the instance doesn't export `on_rehydrate` (ADR-0016 §3: the
    /// bundle is silently discarded when no handler claims it).
    ///
    /// ADR-0016 §4 specifies that a trap here aborts the replace, so errors are
    /// propagated rather than contained (unlike `on_dehydrate` / `unwire`). A
    /// region that can't be allocated, or a bundle past the deliverable ceiling,
    /// propagates as an `Err` too.
    pub fn call_on_rehydrate(&mut self, bundle: &StateBundle) -> wasmtime::Result<()> {
        let Some(f) = self.on_rehydrate.clone() else {
            return Ok(());
        };
        let len = bundle.bytes.len();
        // Wasm32 ABI carries `u32` byte lengths; bundle bytes are
        // bounded by guest memory size (well below `u32::MAX`).
        #[allow(clippy::cast_possible_truncation)]
        let byte_len = len as u32;
        let ptr = match Self::place(
            &mut self.store,
            self.realloc.as_ref(),
            self.small_ptr,
            &mut self.large_ptr,
            &mut self.large_cap,
            len,
        )? {
            Placement::At(ptr) => ptr,
            Placement::Oversize => {
                return Err(wasmtime::Error::msg(format!(
                    "rehydrate state of {len} bytes exceeds the {MAX_DELIVERABLE_MAIL_BYTES}-byte deliverable bound"
                )));
            }
            Placement::NoAllocator => {
                return Err(wasmtime::Error::msg("cannot rehydrate state: guest exports no realloc_p32 allocator"));
            }
        };
        if !bundle.bytes.is_empty() {
            self.memory.write(&mut self.store, ptr as usize, &bundle.bytes)?;
        }
        f.call(&mut self.store, (bundle.version, ptr, byte_len))?;
        Ok(())
    }

    /// Read a `u32` from guest linear memory at `offset`. Test-only
    /// accessor: the production mail path writes into an allocator
    /// region and the guest interprets the bytes — nothing in non-test
    /// code reads guest memory directly.
    ///
    /// # Panics
    /// Panics if the memory read fails — fail-fast per ADR-0063:
    /// tests construct the offset/length pair directly, so an
    /// out-of-bounds read is a test bug.
    #[cfg(test)]
    pub fn read_u32(&mut self, offset: usize) -> u32 {
        let mut buf = [0u8; 4];
        self.memory.read(&mut self.store, offset, &mut buf).expect("test memory read");
        u32::from_le_bytes(buf)
    }

    /// Read `len` bytes from guest linear memory starting at `offset`.
    /// Test-only accessor for verifying that a rehydrate hook copied
    /// bytes to a known marker offset.
    ///
    /// # Panics
    /// Panics if the memory read fails — fail-fast per ADR-0063:
    /// tests construct the offset/length pair directly, so an
    /// out-of-bounds read is a test bug.
    #[cfg(test)]
    pub fn read_bytes(&mut self, offset: usize, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        self.memory.read(&mut self.store, offset, &mut buf).expect("test memory read");
        buf
    }
}
