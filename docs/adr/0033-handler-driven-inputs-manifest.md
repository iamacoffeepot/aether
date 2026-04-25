# ADR-0033: Handler-driven inputs manifest

- **Status:** Accepted (phases 1–3 shipped)
- **Date:** 2026-04-20
- **Accepted:** 2026-04-20

## Context

After ADR-0027, ADR-0028, and ADR-0030 the per-component shape is:

- **Received kinds** are declared twice: once in `type Kinds = (Tick, Key, ...)` (ADR-0027) which the SDK walks at init to populate a runtime `KindTable`, and a second time inside `fn receive` as a chain of `mail.is::<K>()` / `mail.decode_typed::<K>()` checks.
- **Introduced kinds** (everything `#[derive(Kind)]` reaches) ride in the `aether.kinds` wasm custom section (ADR-0028) and are visible to the substrate and hub at load time without executing the component.
- **Kind identity** is compile-time: `K::ID = fnv1a_64(KIND_DOMAIN ++ canonical(name, schema))` (ADR-0030, ADR-0032; domain prefix added in issue #186). The substrate trusts these ids once registered.
- **FFI dispatch** is a single generic `receive_p32(kind_id, ptr, byte_len, count, sender) -> u32` export per component; the SDK's per-component runtime consults the `KindTable` and routes. (At the time this ADR shipped the ABI was `(kind, ptr, count, sender)` — `byte_len` was added later when wiring the postcard receive path; see *Wire-shape selection* in §Decision.)

Three problems compound under this shape:

1. **Typelist is separate from the handler body, so it's a forget-hazard.** Nothing enforces that every `mail.is::<K>()` check in `receive` has a matching entry in `type Kinds`. A missed entry doesn't fail to compile; `mail.is::<K>()` just returns `false` at runtime because the `KindTable` never got populated with `K::ID`. The failure surfaces as "my component doesn't react to Tick" with no diagnostic pointing at the missing typelist entry. The previous session's `handle!`-macro sketch (parked 2026-04-20) targeted exactly this hazard from the receive-body side.

2. **The MCP harness can see *which kinds a component introduces*, but not *which kinds a component actually handles*.** `describe_kinds` reports the `aether.kinds` section — every `#[derive(Kind)]` in the binary. When Claude (via the hub) decides what mail to send to a freshly-loaded component, the substrate has no structured answer to "what does this component listen for?" The typelist is a source-level decoration on the `Component` trait that never crosses the wasm boundary — it only shapes the runtime `KindTable`, which has no reverse-enumeration path. Claude's best current tool is "send it and see if it reacts."

3. **Generic `fn receive(&mut self, ctx, mail)` loses type information at the dispatch site.** Per-kind logic lives inside a type-erased branch chain; no per-kind type is recorded anywhere the substrate or hub could read. Static analysis of a component's capabilities requires guessing from source.

ADR-0027 anticipated this: its "real `match` over a derive-generated dispatch enum" follow-up and the parked `handle!` macro both try to move the typelist-equivalent data closer to where handlers are actually defined. This ADR commits to a cleaner version of that direction: **handler methods themselves are the only declaration**. No parallel typelist, no receive-body branch chain, no macro that scans the receive body.

The same data then feeds a new manifest section so MCP can read it.

## Decision

**A component's received-kind vocabulary is declared by `#[handler]`-tagged methods inside a single `#[handlers] impl Component for C` block. The SDK codegens a compile-time dispatch table from those methods and emits an `aether.kinds.inputs` wasm custom section listing every handled kind's id, name, and agent-facing documentation (extracted from rustdoc `///` comments). `type Kinds` and `Component::receive` are retired. An optional `#[fallback]` method in the same block declares a catchall handler; its rustdoc flows to MCP the same way as typed handlers.**

### Attribute surface

One cohesive `impl Component for C` block carries `init`, every `#[handler]` method, an optional `#[fallback]` method, and any non-handler helpers the component wants colocated. Rustdoc comments on the impl block, each handler, and the fallback feed MCP:

```rust
/// Logs every input event to the broadcast sink.
///
/// # Agent
/// Watch this component's broadcasts to see what input the substrate is
/// seeing at any given time. Useful for confirming that subscribe_input
/// actually wired up after a component reload.
#[handlers]
impl Component for InputLogger {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        InputLogger { observe: ctx.resolve_sink::<InputObserved>("hub.claude.broadcast") }
    }

    /// Emits a heartbeat entry once per frame so observers can spot stalls.
    ///
    /// # Agent
    /// Not useful to send manually — the substrate drives this. Subscribe
    /// to the broadcast sink to see the heartbeats.
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, tick: Tick) { /* ... */ }

    /// Forwards a keypress (with modifiers) to the broadcast sink.
    #[handler]
    fn on_key(&mut self, ctx: &mut Ctx<'_>, key: Key) { /* ... */ }

    #[handler]
    fn on_mouse_move(&mut self, ctx: &mut Ctx<'_>, m: MouseMove) { /* ... */ }

    /// Forwards anything unrecognized to the broadcast sink verbatim.
    #[fallback]
    fn on_anything(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>) { /* ... */ }

    fn shared_helper(&self) -> u64 { /* plain methods are fine too */ 0 }
}
```

`#[handler]` takes no arguments. The handled kind is inferred from the method's third parameter type (after `&mut self` and `&mut Ctx<'_>`) — the parameter is the decoded `K`. `#[fallback]` takes no arguments either; its third parameter must be `Mail<'_>`. Components that omit `#[fallback]` are strict receivers.

#### Wire-shape selection

Receive-side decode is uniform: the dispatcher always emits `mail.decode_kind::<K>()`, which calls `Kind::decode_from_bytes(bytes)`. Wire shape (cast vs postcard) is picked once at the kind's `Kind` derive site, not at every handler:

- A type with `#[repr(C)]` (and the user's existing `#[derive(Pod, Zeroable)]`) gets a cast-shape body that calls `bytemuck::pod_read_unaligned`.
- Anything else (the user's `#[derive(Serialize, Deserialize)]` already in place) gets a postcard-shape body that calls `postcard::from_bytes`.

`Kind::decode_from_bytes` carries no trait bounds — the per-K body uses whichever crate the type's existing derives satisfy, so cast and postcard kinds can share one trait method even though their decode bounds (`AnyBitPattern` vs `DeserializeOwned`) are disjoint. Hand-rolled `Kind` impls inherit a default body that returns `None`; if such a kind needs `#[handlers]` dispatch, the impl overrides `decode_from_bytes` directly.

The receive ABI is `receive_p32(kind, ptr, byte_len, count, sender)`. `byte_len` is the substrate-supplied total payload size (sourced from `mail.payload.len()`); `decode_kind` hands `K::decode_from_bytes` exactly that slice so postcard parsing is bounded by the actual frame and can't read past it into adjacent linear memory. The cast helper (`__derive_runtime::decode_cast`) cross-checks the slice length against `size_of::<K>()` for free.

### Agent documentation extraction

The macro reads each method's (and the impl block's) rustdoc `///` comments and extracts MCP-facing prose via a convention:

- **If the rustdoc contains a `# Agent` section heading**, its body (everything between that heading and the next heading of equal-or-higher level, or end-of-doc) is the MCP-facing text. Other sections (`# Panics`, `# Examples`, prose outside `# Agent`) are ignored for MCP purposes but still render in `cargo doc`.
- **If no `# Agent` section is present**, the entire doc comment is the MCP-facing text. Terse components don't need ceremony; everything they write becomes context.
- **If no rustdoc is present at all**, the manifest record carries `doc: None`.

The `# Agent` heading parallels `# Safety` / `# Examples` — conventional, renders in rustdoc, consumed by a specific reader. The name choice is deliberate: "Agent" describes the consumer (the agent driving the harness), rather than a specific protocol (`# MCP` is too specific; we may surface this to non-MCP agent interfaces later) or internal vocabulary (`# Harness` is correct but less immediately meaningful outside the project).

This convention applies identically to `#[handler]` methods, `#[fallback]` methods, and the `#[handlers] impl` block itself (which carries the component-wide description). The macro parses doc attributes via `syn::Attribute` — no new custom syntax.

### Codegen mechanism: `#[handlers]` attribute on the Component impl

`#[handlers]` is an attribute proc macro that sees the entire `impl Component for C` body in one expansion, plus any rustdoc attached to the impl block itself. For each method it classifies:

- **`#[handler] fn name(&mut self, ctx: &mut Ctx<'_>, arg: K)`** — records (K, method-name, agent-doc).
- **`#[fallback] fn name(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>)`** — records (method-name, agent-doc). At most one per component.
- **`fn init(...) -> Self`** — left on the `impl Component` block as-is.
- **Anything else** — moved to a sibling inherent impl (see below).

Rustdoc on the impl block itself becomes the component-wide agent description (extraction rules described above).

From the collected set it emits:

1. A dispatch fn: `unsafe fn __aether_dispatch(comp: &mut C, ctx: &mut Ctx, kind_id: u64, ptr: *const u8, len: u32) -> u32` that matches `kind_id` against each `<K as Kind>::ID` const and calls `C::<handler_method>(comp, ctx, pod_read::<K>(bytes))`. When no arm matches, a `#[fallback]` method is called with the raw `Mail<'_>`; strict components return `DISPATCH_UNKNOWN_KIND = 1`.
2. The `receive_p32` FFI export wired to `__aether_dispatch`.
3. Per-handler statics in the new custom section (details below) — one per `#[handler]`, at most one for `#[fallback]`, and one for the component-wide description from the impl block's rustdoc.
4. The init-time auto-subscribe walk (ADR-0030 Phase 2) — unchanged in behavior, but sourced from the `#[handler]` set instead of `C::Kinds`.

Because `impl Component for C` only admits trait methods, the macro rewrites the block during expansion: `init` stays on `impl Component for C`, while `#[handler]`, `#[fallback]`, and plain helper methods move to a sibling `impl C { ... }` block. Users see one cohesive source block; the post-expansion output is valid Rust and rustc's diagnostics point at the user's source spans (syn's `Span` preserves them).

The macro refuses to compile when two `#[handler]` methods resolve to the same kind, when `#[fallback]` appears more than once, or when a `#[handler]` method's third parameter isn't a simple type path. These are loud compile errors.

Scoping codegen to one attribute-on-impl-block avoids both `inventory`/`linkme`-style link-time collection (questionable-to-fragile on wasm32; see `project_compile_time_kind_ids_deferred` for the binding constraint) and a separate `handlers!(Tick, Key, ...)` macro alongside impl blocks (reintroduces the forget-hazard we're trying to eliminate). The macro sees every handler in its expansion scope and nothing outside it can silently add one.

### Custom section: `aether.kinds.inputs` v1

```
Section name: "aether.kinds.inputs"
Payload: concat(record*)
Record:
  [version: u8 = 0x01]
  [entry_tag: u8]  // 0x00 = handled kind, 0x01 = fallback, 0x02 = component doc
  [postcard(entry)]

entry for handled kind: { id: u64, name: String, doc: Option<String> }
entry for fallback:     { doc: Option<String> }
entry for component:    { doc: String }
```

Per-record versioning follows ADR-0028's precedent. The section is emitted by linker concatenation of `#[used] #[link_section = "aether.kinds.inputs"]` statics, one per `#[handler]`, at most one for `#[fallback]`, and at most one for the component doc. `id` duplicates `K::ID` rather than relying on the hub to re-derive from name — consistent with ADR-0030's compile-time id stance and avoids a recompute at load time. `doc` is `None` on handler/fallback records when the method has no rustdoc; the component record is simply omitted when the impl block has no rustdoc.

The section is `kinds.inputs` rather than extending `aether.kinds` because the two are semantically distinct: `aether.kinds` advertises the component's *contribution* to the substrate's kind registry (things it introduces, possibly just for sending), while `aether.kinds.inputs` advertises *receive capability*. A component may introduce kinds it never receives (e.g., pure emitter) or receive kinds it doesn't introduce (receiving a substrate-built-in like `Tick`). Keeping the two sections separate lets each be read or ignored independently.

### Hub and MCP exposure

The hub reads `aether.kinds.inputs` during `load_component` alongside `aether.kinds`. Capabilities ride on the component's mailbox descriptor in the hub's state, surfaced to MCP via an extended `describe_component(engine_id, mailbox_id)` result:

```
{
  "name": "input_logger",
  "doc": "Watch this component's broadcasts to see what input the substrate is seeing...",
  "receives": [
    { "id": 0x..., "name": "aether.tick", "doc": "Not useful to send manually..." },
    { "id": 0x..., "name": "aether.key", "doc": "Forwards a keypress (with modifiers)..." }
  ],
  "fallback": { "doc": "Forwards anything unrecognized..." }
}
```

`describe_kinds` (which describes the *engine's* vocabulary) stays unchanged; this is a new mailbox-scoped view. With this in place, Claude-in-harness chooses mail targeting a component with structural ground truth *and* the author's intent for how each inbox should be used — behavioral probing is no longer the fallback path.

### Strict receivers and substrate-side enforcement (deferred)

A component with no `#[fallback]` method is a **strict receiver**. In-SDK, unmatched kinds return a `DISPATCH_UNKNOWN_KIND` status from the generated dispatcher. Whether the substrate *also* rejects unhandled kinds pre-delivery (checking the hub's cached `aether.kinds.inputs` capability set before calling `deliver`) is deferred to a follow-up: pre-delivery rejection gives cleaner error locality at the cost of moving enforcement across the FFI boundary, and the SDK-side guard is enough to ship this ADR.

### Reply mail

Replies (ADR-0013) carry a kind id and route into the same `receive_p32` export. They dispatch through the same handler methods — a reply with kind id `K::ID` calls whichever `#[handler]` method has a `K` third parameter. The `Ctx` already surfaces the caller mailbox when present, so handlers that care about reply provenance read it there.

## Consequences

### Positive

- **Forget-hazard eliminated.** Writing a `#[handler]` method is the only way to handle a kind, and doing so automatically registers the handler in dispatch *and* the manifest. There is no separate list to miss, and no per-use-site decoration to drift out of sync.
- **MCP capability surface is structural and documented.** The hub can answer "what does this component receive, and what did the author say about each inbox?" from wasm metadata without instantiating or probing. Rustdoc `///` comments (with optional `# Agent` section filtering) become the single source of prose for both humans reading the code and agents driving the harness — no parallel description to write or keep in sync.
- **Per-kind type information survives to the handler.** The dispatcher calls a per-kind method; the user body already has `K` typed, no `mail.is::<K>() / decode_typed::<K>()` pair.
- **Handlers are plain methods.** They share state through `&mut self`, call each other, can be `pub` for test access, and sit next to non-handler helpers in the same `impl` block. Receive-side shared-state patterns (`let shared = self.derive();` before dispatch) remain natural — the shared derivation lives in a helper method the handlers call.
- **One cohesive `impl Component for C` block.** `init`, every handler, the optional fallback, and any helpers are colocated. Reading a component means reading one block.
- **Fallback is opt-in with context.** Components that want to observe/proxy everything declare it; MCP sees the `doc` string and can judge whether to route unstructured mail there. Components that don't, don't.
- **Supersedes ADR-0027 cleanly.** `type Kinds` served as a runtime `KindTable` seed; with per-handler dispatch the `KindTable` is gone. The "real `match` over a dispatch enum" follow-up from ADR-0027 also becomes moot — per-method handlers compose the same capability with less ceremony than a derive-generated enum.
- **The parked `handle!` dispatch-macro approach is subsumed.** Same forget-proof property (using the API is declaration), without introducing a third API shape alongside traits and macros.

### Negative

- **`#[handlers]` does source-level rewriting.** `#[handler]`, `#[fallback]`, and plain helper methods live in the source as members of `impl Component for C` but the macro extracts them into a sibling inherent impl during expansion. Users don't see this, but post-expansion diagnostics (borrow-check errors, lifetime issues) point at macro-rewritten spans — workable with syn's span preservation, but a mild debugging wrinkle compared to plain trait-impl dispatch.
- **Kind extraction from fn signatures is proc-macro work.** The macro parses each `#[handler]` method's third parameter type, expects a simple type path, and uses it as `K`. Generic handlers (`fn on_x<T: Kind>(&mut self, ctx, t: T)`) and wrapper types are explicitly rejected with a compile error naming the offending method. ~500 lines of proc-macro code total including validation; lives in the SDK, users never see it.
- **Non-handler methods colocated with handlers become macro input.** Every method inside the `#[handlers]` block passes through macro expansion even if it's unrelated. The macro preserves their tokens verbatim, but a subtle attribute-macro bug would affect them. Mitigated by integration tests that exercise both handler and non-handler methods.
- **Strict-by-default changes surprise shape for extenders.** A component author who expected silently-dropped mail for unhandled kinds instead gets a `DISPATCH_UNKNOWN_KIND` from the SDK dispatcher. Loud failure is the right default per the ADR-0012 precedent, and adding a `#[fallback]` method restores the old behavior when genuinely wanted.

### Neutral

- **Wire, canonical bytes, and compile-time ids are unchanged.** ADR-0030 `K::ID` and ADR-0032 canonical bytes feed the new dispatch table and section payload directly. No wire-format change, no FFI surface change beyond the dispatch-internal error code.
- **`Sink<K>` and the send-side surface are unchanged.** This ADR only touches receive-side plumbing. Sends still resolve through explicit `Sink<K>` fields.
- **Auto-subscribe (ADR-0030 Phase 2) stays.** The walk source shifts from `C::Kinds` to the `#[handler]` set; the emitted `aether.control.subscribe_input` mail is identical.
- **ADR-0028's `aether.kinds` section is unchanged.** Inputs live in a sibling section.

## Alternatives considered

- **Per-kind `impl Receive<K> for C` trait impls** (the original sketch before centralizing). Each handled kind is its own trait impl block; `#[handlers]`-equivalent attribute scans the module for impls. Rejected as primary form: handlers fan out across N impl blocks for an N-kind component, can't share state or helpers in the same scope, `#[fallback]` requires a separate trait (`ReceiveFallback`), and the macro either needs to wrap a module (`#[component] mod foo`) or every impl block individually. The `#[handlers]`-on-one-impl form gets the same forget-proof and MCP-visibility properties with less fan-out. Trait-based dispatch could still be used as the *internal* desugaring target the `#[handlers]` macro emits, but it's not the user-facing form.
- **`component! { impl Component { ... } impl Receive<Tick> { ... } }` block macro** wrapping all impl blocks. Gives the same forget-proof property as `#[handlers]` but impls inside a proc-macro body lose rust-analyzer support (jump-to-def is flaky, rustfmt skips the body, errors point at the macro invocation instead of the real site). The `#[handlers]` attribute wraps one impl block while leaving everything inside it as normal-looking Rust that tools understand.
- **`#[component] mod foo`** attribute over a module containing the component type and its impls. Equivalent in capability; rejected because it forces every component into its own module and the wrapping shape is more ceremonial than attribute-on-impl-block for what is usually one cohesive type.
- **Per-impl `#[handler]` attribute collected via `inventory!` or `linkme`.** Free placement of handler impls or methods, static registration at program start. Rejected: the cross-crate link-time collection story on wasm32 is fragile and already ruled out for kind ids (see `project_compile_time_kind_ids_deferred`). Same constraint applies here.
- **`handlers!(MyComponent; Tick, Key, MouseMove)` macro adjacent to free impl blocks.** Explicit, free-placement `impl` blocks. Rejected: reintroduces the exact forget-hazard this ADR is retiring — the name list and the impl set can drift.
- **`#[derive(Dispatch)]` on a user enum** (originally the ADR-0027 dispatch-enum follow-up). User declares `enum InputDispatch { Tick(Tick), Key(Key) }`, derive emits `Mail -> InputDispatch` plus dispatch. Rejected as the primary form: forces a central enum type and still needs a hand-maintained mapping between enum variants and handlers. The dispatch-enum could still land later for components that want exhaustive `match` syntax, but it's no longer the load-bearing mechanism.
- **Extend `aether.kinds` to carry per-kind "handled" flags rather than a separate section.** Saves a section header. Rejected: conflates introduction and handling (which are semantically distinct and can have different members — a component may introduce kinds it never receives) and forces every `#[derive(Kind)]` site to know whether it's being handled in this component. Separate sections keep each concern local to its source declaration.
- **Bare `fallback: true` flag without a documentation string.** Considered and explicitly rejected in the design discussion: the flag by itself is zero signal for MCP — "anything goes" doesn't help Claude decide what to send. A documented fallback is what earns the manifest entry; components without a fallback simply omit the record.
- **Required `#[fallback(doc = "...")]` attribute argument or required `#[handler(doc = "...")]` for typed handlers.** Considered before the rustdoc-capture direction. Rejected: duplicates rustdoc, the attribute-arg prose doesn't render in `cargo doc`, and every doc change becomes two edits. Rustdoc already exists as the project's documentation surface; the `# Agent` section extracts a subset for MCP without creating a second source.
- **First-paragraph-only capture instead of whole-doc fallback.** An alternative where the absence of `# Agent` sends only the summary paragraph (matching rustdoc's index-summary convention) rather than the full doc. Rejected: asymmetric with the explicit-section path (opt-in gets you "whatever you put there," opt-out gets you a rustdoc-convention subset), creates a surprising truncation for authors who write a single-paragraph doc and wonder whether their second sentence is visible. Whole-doc-when-no-section is the predictable rule.
- **Keep `fn receive` as the primary form and add `#[handlers]` as additive ergonomic sugar.** Soft migration, no breaking change. Rejected: doubles the SDK surface area, keeps the forget-hazard live for any component that stays on the old form, and defeats the MCP-capability-surface goal (because a hybrid component's inputs section is incomplete).

## Follow-up work

### Phased rollout

**Phase 1** — SDK-only, coexists with ADR-0027 surface:
- Ship `#[handlers]` attribute proc macro + `#[handler]` and `#[fallback]` inner attributes in `aether-component`: impl-block scan, per-method kind extraction, rustdoc parsing with `# Agent` section filtering, dispatch codegen, `aether.kinds.inputs` statics, impl-block rewrite (trait methods vs inherent methods).
- Define the `aether.kinds.inputs` v1 section format and parser in `aether-hub-protocol`.
- Migrate `aether-hello-component` to the new form as the canary; keep other in-repo components on the old form during Phase 1.
- `Component::Kinds` and `Component::receive` remain; components pick one style per type (no hybrid within one component).

**Phase 2** — Hub + MCP integration:
- Hub parses `aether.kinds.inputs` at load; stores per-mailbox capability set.
- New MCP tool `describe_component(engine_id, mailbox_id)` returns `{ name, doc, receives, fallback }` including author-written descriptions.
- Substrate emits a warning (not yet an error) when mail arrives for a strict receiver's unhandled kind.

**Phase 3** — Retirement (shipped 2026-04-20):
- Migrated remaining in-repo components (echoer, caller, input_logger, sokoban) to `#[handlers]`.
- Removed `Component::Kinds`, `Component::receive`, `KindList`, `Cons`, `Nil`, tuple-1..=32 impls, runtime `KindTable`. `Mail::is::<K>()` and `Mail::decode_typed::<K>()` were retained with new bodies that compare `K::ID` directly (no `KindTable` lookup).
- `#[handlers]` now emits an inherent `__aether_dispatch(&mut self, ctx, mail) -> u32` method (instead of a trait `receive`); the returned code is `DISPATCH_HANDLED` on match or `DISPATCH_UNKNOWN_KIND` on a strict-receiver miss, and `export!`'s `receive_p32` shim propagates it verbatim. This also fixes the gap filed as #142 (the scheduler's warn-on-unhandled-kind path now actually fires).
- `#[handlers]` prepends `ctx.subscribe_input::<K>()` calls to the user's `init` for every `K::IS_INPUT` handler kind, replacing the retired `KindList::resolve_all` walker.
- ADR-0027 marked **Superseded by ADR-0033**.
- Optional follow-up ADR: substrate-side rejection of unhandled kinds (moves enforcement across the FFI boundary).

### Deferred

- **Substrate-side strict-receiver enforcement.** Reject-pre-delivery rather than SDK-side drop. Own ADR when the Phase 2 warning rate gives concrete data on whether the extra round-trip is worth it.
- **Detection of forgotten `#[handlers]`.** A lint (not a compile error) that flags `fn` methods on a component type whose signatures look handler-shaped (`&mut self, &mut Ctx, K`) but aren't tagged `#[handler]`, or that flags `#[handler]`-tagged methods outside a `#[handlers]` block. Only ships if the mistake pattern actually surfaces in practice.
- **Dispatch-enum derive** (from ADR-0027's own deferred follow-up). Now strictly optional ergonomic sugar for components that want exhaustive `match` against an enum. Ships only if someone asks.
