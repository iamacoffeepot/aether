# Component registry

Aether has two component inventories with different owners. The hub knows which
wasm artifacts are stored. A substrate knows which component actors are live.
Treating one as proof of the other causes most component-operation mistakes.

## Three views, three meanings

| View | Owner | Current public access | What a row proves |
|---|---|---|---|
| stored artifact registry | hub | `upload_component`, registry `list_components` | bytes and manifest are available to selectors |
| trampoline/capability registry | one substrate | `load_component`, `describe_component`; live queries behind them | a lineage mailbox and its retained capabilities at observation time |
| MCP capability/kind caches | `aether-mcp` process | populated and refreshed by tool calls | an optimization, never the authority for liveness |

Registry `list_components` is deliberately artifact-facing. It does not list
loaded instances. The same artifact can be loaded zero times, once, many times
on one engine, or into several engines.

## Current component tools

| Tool | Purpose |
|---|---|
| `upload_component` | ingest a `.wasm` path into the hub store |
| `list_components` | list/filter stored component artifacts |
| `load_component` | instantiate stored wasm in one engine |
| `replace_component` | splice stored wasm behind one live mailbox id |
| `describe_component` | inspect a live component's receive surface |
| `describe_kinds` | inspect its config, input, and reply schemas in the engine's live vocabulary |
| `send_mail` | drive a component, including the generic `aether.component.drop` lifecycle mail |

There is no dedicated `drop_component` MCP tool. Dropping is nevertheless a
current engine operation: use `send_mail` to the `aether.component` mailbox with
kind `aether.component.drop` and the exact `mailbox_id` returned by the load.
Inspect that kind with `describe_kinds` before constructing params. Await its
`aether.component.drop_result`; do not fire-and-forget cleanup.

In current code, drop unloads the wasm, clears the trampoline's capabilities and
cost cells, and purges capability-owned subscriptions/routes. It does **not**
destroy the trampoline. The lineage name and mailbox remain as an empty,
replaceable slot until the substrate terminates. Consequently, drop releases
guest state but does not make the same load name available to a fresh load.

## Upload before selector

Only `upload_component` accepts a filesystem path. `load_component`,
`replace_component`, and boot component specs accept registry selectors—not
paths and not inline wasm bytes.

The reliable sequence is:

1. Build the wasm artifact on the same fleet host whose path the hub can read.
2. Call `upload_component(staged_path, name?)`.
3. Record the returned content hash and optional name.
4. Confirm its manifest with registry `list_components`; when no name was
   supplied, set `include_history: true` and locate the returned hash.
5. Use the hash or name as the later selector.

Upload reads the wasm manifest without executing the module. It records exported
actor namespaces, handled kind ids, fallback presence, provenance, and the
default entry. Identical bytes deduplicate to one hash.

### Which selector?

| Selector | Meaning | Use when |
|---|---|---|
| content hash | exact stored bytes | rollout, rollback, reproducible test |
| name | hash currently pointed to by that name | operator-managed “latest” workflow |
| `module@actor` | stored module plus exported actor type | multi-actor module selection |

An explicit `export` field overrides the actor half of `module@actor`. A
defaultless multi-actor module must be given an export; an omitted export is a
clean load error, not “first actor wins.”

Names are mutable. Re-uploading under the same name repoints it. The old hash
remains stored and, once no other name points at it, becomes unnamed history
eligible for LRU eviction. It is not deleted by repointing. Hashes are therefore
the correct deployment evidence.

## Stored registry behavior

`list_components` can filter stored manifests by exported `namespace` and
`handled_kind`; filters are AND-combined. Its normal result is a bounded,
newest-first page of name-pointed entries. `include_history: true` admits
unnamed hashes. Use the returned `total_matched` and an explicit `limit` when a
complete history is actually needed.

A named artifact is protected from disk-budget LRU eviction. An unnamed,
unpinned history entry is eligible. The current MCP surface has no delete,
unname, or pin tool, so long-term disk policy belongs to the hub operator rather
than an individual component-driving task.

## Loading into an engine

`load_component` resolves the selector at the hub, sends the bytes to the target
engine's `aether.component` cap, and waits for `LoadResult`.

On a single load, record all three outputs:

- `mailbox_id`: the tagged id required by replace/drop lifecycle operations.
- `name`: the full lineage address used as `send_mail.recipient_name` and by
  live `describe_component`.
- `capabilities`: handled kinds, reply contracts, fallback, docs, and config
  kind for the selected actor type.

Never reconstruct the lineage from a short load name. Use the returned value,
normally `aether.component/aether.embedded:NAME`.

### Configuration

Pass either inline JSON in `config` or a JSON-file path in `config_path`, never
both. The MCP layer resolves the selected artifact's declared Config kind and
schema-encodes that JSON before loading. Providing config to a component that
declares no Config kind is an error. Use `describe_component` to learn the kind
name and `describe_kinds` for its exact live schema.

### Replicas

`replicas: N` performs N sequential loads with shared wasm/config and names each
instance `{base}-{index}`. The base is selected from explicit load name, export,
or default entry namespace in that order. The result carries one shared
capabilities block and an `instances` list of ids/names.

A replica fan-out is not transactional. If replica K fails, instances before K
remain live and the error says how many loaded. The failed call does not return
the successful prefix's `instances` records or mailbox ids. Their lineage names
follow the deterministic suffix rule, but the current public listing surface
does not recover their ids. On a task-owned engine, terminate and start clean.
On a shared engine, stop and report the partial prefix rather than guessing ids
or retrying into occupied names.

Boot-time replicas use the same naming rule. `spawn_substrate` waits until every
expected lineage string is present, but the current check neither deduplicates
colliding expectations nor proves the loaded bytes match each requested spec.
Require unique derived names, then describe and safely probe every boot lineage.

## Live introspection

`describe_component` should normally receive the lineage `name`. On a cache
miss, `aether-mcp` forwards that name to the substrate, whose registry owns the
live answer. This works for boot-loaded components, after an `aether-mcp`
restart, and after a successful in-place replacement.

The process-local capability cache is consulted before that live query. Loads
and replacements refresh it, but a generic `aether.component.drop` sent through
`send_mail` does not invalidate it. In that case, a same-process describe can
return pre-drop capabilities. The successful `drop_result` is the unload proof;
only a later name lookup after a cache reset is guaranteed to miss the cleared
substrate capability registry.

The cache key has no hub-epoch component. After a hub restart reuses an
`engine_id`, a boot-loaded component whose deterministic mailbox id also matches
can collide with an earlier capability entry. Use live probes or reset/reconnect
`aether-mcp` when an exact clean-epoch description is required.

A `mbx-…` argument has no live fallback at all: it is only a local MCP cache
fast path. If that cache was never populated or was lost on process restart, the
id alone cannot drive the live name query. Keep the lineage name as the durable
observation handle for the life of the engine.

The live kind vocabulary is also substrate-owned. Loading registers the wasm's
kind descriptors into the same registry served by `aether.inventory`.
`describe_kinds(engine_id: …)` reads that view. The mail encoder refreshes its
per-engine cache once on an unknown kind or a stale-schema encode failure, but
the cache does not make an unloaded kind live. Kind descriptors are not removed
when a component drops, so kind presence is vocabulary evidence, not component
liveness evidence.

The standalone description call currently suppresses a failed live refresh and
can return its static/prior snapshot. Use a bounded live probe when freshness or
engine reachability matters.

## Replacing safely

Use `replace_component` with the current engine id, the exact component
`mailbox_id`, and a previously uploaded selector. Prefer a content hash so the
replacement is unambiguous.

On success the trampoline mailbox stays stable and the returned capabilities
describe the replacement actor type. An omitted export reuses the actor type the
trampoline currently hosts; it does not necessarily select the new module's
default entry.

`drain_timeout_ms` is accepted for wire compatibility but is currently ignored
by the substrate's structural splice path. Do not present it as a functioning
deadline. Require an explicit successful result, then re-run
`describe_component` and a safe probe. Failure is phase-dependent: pre-splice
validation preserves the old guest; an instantiation failure after the old
guest is taken can leave the trampoline empty; a rehydrate failure installs the
new guest but still returns `Err`. Observe live behavior before deciding whether
to retry or roll forward. After a post-splice error, both MCP's cache and the
substrate capability registry can describe the old handler set even when the
slot is empty or the new guest remains installed. Use
[Replacement failure states](components/replacement-failure-states.md) rather
than treating `describe_component` as rollback proof.

## Component cleanup

For a task-owned instance in a shared engine:

- [ ] Preserve its logs and any trace evidence.
- [ ] Send settled `aether.component.drop` through `send_mail`.
- [ ] Require `aether.component.drop_result` success.
- [ ] Do not use a same-process `describe_component` cache hit or lingering kind
      descriptor as unload confirmation.
- [ ] Record that the lineage/mailbox is an empty slot, not a freed name.

For an engine wholly owned by the task, terminating the engine is the simpler
complete cleanup: all its live registries, empty slots, and components die with
it. This does not delete the hub's stored component artifacts.

## Source routes

- Tool contracts and shapes: `crates/aether-mcp/src/tools/mod.rs` and `args.rs`
- Upload/list/load/replace orchestration:
  `crates/aether-mcp/src/tools/components.rs`
- Selector resolution, boot staging, and live kind caching:
  `crates/aether-mcp/src/tools/state.rs`
- Hub artifact store: `crates/aether-engine/src/store/`
- Component host load/list/describe/drop/replace:
  `crates/aether-component/src/component/`
- Empty-slot drop behavior:
  `crates/aether-component/src/trampoline/runtime/mod.rs`
- Trampoline replace behavior:
  `crates/aether-component/src/trampoline/runtime/replace.rs`
- Live kinds: `crates/aether-inventory/src/`

For failure branches, continue with [Recovery](recovery.md).
