# ADR-0027: Component-declared kind dependencies via associated typelist

- **Status:** Superseded by [ADR-0033](0033-handler-driven-inputs-manifest.md)
- **Date:** 2026-04-19
- **Accepted:** 2026-04-19
- **Superseded:** 2026-04-20

> ADR-0033's `#[handlers]` attribute took over the concerns this ADR addressed — a single `impl Component for T` block carries the kind set, the dispatcher, and the hub-facing capability surface, so the forget-hazard between `type Kinds` and `fn receive` is gone along with both surfaces themselves. Phase 3 of ADR-0033 (2026-04-20) retired `type Kinds`, `Component::receive`, `KindList`/`Cons`/`Nil`, and `KindTable` from the SDK.

## Context

ADR-0014 committed the guest SDK to `Component` as a trait with associated types. The shipping shape today is:

```rust
pub struct InputLogger {
    tick: KindId<Tick>,
    key: KindId<Key>,
    mouse_button: KindId<MouseButton>,
    mouse_move: KindId<MouseMove>,
    observe: Sink<InputObserved>,
}

impl Component for InputLogger {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        InputLogger {
            tick: ctx.resolve::<Tick>(),
            key: ctx.resolve::<Key>(),
            mouse_button: ctx.resolve::<MouseButton>(),
            mouse_move: ctx.resolve::<MouseMove>(),
            observe: ctx.resolve_sink::<InputObserved>("hub.claude.broadcast"),
        }
    }

    fn receive(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>) {
        if self.tick.matches(mail.kind()) { ... }
        else if self.mouse_button.matches(mail.kind()) { ... }
        else if let Some(k) = mail.decode(self.key) { ... }
        else { mail.decode(self.mouse_move).map(...); }
    }
}
```

Three layers of bookkeeping per kind a component receives:

1. A `KindId<K>` field on `Self`.
2. A `ctx.resolve::<K>()` call in `init` that populates that field.
3. An arm in `receive` that uses the cached field to test or decode.

The fields and resolve calls are pure plumbing — no logic, no decisions. Adding a new kind handler requires touching three places, and the field name typically duplicates the type name (`tick: KindId<Tick>`). For a 4-kind component like `input_logger` that's 8 lines of pure book-keeping. The proportion grows with kind count.

The receive-side `if/else` chain is real logic but suffers from a related problem: it forces an explicit `KindId<K>` reference at every test site (`self.tick.matches(...)`, `mail.decode(self.key)`), so the field has to exist even when the receive body is the only consumer.

A second related issue: a component's accepted-mail vocabulary is not visible in its type signature. To see what kinds `InputLogger` handles you have to read the body of `init` (the `ctx.resolve` calls) — a documentation gap a reader must traverse imperative code to fill.

## Decision

Move kind dependencies into a `Component::Kinds` associated type and let the SDK generate the cache machinery from it.

### 1. Trait shape

```rust
pub trait Component: Sized + 'static {
    /// The kinds this component receives. Resolved by the SDK at init.
    type Kinds: KindList;

    fn init(ctx: &mut InitCtx<'_>) -> Self;
    fn receive(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>);

    // ADR-0015 lifecycle hooks unchanged
    fn on_replace(&mut self, _ctx: &mut DropCtx<'_>) {}
    fn on_drop(&mut self, _ctx: &mut DropCtx<'_>) {}
    fn on_rehydrate(&mut self, _ctx: &mut Ctx<'_>, _prior: PriorState<'_>) {}
}
```

`KindList` is a SDK-provided trait with two ergonomic implementations:

- **Tuples up to N=32**: `type Kinds = (Tick, Key, MouseMove, MouseButton);`
- **Cons-list for unbounded arity**: `type Kinds = Cons<Tick, Cons<Key, Cons<MouseMove, Nil>>>;`

Both implement `KindList` and feed the same machinery downstream. Tuples cover every realistic component (12+ kinds in one component is already a smell, suggesting the component is too coarse). The cons-list is the escape hatch.

### 2. Init shim resolves the typelist

The `export!` macro's generated `init` shim invokes `<C::Kinds as KindList>::resolve_all(&mut init_ctx)` *before* calling the user's `Component::init`. That walks the typelist, calls the existing `resolve_kind` host fn for each `K::NAME`, and stores the resulting `(TypeId, u32)` pair in a per-component static `KindTable`.

A typo in `K::NAME` panics at the existing `resolve` call site — same loud-at-init failure mode ADR-0012 §2 commits to. Timing is unchanged (still pre-`init`); only the call site moves from user code into SDK-generated code.

User `init` after this ADR shrinks to building state and resolving sinks:

```rust
fn init(ctx: &mut InitCtx<'_>) -> Self {
    InputLogger {
        observe: ctx.resolve_sink::<InputObserved>("hub.claude.broadcast"),
    }
}
```

`Sink<K>` stays an explicit field because each sink is a `(MailboxId, KindId<K>)` pair — the kind is type-driven but the mailbox name is data, so sinks can't be type-resolved alone.

### 3. Receive helpers read the cache

Two new methods on `Mail<'_>`, both type-driven:

```rust
impl<'a> Mail<'a> {
    /// True if the inbound kind matches `K`. For signal-shaped kinds.
    pub fn is<K: Kind + 'static>(&self) -> bool { /* lookup table → matches */ }

    /// Some(K) if the inbound is a `K`, else None. For POD kinds.
    pub fn decode_typed<K: Kind + bytemuck::AnyBitPattern + 'static>(&self) -> Option<K> { ... }
}
```

(Implementation note: the original sketch named the type-driven decode `decode` to mirror the existing `decode(kind_id)`. Rust does not allow two inherent methods with the same name on the same type, so the shipped name is `decode_typed`. `_typed` signals "type-driven via the kind table" rather than "via the explicit `KindId<K>` arg". A symmetric `decode_slice_typed` covers the batched path. The shipped methods return owned `K` (matching `decode`'s `pod_read_unaligned` semantics) rather than borrowed `&K` to keep alignment requirements off the receive body.)

The receive body becomes:

```rust
fn receive(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>) {
    if mail.is::<Tick>() { ... }
    else if mail.is::<MouseButton>() { ... }
    else if let Some(k) = mail.decode_typed::<Key>() { ... }
    else if let Some(m) = mail.decode_typed::<MouseMove>() { ... }
}
```

No `KindId<K>` field references; type alone identifies the kind.

The if-let chain reads close to a `match` but isn't one — Rust has no "match on type," and the kind ids are runtime values assigned at substrate handshake. Real `match` syntax over kinds requires either a macro that rewrites the syntax, or a derive-generated dispatch enum. The latter is a clean upgrade path; see Follow-up work below. v1 ships the if-let chain because it works without any new machinery beyond what this ADR already introduces.

### 4. KindTable: per-component static

The `export!` macro generates a `static __AETHER_KINDS: KindTable = KindTable::new()` next to the existing `static __AETHER_COMPONENT: Slot<C> = Slot::new()`. `KindTable` is:

- A small `(TypeId, u32)` map (linear-scan over a fixed-cap inline buffer, since N is bounded and small)
- Single-threaded write-once-read-many, same invariant as `Slot<T>`
- Uses `UnsafeCell` directly; no `RefCell`/`Mutex` — wasm guest is single-threaded

Sized at `MAX_KINDS = 32` to match the tuple impl ceiling. Cons-list components beyond 32 spill to a heap-allocated fallback (negligible runtime cost; only triggered at the upper end).

### 5. Migration & coexistence

The new shape is mostly additive. Existing components written against the current API (with explicit `KindId<K>` fields and `ctx.resolve::<K>()` calls in init) continue to compile and run unchanged at the **call** sites — `mail.decode(kind_id_field)` / `KindId::matches` stay on the API surface. Authors choose between the two styles per component.

The one non-additive nudge: every `Component` impl must explicitly declare `type Kinds = (...)` (or `type Kinds = ();` if the receive body uses only the older `KindId<K>` field pattern). Default associated types are still nightly-only, so the trait can't ship `type Kinds = () = ...`. The cost is a one-line addition per component; the in-repo examples (echoer, caller, input_logger, hello-component) get migrated to the new shape as part of this ADR's implementation PR.

### 6. Compile-time membership gate (deferred to follow-up)

`mail.decode::<K>()` could carry a `where K: ContainedIn<C::Kinds>` bound that turns "decode a kind not declared in `Kinds`" into a compile error. This requires a typelist membership trait with recursive impls — straightforward but adds machinery. **Deferred to a follow-up** so v1 ships smaller and the runtime cache validates the design before the static-check work lands. A v1 typo against an undeclared kind returns `None` at runtime (silent miss), which is the same failure mode as decoding the wrong kind today.

## Consequences

### Positive

- **Per-kind cost drops from 3 places to 1.** Adding a kind handler: add to `Kinds` typelist + write the receive arm. No field declaration, no resolve line.
- **Component vocabulary visible in the type signature.** Reading `impl Component for InputLogger` immediately shows `type Kinds = (Tick, Key, MouseMove, MouseButton)`. Currently you have to scan `init`'s body.
- **Loud-at-init failure preserved.** SDK-generated init shim resolves before user init; typo behavior is unchanged.
- **No user-facing macros.** Tuple syntax + associated-type declaration are pure Rust trait machinery. Macro use is confined to the SDK's internal generation of `KindList` impls for tuple sizes 1..=32 (matches std's tuple impl pattern; "macros for generation, not intermediates" framing per session discussion).
- **`Sink<K>` stays untouched.** Send-side semantics (mailbox + kind) didn't have the same type-driven simplification available, and not changing what works avoids churn.
- **Migration is opt-in.** Existing components compile unchanged; authors adopt per-component when convenient.

### Negative

- **Hidden static state in the guest.** A new `static __AETHER_KINDS: KindTable` joins the existing `static __AETHER_COMPONENT: Slot<C>`. Single-threaded wasm guest keeps it sound, but it's another piece of "you can't see this from the user code." Mitigated by the fact that the existing `Slot<C>` already established the pattern.
- **Tuple impls have an arity ceiling.** N=32 is generous but not infinite. Components exceeding it must use the cons-list form (`Cons<H, T>` / `Nil`). The cons-list is verbose; if it becomes a real complaint, a `kinds!()` macro is the documented next step (also "generation-only" macro use). Until then, the verbosity is acceptable because the threshold (32) is well above the practical limit of one cohesive component.
- **More trait machinery in the SDK.** `KindList`, `Cons`, `Nil`, plus 32 tuple impls + the cons impl + the `KindTable`. ~150 SDK lines for the new mechanism. Lives in the SDK; users never see it.
- **Compile-time typo gate is not in v1.** A `mail.decode::<UndeclaredKind>()` returns `None` rather than failing to compile. Same failure mode as decoding the wrong kind today; addressed in the deferred follow-up if usage justifies the trait machinery.
- **One more concept to teach.** "Declare your kinds in `type Kinds`" is a new step a new component author has to learn. Argued for by the fact that today's "field + init resolve + receive arm" pattern is also a concept they have to learn — the new one is shorter to state.

### Neutral

- **Substrate side is untouched.** This is purely a guest-SDK ergonomics change. No host-fn changes, no FFI changes, no ADR-0024-shape consequences. The substrate continues to hand `(kind: u32, ptr, count, sender)` into the receive shim as today.
- **No wire-format change.** Mail bytes don't carry kind names; kind ids assigned at handshake stay the same.
- **Existing example components migrate as part of this ADR's implementation PR.** Echoer, caller, and input_logger become before/after demonstrations of the ergonomic delta — and the diff doubles as the public migration guide.

## Alternatives considered

- **`match_mail!` declarative macro for the receive body only.** Sugar over the existing if/else chain, no derive, no associated type. Rejected: keeps the `KindId<K>` field bookkeeping (the larger source of pain), and the user explicitly preferred avoiding macros for intermediate (non-generation) code.
- **Pure type-driven decode without explicit init declaration** (call it "B"). `mail.decode::<K>()` would lazily resolve `K::NAME` on first use and cache. Rejected: loses ADR-0012 §2's loud-at-init failure mode — a typo would only surface at first dispatch, possibly never, which is strictly worse for debugging.
- **`#[component]` proc macro with `#[on(Kind)]` handler attributes** (originally option C). Generate cache, init body, *and* receive dispatch. Rejected for v1: bigger lift (real proc-macro crate work), forces one specific dispatch shape that doesn't fit when one method handles multiple kinds, harder to debug when generated code goes wrong. Worth revisiting later if the typelist+helpers don't go far enough.
- **`#[derive(Component)]` with `#[depends_on(Tick, Key, ...)]` attribute.** Same effect as the associated type, different syntax. Rejected: requires a derive macro where the associated type works with native trait machinery; ADR-0014 already chose associated types as the trait idiom, so this fits the existing direction.
- **Linkme-based auto-discovery.** Every `impl Kind for K` in the binary auto-registers; init walks the registry. Rejected: adds an external dep, can't distinguish "I declared this kind exists" from "I want this component to receive it," makes it impossible for two components in the same binary to differ in kind dependencies.
- **Bump tuple ceiling above 32.** Could go to 64 or 128. Rejected: the cons-list escape hatch already covers truly unbounded cases; any tuple ceiling will be exceeded eventually, so providing the unbounded path matters more than picking a higher arbitrary ceiling. 32 is the standard choice in similar Rust crates.

## Follow-up work

### v1 (this ADR's implementation PR)

- **`aether-component`**: add `KindList` trait, `Cons<H, T>` / `Nil` types, tuple impls for 1..=32 (macro-generated inside the SDK), `KindTable` static type. Add `Mail::is::<K>()` and `Mail::decode::<K>()` (type-driven). Add `Component::Kinds` associated type with default `()`.
- **`aether-component`**: update `export!` macro to emit `static __AETHER_KINDS` and call `<C::Kinds as KindList>::resolve_all(&mut ctx)` in the init shim before user `init`.
- **`aether-component`**: keep existing `KindId<K>` / `ctx.resolve::<K>()` / `mail.decode(kind_id)` API surface — the new and old styles coexist; this PR doesn't deprecate anything.
- **In-repo example components** (echoer, caller, input_logger, hello-component): migrate to the new shape. The diff is the public migration guide.
- **CLAUDE.md**: brief note about the new `type Kinds = (...)` pattern, with a one-line pointer to this ADR for the trait machinery.

### Deferred follow-ups

- **Compile-time membership gate.** `mail.decode::<K>() where K: ContainedIn<Self::Kinds>`. Adds a recursive `ContainedIn` typelist trait. Ships when usage justifies the SDK complexity.
- **`kinds!(K1, K2, ...)` macro for ergonomic cons-list syntax.** Only if cons-list verbosity becomes an actual complaint after v1 lands.
- **Real `match` over a derive-generated dispatch enum.** The user declares an enum (`enum InputDispatch { Tick, Key(Key), MouseMove(MouseMove), MouseButton }`) with a `#[derive(Dispatch)]` that maps variants to kinds via `#[kind(K)]` attributes. `mail.dispatch::<InputDispatch>()` returns the enum, and the receive body becomes a real Rust `match` — exhaustiveness checking, named dispatch type, the works. Macro use is "generation" (a derive that emits the conversion fn) rather than "intermediate" rewriting, so it fits the project's macro discipline. Cost: a derive macro in the SDK + a per-component enum declaration. Ships when the if-let chain feels stale enough to justify the SDK complexity.
- **`#[on(Kind)]` handler attribute proc macro** (originally option C in the design discussion). Builds on the typelist v1 — could generate dispatch as well. Worth its own ADR if pursued; partly subsumed by the dispatch-enum follow-up above.
- **Deprecation of the `KindId<K>` field pattern.** Decide separately once the new pattern has soaked through real components.
