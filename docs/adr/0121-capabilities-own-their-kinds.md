# ADR-0121: Capabilities own their kinds

- **Status:** Proposed
- **Date:** 2026-06-22

## Context

`aether-kinds` is the substrate's kind vocabulary (ADR-0069 split the data layer from mail transport; `aether-kinds` holds the concrete kinds). Over time it accumulated the full mail vocabulary of nearly every native capability — `aether.{fs,http,tcp,audio,text,render,handle,input,inventory,engine,gemini,anthropic,trace,…}.*` — even though each of those kinds is the protocol of exactly one capability and the callers that talk to it.

A kind is the shared contract between whoever sends a mail and whoever receives it. For a chassis capability the receiver is the capability and the senders are other actors and wasm guests, so the kind has to live somewhere both sides can depend on. That requirement is already satisfied without `aether-kinds` being the home: `aether-capabilities` is a two-layer crate — a default `native` feature carries the heavy implementation plus `aether-substrate`, while wasm-safe marker features (`render`/`audio`/`text`/`ui`) let a guest address a capability with no native dependencies — and `aether-data` (the `Kind`/`Schema` derives) is an always-on dependency. A capability can therefore declare its own kinds in its own module without forcing native dependencies onto guests.

Two problems motivate a change. The vocabulary crate grows without bound: every new capability adds its family to one shared file, and cohesion is split — a reader looking for "everything about audio" finds the kinds in `aether-kinds` and the implementation in a single monolithic `audio.rs`. And the capability implementations are thick: `audio.rs` is 5,520 lines, `lifecycle.rs` 1,695, `render.rs` 1,844, `fs.rs` ~2,000, with several others over 600.

One constraint bounds the move. `aether-capabilities` depends on `aether-substrate`, so a kind the substrate **core** dispatches cannot move into a capability without a cycle (`substrate → capabilities → substrate`).

## Decision

1. **Each capability is a directory submodule.** `src/<cap>/{mod.rs, <impl>.rs…, kinds.rs}`. Thick implementation files split along their existing cohesion seams.

2. **A capability owns its mail kinds in `<cap>/kinds.rs`.** The kind types move out of `aether-kinds` and into the capability's own module, riding the always-on (wasm-safe) layer so guests are unaffected.

3. **Must-stay rules — own what you can.** A capability owns the kinds it can; some stay central for two reasons. **(a) The cycle rule** — kinds the substrate core dispatches stay in `aether-kinds` (moving them would cycle `substrate → capabilities`):
   - **lifecycle** — `Tick` and the stage kinds; the scheduler dispatches `Tick` directly (`actor/native/binding.rs`).
   - **component** — the capability-registration kinds (`ComponentCapabilities`/`HandlerCapability`/`FallbackCapability`) the dispatcher reads (`actor/native/mod.rs`, `mail/capability.rs`).
   - **window** — `SetWindowTitle` is core-dispatched (`actor/native/dispatch.rs`); the six-kind family stays whole rather than split for one stuck kind.
   - **render's `FrameCheck` family** — the verification reductions the substrate's `capture.rs` consumes; the drawing kinds move, the verification kinds stay (a clean drawing-vs-verification line).
   - **`Mat4Apply`** — a math-primitive transform kind composing `aether_math` types, not a capability's mail protocol.

   **(b) The harness rule** — kinds the out-of-process MCP harness (`aether-mcp`) consumes by Rust type stay in `aether-kinds`. `aether-mcp` depends only on `aether-kinds` and is deliberately barred from a production dependency on `aether-capabilities`: a feature flag cannot stop Cargo feature unification from pulling the native/wasmtime stack into the harness. The harness-consumed families are the **engine** request/result/descriptor protocol (`SpawnEngine`/`ListEngines`/`ResolveComponent`/…), the **inventory** family (`Manifest`/`Resolve`/`ListKinds`/…), the **handle** describe family (`HandleDescribe`/`HandleSummary`), the **trace** dispatch kinds (`DispatchTraced`/`DispatchTracedAck`), and **render's capture** kinds (`CaptureFrame`/`CaptureFrameResult`/`SimilarityCheck`). For these the capability migration is the file split alone — the kind family stays whole in `aether-kinds`.

4. **`http` and `http_server` collapse into one `http/` submodule** (`client.rs` + `server.rs`). They remain two distinct capabilities (two mailboxes, two cap structs) co-located under one parent module.

5. **The principle, for future readers:** a kind lives with its contract owner, in a crate every consumer can depend on. The substrate core owns the kinds it dispatches; a capability owns the kinds it exchanges with its callers — except where a consumer that cannot depend on the capability (the substrate core, or the MCP harness) needs them, which keeps them central.

This amends ADR-0069: `aether-kinds` remains the home of the kinds the substrate core itself dispatches plus shared primitives, rather than the catch-all for every capability's protocol.

## Consequences

- **Cohesion.** Everything about a capability — implementation, send-side ext, receive-side handler, and kind contract — lives in one directory.
- **`aether-kinds` shrinks** to the substrate-core vocabulary plus shared primitives; its growth no longer tracks every new capability.
- **The crate becomes navigable** as the thick files decompose.
- **Guests are unaffected.** They already depend on `aether-capabilities` through the marker features; the kind types move with the always-on, wasm-safe layer.
- **Wire compatibility holds.** A kind id is `fnv1a_64` over `(KIND_DOMAIN, canonical(name, schema))`; moving a declaration between crates changes neither name nor schema, so ids are unchanged.
- **`describe_kinds` still surfaces the moved kinds** — the descriptor inventory is global and `aether-capabilities` is linked into the chassis binaries, so the per-kind descriptor submission rides the move.
- **Ownership is non-uniform** (the must-stay rules): a handful of capabilities keep their kinds central. The classification has to be stated and maintained.
- **Harness-facing capabilities mostly only split** (engine, inventory, handle, trace, render's capture surface): their kinds are MCP-consumed and stay central, so the migration is the file decomposition rather than a kind move. The deeper fix — making the wasm-safe layer of `aether-capabilities` dep-isolable so `aether-mcp` can depend on it without the native stack — stays open as a future ADR (the "restructure" alternative), and would unblock full ownership for these caps.
- **A large mechanical migration:** roughly twenty PRs, one per capability. Each moves a kind family and deletes the corresponding `aether-kinds` lines; `aether-kinds` vocabulary tests update with each.

## Alternatives considered

- **Leave kinds in `aether-kinds` (status quo)** — rejected: the vocabulary crate grows without bound and cohesion stays split.
- **Per-capability kind crates** (`aether-kinds-audio`, …) — rejected: many tiny crates, failing the "new crates must earn their place" bar; a per-cap module inside `aether-capabilities` gets the cohesion without the crate sprawl.
- **Move every kind, breaking the cycle via a new core-kinds crate upstream of the substrate** — rejected for now: more invasive and its own decision; "own what you can" delivers most of the benefit without restructuring the substrate's dependency graph. The door stays open if uniform ownership later earns its cost.
