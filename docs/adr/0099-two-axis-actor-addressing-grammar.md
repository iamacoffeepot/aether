# ADR-0099: Two-axis actor addressing grammar

- **Status:** Proposed
- **Date:** 2026-06-07

## Context

Every actor has a mailbox name, and `MailboxId = hash(name)` (ADR-0029) — the name is the wire identity, resolved by hashing with no registry lookup. The name's grammar is therefore load-bearing: two spellings of the same logical address hash to different ids, so a sender that picks the wrong spelling reaches nothing.

That gap is live. A loaded component declares a `NAMESPACE` and a cardinality (ADR-0079), but lives under the component host's trampoline at `aether.component.trampoline:camera` (ADR-0096 §3). A peer that addresses it by bare type — `ctx.actor::<Camera>()` — hashes the bare `NAMESPACE`, not the hosted name, and the mail drops (iamacoffeepot/aether#1364). ADR-0098 corrected the contract in prose: a hosted singleton is reached through its scope. Prose is the weak form of the guarantee — nothing stops the wrong call from compiling.

The current name is a flat dotted string with a single `:` for the instance discriminator. The dots carry no structure: `aether.component.trampoline` gives no parseable answer to "where does the parent scope end and the child begin," so the tree a name encodes can't be recovered from the name. As the actor tree gains depth — multi-actor modules and sibling spawn (ADR-0096/0097), and per-scope structure beyond that — a flat string can't express nesting in one unambiguous form.

Two forces, then: the grammar must encode the actor tree losslessly and in exactly one canonical form, and addressing the wrong path should fail at compile time instead of warn-dropping at runtime.

Constraints carried in:

- **ADR-0029.** `MailboxId = hash(name)`; the name is the identity, ids are not registered or looked up.
- **ADR-0079.** `Singleton` / `Instanced` cardinality; `spawn_child`, `Subname`, retire-on-drop.
- **ADR-0096 / ADR-0097.** Loaded and spawned actors live flat at `aether.component.trampoline:<name>`; the spawn lineage (which actor spawned which) is tracked at runtime via `ReplyTo`, held outside the name.
- **ADR-0098.** The scoped-singleton model and the #1364 prose fix this makes structural.

## Decision

Four sub-decisions.

### 1. Three separators on two axes

A mailbox name is a scope path with two structural separators and one cosmetic one:

- **`.`** — cosmetic, within a single segment. `aether.render` and `aether.component.trampoline` are each one segment; the dots are part of the name and carry no structure.
- **`/`** — scope nesting, parent → child. Structural: it marks where one scope segment ends and its child begins, so the tree is recoverable from the string.
- **`:`** — cardinality. It seeds an `Instanced` actor's discriminator (ADR-0079) onto its segment, and marks the boundary between the static path and the runtime instance.

Grammar:

```
path     := segment ( "/" segment )*
segment  := atom ( ":" discriminator )?
atom     := ident ( "." ident )*
```

The two axes are orthogonal: `/` says *where in the tree*, `:` says *which instance*. The spawn lineage stays off the name — who spawned an actor is a runtime relationship held in `ReplyTo`, outside the address.

### 2. Segments keep full namespaces; one lossless canonical form

Each scope segment carries its full dotted namespace; nothing is stripped. The loaded camera is:

```
aether.component/aether.component.trampoline:camera
```

- `aether.component` — the root cap (the component host, a root `Singleton`)
- `/aether.component.trampoline` — the child scope, full namespace kept
- `:camera` — the instance discriminator (runtime)

The whole tree reconstructs from the string — root cap, child scope, instance — with exactly one spelling. There is no short form to disagree with the long one, which is the property #1364 needs: two spellings of one address can't hash apart when there is only one spelling.

The cost is verbosity — a child that shares its parent's root repeats it (`aether.component` appears twice above). Collapsing that redundancy (leaf-segment composition, where a child declares only its leaf and the full namespace composes) is a future ergonomic change, deferred precisely because a collapse rule that's unambiguous only when child and parent share a root would reintroduce the two-forms risk this decision exists to close. Verbosity is the acceptable trade for one canonical form.

### 3. Bare addressing compiles only for root singletons

`ctx.actor::<R>()` — type addressing with no scope — is available only when `R` is a root `Singleton` (a chassis capability: `aether.render`, `aether.component`, `aether.fs`). Root singletons are the one category whose address is unambiguous from the type alone, because there is exactly one and it lives at the root.

Every other actor is reached through a resolver that composes its scope into the canonical path — `host.loaded::<R>(name)` for a hosted component, a scope handle for anything nested. An `Instanced` actor, or a singleton hosted under a scope, has no bare-type address; `ctx.actor::<Camera>()` for a loaded component does not compile, where today it compiles and drops. The #1364 footgun becomes a type error.

This is ADR-0098's prose contract made structural: "a hosted actor is reached scope-relative" stops being a doc a caller can ignore and becomes a constraint the compiler holds.

### 4. Scoped singletons and a parent-constraint axis are reserved, not built

A singleton that lives per-scope as its own addressable segment — a non-root, non-instanced child — has no inhabitant in the current engine. Loaded and spawned actors are all `Instanced` under the trampoline, and root caps are the only singletons. This ADR names that future category `ScopedSingleton`, and names the mechanism that would make it safe — a parent-constraint declared on the type, with `*` (any parent) as the default so the common case declares nothing and nothing couples — but builds neither. They ship with their first real consumer, on this grammar, so the surface isn't designed twice.

## Consequences

### Positive

- **One canonical, lossless name per actor, enforced by the grammar.** The tree is recoverable from any name, and there is exactly one spelling — the property #1364 turned on.
- **The #1364 footgun is a compile error.** Bare-addressing a hosted actor doesn't compile; the only bare-addressable category is the one where bare addressing is unambiguous.
- **The tree is parseable.** `/` marks scope boundaries, so tooling (`actor_logs`, introspection) can split a name into its scope chain rather than guessing at dots.
- **The two axes extend cleanly.** Deeper nesting and intermediate instanced scopes (`a/b:7/c`) fall out of the grammar with no new separators.

### Negative

- **Wire break.** Every instanced or scoped `MailboxId` rehashes: the trampoline registration, `LoadResult.name`, `actor_logs` addressing, route caches, and the `loaded()` / `mailbox_id_from_name_pair` composition all move to the `/`-joined form. Root caps (no `/`, no `:`) are unchanged. Pre-1.0, with no external consumers, this is a contained one-time migration.
- **Verbosity.** A child repeats its parent's namespace root; the collapse that removes it is deferred (§2).
- **A type-level split.** Gating bare addressing on a root-singleton marker gives the cardinality categories a compile-time role beyond naming. Contained to the actor SDK.

### Neutral

- **`MailboxId = hash(name)` holds (ADR-0029).** Only the names change; the hashing and the no-lookup resolution are untouched.
- **Spawn lineage stays at runtime.** `/` encodes the static scope tree; who-spawned-whom stays in `ReplyTo`, as ADR-0097 has it.

### Follow-on

- **Implementation** is scoped on iamacoffeepot/aether#1420 and split into PRs (the grammar + re-spell, the structural enforcement, the migration of name-carrying surfaces).
- **Leaf-segment composition** — collapse the repeated root; a future ergonomic change on this grammar.
- **`ScopedSingleton` + the `*` parent-constraint axis** — built with the first per-scope singleton.

## Alternatives considered

- **Model + enforce now, re-spell later.** Define the grammar and land the structural enforcement, but keep the current flat name and defer the `/` re-spell. Rejected: it leaves the canonical-form guarantee documented-but-unreal for another cycle, and the enforcement is cleaner to land against the names it actually targets. Pre-1.0 is the cheap moment to break the wire.
- **Leaf-segment composition now** (`aether.component/trampoline:camera`). Rejected for now: shorter, but a child's full namespace is then recoverable only through the compose rule, which is ambiguous when child and parent roots differ — a second-form risk against the one-canonical-path invariant. It's the deferred ergonomic follow-on, not the foundation.
- **Encode spawn lineage in the name** (`rootmanager/panel:0`). Rejected: who-spawned-whom is a runtime relationship the engine already tracks via `ReplyTo`; putting it in the address would make a sibling's name depend on its spawner and break the flat `trampoline:<name>` model ADR-0096/0097 rely on.
- **Split `Singleton` into absolute / relative categories.** Rejected: the relative (per-scope) category has no inhabitant today, so the split adds vocabulary with nothing behind it. Reserved as `ScopedSingleton` (§4) for when one appears.

## Related

- ADR-0098 — scoped singletons; this revises its separator (the uniform `:` join becomes `/`-scope + `:`-cardinality) and makes its prose contract structural.
- ADR-0079 — instanced actors and cardinality; the `Singleton` / `Instanced` split this gates bare addressing on.
- ADR-0096 / ADR-0097 — multi-actor modules and sibling spawn; the flat `trampoline:<name>` addressing and runtime lineage this grammar composes over.
- ADR-0029 — `MailboxId = hash(name)`; the reason name grammar is load-bearing.
- iamacoffeepot/aether#1364 — the footgun this closes; iamacoffeepot/aether#1420 — the implementation.
