# ADR-0124: Fold the RPC Wire Vocabulary Back into aether-capabilities

- **Status:** Superseded (#3737 — the RPC wire vocabulary was extracted back to `crates/aether-rpc`)
- **Date:** 2026-07-03

## Context

ADR-0102 extracted the RPC wire vocabulary (`WireFrame` and its substructs, the `Call`/`RpcClient` primitive) and `trace_walk` into a dedicated crate, `aether-rpc`, so the MCP coordinator (`aether-mcp`) could state "no `aether-substrate` / wasmtime / wgpu in the production dep graph" as a topology fact. The crate edge was chosen over a feature flag on two findings: the then-`native` feature gated dependencies without gating the code that used them, and cargo's feature unification re-enables a chassis-demanded feature onto every co-selected consumer, so no feature arrangement could keep the coordinator lean inside a workspace build.

Both findings have shifted since. The identity/runtime split (ADR-0122, issue 2311) turned the old `native` feature into `runtime` and made the gate progressively honest: a marker-only build of `aether-capabilities` is a supported, CI-exercised configuration for wasm guests, and the capability quality sweep is driving the remaining host-side gating debt to zero. And cargo has since stabilized `resolver.feature-unification` (available on the pinned cargo 1.96): setting it to `"package"` resolves each selected package's dependency tree independently, which is exactly the guarantee ADR-0102 said only a crate edge could provide.

The crate edge, meanwhile, buys less than ADR-0102's framing implies. The tunnel pre-build (`scripts/ensure-tunnel.sh`) co-selects `aether-mcp` and `aether-substrate-bundle` in a single cargo invocation, so under default (selected-set) unification the coordinator's own bring-up flow already compiles against a runtime-enabled graph; the lean coordinator materialized only in isolated `-p aether-mcp` builds. Against that thin benefit the extraction costs a workspace-level indirection: the wire vocabulary lives in a crate that no repo map lists, one hop away from the `aether.rpc` capability module that serves it, and the "where does RPC code live" answer requires reading ADR-0102 rather than the crate tree.

## Decision

Dissolve `aether-rpc` and move its two modules into `aether-capabilities`' always-on, target-agnostic layer, each inside the capability it serves — neither is an actor, so neither warrants a crate-root module: the wire vocabulary becomes `aether_capabilities::rpc::wire` (glob-re-exported at the existing `aether_capabilities::rpc::*` paths, next to the `RpcServerCapability` that serves it), and the trace walk becomes `aether_capabilities::trace::walk` (next to the `aether.trace` cap whose `aether.trace.tail` it stitches). The `aether-mcp` coordinator depends on `aether-capabilities` for its wire surface.

The coordinator-leanness invariant transfers from the crate edge to the feature system, in two steps:

1. **Interim (the flatten itself):** `aether-mcp` depends on `aether-capabilities` with default features. The "no native deps" manifest invariant is suspended, stated as deferred in the manifest header rather than deleted.
2. **End state (issue 2534):** once the marker-only host build compiles (`cargo check -p aether-capabilities --no-default-features` — the capability sweep's remaining gating debt), the coordinator flips to `default-features = false` and the workspace sets `resolver.feature-unification = "package"` in `.cargo/config.toml`, so per-package resolution holds the lean coordinator in every build shape — including workspace builds and the tunnel's combined invocation, which the crate edge never covered.

ADR-0102 is superseded by this ADR.

## Consequences

- One less workspace crate; the RPC wire vocabulary is co-located with the capability that serves it and appears in the crate maps (`aether-capabilities` is documented; `aether-rpc` never was). "Wire type or capability?" stops being a two-crate question.
- `aether-mcp` and every wasm guest recompile on any `aether-capabilities` change. For guests this was already true; for the coordinator it is a new coupling to a high-churn crate, accepted deliberately — the coordinator's wire surface is versioned with the caps crate anyway, and the tunnel rebuild flow co-builds the chassis regardless.
- Between the flatten and issue 2534, `aether-mcp` production builds link the substrate stack. The interim is bounded by the capability sweep's completion of the runtime gating.
- `resolver.feature-unification = "package"` changes resolution semantics workspace-wide: any shared crate whose consumers disagree on features compiles once per distinct feature set. The build-time cost is measured as part of issue 2534 before the knob lands, and feature-divergence build breaks it surfaces in other members are fixed there.
- The boundary discipline ADR-0102 enforced by topology (a coordinator change cannot reach substrate types without a visible new crate edge) is re-expressed as a graph check once issue 2534 lands: `cargo tree -p aether-mcp -e normal` carrying no `aether-substrate` / wasmtime / wgpu, enforceable in CI.
- The wire types keep their `aether_capabilities::rpc::*` paths, so those call sites compile unchanged; the trace walk moves from the crate-root `::trace_walk` to `::trace::walk`, a mechanical import swap at its three consumers. The wire format is untouched (the types serialize identically).

## Alternatives considered

- **Keep `aether-rpc` and document it (add it to the crate maps).** Rejected by explicit decision: centralization and legibility of the crate tree outweigh the crate-edge invariant, whose lean-coordinator benefit only ever materialized in isolated `-p` builds.
- **Flatten with the marker-only flip immediately.** Rejected: the marker-only host build does not compile yet (the sweep's remaining gating debt), and per-package unification would also surface the same errors through the wasm-guest workspace members in host builds. The flip is real work with its own measurement step; sequencing it as issue 2534 keeps the flatten mechanical.
- **Flatten into a new module of `aether-substrate-bundle` next to the hub wire vocabulary.** Rejected: the RPC wire types are served by `aether-capabilities`' `rpc` module and consumed by guests-adjacent tooling; the bundle is the chassis assembly layer and would invert the dependency direction for the caps-side server.
