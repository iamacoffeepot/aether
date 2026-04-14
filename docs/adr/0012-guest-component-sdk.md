# ADR-0012: Guest-side component SDK

- **Status:** Proposed
- **Date:** 2026-04-14

## Context

`aether-hello-component` is the first real component and exposes exactly what it takes to write one today:

- Two `#[unsafe(no_mangle)] pub unsafe extern "C" fn` exports (`init`, `receive`) with raw `u32` ptr/len/count parameters.
- A hand-rolled `#[link(wasm_import_module = "aether")] unsafe extern "C"` block declaring `send_mail`, `resolve_kind`, `resolve_mailbox`.
- `unsafe` wrappers that do `name.as_ptr() as u32` and `name.len() as u32` casts.
- `static mut` slots with `u32::MAX` sentinels for every cached kind and mailbox id, plus an explicit sentinel check at every send site.
- Pointer math at send time (`&payload as *const T as u32`, `size_of::<T>() as u32`).
- `#[cfg(target_arch = "wasm32")]` guards around every body so the crate still type-checks for `cargo test --workspace` on the host, where the `aether` module isn't linkable and pointer casts would truncate.

Everything in that list is boilerplate a component author has to get right. Nothing in it is part of what the component is trying to *do*. ADR-0002 said "Claude is just another mail sender"; the guest ergonomics right now push the opposite direction — components look like raw WASM shims, not mail peers.

ADR-0010 makes the pain worse in two ways. First, components are now something agents produce routinely (load, iterate, replace), so the per-component boilerplate multiplies. Second, the `static mut` + sentinel pattern was tolerable when the one component was written once by a human; when Claude generates components, a forgotten sentinel check lands silently as mail to mailbox 0.

Forces at play:

- **Unsafe should be a substrate concern, not a component concern.** The substrate is the trust boundary; it already wraps raw wasmtime APIs in a typed `SubstrateCtx`. The guest surface deserves the same treatment. A component doing `ptr as u32` is not expressing anything the author wanted to express.
- **Typed sends are already possible.** `aether-substrate-mail` defines `#[repr(C)]` payload types with `Kind` impls. The guest does `bytemuck`-style casts by hand; a `ctx.send(sink, &payload)` that does them under the hood is a pure ergonomics win — no new wire behavior.
- **Host-target compilability is a real constraint.** Components live in the workspace and `cargo test --workspace` runs on the host. Today that's handled by `cfg(target_arch = "wasm32")` sprinkled per item. A shim crate that provides a stub `aether` module on non-wasm targets moves that concern out of every component.
- **Dispatch is the open design question.** Send ergonomics are largely mechanical. Receive is where design choices bite: enum-over-all-kinds vs per-kind handler fns vs a trait with a single `receive`. Each has real tradeoffs and none is obviously right before a second real component exists.
- **The SDK is additive.** It sits on top of the existing FFI. The raw `extern` surface stays — nothing in the substrate or host side changes. A component written against the raw FFI today continues to work; new components pick up the SDK.

## Decision

Add a new crate `aether-component`: a guest-side safe wrapper over the substrate's host-function ABI. Components depend on it instead of writing raw `extern "C"` blocks and `static mut` caches.

Scope of this ADR is the **send side** and the **init/resolve side**, plus the plumbing that removes per-component `cfg` noise. The **receive-side dispatch shape** is deliberately left open — see §4.

### 1. Crate shape

`aether-component` is a new crate in the workspace:

- `no_std`-compatible (guests are `no_std`-adjacent; the FFI contract has no allocator dependency).
- Provides a stub `aether` module on non-wasm targets so components compile for `cargo test --workspace` without per-item `cfg` guards. Stub impls panic-or-return-sentinel; components can still be unit-tested for pure logic on the host, they just can't call the FFI there.
- Depends on `aether-mail` (for the `Kind` trait) and nothing substrate-side. A guest component that depends on `aether-component` does **not** pull in substrate or hub code.

The raw `extern "C"` block lives in `aether-component` and is the only place in a guest component tree that writes `unsafe extern`.

### 2. Init and resolved handles

The SDK replaces `static mut u32` caches with two typed handles:

- `KindId<K: Kind>` — returned by `resolve::<K>()`. Internally a `u32`; externally a phantom-typed wrapper. A `KindId<Tick>` cannot be passed where a `KindId<DrawTriangle>` is expected.
- `Sink<K: Kind>` — returned by `resolve_sink::<K>(name)`. Wraps the mailbox id plus the kind id for sends to that sink. `Sink<DrawTriangle>` is the only thing the `render` target needs: the author named it once, and the type system remembers which kind it accepts.

Resolution happens inside an SDK-owned `init` shim. The component author writes a `fn init(ctx: &mut InitCtx) -> State`; the SDK's `#[no_mangle] extern "C" fn init` calls it and stores the returned `State` in a `OnceCell` (or equivalent single-slot container). The `u32::MAX` sentinel pattern goes away because a failed resolve in `init` panics loudly instead of being silently tolerated until first send.

### 3. Typed send

`ctx.send(sink, &payload)` where `sink: &Sink<K>` and `payload: &K::Payload`. The SDK does the `as *const T as u32` cast, the `size_of::<T>() as u32` length, and the `count = 1` case internally. Multi-item sends get a `ctx.send_many(sink, slice)` variant.

Payload types remain defined in `aether-substrate-mail` (or wherever the kind is declared); the SDK doesn't own wire layout, it just knows how to hand a `&T` to the host. `T: bytemuck::Pod` is the bound — same as today, just enforced at the send call rather than implicit in the `as u32` cast.

### 4. Receive dispatch — deferred shape

The receive side has three plausible shapes:

- **Single `receive` method on a `Component` trait.** User matches on kind id or kind name themselves. Simplest; leaks the raw switch to every component.
- **Macro-generated match over per-kind handler fns.** `#[aether_component::on(Tick)] fn on_tick(state, msg)`. Clean per-kind, but macros carry their own ergonomic debt and the dispatch table has to be assembled somewhere.
- **Enum of all inbound kinds.** `enum Inbound { Tick(Tick), ... }` with the SDK decoding into it. Requires the component to declare its inbound set up front; plays well with exhaustiveness checking.

Each has real costs and the current one-component evidence doesn't pick a winner. This ADR commits only to: **a receive shim that decodes kind id → typed reference and hands it to a user function whose exact shape is decided when the second component lands.** Until then, the SDK exposes the simplest version (trait with a single `receive(&mut self, ctx, kind_id, bytes)`) and the hello component uses it. Revisiting happens the first time a component has more than two inbound kinds or needs per-kind state machines — at which point the choice is informed, not speculative.

### 5. What stays out of scope

- **Reply-to-sender.** ADR-0008's token flow is plumbed at the wire level but the host fn doesn't exist yet. When it lands, `Sink<K>` gains a `reply` constructor; no SDK-shape change.
- **Allocator / heap-allocated payloads.** `#[repr(C)] Pod` is the whole wire vocabulary today. Dynamic payloads (strings, `Vec`, variable-length structs) cross into ADR-0007's `Opaque` territory and a separate encoding story.
- **Lifecycle hooks beyond init/receive.** No `on_drop`, no `on_replace`. Replace semantics (ADR-0010 §5) drop the old instance's linear memory; there's nowhere honest to run drop code.
- **Capability subsetting.** Today every component gets every host fn. A capability-gated SDK (component opts into `send` but not `resolve_mailbox`, say) is a future direction; the raw host surface would need to grow gates first.

## Consequences

### Positive

- **Removes `unsafe` from component authorship.** The only `unsafe` is inside `aether-component`. Components become ordinary Rust crates that happen to compile to WASM.
- **Sentinel-check footgun disappears.** Failed resolves surface at `init` time, loudly, instead of becoming a silent "mail to mailbox 0" at first send.
- **Type errors catch kind/sink mixups.** Passing a `Sink<DrawTriangle>` where a `Sink<Tick>` is needed is a compile error. Today it's a runtime bad-dispatch.
- **`cfg(target_arch = "wasm32")` goes away from components.** The SDK owns the host/wasm split; components read like ordinary code.
- **Claude-generated components get smaller and safer.** ADR-0010 made component authoring a routine Claude activity; the SDK cuts the surface area Claude has to get right by an order of magnitude.

### Negative

- **New crate to maintain.** `aether-component` becomes part of the public guest API surface. Breaking it breaks every component. The bar for changing it has to be set accordingly.
- **Host/wasm stub duplicates the extern surface.** The stub exists only to keep `cargo test --workspace` green; every host fn added to the substrate now needs a matching stub. Small cost per fn, but it's per-fn forever.
- **Receive-side decision is deferred, not avoided.** The simplest trait works for one component; the second component will force the choice. Deferring is a bet that the forcing case will be more informative than the current one — probably true, not free.
- **Raw FFI still works.** A component can bypass the SDK and write `extern "C"` directly. Fine for now; if it becomes a maintenance problem, the raw surface can be moved behind a feature flag or renamed.

### Neutral

- **No wire change.** This is a pure guest ergonomics ADR. Substrate, hub, and protocol crates are untouched.
- **`aether-mail` stays a shared dep.** `Kind` continues to live there; the SDK consumes it, components consume it transitively.
- **`aether-substrate-mail` is still the payload home.** The SDK doesn't pull payload types down into itself — payload ownership stays where the substrate and guest both need to agree.

## Alternatives considered

- **Macro-only SDK (no runtime types).** A single `component!` macro generates the whole `init`/`receive`/`extern` block from a declarative spec. Rejected for now: macro-only hides the types, which makes IDE experience worse and forecloses hand-written hybrids. The trait+types SDK can grow a macro sugar layer later if the boilerplate pressure returns.
- **Fold the SDK into `aether-mail`.** One crate, guest and host both consume it. Rejected: `aether-mail` is pure vocabulary (`Kind` trait, nothing else); `aether-component` carries an FFI. Different trust domains, different dep graphs. Mixing them invites substrate-side code to accidentally depend on guest shims.
- **Generate the SDK from substrate host-fn definitions.** A build script reads the `Linker::func_wrap` calls and emits matching guest stubs. Rejected: the host side is small enough (three fns) that hand-written stays readable, and code generation adds a build-time coupling between substrate and SDK that's a net loss until the host surface has a dozen entries.
- **Keep the raw FFI, just add typed helpers.** No new crate; add a `safe_send<T>` free function in `aether-substrate-mail`. Rejected: doesn't remove the `#[unsafe(no_mangle)] extern "C" fn init/receive`, doesn't remove the `static mut` caches, doesn't remove the `cfg` guards. Half-measure.
- **Cap'n-proto-style IDL for component boundary.** Define components in a schema language; generate both guest and substrate sides. Rejected at V0: enormous plumbing for a surface that has four verbs (init, receive, send, resolve). If the boundary grows complex enough to justify IDL, revisit then.

## Follow-up work

- `aether-component` crate with `Component` trait, `InitCtx`, `Ctx`, `Sink<K>`, `KindId<K>`, `resolve` / `resolve_sink` helpers, and the `#[no_mangle]` shim for `init`/`receive`.
- Host-target stub `aether` module so non-wasm builds compile without per-item `cfg`.
- Port `aether-hello-component` to the SDK as the proof — diff size is the headline measurement for whether the ergonomics moved.
- Document the guest-side `Cargo.toml` shape (`crate-type = ["cdylib"]`, opt levels, the SDK dep) in the component author's README.
- **Parked, not committed:** macro sugar for `on(Kind)` handlers, enum-based inbound dispatch, reply-to-sender, non-Pod payload encoding, lifecycle hooks beyond init/receive, capability subsetting, IDL-driven boundary.
