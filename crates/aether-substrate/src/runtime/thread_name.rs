//! ADR-0088 §2/§5/§7 reverse-lookup `resolve` chain — the substrate-side
//! composition of the link-time inventory ([`aether_data`]) with the
//! runtime registry for dynamically-minted names, plus the
//! dispatch-hot-path thread-name cache that is the registry's first
//! producer.
//!
//! ## Why
//!
//! Substrate ids are one-way hashes (ADR-0029/0030/0064): you cannot
//! recover the origin name from the id. The reverse-lookup inventory
//! (ADR-0088) layers a side table over them. The compile-time half —
//! the static name inventory + name templates — lives in [`aether_data`]
//! (`no_std`, where the `Kind` descriptor inventory it generalizes
//! already lives). The runtime half — a process-global `id -> name` map
//! populated the moment a dynamic name is minted — lives here, where the
//! name builders run. [`resolve`] composes both into the full four-step
//! chain (ADR-0088 §2):
//!
//! 1. **Static map** — declared names + bounded/declared template
//!    instantiations, folded once at boot from the link-time inventories
//!    via [`build_static_reverse_map`].
//! 2. *(folded into step 1)* template prehash — `Bounded` / `Declared`
//!    template instantiations are prehashed into the same static map, so
//!    a template hit is an ordinary static-map hit.
//! 3. **Runtime registry** — dynamic instances, registered when minted.
//! 4. **Miss → ADR-0064 hex tag** — the caller renders the tagged-string
//!    form, exactly as before the inventory existed. Nothing regresses.
//!
//! Building the static map reads the link-time inventories (relatively
//! cheap, but not free), so it is built once behind a `OnceLock` and
//! reused for every cold-path lookup.
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
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::{OnceLock, PoisonError, RwLock};
use std::thread;

use aether_data::ThreadId;
use aether_data::build_static_reverse_map;
use aether_data::hash::{MAILBOX_DOMAIN, THREAD_DOMAIN};
use aether_data::name_inventory::{ParamKind, TemplateEntry, inventory};
use aether_data::tagged_id;

/// Upper bound on the worker-id range the `aether-worker-{N}` template
/// prehashes (ADR-0088 §4 `Bounded`). The pool sizes its worker count
/// from `thread::available_parallelism`, so there is no hard cap; 256
/// instantiations cover any realistic core count with a trivial boot
/// cost, and a worker beyond it still reverses through the runtime
/// registry (the name is registered on its first dispatch hop).
const WORKER_TEMPLATE_HI: u64 = 255;

// ADR-0088 §4 thread-name families. The pool names workers
// `aether-worker-N`; native-actor root threads `aether-root-<NAMESPACE>`
// (over the declared mailbox namespaces); instanced-actor threads
// `aether-instanced-<full_name>` (an unbounded runtime parameter).
inventory::submit! {
    TemplateEntry {
        domain: THREAD_DOMAIN,
        template: "aether-worker-{N}",
        param: ParamKind::Bounded { lo: 0, hi: WORKER_TEMPLATE_HI },
    }
}
inventory::submit! {
    TemplateEntry {
        domain: THREAD_DOMAIN,
        template: "aether-root-{NAMESPACE}",
        param: ParamKind::Declared { domain: MAILBOX_DOMAIN },
    }
}
inventory::submit! {
    TemplateEntry {
        domain: THREAD_DOMAIN,
        template: "aether-instanced-{full_name}",
        param: ParamKind::Dynamic,
    }
}

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

/// The link-time static reverse map (declared names + bounded/declared
/// template instantiations), folded once at boot from the [`aether_data`]
/// inventories. `OnceLock` because building it iterates the inventories
/// — cheap but not free, and the result is immutable for the process
/// lifetime.
fn static_map() -> &'static BTreeMap<u64, String> {
    static STATIC_MAP: OnceLock<BTreeMap<u64, String>> = OnceLock::new();
    STATIC_MAP.get_or_init(build_static_reverse_map)
}

/// Runtime-registry-only lookup — the dynamic-instance arm (ADR-0088 §5).
/// Separate from [`resolve`] so the four-step chain reads cleanly and so
/// callers that specifically want the runtime arm (tests, diagnostics)
/// can reach it without the static-map / hex-fallback steps.
fn resolve_runtime(id: u64) -> Option<String> {
    registry()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
        .map(ToString::to_string)
}

/// Resolve a tagged id back to its origin name, walking the full
/// four-step ADR-0088 §2 chain: static map (declared names + prehashed
/// templates) → runtime registry (dynamic instances) → ADR-0064 hex tag.
///
/// The cold render path (`trace_walk::fold_nodes`, MCP) calls this to
/// turn a [`ThreadId`] / [`MailboxId`](aether_data::MailboxId) /
/// [`KindId`](aether_data::KindId) into a display name. A hit returns the
/// real name; a miss returns the ADR-0064 tagged-string form
/// (`thr-XXXX-XXXX-XXXX`), which is exactly what the renderer showed
/// before the inventory existed — so reversal is a strict upgrade,
/// nothing regresses. `None` only when the id's tag bits are reserved /
/// invalid (the `0x0` sentinel), where there is no printable form at all.
#[must_use]
pub fn resolve(id: u64) -> Option<String> {
    if let Some(name) = static_map().get(&id) {
        return Some(name.clone());
    }
    if let Some(name) = resolve_runtime(id) {
        return Some(name);
    }
    tagged_id::encode(id)
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

    /// `resolve` on an id that's in neither the static map nor the
    /// runtime registry falls through to the ADR-0064 hex tag (step 4 of
    /// the chain) — the cold path shows exactly what it showed before the
    /// inventory existed, so reversal is a strict upgrade. The dynamic
    /// arm alone ([`resolve_runtime`]) still reports the miss as `None`.
    #[test]
    fn resolve_unregistered_id_falls_back_to_hex_tag() {
        // A fresh thread-name hash the test never registers and that no
        // bounded/declared template instantiates (so it can't be a
        // static-map hit either).
        let unseen = ThreadId::from_name("aether-never-registered-xyz");
        // The dynamic registry alone misses.
        assert_eq!(resolve_runtime(unseen.0), None);
        // The full chain falls back to the tagged-string form.
        let hex = resolve(unseen.0).expect("Thread-tagged id always tag-encodes");
        assert!(hex.starts_with("thr-"), "expected hex tag, got {hex}");
        assert_eq!(tagged_id::encode(unseen.0).as_deref(), Some(hex.as_str()));
    }

    /// `resolve` on the reserved `0x0` sentinel (invalid tag bits, no
    /// printable form) returns `None` — the only case the chain can't
    /// produce a name or a hex tag for.
    #[test]
    fn resolve_zero_sentinel_is_none() {
        assert_eq!(resolve(0), None);
    }

    /// Static-map hit (chain step 1/2): the `aether-worker-{N}` `Bounded`
    /// template submitted by this module is prehashed into the static
    /// reverse map at boot, so the worker id reverses through the static
    /// map directly — no runtime registration required. Asserts the
    /// static map itself carries the entry (independent of whatever the
    /// shared-process runtime registry happens to hold), then that the
    /// full chain reverses to the real name.
    #[test]
    fn resolve_hits_static_map_for_bounded_worker_template() {
        let id = ThreadId::from_name("aether-worker-3");
        // The prehashed template instantiation is in the static map.
        assert_eq!(
            static_map().get(&id.0).map(String::as_str),
            Some("aether-worker-3"),
        );
        // And the full chain reverses it to the real name.
        assert_eq!(resolve(id.0).as_deref(), Some("aether-worker-3"));
    }
}
