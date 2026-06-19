# ADR-0119: Actor addressing via a Resolver strategy

- **Status:** Proposed
- **Date:** 2026-06-18

## Context

ADR-0099 gave every actor two identities — an `ActorId` (which actor) and a `MailboxId` (where it sits, folded from its lineage) — and made resolution static: a type carries the rule for producing its own mailbox, so `ctx.actor::<R>()` reaches it with no registry lookup. ADR-0079 split that rule by cardinality, and #2048 / #2053 settled the trait surface: `Addressable` carries identity (`NAMESPACE`), and the cardinality markers `Singleton` / `Instanced` each declare a `resolve` with its own default — root-pinned for a singleton, a `hash(NAMESPACE:subname)` fold for an instanced actor.

That surface handles the two ordinary cardinalities, but an **embedded** actor — an FFI/wasm component hosted under a trampoline at `aether.embedded:NAME` (ADR-0099 §5/§6) — resolves differently from either. Its mailbox is its own `NAMESPACE` folded as an instance under the reserved `aether.embedded` scope, onto the component host's carry. Today that is expressed by:

- an `EmbeddedHost` zero-sized type that exists only to hang the `"aether.embedded"` literal (via `Addressable`) and the fold (via `Instanced::resolve`) somewhere callable;
- `#[derive(Embeddable)]`, which emits a `Singleton::resolve` override on each component delegating to `resolve_embedded`;
- the same composition re-derived at three sites (`resolve_embedded`, the derive's override, the trampoline scope), kept in agreement by a comment and one guard test rather than a shared definition.

Two structural facts make this resist a clean fix:

- **An embedded actor is singleton-shaped at the call site but instanced-shaped at the target.** A peer reaches it with `ctx.actor::<Camera>()` — no subname, because the subname *is* the component's own `NAMESPACE` — yet it lands on an instanced node under `aether.embedded`. It is neither a plain `Singleton` (wrong fold) nor a plain `Instanced` (no caller-supplied key).
- **The behavior cannot be shared through a subtrait.** Expressing "embedded" as `EmbeddedHost: Singleton` and having it supply the embedding `resolve` does not compile: a subtrait cannot provide or override a supertrait's method body (`E0046`). The defaulted `resolve` lives wherever the method is *declared*, and the send surface is uniformly bounded `R: Singleton`, so an embedded actor must present a singleton-shaped interface to be sendable-to at all.

So embedding is a third resolution behavior that shares the singleton call shape, and there is no place on the current trait surface to put it without a per-type method override and a phantom type to anchor the formula.

## Decision

Move resolution onto `Addressable` as a **selected strategy**, not a per-cardinality method. An actor declares one associated type — its `Resolver` — and cardinality is implied by that resolver's argument shape rather than declared separately.

```rust
/// A resolution strategy. `Args` is what addressing requires: `()` for a
/// keyless target, a borrowed key for a keyed one.
pub trait Resolve {
    type Args<'a>;
    fn resolve(caller_carry: u64, namespace: &str, args: Self::Args<'_>) -> MailboxId;
}

pub trait Addressable {
    const NAMESPACE: &'static str;
    type Resolver: Resolve;                         // the only addressing choice an actor makes
    fn resolve(carry: u64, args: <Self::Resolver as Resolve>::Args<'_>) -> MailboxId {
        Self::Resolver::resolve(carry, Self::NAMESPACE, args)   // declared once, delegates
    }
}
```

The strategies are ordinary types implementing `Resolve`:

- `Single` — root-pinned, `Args = ()`. Chassis caps.
- `Many` — folds a caller-supplied `subname` onto the carry, `Args = &'a str`. ADR-0079 instanced actors.
- `Embedded` — folds the actor's own `NAMESPACE` as an instance under `EMBEDDED_SCOPE` onto the component host's carry, `Args = ()`. Loaded wasm components. Defined in `aether-capabilities`, because the host carry is a capabilities fact.
- `EmbeddedInstanced` — the keyed embedded variant for `spawn_sibling` children (ADR-0097), `Args = &'a str`.

**Cardinality is derived, never declared.** `Singleton` / `Instanced` become marker traits auto-implemented from the resolver's argument shape, with the constraint in **supertrait position** so it elaborates to call sites:

```rust
pub trait Singleton: Addressable<Resolver: for<'a> Resolve<Args<'a> = ()>> {}
impl<T: Addressable<Resolver: for<'a> Resolve<Args<'a> = ()>>> Singleton for T {}

pub trait Instanced: Addressable<Resolver: for<'a> Resolve<Args<'a> = &'a str>> {}
impl<T: Addressable<Resolver: for<'a> Resolve<Args<'a> = &'a str>>> Instanced for T {}
```

The call surface keeps speaking cardinality — `ctx.actor::<R: Singleton>()`, `ctx.resolve_actor::<R: Instanced>(key)`, and the send helpers' `R: Singleton + HandlesKind<K>` — unchanged. A keyless resolver (`Single` or `Embedded`) makes its actor a `Singleton`; a keyed one (`Many` or `EmbeddedInstanced`) makes it `Instanced`. Nobody writes `impl Singleton`.

**This dissolves the embedded special case.** `EmbeddedHost` is deleted: its literal becomes a single `EMBEDDED_SCOPE` const, its fold becomes `Embedded::resolve`. `#[derive(Embeddable)]` is retired: a component declares `type Resolver = Embedded`, which the `#[actor]` macro emits for the FFI path (with `EmbeddedInstanced` for sibling-spawned children). `#[bridge(singleton)]` / `#[bridge(instanced)]` map to `type Resolver = Single` / `Many`. Because variation lives in *which resolver type is selected* and never in overriding a method, the `E0046` wall does not apply, and a higher crate (`aether-capabilities`) can supply a resolver (`Embedded`) the core never anticipated.

### Mechanism facts (compile-verified on stable, edition 2024)

The shape rests on four facts that were checked directly rather than assumed:

1. An overriding trait **can** pin a supertrait's associated **type** (`Addressable<Resolver = …>`), even though it cannot re-default a supertrait's **method** (`E0046`). Types are pinnable where methods are not.
2. An associated-type equality (`Args = ()`) parked on a marker's **`where`** clause does not elaborate to consumers (`E0308` at the call site). In **supertrait position** it does — the same pattern already used for the `for<'a> Lifecycle<InitCtx<'a> = …>` pin on `FfiActor`.
3. The supertrait pin holds when combined with `HandlesKind<K>` across the send surface and with the GAT lifetime on the keyed path: a `R: Singleton + HandlesKind<K>` body calls `R::resolve(carry, ())` and a `resolve_actor::<R>(key: &'k str)` plumbs `Args<'k> = &'k str` to the returned mailbox, neither requiring the bound to be restated.
4. The boundary is type-safe in both directions: keyless-addressing an `Instanced` actor and keyed-addressing a `Singleton` are each rejected at compile time.

## Consequences

### Positive

- One uniform resolution path. An actor's entire addressing contract is a single associated type plus the inherited `resolve`; the actual address is produced by the type with no host round-trip, as ADR-0099 requires.
- Embedding falls out of the general mechanism instead of being a bolted-on special case. The `EmbeddedHost` phantom and the `#[derive(Embeddable)]` opt-in both disappear, and the embedding fold + `EMBEDDED_SCOPE` live in exactly one place that every site calls — the three re-derivations collapse to one definition.
- Every FFI actor is uniformly addressable by type: `#[actor]` emits `type Resolver = Embedded`, so `ctx.actor::<Camera>()` reaches the host-folded mailbox with no per-component ceremony.
- The `Resolver`-as-associated-type seam lets a higher crate define a strategy the core does not know about, which is what makes the capabilities-layer `Embedded` legal without leaking `ComponentHostCapability` into `aether-actor`.

### Negative

- The keyed path carries a GAT (`Resolve::Args<'a>`), and `Addressable::resolve` carries a lifetime. The keyless side (`Single` / `Embedded`) never sees it; the cost is confined to the `Instanced` definition and `resolve`'s signature.
- A mis-cardinality call surfaces as `E0271` (`<Many as Resolve>::Args<'a> == ()`) rather than a plain "not a `Singleton`." It trails with `required for … to implement Singleton`, so it is diagnosable, but the lead line is cryptic — this warrants a doc note where the markers are defined.
- The `Embedded` resolver lives in `aether-capabilities`, so the strategy set is split across crates. This is a cleaner split than the present one (where the `EmbeddedHost` anchor sits in `aether-actor` while the carry already comes from capabilities), but it is still a split.

### Neutral / follow-on

- This amends ADR-0099 §5/§6: embedded resolution is now a selected resolver rather than a delegation through a reserved host type, and the reserved scope is owned by a const. It relates to ADR-0079: cardinality is now *derived from* the resolver, not declared as a marker.
- The implementation arc supersedes the minimal framing of #2056 (which scoped only naming `embed()` / `EMBEDDED_SCOPE`). It spans `aether-actor`, `aether-capabilities`, the `#[actor]` / `#[bridge]` macros, and the `ctx` / send surface.
- The `EmbeddedInstanced` resolver for `spawn_sibling` children (ADR-0097) is the keyed counterpart of `Embedded`; only the keyless case was spiked, so its declaration mechanism on `#[actor]` should be confirmed during implementation.
- No wire-format change. The resolved `MailboxId`s are byte-identical to today's by construction; the existing equal-by-construction guard test is the regression check.

## Alternatives considered

- **Keep `resolve` on the cardinality traits; only name `embed()` / `EMBEDDED_SCOPE`** (the minimal #2056). Rejected: it tidies the literal duplication but leaves embedding as a per-type `Singleton::resolve` override plus the `EmbeddedHost` phantom — the special case survives.
- **`EmbeddedHost` as a category trait (`EmbeddedHost: Singleton`).** Rejected: `E0046` — a subtrait cannot supply the supertrait's `resolve` body, so members would still hand-write it, and the send surface's `R: Singleton` bound forces a singleton-shaped interface regardless.
- **`type Key` on `Addressable` to collapse the two cardinality `resolve`s into one method.** Rejected: the root-pinned and fold bodies genuinely differ, and one method gets one default — so this forces per-impl boilerplate or re-splitting. Verified not to compile when a subtrait attempts to default it.
- **`type Resolver` with the cardinality constraint on a `where` clause rather than supertrait position.** Rejected: the associated-type equality does not elaborate from a `where`-clause marker to call sites (verified `E0308`), forcing the full bound to be restated at every consumer.
- **Naming the associated type `Cardinality`.** Rejected: `Embedded` is a resolver, not a cardinality — it shares the singleton cardinality with `Single`, differing only in its fold. Naming the slot for an implication mis-files its contents.
- **A separate field/flag to mark embedded actors.** Rejected: embedding is already signalled structurally by the `aether.embedded:` segment in the lineage; a field encodes the same fact twice and lets the two drift.

## Related

- ADR-0099 — Actor identity and addressing. Amended here (§5/§6): resolution is a selected resolver; the reserved scope is a const.
- ADR-0079 — Instanced actors as a first-class category. Cardinality is now derived from the resolver rather than declared.
- ADR-0097 — Sibling spawn. Source of the keyed `EmbeddedInstanced` resolver.
- ADR-0098 — Scoped singletons (already superseded by ADR-0099).
- #2056 — superseded framing (named `embed()` only); reframed as the implementation arc of this ADR.
- #2048 / #2053 — the `Addressable` / `Lifecycle` split this builds on.
