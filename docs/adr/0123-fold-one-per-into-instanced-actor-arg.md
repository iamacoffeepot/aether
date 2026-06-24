# ADR-0123: Fold one_per Cardinality Into the Instanced Actor Arg

- **Status:** Proposed
- **Date:** 2026-06-24

## Context

The `#[actor]` / `#[bridge]` derive macros (`aether-actor-derive`) carry two
cardinality-related arguments on an instanced capability:

- `singleton` / `instanced` selects the `Addressable::Resolver` — the
  load-bearing compile-time addressing contract (`One` keyless vs `Many`
  keyed-by-subname; the `ctx.actor::<Cap>()` arity follows from it, ADR-0119).
- `one_per = "entity"` (ADR-0088 §4 v2) supplies the name-inventory
  `Cardinality::OnePer(entity)` annotation that makes the served reverse-lookup
  manifest self-describing ("one mailbox per loaded component") instead of an
  opaque `Unbounded` family. It feeds only `crates/aether-data/src/name_inventory.rs`'s
  `Cardinality`, read by `aether-capabilities/src/inventory/manifest.rs`
  (→ `CardinalityWire`) and the MCP reverse-lookup surface. It plays no role in
  routing or addressing — an instanced cap with no `one_per` resolves and
  dispatches identically, just reporting `Unbounded`.

At the data-model level these are genuinely orthogonal axes: `ParamKind` (the
*shape* of the name hole) versus `Cardinality` (the *how-many*). But at the
macro **argument** surface they are not independent. `one_per` is a strict
dependent of `instanced`: writing `one_per` on a `singleton` (or a bare
`#[actor]`) is meaningless, so both `parse`-paths defer a cross-argument guard
into the expander (`expand_native_actor_trait` / `expand_bridge`) that rejects
`one_per`-without-`instanced` — the arg order is unspecified, so the check
cannot live in the parser. Every production instanced actor on `origin/main`
(`engine/proxy`, `tcp/listener`, `tcp/session`, `trampoline`) carries
`one_per`; bare `#[actor(instanced)]` (⇒ `Unbounded`) appears only in test
fixtures and the hand-written `aether-instanced-{full_name}` scheduler
thread-name template.

The question (raised during the bridge-cap migration sweep, #2319–#2334): the
sibling `one_per` arg encodes an invalid state — `one_per` without
`instanced` — that exists only to be rejected at expansion time. Can the macro
surface collapse to one cardinality concept?

## Decision

Fold the cardinality entity onto the `instanced` argument as an optional value:

```rust
#[actor(instanced)]              // Cardinality::Unbounded
#[actor(instanced = "component")] // Cardinality::OnePer("component")
```

`one_per` is removed as a standalone `#[actor]` / `#[bridge]` argument. Bare
`instanced` continues to mean `Unbounded` (the genuine cardinality of the
scheduler thread-name family and the test fixtures); `instanced = "entity"`
means `OnePer(entity)`. `singleton` remains a valueless flag — a value on
`singleton` is a hard parse error (cardinality on a keyless resolver is
nonsensical).

This makes `one_per`-without-`instanced` **structurally unrepresentable**: the
entity is syntactically a payload of `instanced`, so it cannot appear without
it. The two deferred cross-argument guards (one in each expander) and their
compile-fail UI test are deleted; the only remaining validation is "no value on
`singleton`", which the parser checks directly.

## Consequences

- **One cardinality concept on the surface.** A cap author reads and writes a
  single `instanced[= "entity"]` arg; the manifest's `OnePer` / `Unbounded`
  distinction is expressed by the presence or absence of the value, mirroring
  how the underlying `Cardinality` already distinguishes them.
- **A whole class of invalid input disappears by construction.** The
  `one_per`-requires-`instanced` guards in `expand_native_actor_trait` and
  `expand_bridge`, plus the `rejects_actor_one_per_without_instanced` UI test,
  are removed — the invalid state can no longer be typed.
- **Breaking change to the derive arg surface.** Every `#[actor(instanced,
  one_per = "x")]` / `#[bridge(instanced, one_per = "x")]` site migrates to
  `#[actor(instanced = "x")]`. The four production sites plus the macro's own
  UI fixtures and ADR-0088's example are updated in the same change. The
  `aether.inventory.cardinality` wire reply and `Cardinality` vocabulary are
  unchanged — this is purely the *declaration* surface.
- **Refines ADR-0088 §4 v2.** The cardinality data model (orthogonal
  `ParamKind` × `Cardinality` axes, the wire mirror, the reverse-map builder
  reading only `ParamKind`) is untouched; only the macro-site spelling of the
  `Cardinality` annotation changes. ADR-0088's `#[bridge(instanced, one_per =
  "component")]` example is updated to the folded form.
- **Slight overload of the `instanced` value.** A reader could momentarily read
  the `"component"` in `instanced = "component"` as routing-relevant when it is
  pure manifest metadata. Mitigated by the macro doc-comment; the `singleton` /
  `instanced` flags already do not telegraph "resolver selection" to a naive
  reader, so this is consistent with the existing surface.

## Alternatives considered

- **Keep `one_per` as a separate arg (status quo).** Preserves a clean
  syntactic separation between the load-bearing resolver-selecting flag and the
  pure-metadata annotation. Rejected: the separation is illusory at the arg
  surface — `one_per` can only ever legally appear with `instanced`, so a
  sibling arg guarded by a runtime check is a less honest encoding than an
  optional value on the parent it depends on, and it keeps an invalid state
  representable.
- **Mandate an entity on every `instanced` (drop `Unbounded` from the macro).**
  Would make "every instanced family is one per something" literally true at
  the macro. Rejected: `Unbounded` is a real, used cardinality (the
  `aether-instanced-{full_name}` scheduler thread-name family and test
  fixtures), so the macro must still emit it — bare `instanced` ⇒ `Unbounded`
  is required.
