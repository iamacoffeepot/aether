# ADR-0098: Scoped singletons

- **Status:** Superseded by [ADR-0099](0099-actor-identity-and-addressing.md)
- **Date:** 2026-06-07
- **Superseded:** 2026-06-07

> **Superseded by ADR-0099 (2026-06-07).** This ADR framed the #1364 gap
> and proposed a flat `{scope}:{segment}` name join (§2/§6) with an open
> question about where a runtime scope name lives (§7). ADR-0099 carries
> the **per-scope-singleton concept forward** — a singleton is "exactly
> one within a scope," enforced by id uniqueness — but replaces the
> addressing mechanism: an actor's identity splits into an **ActorId**
> (what code, the flat per-node hash) and a **MailboxId** (where in the
> tree, a Merkle-chain fold of the node's lineage of ActorIds). The flat
> string join is replaced by that fold; §7 is closed by carrying the
> lineage as a rolling `u64` fold state on the actor's binding (neither
> a name on the handle nor a registry round-trip). The reasoning below
> is preserved as the decision as it stood; see ADR-0099 for the model
> that supersedes it.

## Context

ADR-0079 made cardinality a first-class axis: an actor is either a `Singleton` (one per substrate, addressed by type via `ctx.actor::<R>()`) or `Instanced` (N per substrate under a `{NAMESPACE}:{subname}` prefix, addressed by runtime name via `ctx.resolve_actor::<R>(subname)`). That ADR also fixed the meaning of a singleton's `NAMESPACE`: "For singletons it's the full mailbox name" (§1).

That last claim is where the model breaks. A loaded wasm component declares a `NAMESPACE` (e.g. `"camera"`) and is, conceptually, one-of-a-kind when loaded at its default name. But it does not register at `"camera"`. The load path spawns it as a child of the trampoline, so its mailbox is `aether.component.trampoline:camera` (`crates/aether-capabilities/src/component.rs`, `WasmTrampoline::NAMESPACE = "aether.component.trampoline"`). A sender that type-addresses it — `ctx.actor::<Camera>()` — hashes the bare `"camera"`, gets a `MailboxId` nothing is registered under, and the mail warn-drops. Issue #1364 is this gap: the `Singleton` doc promises type-addressing reaches the component, and the registration disagrees.

The trampoline is one instance of a general shape. The same structure recurs wherever a singleton lives **inside** another actor rather than at the root:

- A loaded component inside its trampoline host.
- One `PlayerState` per `aether.net.session:42` — a different one per session, same type.
- One settings actor per open document, one HUD per camera rig.

In every case the actor is singular *within its parent*, not *within the substrate*. ADR-0079's `Singleton` axis can only express the substrate-global case, so these end up modelled as `Instanced` (losing the "exactly one" guarantee) or reached through a bespoke helper (the `loaded::<R>(name)` extension trait, which exists precisely because the type-addressed path doesn't work for components).

The constraints carried in:

- **ADR-0029.** `MailboxId` is a 64-bit hash of the mailbox name. Whatever we compose for a scoped name must hash through the existing function — wire format unchanged.
- **ADR-0079.** Cardinality axis, the `:` structural separator, name uniqueness enforced globally, tombstone-on-close. This ADR revises §1's "singleton NAMESPACE = full name" and extends the resolution surface; everything else in 0079 stands.
- **Resolution is a hash with no inverse.** `mailbox_id_from_name` (`crates/aether-data/src/hash.rs`) maps a name to a `MailboxId` one-way. An `FfiActorMailbox<R>` carries only that `u64` (`crates/aether-actor/src/ffi/mailbox.rs`), and `spawn_child` returns only a `MailboxId` (`crates/aether-actor/src/ffi/ctx.rs`). A handle therefore cannot tell you its own name, which is why a child's address cannot be reconstructed from a handle alone — the name has to be carried forward or re-derived.

The forces we're balancing:

- A singleton's "exactly one" guarantee should survive being nested. One `PlayerState` per session is as much a singleton as one `RenderCapability` per substrate.
- Type-addressing (`ctx.actor::<R>()`) is valuable because it's a compile-time const hash with no host round-trip. We keep it for the case where it's honest, and stop promising it where it isn't.
- A child's parent is runtime information for instanced parents (which session?) and static for root-singleton parents (which trampoline? the only one). The resolution surface must serve both without forcing the runtime case into a static const.
- Handles are hot-path objects; they should stay cheap (`Copy` `u64`) unless there's a strong reason to fatten them.

## Decision

### 1. A singleton is unique *per scope*, not per substrate

A singleton's cardinality guarantee is "exactly one **within a scope**." A scope is one of:

- **Root** — the substrate itself. One instance per substrate. This is every chassis capability today.
- **A parent instance** — one instance per occurrence of that parent. One `PlayerState` per `session:42`.

`Singleton` stays the marker for "exactly one." What changes is that "one of what" is answered relative to a scope, and the substrate-global case becomes the `Root`-scoped special case rather than the only case.

### 2. A full name is a path; the join rule is uniform

An actor's full mailbox name is its scope's name joined with its own `NAMESPACE` segment:

| Scope | Full name | Cardinality |
| --- | --- | --- |
| `Root` | `NAMESPACE` | one per substrate |
| `Within(parent)` | `{parent's full name}:{NAMESPACE}` | one per parent instance |

The `Root` scope contributes an empty prefix, so a root-scoped singleton's full name is exactly its `NAMESPACE` — every existing chassis cap is unchanged, and ADR-0079's `:` separator and segment-validation rules carry over verbatim. A scoped singleton's `NAMESPACE` is its **segment within its scope**, no longer asserted to be the full name; §1's "singleton NAMESPACE = full mailbox name" narrows to the root case.

The join rule is fixed at `{scope}:{segment}`. The **scope** is the variable, the **rule** is not. We deliberately do not let each parent define its own child-naming scheme (see Alternatives) — one uniform join covers trampoline, session, and nesting, and a per-parent rule is unjustified until a concrete second rule exists.

### 3. Uniqueness-per-scope is enforced by name uniqueness — no new mechanism

Because the scope is part of the full name, "at most one `R` per scope" is identical to "this full name is unique in the registry." ADR-0079 already enforces full-name uniqueness on registration (collision is a hard error; retired names tombstone). Putting the scope in the name means the existing collision check enforces per-scope singleton-ness for free. Two default-name loads of the same component collide at `aether.component.trampoline:camera`; two `PlayerState`s in `session:42` collide at `aether.net.session:42:player`. No `(scope, type)` index is added.

### 4. Resolution: the scope is the receiver

A scope is a value you resolve against, not a type parameter you pass. `ctx` is the root scope; any actor handle is its own instance's scope. One verb — `actor` — means "the singleton of this type in this scope," and the scope is whatever you call it on:

```rust
// Root scope — unchanged. The scope is the substrate; the name is the bare NAMESPACE.
ctx.actor::<Render>()                              // → "aether.render"

// A handle is a scope. Nest by chaining: ComponentHost is a root singleton,
// so its name is a compile-time const and the whole path resolves from types:
ctx.actor::<ComponentHost>().actor::<Camera>()     // → "aether.component.trampoline:camera"

// A runtime-instance handle is a scope too. Which instance is runtime, so the
// handle is the seed; the segment below it composes statically:
session_handle.actor::<PlayerState>()              // → "aether.net.session:42:player"
```

Whether a resolution is a pure const hash or needs runtime data is governed by the scope — the receiver:

- **Statically-scoped** (root, or a singleton nested in a statically-addressable singleton): the entire path is known at compile time, so resolution is a const hash with no round-trip — the property that makes `ctx.actor` cheap. The trampoline/default-load case lives here.
- **Runtime-scoped** (nested in an instance like a session): the instance's subname is runtime data, so resolution seeds from the instance handle.

We do **not** infer a child's scope from its type. A `Camera` does not declare "my parent is the trampoline" — its scope is the receiver the caller resolves against. This keeps the actor type scope-agnostic (the same component can be loaded under different names/parents) and avoids reintroducing a static parent marker on the type (see Alternatives, "Scope const on the child type").

### 5. Nesting: chain the receiver, with tuple sugar for the single-call case

Children of children extend the path; the data model already covers it (`session:42:room:player` is a player singleton scoped to a room singleton scoped to session 42). Two surfaces, same result:

```rust
// Chain — the model. Each .actor steps one scope deeper. Same verb off root or a handle.
ctx.actor::<A>().actor::<B>().actor::<C>()           // fully static
session_handle.actor::<Room>().actor::<Player>()      // runtime seed, static below

// Tuple sugar — one call for the common nested case. The last type is the target;
// the leading types are the scope path from the receiver. Bounded by the arity we impl.
ctx.actor::<(ComponentHost, Camera)>()
session_handle.actor::<(Room, Player)>()
```

Both go through one method, `fn actor<P: ScopePath>(&self) -> FfiActorMailbox<P::Target>`: `ScopePath` is implemented for a bare singleton type (a single segment) and for tuples (a path), so `actor::<R>()` and `actor::<(A, B, C)>()` are the same call shape. The bare comma form `actor::<A, B>()` is not expressible — stable Rust has neither variadic generics nor default type parameters on functions — so the tuple parens are the cost of the single-call sugar, and the chain is the parens-free equivalent.

A path is `[prefix from the seed] + [static singleton segments]`, where the seed is `root` (empty prefix, fully const) or a runtime instance handle (its full runtime name). The resolution rule:

- **Every link is singleton-in-singleton** → the whole path is compile-time; a pure const hash, no runtime data.
- **A runtime instance is in the chain** → resolve statically *downward from the deepest runtime handle you hold*. You cannot cross a runtime link you don't hold a handle for: for `session:42 → conn:7 → player`, holding only the session handle cannot reach `player`, because `7` is runtime and not in your possession — you need the `conn` handle.

The cost of "the caller declares the chain rather than the framework inferring it": a wrong chain composes a name nothing is registered under and warn-drops — the #1364 failure, now N levels deep. Nesting therefore raises the stakes on the open question in §7 (validation), it does not add a new decision.

### 6. Handles stay ids; the name lives in the transient resolver

The path string accumulates only in the **resolver** — the intermediate value a `.actor` chain (or a tuple's `ScopePath`) threads on the stack — never in a long-lived handle. The terminal step hashes once and yields an `FfiActorMailbox<R>` carrying just the `u64`, so handles stay `Copy` and FFI-cheap. The only irreducible runtime touch is **seeding** a chain from a runtime instance, which needs that instance's name; root-seeded chains need nothing and stay fully const.

A new const helper composes without allocating a joined string: `mailbox_id_from_name_pair(prefix, segment)` hashes `prefix`, then `:`, then `segment`, mirroring `mailbox_id_from_name` and keeping ADR-0029's hash identity (the pair helper over `("a", "b")` hashes identically to the literal `"a:b"`). Each step of the chain applies it.

A path-depth and total-length cap bounds names as registry keys (each segment already obeys `NAMESPACE_SEGMENT_MAX_LEN`; the cap bounds the concatenation), erroring rather than letting a runaway chain bloat the key space.

### 7. Open decision: where the runtime scope name lives

When the scope is a runtime instance, §6's "seed needs the instance's name" has two honest implementations. This is the one decision that picks the implementation; everything above is independent of it.

- **(C) Registry-derived.** Handles stay a bare `u64`. Seeding asks the substrate, "the child of `<parent id>` named `<segment>`" — the registry holds each entry's name, composes the child, and returns its id. Cost: a resolve-time host round-trip (resolution stops being a pure const for the runtime case). Pays back: handles stay cheap; resolution can confirm each segment is actually live and parented as claimed, so a stale or wrong chain fails **loudly** instead of warn-dropping. Resolution is once-per-peer and the handle is cached, so the round-trip is amortized; the send path stays a bare-id dispatch regardless.
- **(B) Handle-carried.** The instance handle carries its resolved name alongside the id. Seeding composes locally with no host call. Cost: instance handles stop being `Copy` `u64` (they hold a name — heap/`&str`, heavier across the wasm FFI boundary). Pays back: no round-trip; lineage composes purely in the guest.

The recommendation leans **C**: handles stay cheap, and registry validation turns the #1364 silent-warn-drop into a loud failure exactly where nesting makes silent-wrong-chains more likely. The lean is mild — if guest-local composition with zero host calls turns out to matter for a hot resolution path, B is the answer. This is the call to make before implementation; the rest of the model holds either way.

## Consequences

### Positive

- **#1364 closes with the contract made honest, not patched.** A loaded component is a singleton scoped to its component-host. `ctx.actor::<R>()` reaching the bare `NAMESPACE` is root-scope resolution applied to a non-root actor — the surfaced bug. The correct surface is scope-qualified (`ctx.actor::<ComponentHost>().actor::<R>()`, the `ctx.actor::<(ComponentHost, R)>()` sugar, or the existing `loaded::<R>(name)` for a non-default name). The `Singleton` doc's "senders address by type" becomes precise: by type **within a scope**.
- **The "exactly one" guarantee survives nesting.** One `PlayerState` per session is a singleton, enforced by the existing name-collision check, without modelling it as `Instanced` and losing the guarantee.
- **No new enforcement machinery.** Per-scope uniqueness rides on full-name uniqueness, which already exists. The scope is encoded in the name; the registry does the rest.
- **Type-addressing stays cheap where it's honest.** Root and fully-static-scoped chains resolve as const hashes with no round-trip — the ADR-0079 property is preserved for exactly the cases where it's true, and dropped only where it was lying.
- **Handles stay cheap.** Under §6, the name lives in the transient resolver; the long-lived handle remains a `Copy` `u64` (modulo the §7 choice for runtime seeds).

### Negative

- **The static-vs-runtime boundary has to be taught.** One verb (`actor`) resolves in every scope, but whether a given call is a const hash or needs a runtime seed depends on the receiver, not the signature. That is a real property of the scope, not incidental API sprawl, but it is a rule a reader learns rather than reads off the type.
- **The caller declares the chain.** No inference means a wrong chain warn-drops (or fails loudly, under §7-C). The honesty cost lands on the caller, deepened by nesting.
- **A new const-composition primitive.** `mailbox_id_from_name_pair` is additive but is a second hashing entry point that must stay bit-identical to `mailbox_id_from_name` over the joined string (a round-trip test guards it).
- **§7 is unresolved at proposal time.** The handle-vs-registry choice gates the implementation shape and needs a decision before code.

### Neutral

- **ADR-0079 §1 is revised, not superseded.** The cardinality axis, separator, uniqueness, and tombstone rules all stand. Only "singleton NAMESPACE = full mailbox name" narrows to the root case.
- **`MailboxId` storage and wire format unchanged.** A scoped name is still just bytes joined by `:`; ADR-0029 holds.
- **Instanced and scoped-singleton are siblings, not rivals.** `Instanced` is "a scope with a subname slot, many occupants"; a scoped singleton is "a scope with exactly one occupant." Both compose onto a parent path the same way.

## Alternatives considered

- **Scope const on the child type** (`const HOST` / `type Scope = Parent`). Encode the parent statically on the singleton so `ctx.actor::<R>()` composes it. Rejected because a runtime scope (which session?) cannot be a compile-time const, so it only ever serves the static case while bolting parentage onto a type that should stay scope-agnostic (the same component loads under different names/parents). It is the marker-on-the-type smell that signals the model is in the wrong place.
- **Alias the bare name at registration** (issue #1364 option b). Register a default-name load under its bare `NAMESPACE` as well, so `ctx.actor::<R>()` resolves. Rejected: it reintroduces the bare-name collision surface and gives a component two identities (a trampoline instance and a bare singleton), with no per-scope guarantee.
- **Per-parent child-naming rules.** Let each parent encode how its children compose (not just the scope, but the join). Rejected as YAGNI — one uniform `{scope}:{segment}` covers every parent in hand; a per-parent rule is speculation until a second rule is forced.
- **Always-fat handles.** Carry the resolved name in every `FfiActorMailbox`, not just instance seeds. Rejected: it taxes the hot send path with a non-`Copy`, FFI-heavy handle to serve a resolution-time need; §6 confines the name to the transient resolver instead.
- **Infer parentage from the type and walk it.** Have the framework derive a child's scope chain from declared parent types and auto-resolve `ctx.actor::<C>()` to its full path. Rejected for the same reason as the scope const: it only works when every link is static, and it puts runtime lineage where a const can't represent it.

## Related

- ADR-0079 — Instanced actors as a first-class category. This ADR revises §1 (singleton NAMESPACE semantics) and extends §3 (resolution surface); the rest stands.
- ADR-0029 — `MailboxId = hash(name)`. Preserved; the pair helper hashes the joined name identically.
- ADR-0097 — Wasm sibling spawn (`ctx.spawn_child`). Spawn names children through `Subname`; scoped resolution is the read side of the same path model.
- ADR-0096 — Multi-actor wasm modules. Several actors per module sharpen "reach a specific one by scope."
- Issue #1364 — the doc-vs-behavior gap this ADR resolves; first consumer of scoped resolution.
- Issue #1355 — agent-guide actor-model page; surfaced the gap while documenting the working path.
