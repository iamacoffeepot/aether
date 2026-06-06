# ADR-0097: Wasm sibling spawn

- **Status:** Proposed
- **Date:** 2026-06-06

## Context

ADR-0096 gave a wasm crate several exported actor types in one resident module, plus a load-path selector that names which type to instantiate. It deferred the runtime counterpart — a running actor spawning one of its sibling types — to this ADR, and flagged an open granularity question: at what unit, and under what rules, a wasm actor can spawn another.

Native actors already have this. `NativeCtx::spawn_child::<A>(subname, config)` (ADR-0079) spawns an `Instanced` actor as a child of the caller: synchronous, init on the spawning thread, returns the new `MailboxId` (no strong handle), stamps the spawner's mailbox as the child's `ReplyTo`, names the instance via `Subname::Counter | Named`, and retires the name on drop under the unique-owner invariant. Wasm has no equivalent; the closest analogue is loading a fresh module by name.

The motivating shape, carried from the #1363 design dialogue, is a UI `RootManager` that spawns `Panel` children at runtime.

Constraints carried in:

- **ADR-0096.** Each exported type has a stable actor-type tag (`mailbox_id_from_name(NAMESPACE)`); `init_typed_p32(tag, config_bytes)` constructs a chosen export; the compiled module is resident once and shared by wasmtime across instances, with only linear memory per-instance.
- **ADR-0079.** `Instanced` / `Singleton` cardinality, `spawn_child` semantics, and the drop / name-retirement rules.
- **ADR-0024.** The `_p32` FFI shim surface — the contract this extends with a spawn import.
- **ADR-0090.** Init-config crosses from the host into the guest's typed `init` as wire bytes.

## Decision

Five sub-decisions.

### 1. A guest spawns a sibling by type

```rust
let panel = ctx.spawn_child::<Panel>(Subname::Counter, &PanelConfig { /* … */ })?;
```

Guest-side, typed, mirroring native. The SDK lowers `::<Panel>` to the sibling's compile-time tag (`mailbox_id_from_name(Panel::NAMESPACE)`) and encodes the config to wire bytes. A new host-fn import on the `_p32` surface carries `(tag, subname, config_bytes)` across the boundary and returns the new `MailboxId`. The typed call site is available because the guest crate compiled `Panel`: resolving `Panel::TAG` and encoding `Panel::Config` happen entirely at compile time, with no cross-boundary monomorphization.

### 2. Spawnable siblings are `Instanced`; spawn reuses ADR-0079 wholesale

A spawnable sibling is an `Instanced` exported type. `Subname::Counter | Named`, the `MailboxId`-only return (no strong handle), retire-on-drop, and the unique-owner invariant carry over unchanged. The spawned sibling's `ReplyTo` stamps the spawner's mailbox, so its replies route back to the parent. This answers ADR-0096's open granularity question: the spawnable unit is an `Instanced` sibling type, and its lifecycle vocabulary is the existing native one rather than a parallel wasm-only model.

### 3. Sibling-only — foreign instantiation is a load, not a spawn

`spawn_child` instantiates only types compiled into the calling module. Instantiating a foreign module — another binary, by path, embedded bytes, or name — is a load, served by `load_component` (a guest mails `aether.component.load`; ADR-0096 gave that path an export selector). The line falls out of the typed primitive: there is no `::<ForeignType>` to write for a type the crate did not compile against, so anything foreign arrives as a name plus opaque bytes, which is the load shape. The cost model reinforces it — `spawn_child` re-instantiates an already-compiled, already-registered resident module with no disk read, compile, or kind registration, whereas a foreign child pays a full module load. And the authority differs: spawning own siblings stays inside the authority granted at load, while pulling arbitrary other binaries into the host is a larger grant that belongs to a governed load. The partition: spawn is one more instance of code the actor already is; load brings in code it is not.

### 4. The host reuses the load path's instantiation

The host-fn closure already holds the trampoline's `NativeCtx` (with its `Spawner`) and the resident `Module`. It runs `spawn_child::<WasmTrampoline>(subname, WasmTrampolineConfig { module: <the caller's resident module>, type_tag, config, .. })` — the same call `handle_load` makes, fired from a running guest instead of a load mail. `init_typed_p32` constructs the sibling; the config bytes ride the ADR-0090 path. The spawn is synchronous and returns the new `MailboxId`, matching native; the spawner (the trampoline) is the new instance's `ReplyTo`, and since the guest's mailbox is the trampoline's mailbox, replies land at the guest.

### 5. No automatic lifecycle cascade

A spawned sibling has independent lifecycle, mirroring native. Children self-close; the parent tracks them by `MailboxId` and, if it wants "close the root, children go away," sends close-mail to its tracked children from its own `unwire`. The framework does not cascade parent drop to children — that would be new lifecycle surface the native path lacks, and the UI case it serves is fully expressible in guest code with the `MailboxId`s `spawn_child` already returns.

## Consequences

### Positive

- **A wasm subsystem composes at runtime, not just at load.** A coordinator type stands up its worker types on demand from one resident module — the UI-root-spawns-panels shape, and per-connection / per-entity patterns generally.
- **The wasm and native spawn surfaces are symmetric** — same `spawn_child` shape, same `Subname` / cardinality / drop semantics — so an author moving between targets carries one model.
- **It reuses the #1363 machinery end to end** — resident module, type-tag, `init_typed_p32`, config-as-bytes, trampoline spawn. The new surface is one host-fn import plus the SDK method and its tag-lowering.

### Negative

- **The `_p32` contract grows a spawn import**, and the SDK grows a guest-side `spawn_child` + `Subname`. Contained to `aether-actor` / `aether-actor-derive` plus the trampoline's host-fn registration.
- **Spawn failure now has a guest-visible error path** (bad tag, subname invalid / retired / in use, init failure) that must cross the boundary as a result rather than a trap.

### Neutral

- **Addressing and wire format unchanged.** Spawned siblings live under `aether.component.trampoline:<name>` like every other loaded actor; `MailboxId = hash(name)` holds.
- **Foreign instantiation is unchanged** — still `load_component`, still capability-governed.

### Follow-on

- None required. Sibling spawn completes the ADR-0096 arc (multi-actor modules, load-time selection, runtime spawn). A future load-path optimization could instantiate another instance of an already-resident foreign module without a re-compile, but that stays on the load side and is out of scope here.

## Alternatives considered

- **Spawn foreign types by path or embedded bytes.** Rejected (also rejected in #1363): a foreign type can't be named by a typed `::<T>`, it pays a full load, and it's a larger authority grant — all of which make it a load, which already exists.
- **Every exported type implicitly spawnable.** Rejected: drops the `Instanced` / `Singleton` distinction on the wasm side and invents a parallel cardinality model instead of reusing ADR-0079.
- **Framework parent→child cascade-close.** Rejected: an asymmetry the native path doesn't carry, and the UI use case it targets is expressible in guest code via `unwire` plus tracked `MailboxId`s.
- **Spawn via mail to `aether.component`.** Rejected: a guest mailing a spawn request and awaiting a reply is async and clunkier at the call site than the synchronous host-fn, which the trampoline's live `Spawner` already makes feasible.

## Related

- ADR-0096 — multi-actor wasm modules; this is its deferred runtime follow-on and resolves its open granularity question.
- ADR-0079 — instanced actors, cardinality, and the `spawn_child` semantics reused here.
- ADR-0024 — dual-target `_p32` shims; the contract this extends with a spawn import.
- ADR-0090 — init-config byte carrier; a spawned sibling's config rides the same path.
- iamacoffeepot/aether#1409 — the tracking issue.
