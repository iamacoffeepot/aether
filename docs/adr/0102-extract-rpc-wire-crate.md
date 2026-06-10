# ADR-0102: Extract the RPC wire vocabulary into its own crate

- **Status:** Proposed
- **Date:** 2026-06-10

## Context

The MCP coordinator (`aether-mcp`) is an out-of-process RPC *client*: it dials the hub's `RpcServerCapability` and relays each tool call as a wire `Call`. It never hosts an actor system, never loads wasm, never touches the GPU. Its manifest header states this as an invariant — "no `aether-substrate` / wasmtime / wgpu in the dep graph: the coordinator just speaks the wire."

That invariant is false today. `aether-mcp` depends on `aether-capabilities`, whose `default = ["native"]` feature pulls `dep:aether-substrate` + `dep:wasmtime`. `cargo tree -p aether-mcp` shows both in the graph. The coordinator's *actual* production surface from that crate is tiny and pure-data:

- `aether_capabilities::rpc::{MailEnvelope, MailboxAddress}` (used in `tools.rs`, `rpc.rs`)
- `aether_capabilities::trace_walk::TreeWalk` (used in `tools.rs`)

Everything heavier it references (`EngineServer`, `EngineConfig`, `TraceDispatchCapability`, `InventoryCapability`) is confined to `#[cfg(test)]` modules. So the coordinator needs the RPC client wire types and a trace tree-walker — nothing that requires wasmtime or the substrate.

Issue #1525 tried to fix this by setting `default-features = false` on the dependency and making the `native` feature gate the code honestly. That attempt bounced (Executing → Design) on two findings, both since re-verified against current code:

1. **The `native` feature gates deps, not code.** `aether-capabilities` marks its native surface with `cfg(not(target_arch = "wasm32"))` (52 sites), not `cfg(feature = "native")` (14 sites). The feature switches on `aether-substrate` + wasmtime as dependencies but does not gate the code that uses them. `cargo check -p aether-capabilities --no-default-features` on a host target produces 236 errors across ~25 modules: native code compiled with its deps removed. The feature is, in effect, a lie that only holds because nothing builds the crate without `native`.

2. **Feature gating cannot satisfy the invariant under workspace feature unification.** Cargo unifies features across all workspace members during resolution. `aether-substrate-bundle` *is* the native host and must enable `native` on `aether-capabilities`; that unifies onto every other member's view, including `aether-mcp`. With `default-features = false` on the dependency and the dev-dep removed entirely, `cargo check -p aether-mcp -v` still passes `feature="native"` to `aether-capabilities` and still compiles `aether-substrate`. No amount of in-crate feature/`cfg` work changes this: as long as one workspace member needs the native variant of a crate, every member resolves to it.

The conclusion that follows from (2): the stated invariant is a property of the *dependency graph*, and a feature flag is the wrong layer to express it. A crate edge is the only mechanism that survives workspace unification, because unification operates on features *within* a crate, not across distinct crates.

## Decision

Extract the coordinator-reachable RPC wire vocabulary into a new crate, `aether-rpc`, that has no path to `aether-substrate`, wasmtime, or wgpu.

The new crate holds:

- The RPC wire types and client primitive: `MailEnvelope`, `MailboxAddress`, the `Call` client surface, and the supporting wire structs currently under `aether_capabilities::rpc` that carry no native dependency.
- `trace_walk` (`TreeWalk` and its helpers) — pure tree reconstruction over trace records.

`aether-capabilities` depends on `aether-rpc` and re-exports its types at the existing paths (`aether_capabilities::rpc::*`, `aether_capabilities::trace_walk::*`) so existing call sites and the native `rpc::server` keep compiling unchanged. The substrate-bound `rpc::server` (`RpcServerCapability`) stays in `aether-capabilities` under its native gating — it depends on `aether-rpc` for the wire types it serves.

`aether-mcp` depends only on `aether-rpc` for its production surface. Its `#[cfg(test)]` use of `EngineServer` / `EngineConfig` continues to ride a dev-dependency on `aether-capabilities` with `native` enabled (dev-dep feature unification applies to that crate's test builds only and does not enter the production graph the standalone coordinator build resolves).

The split forces the `native` feature's existing dishonesty to be resolved *at the seam that matters*: the wire types move to a crate with no native deps, so they are correct-by-construction; the native server surface keeps its current gating in place.

The acceptance signal changes from the (unachievable) in-workspace `cargo tree -p aether-mcp` to a structural one: `aether-rpc` builds with no `aether-substrate` / wasmtime / wgpu in its tree, and the `aether-mcp` → `aether-capabilities` production edge is gone (replaced by the `aether-mcp` → `aether-rpc` edge). The win is then real for any standalone build of the coordinator, and the manifest header becomes true rather than aspirational.

## Consequences

- The `aether-mcp` manifest invariant becomes enforced by topology, not asserted by comment. A future change cannot silently re-pull wasmtime into the coordinator without adding a new crate edge that a reviewer would see.
- A new crate, `aether-rpc`. This clears the project's "new crates must earn their place" bar: the boundary is the *mechanism* that makes a stated architectural invariant hold (feature gating provably cannot, per Context finding 2), and it expresses a genuine layer — wire vocabulary versus native capability host — that is currently co-located by accident. The lighter dependency graph is a consequence of the layering, not the motive for the crate.
- `aether-capabilities` re-exports keep all current call sites source-compatible; the change is additive at the API surface. No wire format changes — the moved types serialize identically.
- The latent `native`-gates-deps-not-code bug is narrowed: the wire vocabulary no longer lives behind a feature that lies. The remaining 52 `cfg(not(target_arch = "wasm32"))` sites in `aether-capabilities` are out of scope here and may be tidied separately, but they no longer block the coordinator trim.
- Scoping risk: `aether_capabilities::rpc` must be split cleanly into wire types (move) versus the `RpcServerCapability` server (stay). If any wire type currently reaches into a native dependency, that coupling has to be broken as part of the move — this is the real work of the implementing PR and should be confirmed before the crate is created.
- Follow-on: once `aether-rpc` exists, the `default-features = false` flip drafted on the bounced #1525 worktree becomes a small true-up at the end, not the fix itself. The dev-dependency on `aether-capabilities` for the test-only `EngineServer` usage remains.

## Alternatives considered

- **Re-gate `aether-capabilities` in place (target-arch → feature) and split the `rpc` module internally.** Rejected: even done perfectly it cannot make `cargo tree -p aether-mcp` clean inside the workspace, because feature unification re-enables `native` for the whole resolution (Context finding 2). It addresses the lie but not the invariant.
- **A `wire` feature on `aether-capabilities` exposing only the coordinator surface.** Rejected for the same reason — a feature is defeated by unification. It also keeps the wire vocabulary co-located with the native host, so the manifest invariant stays a comment rather than a graph fact.
- **Keep default features and `cfg`-gate inside `aether-mcp`.** Rejected: leaves the coordinator build heavy and the header false; the problem is the dependency edge, not the coordinator's own code.
- **Drop / defer #1525 entirely.** Reasonable on timing (the benefit is partly latent until a standalone coordinator build exists), but it leaves a false manifest invariant and a latent feature-gating bug in place. The extraction is cheap relative to the correctness it buys, so deferral was not chosen.
