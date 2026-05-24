//! ADR-0088 §5/§7 reverse-lookup registry for dynamically-minted names,
//! plus the dispatch-hot-path thread-name cache that is its first
//! producer (Phase 1).
//!
//! ## Why
//!
//! Substrate ids are one-way hashes (ADR-0029/0030/0064): you cannot
//! recover the origin name from the id. The reverse-lookup inventory
//! (ADR-0088) layers a side table over them; this module holds the
//! runtime arm — a process-global `id -> name` map populated the moment
//! a dynamic name is minted, read cold at render time.
//!
//! ## The thread-name perf shave (the forcing function)
//!
//! The dispatch hot path used to build a fresh `String` per mail hop via
//! `thread::current().name().map(str::to_owned)` to stamp
//! [`aether_kinds::trace::TraceEvent::Received`]. The name is the
//! constant `aether-worker-N` for a given worker, so allocating it per
//! hop was pure waste (~25% of the warm hop, a 1-worker saturation
//! profile under macOS `sample` — iamacoffeepot/aether#1059 /
//! iamacoffeepot/aether#1101).
//!
//! [`current_thread_id`] resolves the calling thread's name to a `Copy`
//! [`ThreadId`] **once per thread** (cached in a thread-local) and
//! registers `name -> id` in the process-global registry on that first
//! compute. The trace event then stores the `Copy` id; the display name
//! is recovered on the cold render path via [`resolve`]. This kills both
//! the per-hop `str::to_owned` and the `thread::current()` `Arc` bump.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{OnceLock, PoisonError, RwLock};
use std::thread;

use aether_data::ThreadId;

/// Process-global reverse-lookup map: tagged id -> origin name. Keyed on
/// the raw `u64` so the same table can hold any id family that mints
/// dynamic names (ADR-0088 §5); Phase 1 only registers [`ThreadId`]s.
///
/// `RwLock` because writes are rare (a name is minted once per
/// instance / once per worker thread) and reads are cold (render time),
/// so the lock is uncontended in practice and never on the dispatch hot
/// path. Initialised lazily on first use so the map costs nothing in a
/// process that never traces.
fn registry() -> &'static RwLock<HashMap<u64, Box<str>>> {
    static REGISTRY: OnceLock<RwLock<HashMap<u64, Box<str>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register `id -> name` in the process-global reverse-lookup registry.
/// Idempotent — re-registering the same id with the same name is a
/// no-op; an id is never expected to map to two different names (the
/// hash is name-derived), so a second insert simply overwrites with the
/// identical value. Off the dispatch hot path: called at name-mint
/// time, not per hop.
///
/// Poison recovery: a writer that panicked mid-update is tolerated — the
/// registry is a best-effort observability side table, never load-
/// bearing for dispatch, so a poisoned lock recovers its inner map
/// rather than aborting the process.
pub fn register(id: u64, name: &str) {
    let mut map = registry().write().unwrap_or_else(PoisonError::into_inner);
    // Only allocate a `Box<str>` on a genuine first insert.
    map.entry(id).or_insert_with(|| name.into());
}

/// Resolve a tagged id back to its registered origin name, if any. The
/// cold render path (`trace_walk::fold_nodes`, MCP) calls this to turn a
/// [`ThreadId`] (or any future dynamically-registered id) into a display
/// name. `None` means the id was never registered — the caller falls
/// back to the ADR-0064 hex tag, which is exactly what it showed before.
#[must_use]
pub fn resolve(id: u64) -> Option<String> {
    registry()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
        .map(ToString::to_string)
}

/// Per-thread cache state for [`current_thread_id`], distinguishing the
/// three cases clippy's `option_option` lint asks us to name:
/// not-yet-computed, computed-to-a-name, and computed-but-anonymous (an
/// OS thread with no `Builder::name`).
#[derive(Copy, Clone)]
enum CachedThreadId {
    /// First [`current_thread_id`] call on this thread hasn't run yet.
    Uncomputed,
    /// Resolved once: `Some` for a named thread, `None` for an anonymous
    /// one.
    Computed(Option<ThreadId>),
}

thread_local! {
    /// Per-thread cache of this thread's resolved [`ThreadId`]. Computed
    /// once on first [`current_thread_id`] call (reads the OS thread
    /// name, hashes it, registers name -> id), then reused for every
    /// subsequent hop on that thread with zero allocation. `Cell` because
    /// [`CachedThreadId`] is `Copy`.
    static CACHED_THREAD_ID: Cell<CachedThreadId> =
        const { Cell::new(CachedThreadId::Uncomputed) };
}

/// The calling thread's [`ThreadId`], computed once per thread and cached
/// in a thread-local. Returns `None` for an anonymous OS thread (no
/// `Builder::name`) — e.g. a `std::thread::spawn` worker or an
/// unnamed test thread — matching the prior `thread_name: None`.
///
/// On the first call from a thread this reads `thread::current().name()`,
/// hashes it via [`ThreadId::from_name`], and registers `name -> id` in
/// the process-global registry so the cold render path can reverse it.
/// Every later call is a thread-local read of a `Copy` value — no alloc,
/// no `thread::current()` `Arc` bump — which is the dispatch-hot-path
/// win (ADR-0088 §7).
#[must_use]
pub fn current_thread_id() -> Option<ThreadId> {
    CACHED_THREAD_ID.with(|cell| {
        if let CachedThreadId::Computed(cached) = cell.get() {
            return cached;
        }
        let resolved = thread::current().name().map(|name| {
            let id = ThreadId::from_name(name);
            register(id.0, name);
            id
        });
        cell.set(CachedThreadId::Computed(resolved));
        resolved
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A named thread resolves to a stable `ThreadId`, registers its
    /// name, and `resolve` recovers the original name from the id.
    #[test]
    fn named_thread_resolves_and_registers() {
        let handle = thread::Builder::new()
            .name("aether-test-worker-7".to_owned())
            .spawn(|| {
                let first = current_thread_id().expect("named thread has a ThreadId");
                // Cached: a second call yields the identical id.
                let second = current_thread_id().expect("cached ThreadId");
                assert_eq!(first, second);
                first
            })
            .expect("spawn named thread");
        let id = handle.join().expect("join named thread");

        // The id is the name-derived hash, deterministically recomputable.
        assert_eq!(id, ThreadId::from_name("aether-test-worker-7"));
        // The registry reversed it to the origin name.
        assert_eq!(resolve(id.0).as_deref(), Some("aether-test-worker-7"));
    }

    /// An anonymous thread (no `Builder::name`) resolves to `None` —
    /// matching the prior `thread_name: None` behaviour — and registers
    /// nothing.
    #[test]
    fn anonymous_thread_resolves_to_none() {
        let resolved = thread::spawn(current_thread_id)
            .join()
            .expect("join anonymous thread");
        assert_eq!(resolved, None);
    }

    /// `resolve` on an unregistered id returns `None`, so the cold path
    /// falls back to the hex tag (no regression vs. the prior behaviour).
    #[test]
    fn resolve_unregistered_id_is_none() {
        // A fresh hash that the test never registers.
        let unseen = ThreadId::from_name("aether-never-registered-xyz");
        assert_eq!(resolve(unseen.0), None);
    }
}
