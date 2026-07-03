# ADR-0125: Dissolve aether-codec into aether-data and aether-capabilities

- **Status:** Proposed
- **Date:** 2026-07-03

## Context

ADR-0069 established `aether-data` as the universal data layer — the `no_std` + `alloc` foundation that describes typed bytes (the schema vocabulary, the `Kind`/`Schema` traits, the wire encode/decode of a value given its Rust type) and that every wasm guest links. ADR-0072 then folded the `aether-hub-protocol` crate's generic stream framing into `aether-codec` alongside the schema-driven transcoder, making `aether-codec` the shared home for two byte-handling populations.

Those two populations have since diverged into unrelated halves with disjoint consumers and opposite `no_std` needs:

1. **The schema-driven transcoder** (`encode_schema` / `decode_schema` + `cast` + `conformance`) walks a runtime `aether_data::SchemaType` to convert agent-supplied JSON to and from wire bytes. It is the reflective counterpart to `aether-data`'s own compile-time `encode<T>` / `decode<T>`. Its sole consumer is `aether-mcp` (`crates/aether-mcp/src/tools.rs`). Its production code touches only `core`/`alloc`-available primitives (`fmt`, `error::Error`, `str`, `BTreeMap`) — `core::error::Error` is stable on the pinned 1.96 toolchain — so it is `no_std`-capable given `serde_json`'s `alloc` feature.
2. **`frame`** (length-prefix stream framing, generic over `<T: Serialize>`) is hard-bound to `std::io::{Read, Write}` + `OnceLock` + `env`, with no `no_std` path. Its consumers are the RPC stack — `rpc/server`, `engine/proxy`, `aether-mcp`, and the `aether-substrate-bundle` fleet tests — which after ADR-0124 all live in or route through `aether-capabilities`.

The crate's separateness was justified in part by a supposed dependency cycle — `aether-codec` was said to sit above the kind crates and so could not fold downward. That justification was an artifact of a single junk dev-dependency: `aether-capabilities` + `aether-kinds` were pulled into the dev-graph solely to feed one symmetric-roundtrip test, removed in #2536. `aether-codec`'s production dependency is now just `aether-data`. The crate is two unrelated halves that want different homes, held together by a founding rationale that no longer holds.

## Decision

Dissolve `aether-codec`, splitting it along the `no_std` line that separates its two halves:

- **The transcoder moves to `aether_data::codec`** — a module behind a new `codec` cargo feature on `aether-data`. The feature adds `serde_json` (`default-features = false, features = ["alloc"]`) as an optional dependency; the module's `std::` imports become `core::`/`alloc::`. `aether-data` stays `#![no_std]` with no `extern crate std`: with the feature off (the default, and what every wasm guest builds) nothing changes, and with it on the module compiles under `no_std` + `alloc`. Its sole consumer, `aether-mcp`, enables the feature. This restores the ADR-0069 framing — `aether-data` is "how the vocabulary's bytes are walked," and JSON↔wire transcoding is that same walk seen from the agent-facing side.
- **`frame` moves to `aether_capabilities::rpc::frame`** — native-gated, beside the `rpc::wire` types it frames, following ADR-0124's rule that a non-actor module lives inside the capability it serves. It stays `std`-bound; `aether-capabilities` is a `std` crate that already carries `serde` (with `alloc`) and `aether-data`, so the move needs no new dependencies.

`crates/aether-codec` is deleted and the workspace member removed. `aether-mcp`, `aether-substrate-bundle`, and `aether-capabilities` drop their `aether-codec` dependency; the `aether_codec::frame` call sites re-point to `aether_capabilities::rpc::frame`; the transcoder call sites (`aether-mcp` only) re-point to `aether_data::codec`.

ADR-0072 is superseded by this ADR. ADR-0069 is extended: the codec module joins the data layer it always described.

## Consequences

- One less workspace crate. The transcoder co-locates with the data layer whose bytes it walks; `frame` co-locates with the RPC capability it serves. "Codec or data?" and "frame or capability?" stop being cross-crate questions, and neither half lives in a crate the repo maps omit.
- `aether-data` gains an optional `serde_json` dependency, off by default and `alloc`-only. Its default build stays `no_std` with no new required dependency and no `extern crate std`; the `codec` feature is enabled only by `aether-mcp`. Enforced by the existing `no_std` build: `cargo build -p aether-data` (no features) and `--features codec` must both hold.
- The transcoder's `std::` imports become `core::`/`alloc::` — mechanical, no behavior change.
- No wire-format change. The transcoder and `frame` serialize identically; call sites move at unchanged (`encode_schema` / `decode_schema`) or mechanically-swapped (`aether_codec::frame` → `aether_capabilities::rpc::frame`) paths.
- `frame` stays native-gated inside `aether-capabilities`' `rpc` module; it does not compile for wasm guests, matching its reachability before the move.
- Coordination with issue 2534 (the `aether-mcp` marker-only `aether-capabilities`-dep flip): both edit `aether-mcp/Cargo.toml`. This change drops `aether-mcp`'s `aether-codec` dependency and adds `features = ["codec"]` to its `aether-data` dependency; 2534 flips the `aether-capabilities` dependency to `default-features = false`. The edits are independent and resolvable in either landing order.

## Alternatives considered

- **Fold everything, `frame` included, into `aether_data::codec` behind a `std` feature.** Rejected: `frame`'s `std::io` binding would force `aether-data` to conditionally `extern crate std`, puncturing the `#![no_std]` foundation guarantee that every wasm guest links and that ADR-0069 exists to protect. Splitting `frame` out to `aether-capabilities` preserves it; the transcoder, being `no_std`-capable, folds in without the puncture.
- **Keep `aether-codec` and add it to the crate maps.** Rejected: the crate is two unrelated halves with disjoint consumers, and the founding cycle rationale is gone (#2536). Centralization and legibility favor placing each half with the crate that owns its consumers — the same reasoning ADR-0124 applied to `aether-rpc`.
- **Move `frame` into `aether-substrate-bundle`, next to the hub wire vocabulary.** Rejected: `frame`'s caps-side consumers (`rpc/server`, `engine/proxy`) live in `aether-capabilities`; the bundle is the chassis assembly layer and would invert the dependency direction for the caps-side server — the same inversion ADR-0124 rejected for the RPC wire types.
