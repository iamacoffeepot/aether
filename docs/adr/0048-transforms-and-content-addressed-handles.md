# ADR-0048: Transforms and content-addressed handle ids (Phase 3 of ADR-0045)

- **Status:** Proposed
- **Date:** 2026-04-25

## Context

ADR-0045 Phase 1 shipped handles as a wire type (`Ref<K>`) and a substrate-global handle store with refcounted entries. ADR-0047 (Phase 2) added DAG submit/cancel/status with sources + observers. Both phases let a caller compose multi-step pipelines out of *sink calls* — fetch, read, write, draw — but neither lets the caller insert pure compute between them. A pipeline that needs to parse JSON between a `Fetch` and a `Write` either drops back into a component's `#[handler]` chain (giving up DAG-shaped composition for that segment) or dispatches the parse as a side trip to some other component over mail (paying the wasm round-trip cost ADR-0045 was supposed to retire).

The missing primitive is **transforms**: pure guest functions the substrate calls directly via wasmtime `Func::call`, with inputs sourced from resolved handles and output written back as a new handle. ADR-0045 §6 sketched the shape; this ADR is the focused review surface for the macro, the custom section, the wasmtime integration, the handle id derivation, and the failure semantics.

Two design pressures shape Phase 3 specifically:

- **Content-addressing.** A transform applied to the same inputs should produce the same handle id, so two callers that wire identical compute share a single cached output. ADR-0045 §3 commits to `HandleId = fnv64(component_mailbox ++ transform_index ++ input_handle_ids)` for transforms — content-addressed within a transform-hosting component, position-based across components. This phase implements that derivation and validates it under the actor-per-component scheduler (ADR-0038).
- **Pure execution.** Transforms must not call host fns, must not access mutable state, must not depend on time / RNG / external I/O. The phase-3 macro and the substrate's invocation path together enforce this — at compile time where possible, at runtime where not. State changes only happen at observer nodes (which already dispatch through normal mail).

The persistent handle store across substrate restart, also called out in ADR-0045 Phase 4 and ADR-0046's prerequisites list, is split into ADR-0049. That work depends on the content-addressing this ADR introduces (without it, a restored handle id would point to a value the substrate would recompute under a different id and never look up), but its on-disk layout and restore semantics are enough surface to deserve their own review.

## Decision

### 1. The `#[transform]` attribute macro

Guest authors declare transforms as free-standing functions decorated with `#[transform]`:

```rust
use aether_sdk::transform;

#[transform]
fn extract_bytes(input: ReadResult) -> Vec<u8> {
    match input {
        ReadResult::Ok { bytes, .. } => bytes,
        ReadResult::Err { .. } => alloc::vec![],
    }
}

#[transform]
fn parse_prompt_manifest(bytes: Vec<u8>) -> PromptManifest {
    postcard::from_bytes(&bytes).unwrap_or_default()
}

#[transform]
fn compose_prompt(manifest: PromptManifest, embedding: Embedding) -> ComposedPrompt {
    ComposedPrompt {
        text: manifest.template.fill(&embedding),
        params: manifest.params,
    }
}
```

The macro accepts up to 8 input parameters and a single return type. Each parameter type and the return type must implement `Kind` — the same trait kind values use across the rest of the system (ADR-0028, ADR-0030, ADR-0032). Multi-output transforms are deferred (a future revision adds tuple returns; for now, structure the output as a single struct kind).

The macro enforces purity at compile time where it can:

- Marks the function `extern "C"` and gives it a stable mangled name (`__aether_transform_<index>_p32`). This name is what the substrate calls into.
- Wraps the body in a pure-context shim that has no `Ctx<'_>`, no `&mut self`, no access to component state. The shim's signature only sees the input kinds.
- Emits a `compile_error!` if the body references any item from `aether_sdk::ctx`, `aether_sdk::handlers::Ctx`, or any host-fn import (`aether::send_mail_p32`, `aether::reply_mail_p32`, etc.). This is a deny-list scan over the syntax tree; a determined author can defeat it (e.g., by re-exporting), but the substrate's runtime check (§3) catches what the compile-time scan misses.

Things the macro *cannot* enforce at compile time but documents and validates at runtime:

- No reading of clock / RNG / `std::env`. Wasm doesn't have a clock except via host fns the SDK doesn't surface, but a careless `core::ptr` read of uninitialized memory is technically nondeterministic. The substrate's runtime check in §3 verifies the output is content-addressable by re-running the transform on identical inputs in CI tests; production substrates trust the macro.
- No infinite loops. A transform that doesn't return is a runtime concern handled by the substrate's per-call timeout (§3).

The macro records the transform's metadata into the `aether.dag.transforms` custom section (see §2). It does *not* register the transform with the host at component init — registration is implicit in the component's wasm binary, read at `load_component` time.

### 2. The `aether.dag.transforms` custom section

A wasm custom section written by the `#[transform]` macro at compile time, parallel to ADR-0028's `aether.kinds` and ADR-0033's `aether.kinds.inputs`:

```
section_name = "aether.dag.transforms"
version_byte = 0x01
entry_count  = u32 (LE)
entries      = TransformEntry × entry_count

struct TransformEntry {
    transform_index : u32 LE          // matches the macro-assigned index
    input_count     : u8              // 0..=8
    input_kind_ids  : [u64 LE; input_count]  // ordered by parameter position
    output_kind_id  : u64 LE
    name_len        : u16 LE
    name            : [u8; name_len]  // utf-8, the function's source name (for diagnostics only)
}
```

The hub reads this section at `load_component` (the same path that builds the `aether.kinds.inputs` manifest) and exposes the transform catalog to the executor. ADR-0047's validation phase 2 cross-checks `Transform { transform: TransformRef { component, index }, output_kind_id }` against the loaded component's catalog; mismatch is a `DagError::TransformOutputMismatch` or `DagError::UnknownTransform`.

`name` is for diagnostics (engine_logs, error messages, MCP `describe_component`) only — dispatch is by `transform_index`. Renaming a transform across builds is a non-event for the wire; reordering them is a breaking change because `transform_index` is positional.

The section is required for any component that exports transforms. A component with no `#[transform]`s emits no section (zero-byte addition). The section is read once at load and cached in the substrate's per-component metadata; later DAG submissions are a hashmap lookup, not a wasm parse.

### 3. Wasmtime invocation path

When a DAG executor sees a `Transform` node whose input handles have all resolved, it:

1. Looks up the transform in the owning component's catalog by `(component_mailbox, transform_index)`.
2. Resolves each input handle from the handle store, getting back the canonical-bytes encoding of the input kind value (ADR-0032).
3. Allocates linear-memory buffers in the component's wasm instance for each input (one `wasm_alloc` per input — same shim guests use today for receive_p32 dispatch). Copies the canonical bytes in.
4. Invokes the FFI export `__aether_transform_<index>_p32(in_ptrs, in_lens, out_ptr_ptr, out_len_ptr) -> i32` via `wasmtime::Func::call`. Return value is `0` on success, nonzero on failure (decode error in any input, output encoding overflow, transform-internal abort).
5. On success, reads the output bytes from `out_ptr` / `out_len`, frees the linear-memory allocations, and stores the bytes in the handle store keyed on the content-addressed handle id (§4).

The transform runs **on the owning component's actor thread** (ADR-0038). This serialises transforms with that component's regular `#[handler]` mail, which is the simplest model: a component's wasmtime instance is a single owner, no contention, no cross-thread linear memory access. Transforms are CPU-bound and (intentionally) bounded in runtime; serialising them with mail is fine for any DAG that doesn't pin a component's whole actor for minutes.

Per-call constraints:

- **Timeout.** Each transform invocation has a wall-clock deadline (default 30s, configurable per descriptor via an optional `timeout_ms` field on `Node::Transform` — wire-additive). Exceeding it surfaces as `DagError`-shaped failure: the DAG aborts, the offending node is marked Failed, downstream handles never resolve. Implemented via wasmtime's epoch interruption (one epoch tick per second; trap-on-deadline).
- **Memory cap.** A transform can't allocate more than `AETHER_TRANSFORM_MAX_MEMORY_BYTES` (default 64MB) inside its wasm instance during execution. This is the existing per-component memory cap from earlier ADRs; transforms inherit it.
- **No host-fn imports.** wasmtime instantiates the component with the standard host-fn linking (send_mail_p32, reply_mail_p32, etc.). The transform shim is generated to *not call any of them*; wasmtime can't enforce this at link time without a separate instance, but the macro's deny-list scan + the runtime fact that calling a host fn from a transform context is undefined behaviour (the host fn implementations gate on Ctx-context state that doesn't exist for a transform call) catches obvious mistakes. Future ADR (probably under capabilities, ADR-0044) can tighten this with a separate transform-only linker that omits non-pure imports.
- **Trap = failure.** A wasm trap (memory-access-OOB, division-by-zero, panic via `unreachable`) terminates the transform with the DAG failure path described in ADR-0047 §6. The substrate captures the trap site via wasmtime's stack trace and writes it to engine_logs; the DAG's `status_result::Failed` carries a one-line summary.

### 4. Content-addressed transform handle ids

For a transform invocation:

```
HandleId = fnv1a_64(
    HANDLE_DOMAIN
    ++ component_mailbox        : u64 LE
    ++ transform_index           : u32 LE
    ++ input_count               : u8
    ++ for each input in slot-index order:
        slot_index               : u8
        input_handle_id          : u64 LE
)
```

`HANDLE_DOMAIN` is a 16-byte constant, disjoint from `KIND_DOMAIN` (ADR-0030) and `MAILBOX_DOMAIN` (ADR-0029), so a 64-bit collision in the FNV space can't cross-pollinate the registries.

Inputs are emitted in slot-index order (canonical, deterministic) rather than the order the executor happens to resolve them. This makes the derivation insensitive to source-dispatch parallelism — a node that receives its inputs in a different order across runs still produces the same id. Including the explicit slot index protects transforms that take multiple handles of the same kind in positionally-distinct slots (`compose(a: ReadResult, b: ReadResult) -> Joined`): swapping the edges across the two slots produces a different handle id, correctly, because the transform's semantics differ.

**Auto-dedup behavior.** Two DAGs that include the same transform on the same inputs produce the same handle id, so the second submission's executor finds the handle already resolved in the store and skips the wasmtime call. This is the headline value: a content-gen pipeline that frames a fact through the same lens with the same context twice doesn't pay twice.

**Cross-component non-dedup.** Two components shipping byte-identical `parse_json` transforms have different `component_mailbox` values and produce different handle ids. They don't auto-dedup. This is intentional: cross-component dedup would require validating that the wasm bodies are actually identical (Merkle-DAG over normalized wasm bytes), which is forward-compatible (`HandleId` derivation is opaque on the wire) but engineering-expensive. ADR-0045 §6 commits to the deferral.

**Determinism.** Same `(component_mailbox, transform_index, input handle_ids)` → same handle id, always. This is the property persistent handles (ADR-0049) depends on — a substrate restart that recovers a content-addressed handle from disk gets the same id it would compute from scratch.

### 5. SDK-side ergonomics

Components don't write transforms in the same module as `#[handlers]`. The macro's purity scan rejects it if the body references handler context. Convention is `src/transforms.rs` (or a `transforms/` directory) for transforms, separate from `src/lib.rs`'s component impl block. The crate's `Cargo.toml` exports both.

A transform's input kinds and output kind are imported from the same `aether-kinds`-style crate components use today. Reply kinds (`ReadResult`, `FetchResult`) are natural transform inputs because they're already structured. Output kinds either reuse existing kind types or define new ones (e.g., `ParsedManifest`, `ComposedPrompt`).

The SDK exposes no runtime-level "register transform" call. Registration is implicit in the wasm binary's `aether.dag.transforms` section. `init` doesn't need to know about transforms; the substrate reads the section at load and the DAG executor is the only component-external consumer.

### 6. Failure semantics

Transform failures partition into three classes:

1. **Input decode failure** — bytes in the handle store don't decode to the input kind's expected schema. This is a substrate-side bug (the type-compatibility check in ADR-0047 §3 should have caught it) and surfaces as `DagError::EdgeTypeMismatch` retroactively; the DAG fails. Should be unreachable in correct code; tracked via a metric.
2. **Output encoding failure** — the transform returned a value that overflows the output buffer (the substrate sizes the buffer with a heuristic + grows once on overflow). After one grow attempt, hard fail with a specific `Failed { error: "transform output exceeded N bytes" }` message; component author needs to either return a smaller value or split the transform.
3. **Trap or timeout** — wasm trap, panic, or epoch deadline. DAG fails with `Failed { node_id, error: "trap: <site>" }` or `"timeout: <Nms>"`. engine_logs carries the full stack.

For all three, downstream parked mail drops with the `CapabilityDenied`-shaped diagnostic ADR-0045 §8 names. Handles assigned to the failed node and any descendants are released; the node's input handles are released (one less consumer). Handles that aren't part of the failed sub-DAG are unaffected.

A transform whose semantics include "produce an Err variant" (e.g., a parser that returns `Result<Parsed, ParseError>`) doesn't fail the DAG — the Err is the transform's output value. Downstream consumers either handle the Err (a transform whose input is `Result<_, _>` and matches on it) or dispatch through the Err to an observer that processes parse failures. This is normal Rust-style error handling; the DAG-level failure path is reserved for runtime aborts.

### 7. Lifecycle

- **Component drop.** All transforms hosted by a dropped component become invalid. In-flight DAG transform calls trap if the wasmtime instance is torn down mid-call (handled gracefully — the trap goes through the failure path in §6). New DAG submissions referencing a dropped component fail validation in ADR-0047 §3 phase 2 with `UnknownRecipient`.
- **Component replace** (ADR-0022 freeze-drain-swap). Transforms parked behind freeze run on whichever instance ends up bound — same rules as mail. Cache entries from the old instance are invalidated *only if* the new instance's `aether.dag.transforms` section differs from the old one (different `transform_index → output_kind_id` mapping or different `input_kind_ids`). Identical sections preserve the cache; this is the expected case for hot-reload-during-iteration. Implementation: the substrate hashes the section bytes at load and compares; mismatch flushes that component's transform handles from the store.
- **Substrate restart.** Without ADR-0049 (persistent handle store), all transform handles drop on restart. With ADR-0049, content-addressed transform handles persist; sources don't (per ADR-0045 §3, sources are ephemeral observations). DAG state itself doesn't persist (per ADR-0047 §7), so a restarted substrate doesn't auto-resume DAGs — but a re-submitted DAG with the same descriptor finds its transform-output handles already resolved on disk and skips the recomputation.

### 8. Chassis coverage

- **Desktop / headless** — full transform support.
- **Hub** — no transforms. Transforms run on the owning component's wasmtime instance, and the hub doesn't host components. Transform-bearing components live on substrate children (ADR-0035 / ADR-0037); a DAG submitted to the hub bubbles its transform nodes via mail forwarding, which doesn't make sense — DAG submission is rejected at the hub per ADR-0047 §8.

## Consequences

### Positive

- **Pure compute moves into the DAG.** A pipeline that needs to parse, decode, compose, or transform between sink calls expresses it as a transform node, not a side-trip mail dispatch. ADR-0046's content-gen pipeline picks this up: lens-fill / distill / compose / scrub all become transforms.
- **Auto-dedup for shared work within a component's transform set.** Two callers wiring identical transforms on identical inputs share the cached output. Cost-asymmetric pipelines (cheap-text + expensive-image) benefit most from the cheap-text caching.
- **Content-addressing makes restart-recovery viable.** Persistent handles (ADR-0049) hinge on content addressing — without it, restored handles wouldn't match recomputed ones. This ADR provides the foundation.
- **Component composition via transforms.** Two components can wire DAGs through each other's transforms — A's source feeds B's transform feeds C's observer. This is what the harness composition story needs to compose existing components into custom pipelines without writing a new component.
- **No new wire format.** ADR-0047's descriptor already has the `Transform` variant. This ADR lights up dispatch and adds the custom section, neither of which changes the descriptor wire.

### Negative

- **The pure-execution constraint is enforced by convention more than by the type system.** Compile-time deny-list catches obvious mistakes; runtime "trust the macro" covers the rest. A determined author can still write a non-pure transform that produces nondeterministic outputs and breaks content-addressing silently. The substrate's CI tests run the transform-determinism check (re-run with same inputs, compare output bytes); production substrates trust authored transforms. A future ADR can tighten this via a transform-only wasmtime linker that omits non-pure imports.
- **Transform serialisation with handler mail.** Running on the owning component's actor thread keeps the model simple but means a long-running transform delays handler mail dispatch on that component. Components hosting heavy transforms should size them for the actor-cadence budget. A future ADR can offload transforms to a worker pool (separate wasmtime instance) if a forcing function emerges.
- **Position-based `transform_index` is fragile across edits.** Adding a new `#[transform]` between existing ones renumbers every later index, which invalidates persistent handles and cached outputs. Mitigation: convention is to append, not insert. ADR-0049 includes a section-hash check that flushes affected handles when the section changes meaningfully.
- **Memory pressure from transform input/output transit.** Each transform invocation copies input bytes from the handle store into wasm linear memory and output bytes back out. For large inputs (megabytes), this is a real cost; the byte-bypass benefit only applies when the *next* node (downstream of the transform) consumes the output as a `Ref<K>` rather than inlining it. ADR-0046's content-gen pipeline keeps text-side transforms small (kilobytes), so the cost is negligible there. **PNG-tier image outputs (~600-700 KB) work fine** — empirically validated by ADR-0046's Spike B running image-gen as a transform-shaped operation with no memory-pressure issues. The "binary blobs through transforms" worry is real only above several MB; multi-MB outputs (high-resolution images, video frames) should stay sink-side or be wrapped in observers.

### Neutral

- **Existing kinds dispatch unchanged.** Transforms consume / produce regular kind values via the same `Schema`-derived encoding (ADR-0007, ADR-0019, ADR-0032). No special transform-only encoding.
- **ADR-0044 capabilities orthogonal in v1.** Transforms run inside their owning component's wasm instance with that component's capability grant. A transform doesn't make new capability requests (it can't — no host fns). When ADR-0044 unparks, transforms inherit the host component's caps without further gating.
- **Hub MCP unchanged.** ADR-0047's `submit_dag` accepts descriptors with `Transform` nodes; hub-side validation lookups consult the substrate's component catalog the same way. No hub MCP changes for Phase 3.

## Alternatives considered

- **Closures captured at submit time.** A guest passes a closure-shaped value to the substrate and the executor invokes it by capture-pointer dereference. Rejected: closures don't cross the FFI cleanly, capture lifetimes don't compose with the substrate's actor model, and the transform-as-positional-export approach is simpler. Same effect, no closure machinery.
- **Transforms as mail kinds dispatched to a pseudo-mailbox.** Wraps every transform in the existing sink-dispatch path, which is nice for symmetry but adds a per-call mail allocation, a per-call recipient lookup, and a per-call kind-id lookup that the direct-call path skips. Rejected for the cost; symmetry isn't worth it for an internal-to-the-executor dispatch.
- **Worker-pool execution.** Spawn a separate wasmtime instance per transform invocation, executed on a substrate-owned thread pool. Rejected for v1: the actor-per-component model (ADR-0038) is the substrate's serialisation discipline; adding a parallel execution context for transforms requires reconciling memory models, instance lifetimes, and capability grants. A future ADR can introduce worker-pool transforms if a forcing function emerges (e.g., a single transform that genuinely needs minutes of compute).
- **Content-addressed by normalized wasm body.** ADR-0045 §6 already considered and parked this. Auto-dedup byte-identical transforms across components by hashing the function body post-validate (strip cargo metadata + debug info + name section, hash function bytes). Forward-compatible because `transform_id` is opaque on the wire. Wins (multi-substrate sync, untrusted-author dedup, replay/audit) all need forcing functions we don't have. Position-based identity covers in-tree dedup at zero engineering cost.
- **Tuple / multi-output transforms.** A transform that returns `(A, B, C)` produces three handles in one call. Useful when a parsing step naturally yields multiple outputs. Deferred for v1: structuring the output as a single struct kind covers the case at the cost of a downstream "extract field X" transform; the latter's output is content-addressed and cached too, so the only runtime cost is one extra wasmtime call per extract. Phase 4+ ADR can add tuple returns as a wire-additive change.
- **Transforms with side effects allowed (just call host fns and accept that handle ids will be ephemeral).** Rejected at the foundational level: content-addressing is the headline value, and a transform that can call host fns can't be content-addressed because outputs depend on time / RNG / external state. ADR-0046's pipeline-shaped use cases all want determinism and dedup. Side-effecting "transform-shaped operations" stay sinks (ADR-0047 §1 sources).

## Follow-up work

- **PR**: SDK macro — `#[transform]`, deny-list scanner, FFI shim codegen, custom-section emitter. Tests cover: pure body accepted, body referencing `Ctx` rejected, body calling `aether::send_mail_p32` rejected, multi-input transforms (up to 8), output-buffer-grow path.
- **PR**: substrate executor + custom section parser — read `aether.dag.transforms` at component load, populate per-component catalog, add `Transform` dispatch path to ADR-0047's executor. Integration tests cover content-addressed handle dedup (run a DAG twice, assert second run hits cache for transform nodes), transform timeout, transform trap, transform output overflow.
- **PR**: hub MCP — `describe_component` (ADR-0033) extends to include transform-side capabilities (transform names, input/output kinds, doc strings extracted from transform fn rustdoc).
- **Parked, ADR-0049**: persistent handle store across substrate restart. Depends on the content-addressing this ADR introduces.
- **Parked, future ADR**: worker-pool transforms (offload long-running transforms from the owning component's actor).
- **Parked, future ADR**: content-addressed-by-wasm-body transform identity (cross-component auto-dedup). Forcing function: distribution / verifiable replay / untrusted-author dedup.
- **Parked, future ADR**: tuple / multi-output transforms. Wire-additive when the use case forces it.
