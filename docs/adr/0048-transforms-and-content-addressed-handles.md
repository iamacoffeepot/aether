# ADR-0048: Native transforms and content-addressed handle ids (Phase 3 of ADR-0045)

- **Status:** Accepted
- **Date:** 2026-04-25
- **Revised:** 2026-05-20 — re-scoped from wasm `Func::call` transforms to **native transforms only** before implementation. The original draft made a transform a wasm export the substrate called via `wasmtime::Func::call`, carried across the load boundary in an `aether.dag.transforms` custom section. That design predated the unified actor model (ADR-0074) settling and inverted the engine's native/wasm relationship — it made a bespoke wasm calling convention the primary transform mechanism. This revision makes transforms native pure Rust functions and defers wasm transforms to a future ADR with an explicit forcing function. See §"Why native, why defer wasm" and the reworked Alternatives.

## Context

ADR-0045 Phase 1 shipped handles as a wire type (`Ref<K>`) and a substrate-global handle store with refcounted entries. ADR-0047 (Phase 2) added DAG submit/cancel/status with sources + observers. Both phases let a caller compose multi-step pipelines out of *mailbox calls* — fetch, read, write, draw — but neither lets the caller insert pure compute between them. A pipeline that needs to parse JSON between a `Fetch` and a `Write` either drops back into a component's `#[handler]` chain (giving up DAG-shaped composition for that segment) or dispatches the parse as a side trip to some other component over mail.

The missing primitive is **transforms**: pure functions the DAG executor invokes between source and observer nodes, with inputs sourced from resolved handles and output written back as a new handle. ADR-0045 §6 sketched the shape; this ADR is the focused review surface for the macro, the registration path, the invocation path, the handle id derivation, and the failure semantics.

Two design pressures shape Phase 3 specifically:

- **Content-addressing.** A transform applied to the same inputs should produce the same handle id, so two callers that wire identical compute share a single cached output. ADR-0045 §3 commits to a content-addressed `HandleId` for transforms. This phase implements that derivation and validates it under the actor-per-component scheduler (ADR-0038).
- **Pure execution.** Transforms must not call host fns, must not access mutable state, must not depend on time / RNG / external I/O. The `#[transform]` macro enforces this at compile time via a deny-list scan. Because a native transform runs without a sandbox, that scan is the *only* guard — there is no runtime isolation to backstop it (a real difference from the deferred wasm path; see §"Why native"). State changes only happen at observer nodes (which already dispatch through normal mail).

The persistent handle store across substrate restart (ADR-0045 Phase 4) is split into ADR-0049. That work depends on the content-addressing this ADR introduces (without it, a restored handle id would point to a value the substrate would recompute under a different id and never look up), but its on-disk layout and restore semantics are enough surface to deserve their own review.

### Why native, why defer wasm

A transform could in principle run as native Rust or as a wasm export. WASM would buy three things:

1. **Sandbox isolation** — run untrusted or third-party transform code without trusting it to be pure or memory-safe.
2. **Runtime-loadable transforms** — `load_component` brings new transforms into a running substrate without a rebuild.
3. **Clean preemption / resource caps** — wasmtime epoch-interruption can kill a runaway transform mid-execution; memory is capped per-instance.

0.4 needs none of these. Every transform we have (ADR-0046's content-gen pipeline — lens-fill / distill / compose / scrub — and the image-to-mesh extraction step) is **first-party code that ships with the engine**. There is no untrusted author, no need to add a transform without a rebuild, and the transforms are reviewed code we trust to terminate. Against that, wasm costs ~3× native runtime (measured; the substrate's existing wasm-vs-native flame profiles) on exactly the data-crunching workload transforms exist for, plus the FFI marshaling of every input/output across the linear-memory boundary on every call.

So 0.4 ships native transforms only. The three wasm wins above are precisely the forcing functions that would justify reopening this: an untrusted-transform-author story, a hot-loadable-transform requirement, or a transform that needs hard resource preemption. When one of those arrives, wasm transforms are an *additive* executor-invocation path (see §3 and Alternatives), not a reshape.

## Decision

### 1. The `#[transform]` attribute macro

A transform is a **data-layer primitive** — a pure `Kind → Kind` function with zero dependence on the actor framework (no `Ctx`, no mail, no lifecycle). Its SDK surface therefore lives in the data layer, not the actor SDK: the macro is imported as `aether_data::transform` (the proc-macro impl lives in a sibling derive crate re-exported from `aether-data`, since `aether-data` is `no_std`+`alloc` and can't itself be a proc-macro crate; the runtime types — `TransformEntry`, the registry, `TransformError` — live in `aether-data` next to `Kind` and the native descriptor inventory). Authors declare transforms as free-standing functions decorated with `#[transform]`, in any crate the substrate links (the content-gen transforms live alongside their cap in `aether-capabilities`, in a `transforms.rs` module separate from any `#[handlers]` impl — but the import and registration are independent of where the code sits):

```rust
use aether_data::transform;

#[transform]
fn extract_bytes(input: ReadResult) -> Vec<u8> {
    match input {
        ReadResult::Ok { bytes, .. } => bytes,
        ReadResult::Err { .. } => alloc::vec![],
    }
}

#[transform]
fn compose_prompt(manifest: PromptManifest, embedding: Embedding) -> ComposedPrompt {
    ComposedPrompt {
        text: manifest.template.fill(&embedding),
        params: manifest.params,
    }
}
```

The macro accepts up to 8 input parameters and a single return type. Each parameter type and the return type must implement `Kind` (ADR-0028, ADR-0030, ADR-0032). Multi-output transforms are deferred (a future revision adds tuple returns; for now, structure the output as a single struct kind).

The macro's responsibilities:

- **Compute a stable `transform_id`.** `transform_id = fnv1a_64(TRANSFORM_DOMAIN ++ canonical("{crate}::{module_path}::{fn_name}"))`, where `TRANSFORM_DOMAIN` is a 16-byte constant disjoint from the kind / mailbox / handle domains. Identity is **name-based**, not position-based: inserting or reordering transforms in a file does not change any id; *renaming or moving* a transform fn changes its id (and so invalidates that transform's cached / persistent handles). This is strictly better than the original position-based scheme — the common edit (append a transform) is a non-event, and the fragile edit (rename/move) is rarer and intentional.
- **Deny-list purity scan.** Walk the function body's syntax tree and `compile_error!` on any reference to: host-fn imports (`aether::send_mail_p32`, `reply_mail_p32`, `resolve_*`, …), the handler-context types (`aether_actor::Ctx`, `MailCtx`), the sync request/reply primitive (`wait_reply`), and the obvious nondeterminism sources catchable at compile time (`std::env::*`, `std::time::*`, `core::time::*`). The error points at the offending span and cites this ADR. The scan is best-effort: it sees only the immediate body, not the bodies of helper fns the transform calls, and there is no sandbox to catch what it misses (see §"Why native" and Consequences/Negative). It is the purity contract's first and only line of defense, which is acceptable for first-party transforms and is itself the signal for when the wasm-sandbox path earns its keep.
- **Register into the native transform inventory.** Emit a link-time inventory submission (the same pattern as `aether-data`'s native descriptor inventory) carrying `{ transform_id, input_kind_ids: [KindId], output_kind_id: KindId, name: &str, invoke: fn(&[&[u8]]) -> Result<Vec<u8>, TransformError> }`. The `invoke` thunk is a generated, type-erased wrapper that decodes each input slice against its input kind's canonical-bytes path (the same path `aether_actor::Mail::decode` uses), calls the user fn, and encodes the output via `<OutputKind as Kind>::encode`. No FFI shim, no `extern "C"`, no custom section — the transform is plain Rust collected at link time.

### 2. The native transform registry

There is no wasm custom section (the original §2). Native transforms are collected at **link time** into a substrate-global registry, built once at startup from the inventory the macro populates:

```rust
struct TransformRegistry {
    by_id: HashMap<TransformId, TransformEntry>,
}

struct TransformEntry {
    input_kind_ids: SmallVec<[KindId; 8]>,
    output_kind_id: KindId,
    name: &'static str,                      // diagnostics: engine_logs, MCP introspection
    invoke: fn(&[&[u8]]) -> Result<Vec<u8>, TransformError>,
}
```

The registry is fixed for the lifetime of the substrate process — a transform set is a build-time property, not a load-time one (this is the runtime-loadable flexibility wasm would have bought; deferred deliberately). ADR-0047's validation phase 2 cross-checks each `Transform { transform_id, output_kind_id }` node against the registry; an unknown id is `DagError::UnknownTransform`, an output-kind mismatch is `DagError::TransformOutputMismatch`. Lookup is a hashmap hit, infallible after validation.

`describe_kinds`-style MCP introspection gains a transform listing (id, name, input/output kinds, rustdoc) so an agent can see the available transforms without reading source — the registry is self-describing.

### 3. Invocation path

When a DAG executor sees a `Transform` node whose input handles have all resolved, it:

1. Looks up the `TransformEntry` in the registry by `transform_id`.
2. Computes the content-addressed handle id (§4) from the transform id + the input handles. **If the handle store already holds that id, the transform is not invoked at all** — the cached output resolves the node directly (auto-dedup; the headline value).
3. On a cache miss, resolves each input handle from the store to its canonical-byte slice (ADR-0032), in slot-index order.
4. Invokes `entry.invoke(&inputs)` **off the executor thread** (see below), getting back `Result<Vec<u8>, TransformError>`.
5. On `Ok(bytes)`, stores the bytes under the content-addressed id and resolves the node's handle, flushing any mail parked on it. On `Err`, fails the node per §6.

**Threading — transforms get their own compute pool.** A native transform is a pure `fn` with no instance and no thread affinity — it is `Send`, unlike a wasm instance pinned to its component's actor thread. The executor does *not* run transforms inline (a slow transform would stall its parking/reaping loop — ADR-0047/#976), and it does *not* run them on any actor thread (the wasm-design constraint that forced that is gone — there is no shared wasmtime store to serialise against). Instead, transforms execute on a **dedicated transform compute pool** owned by the executor — their own bounded set of OS threads, separate from every actor thread. This makes transform throughput a first-class, isolated budget: a transform can't be starved behind an actor's mail queue, and a heavy transform can't stall an actor. The pool is sized independently of actor count (default: available parallelism); the executor dispatches a resolved transform node onto it and awaits the result while continuing to advance other DAG branches. This is emphatically **not** the actor worker-pool ADR-0038 retired — that was actor *dispatch* (instances, lifecycles, strand-claims to reconcile); this is a pure-compute pool of stateless `fn` calls with none of that machinery.

**The invocation seam.** The executor calls transforms through one narrow internal interface (`fn invoke(transform_id, inputs) -> Result<Vec<u8>, TransformError>` resolved against the registry). Today there is exactly one implementation (native, in-process). If wasm transforms are ever reopened (§"Why native"), they slot in as a *second* resolution behind the same seam — a wasm transform would be reached through the existing trampoline (ADR-0074) by mail with a correlated reply, identical from the executor's side. Keeping this seam narrow is the whole reason deferring wasm costs nothing later.

Per-call constraints:

- **Timeout (best-effort).** The executor sets a wall-clock deadline per transform (default 30s, configurable per descriptor via an optional `timeout_ms` field on `Node::Transform` — wire-additive). A native thread **cannot be safely preempted**: on deadline the executor marks the node `Failed { error: "timeout: <N>ms" }`, drops downstream parked mail, and moves on, but the runaway thread is orphaned (it runs until it returns or the process exits). This is a genuine downgrade from wasm's epoch-interruption and is acceptable only because transforms are first-party, reviewed code; an infinite loop is a bug caught in testing, not an adversary. Clean preemption is one of the wasm forcing functions.
- **Panic = failure.** A `panic!` (or `unwrap` on `None`/`Err`, OOB index, etc.) inside a transform is caught at the invocation boundary via `std::panic::catch_unwind` (transforms run on their own thread, so the catch is clean). It maps to `Failed { node_id, error: "transform panicked: <message>" }`; the panic message + location go to engine_logs. Because the transform is a separate thread with no shared mutable state (`feedback_actor_state_no_locks`), a panic cannot poison the executor or any actor.
- **No host access.** Enforced by the deny-list scan at compile time (§1). There is no link-time guarantee — a transform that calls a helper which does I/O compiles. First-party review is the backstop.

### 4. Content-addressed transform handle ids

For a transform invocation:

```
HandleId = fnv1a_64(
    HANDLE_DOMAIN
    ++ transform_id              : u64 LE
    ++ input_count               : u8
    ++ for each input in slot-index order:
        slot_index               : u8
        input_handle_id          : u64 LE
)
```

`HANDLE_DOMAIN` is a 16-byte constant, disjoint from `KIND_DOMAIN` (ADR-0030), `MAILBOX_DOMAIN` (ADR-0029), and `TRANSFORM_DOMAIN` (§1), so a 64-bit collision in the FNV space can't cross-pollinate the registries.

The id keys on the **global `transform_id`**, not the original `(component_mailbox, transform_index)` pair. A native transform is not owned by a component instance — its identity is global to the substrate build. This is simpler *and* more robust for persistence: the same transform on the same inputs produces the same handle id on any substrate built from the same source, independent of which component (if any) referenced it or what mailbox it ran under.

Inputs are emitted in slot-index order (canonical, deterministic) rather than the order the executor happens to resolve them, so the derivation is insensitive to source-dispatch parallelism. The explicit slot-index byte before each input handle id protects transforms taking multiple handles of the same kind in positionally-distinct slots (`compose(a: ReadResult, b: ReadResult) -> Joined`): swapping the edges across slots produces a different id, correctly, because the semantics differ.

**Auto-dedup.** Two DAGs that include the same transform on the same inputs produce the same handle id, so the second submission finds the handle already resolved and skips the call entirely (§3 step 2). A content-gen pipeline that frames a fact through the same lens with the same context twice doesn't pay twice — now engine-wide, not per-component.

**Determinism.** Same `(transform_id, input handle ids)` → same handle id, always. This is the property persistent handles (ADR-0049) depend on — a substrate restart that recovers a content-addressed handle from disk gets the same id it would compute from scratch.

### 5. SDK-side ergonomics

Transforms live in their own module (`transforms.rs`), separate from any `#[handlers]` impl — the purity scan rejects a body that references handler context, so the separation is enforced, not just stylistic. Input kinds and output kinds come from the same `aether-kinds`-style crates components already use; reply kinds (`ReadResult`, `FetchResult`) are natural transform inputs because they're already structured. There is no runtime "register transform" call — registration is the link-time inventory submission the macro emits, collected into the registry at startup.

### 6. Failure semantics

Transform failures partition into:

1. **Input decode failure** — bytes in the handle store don't decode to the input kind's schema. This is a substrate-side bug (ADR-0047 §3's type-compatibility check should have caught it); surfaces as `DagError::EdgeTypeMismatch` retroactively, the DAG fails, tracked via a metric. Should be unreachable in correct code.
2. **Output encoding failure** — the encoded output overflows the configured cap (`AETHER_TRANSFORM_MAX_OUTPUT_BYTES`, default 64MB). Hard fail with `Failed { error: "transform output exceeded N bytes" }`.
3. **Panic or timeout** — covered in §3. Panic → `catch_unwind` → `Failed`. Timeout → best-effort deadline → `Failed`, thread orphaned.

For all classes, downstream parked mail drops with the diagnostic ADR-0045 §8 names; handles assigned to the failed node and its descendants are released; the node's input handles lose one consumer. Handles outside the failed sub-DAG are unaffected.

A transform whose semantics include an Err *value* (a parser returning `Result<Parsed, ParseError>`) does **not** fail the DAG — the Err is the output value, content-addressed and cached like any other. Downstream consumers match on it. The DAG-level failure path is reserved for runtime aborts (panic / timeout / encode overflow), not domain errors.

### 7. Lifecycle

Native transforms are **build-time**, not load-time, so most of the original lifecycle surface evaporates:

- **No load/drop/replace coupling.** Transforms are not carried in a component's wasm binary; loading, dropping, or replacing a component does not add, remove, or invalidate any transform. The registry is fixed at substrate startup. (Section-hash invalidation on `replace_component` — original §7 — is gone entirely.)
- **Renaming a transform** changes its `transform_id` (§1), which changes the content-addressed ids of its outputs. Cached entries under the old id become unreachable (and are eventually evicted / never restored); a re-run recomputes under the new id. Convention: treat a transform's fully-qualified name as part of its contract — rename intentionally, knowing it invalidates that transform's persisted handles.
- **Substrate restart.** Without ADR-0049, all transform handles drop on restart. With ADR-0049, content-addressed transform handles persist (sources don't — they're ephemeral observations per ADR-0045 §3). DAG state itself doesn't persist (ADR-0047 §7), but a re-submitted DAG with the same descriptor finds its transform-output handles already on disk and skips recomputation.

### 8. Chassis coverage

- **Desktop / headless** — full transform support; the DAG executor runs there.
- **Hub** — DAG submission is rejected at the hub per ADR-0047 §8, so transforms don't run there either. (Unlike the original design, the reason is no longer "transforms need a wasmtime instance the hub lacks" — a native transform would run anywhere — it's simply that the hub doesn't host DAG execution.)

## Consequences

### Positive

- **Native speed, no FFI tax.** Transforms run at native Rust speed with no ~3× wasm penalty and no per-call linear-memory marshaling of inputs/outputs. This is the workload (data crunching) where that matters most.
- **Much simpler lifecycle.** No custom-section format, no load-time section parse, no replace-time section-hash invalidation, no per-component transform catalog. A link-time inventory + a startup registry build replaces all of it.
- **Trivially testable transforms.** A transform is a pure `fn` — unit-test it directly with no substrate, no wasm build, no fixture component. The determinism property is a plain `assert_eq!` on two calls.
- **Independently resourced, decoupled from actors.** Transforms run on their own compute pool (§3), not on actor threads, so transform throughput is isolated from actor mail scheduling — a heavy transform can't stall an actor and vice versa. And because a transform is a pure `Kind → Kind` data-layer primitive (no `Ctx`, mail, or lifecycle), its SDK surface and runtime types live in the data layer, not bundled under the actor SDK — they're conceptually and structurally independent of the actor framework.
- **Engine-wide content-addressing.** Keying on the global `transform_id` (not a per-component pair) means identical compute dedups across the whole substrate and across restarts, not just within one component.
- **No new wire format.** ADR-0047's descriptor already has the `Transform` variant (the `transform_id` field replaces the old `TransformRef { component, index }` — a wire-shape simplification, folded into #974).

### Negative

- **Purity is enforced by a best-effort compile-time scan with no runtime backstop.** The deny-list scan sees only the immediate fn body, not called helpers, and there is no sandbox to catch a non-pure transform at runtime. A determined or careless author can break content-addressing silently (nondeterministic output → wrong cache hits). Mitigation: first-party review + a CI determinism check (run a transform twice on identical inputs, compare output bytes). This is weaker than the deferred wasm path's runtime isolation and is the clearest forcing function for reopening wasm transforms (untrusted authors).
- **Transforms are build-time, not runtime-loadable.** Adding a transform requires rebuilding the substrate. Fine for a first-party transform set that ships with the engine; the moment a use case needs hot-loaded transforms, that's a wasm forcing function.
- **Timeouts are best-effort; runaway transforms leak a thread.** A native thread can't be safely preempted, so a non-terminating transform is detected and the node failed, but the thread runs until process exit. Acceptable for reviewed first-party code; clean preemption is a wasm forcing function.
- **Name-based identity is fragile across renames/moves.** Renaming or relocating a transform fn changes its `transform_id` and invalidates its persisted handles. This is rarer and more intentional than the original position-based scheme's "insert renumbers everything," but it's still a real edit hazard documented in §7.

### Neutral

- **Existing kinds dispatch unchanged.** Transforms consume / produce regular kind values via the same `Schema`-derived encoding (ADR-0007, ADR-0019, ADR-0032).
- **ADR-0044 capabilities orthogonal.** A native transform makes no capability requests (it can't — the scan forbids host access). When ADR-0044 unparks, transforms are simply outside its surface.
- **Hub MCP unchanged.** `submit_dag` validation consults the substrate's transform registry the same way it consults the kind vocabulary; no hub MCP changes for Phase 3.
- **The executor's invocation seam is single-impl today.** The narrow `invoke(transform_id, inputs)` interface (§3) has one resolution (native). It is shaped so a future wasm resolution slots in behind it, but that abstraction is one method, not a framework — it doesn't pay for itself until/unless wasm transforms land.

## Alternatives considered

- **WASM transforms via `wasmtime::Func::call` (the original design for this ADR).** A transform is a wasm export, carried in an `aether.dag.transforms` custom section, invoked synchronously into the owning component's wasmtime instance. Deferred, not rejected: it buys sandbox isolation, runtime-loadable transforms, and clean preemption (§"Why native"), none of which 0.4 needs, at ~3× native runtime plus per-call FFI marshaling on a data-crunching workload. Reopens under a clear forcing function (untrusted transform authors, hot-loadable transforms, or hard resource preemption), and when it does it rides the existing trampoline behind the §3 invocation seam rather than reintroducing a bespoke call path.
- **Transforms as mail-dispatched actors.** Wrap each transform in an actor and have the executor invoke it by mail with a correlated reply (symmetric with how a future wasm transform would be reached). Rejected for the native case: a pure fn has no state and no thread affinity, so wrapping it in an actor adds a mailbox, a per-call mail allocation, and a round-trip for nothing — direct off-thread invocation (§3) is strictly simpler. The mail path is the *right* shape only for wasm transforms (which are pinned to an instance/thread), which is exactly where it returns if wasm reopens.
- **Run transforms inline on the executor thread.** Simplest, but a slow transform stalls the executor's parking/reaping loop. Rejected — pure fns are `Send`, so off-thread costs nothing and isolates panics. (§3.)
- **Worker-pool of wasm instances.** The original's deferred option; moot under native (no instances to pool).
- **Closures captured at submit time.** Closures don't cross any boundary cleanly and don't compose with content-addressing (no stable identity). Rejected.
- **Content-addressed by normalized fn body / wasm body.** Hash the compiled body so byte-identical transforms dedup regardless of name. Forward-compatible (`transform_id` is opaque on the wire) but needs a normalization pass and pays for forcing functions we don't have (distribution, verifiable replay, untrusted-author dedup). Name-based identity covers in-tree dedup at zero cost.
- **Tuple / multi-output transforms.** Returning `(A, B, C)` to produce three handles in one call. Deferred: a single struct kind output covers the case; a downstream "extract field X" transform's output is itself content-addressed and cached. Wire-additive when forced.
- **Side-effecting transforms (allow host fns, accept ephemeral ids).** Rejected at the foundational level — content-addressing is the headline value and depends on purity. Side-effecting "transform-shaped operations" stay sources/observers (ADR-0047 §1).

## Follow-up work

- **iamacoffeepot/aether#979** — `#[transform]` macro, homed in the data layer (proc-macro in a sibling derive crate re-exported as `aether_data::transform`; runtime types in `aether-data`), **not** the actor SDK: name-based `transform_id`, deny-list purity scan, link-time inventory submission with a generated type-erased `invoke` thunk (no FFI shim, no custom section). Tests: pure body accepted, body referencing `Ctx` / host fn / `std::time` rejected, multi-input (up to 8) accepted, 9-input rejected, determinism check.
- **iamacoffeepot/aether#978** (umbrella) — re-scoped to the native trio: the macro (#979), the executor native-invocation path (refiled to replace the closed wasm-dispatch issue), and content-addressing (#982).
- **iamacoffeepot/aether#982** — content-addressed handle id keyed on global `transform_id` in `aether-data`; executor pre-dispatch cache check.
- **New issue (to file once this ADR merges)** — substrate executor native-transform invocation: build the `TransformRegistry` from the inventory at startup, run invocations on a dedicated transform compute pool (separate from actor threads) with `catch_unwind` + best-effort deadline, store output under the content-addressed id, flush parked mail. (Replaces the closed wasm `Func::call` dispatch issue.)
- **Parked, ADR-0049** — persistent handle store across substrate restart. Depends on this ADR's content-addressing.
- **Parked, future ADR** — wasm transforms. Forcing functions: untrusted transform authors, hot-loadable transforms without a rebuild, or hard resource preemption. Slots in behind the §3 invocation seam via the trampoline.
- **Parked, future ADR** — tuple / multi-output transforms. Wire-additive when the use case forces it.
- **Parked, future ADR** — content-addressed-by-body transform identity (cross-build dedup). Forcing function: distribution / verifiable replay.
