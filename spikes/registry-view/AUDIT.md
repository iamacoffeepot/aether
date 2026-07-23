# Registry consumer-contract audit

Companion to the benchmark: every production call site of the `Registry`
surface, classified against the proposed contract (snapshot views for
reads, mutations staged to a single-writer owner, results via
settlement). Test call sites are excluded — all of them poke the
concrete API and adapt mechanically; none encodes a contract production
must preserve.

## Corrections to the investigation's working assumptions

- **`set_on_mailbox_change` has zero production callers.** Only
  `registry/tests.rs` installs it; every production boot leaves the hook
  `None`, so `notify_mailbox_change` is a no-op and the feared
  O(n)-snapshot-per-registration cost does not occur in the current
  tree. Inventory egress actually happens directly in
  `component/runtime/load.rs` (`egress_mailboxes_changed`), per
  component load, not per spawn. The hook mechanism deletes trivially.
- **`Registry::drop_mailbox` has zero production callers.** Its doc
  comment claiming the WasmTrampoline shutdown path calls it is stale —
  the trampoline clears `capability_registry` + `cost_table` and leaves
  the slot live as an empty trampoline. Async-staging this method
  changes nothing.
- **`Mailer` proxies no registry method** — consumers reach the registry
  via `mailer.registry()` handles; the only internal use is
  `route_lookup` inside `push` → `route_mail`.

## Triage summary (production sites)

| verdict | count | shape |
|---|---|---|
| HOLDS | 12 | all hot dispatch reads (`route_mail`, blob demuxer, wasm send/host-fn paths) and all control-plane reads (`list_components`, inventory resolve, capture bundle resolve) — every one is snapshot-tolerant, several already clone-out-and-drop-guard |
| ADAPTABLE | 6 | subscribe-validation reads (input/http/window — racy today, see cluster 2), boot unwind `remove_closure`, spawn rollback, kind `register_or_match_all`, wasm inline-spawn alias |
| BROKEN | 3 clusters | boot, component load, spawn — resolutions below |

## Cluster 1 — chassis boot needs direct apply

Boot claims singleton mailboxes, registers the kind vocabulary and
inline sinks, and branches on synchronous `Result`s to abort
(`chassis/ctx.rs:292,528`, `boot.rs:117,135`), then does
read-your-writes against them later in boot
(`native_actor_boot.rs:291` seize install; headless `kind_id(Tick)`
lookups). An owner actor cannot serve this phase — it does not exist
yet.

**Resolution:** boot is single-threaded and precedes the worker pool, so
it builds the table directly and seals it to the owner at pool start.
The contract reads "all mutations are staged" with a boot carve-out:
staging activates at steady state. No boot code changes semantics.

## Cluster 2 — component load must not read back its own registrations

`load.rs` registers the trampoline mailbox + module kinds, then in the
same handler reads `list_mailbox_descriptors` / `list_kind_descriptors`
/ `mailbox_name` (`load.rs:319,320,332,478`) to build the synchronous
`LoadResult` reply and the egress snapshots. Under one-batch-stale
views, all three reads miss their own writes; the hub cache and any
immediately-subscribing agent see a loaded-but-unaddressable component.
The subscribe-validation reads (input `runtime.rs:84`, http
`routing.rs:75`, window `subscribers.rs:148`) inherit correctness from
this ordering.

**Resolution (either suffices):** build the reply and egress payloads
from the staged effect's own contents (the handler knows exactly what
it registered — reading it back from the table was always a detour), or
gate `LoadResult` emission on settlement of the registration effect.
The first is simpler and also removes the read-back entirely.

## Cluster 3 — spawn conflict surface and the seize install must fuse

`spawn.rs:470` returns `SubnameInUse` synchronously (load turns it into
`LoadResult::Err`), and `spawn.rs:595` installs the seize handle
against the entry `:470` just inserted — a read-your-write; miss it and
the blob-demuxer fast path is silently lost. The mailbox id itself is
deterministic (lineage fold, computed before any write), so returning
the id never required the write.

**Resolution:** (i) conflict detection for lineage spawns is
parent-local (own children ∪ own staged set, checked synchronously);
cross-parent collision at 64 bits stays fail-fast, and the settled
`SpawnError` path covers apply-time surprises. (ii) Registration,
seize-cell handle, and rollback fuse into one staged effect the owner
applies atomically — three ops with read-your-write dependencies become
one op with none.
