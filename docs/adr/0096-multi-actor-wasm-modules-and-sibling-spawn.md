# ADR-0096: Multi-actor wasm modules and sibling spawn

- **Status:** Proposed
- **Date:** 2026-06-06

## Context

On the wasm side, one crate is one actor. `export!` takes a single `FfiActor` type and emits that module's FFI shims for it, so a wasm module *is* exactly one actor. A set of related actors — a coordinator plus a handful of sub-actor roles — therefore becomes a set of one-actor crates, each separately built, loaded, and versioned. That doesn't scale: as the actor population grows, the single-serving-actor crate turns every role into its own binary, and an actor has no way to bring a sibling role into existence from inside its own code.

Native actors don't have this problem. A native crate compiles many actor types into one binary, and a native actor spawns a sibling with `ctx.spawn_child::<A>(subname, config)` (ADR-0079) — the listener/session pattern, where `TcpListenerActor` mints `TcpSessionActor` children. Siblings are reachable because they share the binary; the spawn names the child by a compile-time type.

ADR-0079 §Consequences left the wasm half open: *"Wasm-side spawn is deferred. Native is the v1 surface. When wasm components need to spawn instanced children… the host-fn shape needs settling."* This ADR settles it. The forcing function is structural — the one-crate-one-actor ceiling — and has upcoming pressure as the actor population grows, independent of any single consumer.

Constraints carried in:

- **The FFI boundary carries only scalars and `(ptr, len)` regions.** Type parameters and lifetimes don't cross it; a `spawn_child::<A>` on the guest cannot call the native `spawn_child::<A>` directly. (ADR-0024 — dual-target `_p32` shims.)
- **`FfiActor::Config: Kind`** — a guest actor's config is wire-shaped by construction, unlike `NativeActor::Config: Send + 'static`, which carries live native handles (`TcpStream`, `Arc<Engine>`) that cannot be reconstructed from guest-supplied bytes.
- **ADR-0029.** `MailboxId = hash(name)`, so a child's address is computable from its name without a host round-trip. Wire format unchanged.
- **ADR-0033.** A component's handler set rides in the wasm's `aether.kinds.inputs` custom section and surfaces through `describe_component`.
- **ADR-0066.** A component and its peers share the kind crate; under the `runtime` feature the same crate emits the cdylib via `export!`.
- **ADR-0090.** Init-config bytes already cross from `LoadComponent` into the guest's typed `init` via `Component::instantiate`.

## Decision

Five sub-decisions, designed together.

### 1. A wasm crate exports a set of actors

`export!` accepts more than one `FfiActor` type:

```rust
aether_actor::export!(RootManager, Panel, Button);
```

It emits one module-level FFI surface that dispatches by an **actor-type tag** rather than binding the module to a single type. The tag is a stable per-type id (the type's `NAMESPACE`, hashed the same way `MailboxId` is). The `#[actor]` macro continues to generate each type's handler table; `export!` generates the top-level dispatch across the set. The module carries every exported type's code; an instance *is* one of them.

A wasm module with one exported actor is the degenerate case of this and behaves exactly as before.

### 2. An instance is told its type at init

A fresh instance learns which exported type it is at `init`, from a tag threaded in alongside the config bytes (the ADR-0090 carrier gains a leading tag). The instance stores it; subsequent `receive` dispatch routes to that type's handler table, and the type's `Config` decodes from the same bytes. The wasm shim contract (ADR-0024) grows the tag parameter; the existing single-actor path is the tag-resolves-to-the-only-type case.

### 3. Sibling spawn re-instantiates the same module as the named type

A guest handler spawns a sibling:

```rust
let panel = ctx.spawn_child::<Panel>(Subname::Named("left-pane"), &PanelConfig { .. })?;
```

This reads exactly like native `spawn_child::<A>`, and works for the same reason: `Panel` is a sibling type in the same module, the way a native sibling is a type in the same binary. The shared wasm module plays the role of the shared native binary.

Under the surface, the guest ships (`Panel`'s type tag, the subname, the `Kind`-encoded `PanelConfig`) to the component host. The host re-instantiates the **parent's already-resident module** as the sibling type — nothing new is loaded, compiled, or sourced — and registers the child at `aether.component.trampoline:<subname>` as an instanced trampoline. The child's `MailboxId` is `hash("aether.component.trampoline:<subname>")`, which the guest computes locally; `spawn_child` returns it without a round-trip.

### 4. Nothing new crosses the boundary

The reason this is small: `FfiActor::Config: Kind`, so the config is already wire-shaped and crosses as the same payload `LoadComponent` uses today; and the child runs the module the parent already holds, so no bytes, path, or compiled artifact need to travel. The two things that *can't* cross a binary boundary — the type parameter and a live native handle — never have to: the type collapses to a tag the host resolves against the resident module, and a wasm config has no handles by construction.

### 5. Spawn stays native; the wasm path rides the component host

There is no first-class `spawn` primitive on `FfiCtx` paralleling the native one. `spawn_child::<Sibling>` is sugar over a mail to the component host (`aether.component`), which holds the resident module and is the sole owner of wasm-instance creation. This is the same seam `LoadComponent` already is: the host fills the native trampoline config (engine, linker, the module), the guest supplies only the wire bits (tag, subname, config). The native `spawn_child::<A>` is untouched and keeps naming compile-time native types.

## Consequences

### Positive

- **Crates scale as libraries.** One crate, one binary, many actor types. A subsystem stops fragmenting into one-actor crates.
- **Sibling spawn is symmetric with native.** Same call shape, same mental model; the shared module is the only thing standing in for the shared binary.
- **No boundary machinery.** No handle marshaling, no embedded-bytes pipeline, no native-spawn type registry, no asset paths. The change is additive over the existing config carrier and trampoline.
- **Multi-actor `export!` is useful on its own.** Even with no spawn, a crate can ship several actors the harness loads independently. That makes it a clean foundation the spawn layer rides on.

### Negative

- **Macro and FFI-shim rework.** `export!` and the `_p32` shim contract grow an actor-type tag and module-level dispatch; the ADR-0033 manifest grows from one handler set to one per exported type. Contained to `aether-actor` / `aether-actor-derive` plus the trampoline and load path.
- **Each child is its own instance.** A spawned sibling is a full instanced trampoline — its own mailbox, its own scheduling slot, its own linear memory (the compiled module is shared across instances by wasmtime; the per-instance cost is memory, not code). This is right for coarse actors and wrong for fine-grained swarms. **Guidance: spawn siblings for roles that hold real independent state and are worth addressing by mail (a coordinator, per-document or per-session managers), not for leaf elements that a retained data structure models better.** "Actor per UI widget" is the misuse this enables and should not encourage.
- **The load path needs a type selector.** Loading a multi-actor module must say which exported type the loaded instance is (a designated entry type, or an explicit selector on the load). Single-actor modules keep their current behavior.

### Neutral

- **Addressing and wire format unchanged.** Children live under `aether.component.trampoline` like every loaded component; `MailboxId = hash(name)` holds.
- **Wasm-side lifecycle control stays open.** Self-`shutdown` / `monitor` for wasm children remain native-only (issue 607); a spawned sibling tears down by the same paths a loaded component does. Out of scope here.

### Follow-on work

Split into two PRs with a clean dependency:

- **Foundation** — multi-actor `export!`, module-level FFI dispatch by actor-type tag, per-type manifest. Lands and proves out independently (multi-component loads from one crate).
- **Spawn layer** — `FfiCtx::spawn_child::<Sibling>` and the trampoline's sibling-spawn handler. Depends on the foundation.

## Alternatives considered

- **Spawn native actors from wasm.** Expose the trampoline's `NativeCtx::spawn_child` upward. Rejected on `config: A::Config`: native configs are `Send + 'static` and carry live handles (`TcpStream`, `Arc<Engine>`) that can't cross FFI as bytes, and the actors worth spawning are exactly the resource-holders whose handles can't be reconstructed from a spawn request. A per-type `assemble(wire_config, ctx) -> A::Config` seam could expose native actors whose config is fully wire-shaped, but that set is thin and the capability reads as odd. The trampoline-fills-config seam already exists for the one case that matters (`LoadComponent` building `WasmTrampolineConfig`).
- **Cross-binary / embedded-bytes spawn.** Let a component spawn a child running a *different* module, naming it by type with the wasm bytes embedded (`WasmComponent::BYTES`). Rejected: a type carrying its own bytes forces a two-pass build (a crate embedding its own cross-target artifact, breaking a bare `cargo build`) and fractures the `FfiActor` concept (some actors carry bytes, some don't); sourcing the module by `assets://` path forfeits a self-contained release binary. The same-module/sibling approach removes the need — different roles are sibling types in one module, not different binaries.
- **Clone (spawn another instance of the same type).** Rejected: a coordinator and its sub-actors are different roles, not copies; cloning a coordinator yields another coordinator. Same-type spawn only serves identical-worker fan-out, which is a separate, narrower capability parked until something needs it.
- **Children as embedded environments inside one instance.** Host the sub-actors inside the parent's wasm instance rather than as sibling trampolines. Rejected: an independently-mailable, observable child needs its own mailbox (ADR-0029, one mailbox per actor) and its own scheduling slot; folding several into one instance forces sub-addressing within a mailbox and serializes them behind one run-token. Children that genuinely don't need to be addressed from outside are the parent's internal data, not actors.
- **A first-class FFI `spawn` primitive.** A `spawn` on `FfiCtx` mirroring the native one. Rejected: native `spawn` names compile-time native types and stays focused there; the wasm path is sugar over the component host, which already owns wasm-instance creation, rather than a parallel primitive.

## Related

- ADR-0079 — Instanced actors as a first-class category. This ADR unparks its deferred wasm-side spawn.
- ADR-0024 — Dual-target host-fn / FFI shims (`_p32`); the shim contract this extends with an actor-type tag.
- ADR-0033 — Handler-driven inputs manifest; grows to one handler set per exported type.
- ADR-0066 — Component and peers share the kind crate; the `runtime` feature emits the cdylib.
- ADR-0090 — Init-config byte carrier; gains a leading actor-type tag.
- ADR-0029 — `MailboxId = hash(name)`; child addresses stay computable guest-side.
- Issue 607 — wasm-side lifecycle control (`shutdown` / `monitor`), still open.
