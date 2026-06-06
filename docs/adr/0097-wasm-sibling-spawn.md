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
- **Crate layering.** The wasmtime `Linker<ComponentCtx>` and the host-fn surface are built in `aether-substrate` (`boot.rs` → `host_fns::register`); `WasmTrampoline` lives in `aether-capabilities`, which depends on substrate, not the reverse. A substrate-registered host-fn therefore cannot name `WasmTrampoline`, so `spawn_child::<WasmTrampoline>` cannot be monomorphized at the host-fn site. The actual spawn must execute in the capabilities layer, which shapes the mechanism below.

## Decision

Five sub-decisions.

### 1. A guest spawns a sibling by type

```rust
let panel = ctx.spawn_child::<Panel>(Subname::Counter, &PanelConfig { /* … */ })?;
```

Guest-side, typed, mirroring native. The SDK lowers `::<Panel>` to the sibling's compile-time tag (`mailbox_id_from_name(Panel::NAMESPACE)`) and encodes the config to wire bytes. A new host-fn import on the `_p32` surface carries `(tag, subname, config_bytes)` across the boundary and returns the new `MailboxId` synchronously — the id is `hash(name)` (ADR-0029), so it is known before the instance finishes spawning (§4). The typed call site is available because the guest crate compiled `Panel`: resolving `Panel::TAG` and encoding `Panel::Config` happen entirely at compile time, with no cross-boundary monomorphization.

### 2. Spawnable siblings are `Instanced`; spawn reuses ADR-0079 wholesale

A spawnable sibling is an `Instanced` exported type. `Subname::Counter | Named`, the `MailboxId`-only return (no strong handle), retire-on-drop, and the unique-owner invariant carry over unchanged. The spawned sibling's `ReplyTo` stamps the spawner's mailbox, so its replies route back to the parent. This answers ADR-0096's open granularity question: the spawnable unit is an `Instanced` sibling type, and its lifecycle vocabulary is the existing native one rather than a parallel wasm-only model.

### 3. Sibling-only — foreign instantiation is a load, not a spawn

`spawn_child` instantiates only types compiled into the calling module. Instantiating a foreign module — another binary, by path, embedded bytes, or name — is a load, served by `load_component` (a guest mails `aether.component.load`; ADR-0096 gave that path an export selector). The line falls out of the typed primitive: there is no `::<ForeignType>` to write for a type the crate did not compile against, so anything foreign arrives as a name plus opaque bytes, which is the load shape. The cost model reinforces it — `spawn_child` re-instantiates an already-compiled, already-registered resident module with no disk read, compile, or kind registration, whereas a foreign child pays a full module load. And the authority differs: spawning own siblings stays inside the authority granted at load, while pulling arbitrary other binaries into the host is a larger grant that belongs to a governed load. The partition: spawn is one more instance of code the actor already is; load brings in code it is not.

### 4. The host-fn stages the request; the trampoline performs the spawn

Because a substrate-registered host-fn cannot name `WasmTrampoline` (see §Context — crate layering), the host-fn does not spawn directly. It stages `(tag, subname, config_bytes)` onto `ComponentCtx` — the same host-fn-stages / host-drains pattern `save_state` and `init_failed` already use — and returns the new `MailboxId`, computed synchronously as `hash("aether.component.trampoline:<subname>")` (ADR-0029). After `Component::deliver` returns, the `WasmTrampoline` — in `aether-capabilities`, holding its `NativeCtx`, engine, linker, and a retained handle to its resident `Module` — drains the staged request and runs `ctx.spawn_child::<WasmTrampoline>(subname, WasmTrampolineConfig { module, type_tag, config, .. })`, the same call `handle_load` makes. `init_typed_p32` constructs the sibling; the config bytes ride the ADR-0090 path; the spawner stamps the trampoline's mailbox as the sibling's `ReplyTo`, and since the guest's mailbox is the trampoline's mailbox, replies land at the guest.

Because the id is the name's hash, the guest receives it synchronously even though the spawn completes just after the host-fn returns — the call site stays `let panel = ctx.spawn_child::<Panel>(…)?` as in §1. The trade is that **success and failure split**: synchronously-checkable errors (subname invalid / too long) come back as the host-fn's `Err`, but a spawn-time failure (subname retired or in use, guest `init` error) surfaces asynchronously — logged on the trampoline and, where it matters, mailed back to the parent — rather than as the `spawn_child` return value.

### 5. No automatic lifecycle cascade

A spawned sibling has independent lifecycle, mirroring native. Children self-close; the parent tracks them by `MailboxId` and, if it wants "close the root, children go away," sends close-mail to its tracked children from its own `unwire`. The framework does not cascade parent drop to children — that would be new lifecycle surface the native path lacks, and the UI case it serves is fully expressible in guest code with the `MailboxId`s `spawn_child` already returns.

## Consequences

### Positive

- **A wasm subsystem composes at runtime, not just at load.** A coordinator type stands up its worker types on demand from one resident module — the UI-root-spawns-panels shape, and per-connection / per-entity patterns generally.
- **The wasm and native spawn surfaces are symmetric** — same `spawn_child` shape, same `Subname` / cardinality / drop semantics — so an author moving between targets carries one model.
- **It reuses the #1363 machinery end to end** — resident module, type-tag, `init_typed_p32`, config-as-bytes, trampoline spawn. The new surface is one host-fn import, the SDK method + tag-lowering, a staging field on `ComponentCtx`, and the trampoline's drain step.

### Negative

- **The `_p32` contract grows a spawn import**, the SDK grows a guest-side `spawn_child` + `Subname`, `ComponentCtx` grows a staged-spawn field, and the trampoline grows a post-`deliver` drain. The host-fn body stays in `aether-substrate` (it only stages); the typed `spawn_child::<WasmTrampoline>` stays in `aether-capabilities`.
- **The trampoline retains its compiled `Module`** to re-instantiate siblings — a cheap `Arc` clone (wasmtime `Module` is `Arc`-backed), not a second copy of the code.
- **Spawn success and failure split.** The `MailboxId` returns synchronously (it is the name hash) and synchronous subname validation errors come back as the host-fn's `Err`, but a spawn-time failure (retired / in-use subname, guest `init` error) surfaces asynchronously rather than at the call site.

### Neutral

- **Addressing and wire format unchanged.** Spawned siblings live under `aether.component.trampoline:<name>` like every other loaded actor; `MailboxId = hash(name)` holds.
- **Foreign instantiation is unchanged** — still `load_component`, still capability-governed.

### Follow-on

- None required. Sibling spawn completes the ADR-0096 arc (multi-actor modules, load-time selection, runtime spawn). A future load-path optimization could instantiate another instance of an already-resident foreign module without a re-compile, but that stays on the load side and is out of scope here.

## Alternatives considered

- **Spawn foreign types by path or embedded bytes.** Rejected (also rejected in #1363): a foreign type can't be named by a typed `::<T>`, it pays a full load, and it's a larger authority grant — all of which make it a load, which already exists.
- **Every exported type implicitly spawnable.** Rejected: drops the `Instanced` / `Singleton` distinction on the wasm side and invents a parallel cardinality model instead of reusing ADR-0079.
- **Framework parent→child cascade-close.** Rejected: an asymmetry the native path doesn't carry, and the UI use case it targets is expressible in guest code via `unwire` plus tracked `MailboxId`s.
- **Spawn via mail to `aether.component`.** Rejected: a guest mailing a spawn request and awaiting a reply is fully async at the call site, losing the synchronous `spawn_child` symmetry with native. The crate-layering wall (the host-fn can't name `WasmTrampoline`) is a real argument for it, but stage-and-drain (§4) keeps the synchronous id while still performing the typed spawn in the capabilities layer, so the layering doesn't force the async surface. Kept as the fallback if the sync-id / async-failure split (§4) proves unworkable in practice.
- **Type-erased spawn hook injected at boot.** A `dyn` trampoline-spawn hook installed onto the substrate `Spawner` by the bundle, called by the host-fn. Rejected: it's a cross-boundary indirection invented to dodge the layering, where stage-and-drain instead reuses the existing `ComponentCtx` staging pattern (`save_state` / `init_failed`) and lets the trampoline — which already owns the spawn context — do the typed call.

## Related

- ADR-0096 — multi-actor wasm modules; this is its deferred runtime follow-on and resolves its open granularity question.
- ADR-0079 — instanced actors, cardinality, and the `spawn_child` semantics reused here.
- ADR-0024 — dual-target `_p32` shims; the contract this extends with a spawn import.
- ADR-0090 — init-config byte carrier; a spawned sibling's config rides the same path.
- iamacoffeepot/aether#1409 — the tracking issue.
