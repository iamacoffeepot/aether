# ADR-0096: Multi-actor wasm modules

- **Status:** Accepted (hosted-actor addressing later revised by [ADR-0099](0099-actor-identity-and-addressing.md); the actor-type tag here is reused as the ActorId)
- **Date:** 2026-06-06

## Context

On the wasm side, one crate is one actor. `export!` takes a single `FfiActor` type and emits that module's FFI shims for it, so a wasm module *is* exactly one actor. A set of related actors — a coordinator plus a handful of sub-actor roles — therefore becomes a set of one-actor crates, each separately built, loaded, and versioned. That doesn't scale: as the actor population grows, the single-serving-actor crate turns every role into its own binary.

Native crates don't carry this constraint — a native binary compiles many actor types, and the framework spawns and addresses them individually. The wasm host should be able to hold a *library* of actors the same way: one crate, one binary, multiple exported actor types.

This ADR covers that foundation only. Letting an actor spawn one of its sibling types at runtime — the wasm analogue of native `spawn_child::<A>` — builds on this and is deferred to a follow-on ADR, which carries its own decisions and an open granularity question. ADR-0079 §Consequences deferred wasm-side spawn; that unparks on the follow-on, not here.

Constraints carried in:

- **The FFI surface is a fixed set of module-level shims** (`init_with_config_p32`, `receive_p32`, …) bound today to a single actor (ADR-0024, `_p32`).
- **ADR-0033.** A component's handler set rides in the wasm's `aether.kinds.inputs` custom section and surfaces through `describe_component`.
- **ADR-0066.** A component and its peers share the kind crate; under the `runtime` feature the same crate emits the cdylib via `export!`.
- **ADR-0090.** Init-config bytes already cross from `LoadComponent` into the guest's typed `init` via `Component::instantiate`.

## Decision

Three sub-decisions.

### 1. A wasm crate exports a set of actors

`export!` accepts more than one `FfiActor` type:

```rust
aether_actor::export!(RootManager, Panel, Button);
```

It emits one module-level FFI surface that dispatches by an **actor-type tag** rather than binding the module to a single type. The tag is a stable per-type id (the type's `NAMESPACE`, hashed the way `MailboxId` is). The `#[actor]` macro continues to generate each type's handler table; `export!` generates the top-level dispatch across the set. The module carries every exported type's code; an instance *is* one of them. A single-actor module is the degenerate case of this and behaves exactly as before.

### 2. An instance is told its type at init

A loaded instance learns which exported type it is at `init`, from a tag threaded in alongside the config bytes (the ADR-0090 carrier gains a leading tag). The instance stores it; `receive` dispatch routes to that type's handler table, and the type's `Config` decodes from the same bytes. The wasm shim contract (ADR-0024) grows the tag parameter; the existing single-actor path is the tag-resolves-to-the-only-type case.

### 3. The load path selects the type

Loading a multi-actor module names which exported type the instance becomes. `LoadComponent` and the MCP `load_component` tool gain an optional **export selector** — the target type's `NAMESPACE` — defaulting to a designated entry type when omitted, so single-actor modules and "load the main one" both work unspecified. Each loaded instance is one actor of one type, addressed as today at `aether.component.trampoline:<name>`. The `aether.kinds.inputs` manifest (ADR-0033) grows from one handler set to one per exported type, which makes a module's exported actors introspectable — an agent reads the available exports and picks which to load, and `describe_component` reports the loaded instance's type.

## Consequences

### Positive

- **Crates scale as libraries.** One crate, one binary, many actor types — a subsystem stops fragmenting into one-actor crates.
- **A crate can ship several actors the harness loads independently**, no spawn involved.
- **It's the foundation the wasm sibling-spawn follow-on rides on**, and it stands on its own without it.

### Negative

- **Macro and FFI-shim rework.** `export!` and the `_p32` contract grow an actor-type tag and module-level dispatch; the ADR-0033 manifest grows to one handler set per exported type. Contained to `aether-actor` / `aether-actor-derive` plus the trampoline and load path.
- **The module is a single blob with every exported type's code behind a small dispatch router.** It's larger than a single-actor module, but smaller than the N one-actor crates it replaces — those each duplicate the shared dependencies (kind crate, serialization, runtime shims) that one module carries once. The compiled module is shared across all its instances by wasmtime, so code is resident once per module regardless of instance count; only linear memory is per-instance. The cost that remains is bundling: loading the module to use one actor still carries the rest. **Guidance: group actors that belong together (a subsystem), the way you'd scope any library; don't pool unrelated actors into one crate.**

### Neutral

- **Addressing and wire format unchanged.** Loaded actors live under `aether.component.trampoline` as today; `MailboxId = hash(name)` holds. *(Revised by [ADR-0099](0099-actor-identity-and-addressing.md): a hosted actor's `MailboxId` is now the lineage fold, not `hash(name)`, and the embedding-host node is renamed `aether.embedded`. The multi-actor-module foundation above is unchanged, and its actor-type tag is reused as the ActorId.)*
- **The load path gains a type selector.** Single-actor modules keep their current behavior.

### Follow-on

- **Wasm sibling spawn** — `ctx.spawn_child::<Sibling>` re-instantiating the same resident module as a sibling type, the wasm analogue of native `spawn_child::<A>`. Its own ADR and issue; this foundation is its prerequisite.

## Alternatives considered

- **One-actor-per-crate, with better many-crate ergonomics.** Rejected: it doesn't lift the ceiling — every role stays a separate binary and a separate load.
- **One crate compiling to several wasm modules (one cdylib per actor).** Rejected: a crate emits one cdylib, so "several modules" means several crates — the status quo under another name.
- **Runtime dynamic linking of separate wasm modules sharing memory.** Rejected: wasm component-model territory, far heavier than the problem needs; one module with several actor types is sufficient.

## Related

- ADR-0079 — Instanced actors as a first-class category; its deferred wasm-side spawn unparks on the sibling-spawn follow-on, which builds on this.
- ADR-0099 — Actor identity and addressing. Reuses this ADR's **actor-type tag** as the ActorId, and revises the hosted-actor **addressing**: a loaded actor's `MailboxId` is the lineage fold (not `hash(name)`) and the embedding-host node is `aether.embedded`. The multi-actor-module foundation here stands.
- ADR-0024 — Dual-target `_p32` shims; the contract this extends with an actor-type tag.
- ADR-0033 — Handler-driven inputs manifest; grows to one handler set per exported type.
- ADR-0066 — Component and peers share the kind crate; the `runtime` feature emits the cdylib.
- ADR-0090 — Init-config byte carrier; gains a leading actor-type tag.
