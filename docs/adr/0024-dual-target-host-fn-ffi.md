# ADR-0024: Dual-target wasm32/wasm64 host fn FFI

- **Status:** Accepted (Phase 1 only; Phase 2 deferred)
- **Date:** 2026-04-17
- **Accepted:** 2026-04-19

## Context

The aether-component FFI is wasm32-pointer-shaped end to end. Every host fn the substrate registers — `aether::send_mail`, `aether::resolve_mailbox`, `aether::resolve_kind`, `aether::reply_mail` (ADR-0013), `aether::save_state` (ADR-0016) — takes pointer-typed arguments as `u32`. The guest exports the substrate looks up at instantiation (`receive`, `on_rehydrate`) match the same shape. The guest SDK reflects all of it: `bytes.as_ptr().addr() as u32` is the documented pattern that survives `cargo check` for both wasm32 and host targets, and `Mail::__from_raw(u32)` is the FFI entry point. This is fine for `wasm32-unknown-unknown` and exactly what wasmtime expects for wasm32 modules.

It is also the single thing standing between the project and ever loading a wasm64 component.

Wasm32's hard linear-memory ceiling is 4 GiB — the address space, not anything aether sets. For current components that's not load-bearing: the hello-component is ~17 KiB compiled, runtime allocation is small, and the architecture pushes asset memory (textures, vertex buffers, audio) onto the substrate side via host fns rather than into the guest's linear memory. So far so good.

The 4 GiB ceiling becomes load-bearing in two cases worth taking seriously:

- **Debug builds.** Unoptimized wasm carries debug info inline, doesn't dead-code-eliminate, doesn't merge allocations, and runs without LTO. A component that's 100 MiB resident in release can be several times that in debug. Not 4 GiB today, but the gap closes faster than it opens as the engine grows.
- **In-process simulation state.** Voxel grids, large entity systems, in-component physics state, ML inference weights — these don't always fit the substrate-side resource pattern. When they don't, they live in the guest's linear memory, and 4 GiB is a real ceiling rather than a theoretical one.

wasmtime supports the memory64 proposal natively. The blocker is not the runtime; it's the import / export names the substrate publishes and the SDK declares.

### Why two phases

The full design (Phase 2, below) registers every host fn at *both* a `u32`- and `u64`-pointer signature, distinguished by name suffix. Implementing that today buys forward-compat that the project genuinely wants, but it pulls in costs that aren't paid back yet:

- A `register_dual` helper and per-host-fn double maintenance every time a host fn is added.
- A `target` field on `LoadComponent`, changing wire surface that has to be versioned.
- Validating wasm64 in CI requires a nightly toolchain plus `-Z build-std=std,panic_abort` plus the `rust-src` component (a 2026-04 toolchain probe confirmed: stable can't add the target, nightly has no prebuilt artifacts, and `build-std` is the only path). Either the workspace pins to nightly (punishes the stable build for one example) or the wasm64 smoke gets carved out of CI (smoke without CI guards isn't worth much).
- wasm64's Tier 3 status in Rust shows no concrete promotion timeline — multi-year horizon is the realistic forecast.

Meanwhile, the *thing* that locks in forward-compat — the import / export naming convention — is small, mechanical, and one-time. Renaming the existing FFI surface to a `_p32` suffix now (while the audience is N=0 external component authors) means future wasm64 support is purely additive: drop in `_p64` siblings, no further breaking change. Shipping that piece on its own captures the strategic win without taking on the carry cost.

## Decision

Two phases, with a hard split.

### Phase 1 — naming convention only (this ADR)

Rename every pointer-typed FFI symbol to a `_p32` suffix. This is the entire scope of Phase 1; nothing else in the substrate or SDK changes.

**Imports renamed (substrate side, in `aether-substrate/src/host_fns.rs`):**

| Before                       | After                              |
| ---------------------------- | ---------------------------------- |
| `aether::send_mail`          | `aether::send_mail_p32`            |
| `aether::reply_mail`         | `aether::reply_mail_p32`           |
| `aether::resolve_kind`       | `aether::resolve_kind_p32`         |
| `aether::resolve_mailbox`    | `aether::resolve_mailbox_p32`      |
| `aether::save_state`         | `aether::save_state_p32`           |

**Exports renamed (guest side, emitted by the `export!` macro):**

| Before          | After                |
| --------------- | -------------------- |
| `receive`       | `receive_p32`        |
| `on_rehydrate`  | `on_rehydrate_p32`   |

**Exports unchanged** because they take no pointer arguments: `init`, `on_replace`, `on_drop`. No width concern, no rename benefit, no churn.

The substrate's `get_typed_func` lookups for `receive` / `on_rehydrate` are updated to the new names. The SDK's `raw` module keeps its Rust-side identifiers (`raw::send_mail`, etc.) and uses `#[link_name = "..._p32"]` attributes to remap the wasm-visible name — this minimises the diff in `aether-component/src/lib.rs`, which calls `raw::send_mail` and friends from many sites.

Length / count arguments stay `u32` regardless of target. Pointer-width is exactly what the suffix gates; only pointers will widen in Phase 2.

**Forward-compat property.** After Phase 1 ships, adding wasm64 in Phase 2 is purely additive: register `aether::send_mail_p64` next to `aether::send_mail_p32`, emit `receive_p64` next to `receive_p32`, no rename, no break. Existing components built against `_p32` keep loading exactly as they do today.

### Phase 2 — dual registration, deferred

Phase 2 is the originally-proposed design. It is bookmarked here, not implemented now. Trigger to revisit: wasm64 is promoted to Tier 2 (a Rust toolchain shift), or a real component approaches the 4 GiB ceiling, whichever comes first.

The design that will ship when Phase 2 is unblocked:

- A `register_dual<F32, F64>` helper in the substrate's host-fn module, registering both pointer-width variants of each host fn from one source. Implementations differ only in the `as usize` cast above the FFI boundary; the dispatch / state save / reply paths are pointer-width-agnostic.
- `Config::wasm_memory64(true)` enabled unconditionally on the substrate engine (wasmtime supports both targets in a memory64-enabled engine; wasm32 modules just don't use the wider addressing).
- A `PointerTarget` enum and `target: Option<PointerTarget>` field on `LoadComponent`, defaulting to `Wasm32`. The substrate uses the declared target to pick the receive-shim export name (`_p32` vs `_p64`), validate memory limits against the target's ceiling, and surface a clear `LoadResult::Err` when declared imports don't match the declared target.
- The SDK's `raw` module gates the `extern "C"` block on `target_pointer_width`; the `export!` macro emits the correctly-named/typed receive shim per target. Mail/Sink/Ctx surface stays unchanged — already `usize`-based per ADR-0014.
- A wasm64 smoke component (likely a wasm64 build of hello-component) demonstrating the dual-target path end-to-end: load, deliver, reply, save_state. CI carve-out for the nightly + `build-std` requirement, or run as a manually-invoked script outside CI; resolve at the time.

Phase 2 is a meaningful chunk of work, but Phase 1 makes it strictly additive — no FFI break for any component that loaded successfully against the Phase 1 surface.

## Consequences

### Positive

- **Forward-compat is locked in cheaply.** ~30 lines of mechanical rename across substrate + SDK + test fixtures. Future wasm64 add becomes additive — no breaking FFI rename when the trigger fires.
- **Carry cost is zero.** No `register_dual` helper, no per-host-fn double maintenance, no `target` field on `LoadComponent`. Adding a new host fn in Phase 1's world looks identical to today — just register one `*_p32`-suffixed name.
- **Toolchain debt is zero.** Phase 1 doesn't touch the wasm64 toolchain. Workspace stays on stable; CI doesn't grow a nightly carve-out.
- **Wasm32 stays the only deployed target.** Toolchain-mature path stays the easy path. wasm64 remains opt-in, unimplemented until needed.
- **The breaking change happens once, while the audience is N=0.** All in-repo example components rebuild as part of this PR; no external consumers exist to migrate.

### Negative

- **The `_p32` suffix is aspirational.** Until Phase 2 ships, every host fn name carries a suffix that points at a target distinction the substrate doesn't yet implement. A newcomer will reasonably ask "why the suffix?" The answer is "future wasm64." The alternative — leaving names un-suffixed and renaming when wasm64 lands — is uglier (breaks every component that exists at that point).
- **The `_p32` rename is itself a breaking FFI change.** Mitigated by the breakage being fully internal: hello-component, echoer, caller, input_logger all rebuild from this PR. No external authors to coordinate.
- **Phase 2 is genuinely deferred, not "soon."** wasm64 might be years away. If a project pressure forces it sooner, Phase 2 still has to be implemented at that point — Phase 1 doesn't shortcut Phase 2's work, only its breakage cost.

### Neutral

- **Engine ↔ hub wire format is untouched.** Pointer width is a substrate-internal concern; mail bytes don't carry it. This stays true through Phase 2.
- **No host-side runtime change.** The substrate's mail dispatch, state save, and reply paths don't grow new code paths in Phase 1. Phase 2 adds dual registration but the inner logic remains pointer-width-agnostic.
- **No component-model adoption.** The component model has its own pointer-polymorphism story via canonical ABIs; Phase 1 does not preempt or preclude eventually moving to it.

## Alternatives considered

- **Wasm32-only forever.** Rejected: the strategic concern (debug-build ceilings, in-process simulation state) is real enough that "deal with it later" is a deferral rather than a decision. Phase 1 captures the cheap forward-compat win without committing to Phase 2's full implementation.
- **Wasm64-only.** Rejected: wasm64 toolchain maturity is genuinely behind wasm32 (probe-confirmed: stable can't build the target; nightly + `build-std` is the only path), and the bounds-check perf overhead is a real cost for components that don't need the address space.
- **Always-u64 host fn signatures with wasm32 guests zero-extending.** wasmtime resolves imports by exact signature match; a wasm32 guest declaring an import as `(u64) -> u32` and the host registering `(u64) -> u32` doesn't help, because the guest's pointer values are 32-bit and the wasm import declaration would need to be `(u32) -> u32` to compile against `wasm32-unknown-unknown`. Rejected as not actually possible without a wasmtime-side adapter.
- **Two import modules (`aether32`, `aether64`) instead of suffixed names.** Same effect; cosmetic difference. Rejected because pairing names within one module is closer to the existing surface and keeps `aether::*` as the single conceptual namespace.
- **Migrate to the wasmtime component model.** Component model has canonical ABI for pointer polymorphism and would handle this more elegantly. Rejected as out of scope for this ADR — much larger surface change with its own design questions. The dual-registration approach is a tactical fix that doesn't preclude eventually moving to the component model.
- **Defer the whole ADR (no rename now).** Rejected after deliberation: the cost of the rename is fixed and small (audience is N=0); the cost of breaking the FFI later when external component authors exist is variable and grows with project adoption. Locking the convention while the cost is bounded is the higher-leverage move.
- **Skip the `_p32` suffix; only suffix Phase 2's wasm64 additions.** Rejected: it leaves wasm32 imports asymmetrically un-suffixed forever, which is the cosmetic-debt outcome the explicit naming convention is trying to avoid.

## Follow-up work

### Phase 1 (this PR)

- **`aether-substrate`**: rename the five `linker.func_wrap("aether", "<name>", ...)` entries in `host_fns.rs` to `<name>_p32`; update `get_typed_func` lookups for `receive` and `on_rehydrate` in `component.rs` to `_p32` siblings; update inline WAT test fixtures across `component.rs` and `control.rs` to match.
- **`aether-component`**: add `#[link_name = "..._p32"]` attributes to each `extern "C"` import in `raw.rs`; update the `export!` macro in `lib.rs` to emit `receive_p32` and `on_rehydrate_p32` (other exports unchanged).
- **Example components**: rebuild hello-component + echoer + caller + input_logger — no source change, just cargo build.
- **CLAUDE.md**: brief note that the FFI surface is `_p32`-suffixed in anticipation of wasm64.

### Phase 2 (deferred — not committed by this PR)

- **`aether-substrate`**: implement `register_dual` helper; add `_p64` siblings for every existing host fn. Enable `Config::wasm_memory64(true)` unconditionally.
- **`aether-substrate`**: thread `target` from `LoadComponent` through instantiation; pick the receive-shim export name based on it; validate import-target match and emit `LoadResult::Err` with a clear reason on mismatch.
- **`aether-kinds`**: add `PointerTarget` enum + `target: Option<PointerTarget>` field on `LoadComponent` (additive, defaults to `Wasm32`).
- **`aether-component`**: gate the `raw` module's `extern "C"` blocks on `target_pointer_width`; update `export!` macro to emit the correctly-named/typed receive shim per target.
- **Tests**: build a minimal wasm64 smoke component (or repurpose hello-component) and verify the dual-target path end-to-end. Resolve the nightly + `build-std` toolchain question (in-CI carve-out vs out-of-CI script) at the time.
- **CLAUDE.md**: document `target` field default and the `rustup component add rust-src --toolchain nightly` step for components that opt in.

### Parked, not committed

- Substrate-level transcoding between targets (compile a wasm32 module to wasm64 on load) — out of scope and unmotivated.
- Per-component custom address-space ceilings beyond the 64 GiB substrate-level wasm64 sanity cap.
- Component-model migration (separate larger ADR if/when adopted).
- Wider integer parameters (e.g. count args going to u64) — kept as u32 here; revisit if ADR-0006's 1 MiB frame cap is ever lifted.
