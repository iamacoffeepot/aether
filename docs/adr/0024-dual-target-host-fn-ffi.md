# ADR-0024: Dual-target wasm32/wasm64 host fn FFI

- **Status:** Proposed
- **Date:** 2026-04-17

## Context

The aether-component FFI is wasm32-pointer-shaped end to end. Every host fn the substrate registers — `aether::send_mail`, `aether::resolve_mailbox`, `aether::reply_mail` (ADR-0013), `aether::save_state` (ADR-0016), and the per-component receive shim (ADR-0014) — takes pointer-typed arguments as `u32`. The guest SDK reflects the same: `bytes.as_ptr().addr() as u32` is the documented pattern that survives `cargo check` for both wasm32 and host targets, and `Mail::__from_raw(u32)` is the FFI entry point. This is fine for `wasm32-unknown-unknown` and exactly what wasmtime expects for wasm32 modules.

It is also the single thing standing between the project and ever loading a wasm64 component.

Wasm32's hard linear-memory ceiling is 4 GiB — the address space, not anything aether sets. For current components that's not load-bearing: the hello-component is ~17 KiB compiled, runtime allocation is small, and the architecture pushes asset memory (textures, vertex buffers, audio) onto the substrate side via host fns rather than into the guest's linear memory. So far so good.

The 4 GiB ceiling becomes load-bearing in two cases worth taking seriously now:

- **Debug builds.** Unoptimized wasm carries debug info inline, doesn't dead-code-eliminate, doesn't merge allocations, and runs without LTO. A component that's 100 MiB resident in release can be several times that in debug. Not 4 GiB today, but the gap closes faster than it opens as the engine grows. "We hit the ceiling first in debug" is an annoying failure mode — it punishes the iteration loop, not production.
- **In-process simulation state.** Voxel grids, large entity systems, in-component physics state, ML inference weights — these don't always fit the substrate-side resource pattern. When they don't, they live in the guest's linear memory, and 4 GiB is a real ceiling rather than a theoretical one.

The cost of dual-targeting *now* is bounded — five host fns on the substrate side, the SDK's `raw` module on the guest side, the `export!` macro's receive shim. The cost of dual-targeting *later* is the same five host fns plus every new host fn added between now and then, plus every guest crate that picked up the wasm32-only assumption, plus every test that hardcoded the pointer width. That's the wrong direction for a cost curve.

wasmtime supports the memory64 proposal natively. The blocker is not the runtime; it's the import signatures the substrate publishes.

## Decision

Every host fn is registered at *both* a `u32`-pointer and a `u64`-pointer signature, distinguished by name suffix. Guests declare one or the other based on their compile-time `target_pointer_width`. `LoadComponent` carries the target so the substrate enables the right wasmtime config and rejects modules whose declared imports don't match.

### 1. Naming convention

Each host fn ships under two names in the same `aether` module, suffixed by pointer width:

```
aether::send_mail_p32   (ptr: u32, len: u32, ...)
aether::send_mail_p64   (ptr: u64, len: u64, ...)

aether::resolve_mailbox_p32 / _p64
aether::reply_mail_p32      / _p64
aether::save_state_p32      / _p64
```

Wasmtime resolves imports by `(module, name)` and validates signatures at instantiation. Pairing names rather than pairing modules (`aether32::send_mail` vs `aether64::send_mail`) keeps the import surface a single conceptual namespace and means a guest accidentally importing the wrong width fails with a clear "function not found" error rather than a silent module swap.

The receive shim that the guest *exports* follows the same pattern: `__aether_receive_p32` and `__aether_receive_p64`. The substrate, on instantiation, looks up whichever export matches the component's declared target.

Length / count arguments stay `u32` regardless of target. The pointer-width parameter is exactly that — only pointers widen. Frame sizes are still capped at 1 MiB per ADR-0006; nothing about that changes for wasm64 guests.

### 2. Substrate-side registration

A small helper in the substrate's host-fn module registers both variants of each host fn from one source:

```rust
fn register_dual<F32, F64>(
    linker: &mut Linker<SubstrateCtx>,
    name: &str,
    f32_impl: F32,
    f64_impl: F64,
) -> wasmtime::Result<()> { ... }
```

The two implementations differ only in pointer width — both cast their pointer args to `usize` and call into a shared inner function. There is no per-target logic above the FFI boundary; the substrate's mail dispatch, state save, and reply paths are pointer-width-agnostic.

The substrate's `Config` enables `wasm_memory64(true)` unconditionally (wasmtime supports both targets in a memory64-enabled engine — wasm32 modules just don't use the wider addressing). Per-engine config; not per-component.

### 3. `LoadComponent` declares the target

```rust
pub enum PointerTarget {
    Wasm32,
    Wasm64,
}

pub struct LoadComponent {
    // ... existing fields ...
    pub target: Option<PointerTarget>,   // None defaults to Wasm32
}
```

Defaulting to `Wasm32` matches the current toolchain reality — `wasm32-unknown-unknown` is Tier 1 in Rust; `wasm64-unknown-unknown` is Tier 3 (works for basic crates, no automated Rust CI coverage, some dependencies may not compile). Components opt into wasm64 explicitly.

The substrate uses the declared target to:

1. Pick the receive-shim export name to look up after instantiation (`_p32` vs `_p64`).
2. Validate the loaded module's memory limits respect the target's ceiling — a wasm32 module declaring `memory.grow` past 4 GiB is rejected before instantiation; a wasm64 module declaring memory beyond `ComponentBudget.max_memory_bytes` is rejected the same way.
3. Surface a clear `LoadResult::Err { reason: "module imports aether::send_mail_p64 but target is Wasm32" }` if the declared target doesn't match the imports the module actually pulls in.

### 4. Guest SDK selects at compile time

`aether-component`'s `raw` module emits the right `extern "C"` block per target:

```rust
#[cfg(target_pointer_width = "32")]
extern "C" {
    #[link_name = "send_mail_p32"]
    fn send_mail(ptr: u32, len: u32, ...) -> u32;
    // ...
}

#[cfg(target_pointer_width = "64")]
extern "C" {
    #[link_name = "send_mail_p64"]
    fn send_mail(ptr: u64, len: u64, ...) -> u32;
    // ...
}
```

`Mail`, `Sink<K>`, `Ctx`, `InitCtx`, `Component`, `KindId<K>` and the rest of the user-facing surface stay unchanged — they already use `usize` internally per ADR-0014. The pointer-width difference is invisible above the `raw` module.

The `export!` macro emits a single receive shim function with the correct name and signature for the target — `__aether_receive_p32` on wasm32, `__aether_receive_p64` on wasm64. Components don't pick; the macro picks.

### 5. What this ADR does *not* do

- **No mid-life retargeting.** A component is wasm32 or wasm64 for its lifetime. Replace can swap to a different target (rebuild + reload), but a single instance can't switch.
- **No fat binaries.** Each compiled `.wasm` is one target. The agent picks which to load; the substrate doesn't transcode.
- **No component-model adoption.** The component model has its own pointer-polymorphism story via canonical ABIs, but the aether-component SDK uses raw wasm imports. Migrating to the component model is a separate, much larger ADR.
- **No automatic toolchain bootstrapping.** `rustup target add wasm64-unknown-unknown` is the developer's responsibility; the substrate just refuses to load a module whose imports it can't satisfy.

## Consequences

### Positive

- **The 4 GiB ceiling stops being a strategic risk.** Components that need more declare wasm64 at load and get the room without any substrate-side change after this ADR ships.
- **No retooling debt accumulates.** Every host fn added from this point forward is dual-registered automatically (the helper enforces it). The "migration to wasm64" never has to happen as a discrete event.
- **Debug-build headroom is fixed.** A component that hits 4 GiB only in debug can flip to wasm64 for the dev cycle and back to wasm32 for release without changing engine code.
- **Clean failure modes.** Mismatched targets fail at load (clear error message in `LoadResult::Err`), not at runtime. Imports resolve or they don't; wasmtime's signature checker does the work.
- **Wasm32 stays the default.** Toolchain-mature path stays the easy path. wasm64 is opt-in for the cases that need it.

### Negative

- **Every host fn is now two entries.** The `register_dual` helper keeps it mechanical, but a developer adding a host fn has to remember to write both impls. Mitigated by code review and by the helper signature requiring both. Still a sharp edge.
- **Wasm64 toolchain is Tier 3.** A component author who flips to wasm64 may find a transitive dependency that doesn't compile. Mitigated by defaulting to wasm32 — only components that need wasm64 pay the toolchain tax.
- **wasm64 has measurable bounds-check overhead.** wasmtime can't use the same signal-handler / guard-page tricks for 64-bit linear memories that it uses for 32-bit. Benchmarks suggest 10–20% throughput on memory-heavy hot loops. Acceptable for components that need the address space; an active reason not to default to wasm64.
- **Receive shim export name lookup adds one branch at instantiation.** The substrate picks `_p32` or `_p64` based on the declared target. Negligible cost; one boolean.
- **Error surface widens slightly.** "Wrong target" joins the existing list of `LoadResult::Err` reasons. Worth it for the explicit failure mode.

### Neutral

- **Engine ↔ hub wire format is untouched.** Pointer width is a substrate-internal concern; mail bytes don't carry it.
- **No host-side change after this ADR ships.** Every future host fn just calls `register_dual`; the substrate runtime doesn't grow new code paths.
- **Existing components don't migrate.** Hello-component stays wasm32, declared-by-default. No migration; no rebuild required.

## Alternatives considered

- **Wasm32-only forever, plan to migrate later.** Rejected: the cost of migrating later is the cost of doing it now plus the cost of every host fn added in between, and the strategic concern (debug-build ceilings, in-process simulation state) is real enough that "deal with it later" is a deferral, not a decision.
- **Wasm64-only.** Pick one target, the bigger one. Rejected: wasm64 toolchain maturity is genuinely behind wasm32, and the bounds-check perf overhead is a real cost for components that don't need the address space. Forcing every component onto wasm64 is paying for a cost without getting the benefit.
- **Always-u64 host fn signatures with wasm32 guests zero-extending.** wasmtime resolves imports by exact signature match; a wasm32 guest declaring an import as `(u64) -> u32` and the host registering `(u64) -> u32` doesn't help, because the guest's pointer values are 32-bit and the wasm import declaration would need to be `(u32) -> u32` to compile against `wasm32-unknown-unknown`. Rejected as not actually possible without a wasmtime-side adapter.
- **Two import modules (`aether32`, `aether64`) instead of suffixed names.** Same effect; cosmetic difference. Rejected because pairing names within one module is closer to the existing surface and keeps `aether::*` as the single conceptual namespace agents and component authors think about.
- **Migrate to the wasmtime component model.** Component model has canonical ABI for pointer polymorphism and would handle this more elegantly. Rejected as out of scope for this ADR — it's a much larger surface change with its own design questions (typed interfaces, world definitions, host adapter generation). The dual-registration approach is a tactical fix that doesn't preclude eventually moving to the component model.
- **Defer until a component actually hits 4 GiB.** Same shape as "wasm32-only forever." Rejected for the same reason: by the time the trigger fires, the migration is more expensive than the design.

## Follow-up work

- **`aether-substrate`**: implement `register_dual` helper; convert every existing host fn (`send_mail`, `resolve_mailbox`, `reply_mail`, `save_state`) to dual-registration. Enable `Config::wasm_memory64(true)` unconditionally.
- **`aether-substrate`**: thread `target` from `LoadComponent` through instantiation; pick the receive-shim export name based on it; validate import-target match and emit `LoadResult::Err` with a clear reason on mismatch.
- **`aether-substrate-mail`**: add `PointerTarget` enum + `target: Option<PointerTarget>` field on `LoadComponent` (additive, defaults to `Wasm32`).
- **`aether-component`**: gate the `raw` module's `extern "C"` blocks on `target_pointer_width`; update `export!` macro to emit the correctly-named/typed receive shim per target. Mail/Sink/Ctx surface unchanged.
- **Tests**: build a minimal wasm64 smoke component (or repurpose hello-component) and verify the dual-target path end-to-end — load, deliver, reply, save_state. Confirm wasm32 hello-component still loads unchanged.
- **CLAUDE.md**: brief note in the MCP harness section about `target` defaulting to wasm32 and the `rustup target add wasm64-unknown-unknown` step for components that opt in.
- **Parked, not committed:**
  - Substrate-level transcoding between targets (compile a wasm32 module to wasm64 on load) — out of scope and unmotivated.
  - Per-component custom address-space ceilings beyond the 64 GiB substrate-level wasm64 sanity cap.
  - Component-model migration (separate larger ADR if/when adopted).
  - Wider integer parameters (e.g. count args going to u64) — kept as u32 here; revisit if ADR-0006's 1 MiB frame cap is ever lifted.
