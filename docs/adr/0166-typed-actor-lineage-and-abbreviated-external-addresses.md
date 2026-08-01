# ADR-0166: Typed actor lineage and abbreviated external addresses

- **Status:** Accepted
- **Date:** 2026-07-24
- **Accepted:** 2026-07-24
- **Last amended:** 2026-07-31

## Context

ADR-0099 made an actor's canonical address the rendering of its runtime
lineage and made `MailboxId` the fold of that lineage. ADR-0119 made an
actor's ZST identity select its resolver, so ordinary Rust callers can resolve
mailboxes through types without a registry lookup. Those decisions establish
the identity mechanism, but the code still does not express which placements
are legal:

```rust
ctx.spawn_child::<AnyInstancedActor>(...);
```

Every `Instanced + NativeActor` currently satisfies the native spawn surface,
regardless of whether it is meaningful beneath the calling actor. The wasm
sibling surface has the same shape. The post-init contexts also erase the
current logical actor type, so the compiler cannot recover the parent from the
context alone.

Relative resolution has the same missing fact. A mailbox that already names a
parent can manually fold an arbitrary scope and discriminator:

```rust
parent.resolve_peer_scoped::<Recipient>(scope, name)
```

The component host uses that escape hatch to reach the `WasmTrampoline` that
hosts a loaded component. It reads the trampoline namespace from its owner,
which avoids a copied literal, but neither the type system nor the macro says
that the trampoline is a permitted child of the component host.

String-addressed boundaries have the opposite problem. MCP operations,
`NamedMail`, harness calls, configuration, and future declarative composition
need to name a mailbox, but the canonical rendering is intentionally
exhaustive:

```text
aether.component/aether.embedded:camera
```

That spelling is useful for durable identity and diagnosis but cumbersome for
an operator who already chose the component root and only wants the `camera`
instance. ADR-0099 explicitly permits alternate display spellings because the
string is not the identity, but there is no shared abbreviation mechanism
today. Teaching MCP or each capability a separate shortened root name would
duplicate namespace ownership and let clients disagree.

The window arc is the forcing future consumer: a root window manager should
spawn and resolve per-window actors, and different managers must be able to
host the same window actor type without flattening their lineages. The
component host and trampoline are the smaller first consumer because they
already form a real root-to-instanced-child lineage on both the spawn and
sender sides.

Constraints:

- Rust callers continue to resolve through actor ZSTs. External abbreviations
  do not become an SDK routing language.
- `Addressable::NAMESPACE` remains declared only by its owning actor. Neither
  relationships nor abbreviations repeat namespace strings.
- An actor may be allowed beneath more than one parent. A relationship is a
  placement permission, not one global topology or a claim that the placement
  is always live.
- Canonical lineage, `MailboxId`, registration names, and reverse lookup remain
  authoritative. An abbreviated address must expand to a canonical lineage
  before hashing or lookup.
- Shared logical identities remain valid. Runtime variants with the same
  namespace and future embedding mechanisms sharing the embedding-host class
  are one address node, not a child ambiguity.

## Decision

### 1. Actor types declare root and child placement permissions

Add two identity-level marker traits beside `Addressable`:

```rust
pub trait Root: Addressable {}

pub trait ChildOf<P: Addressable>: Addressable {}
```

`Root` means the actor may be composed or spawned without an actor parent.
`ChildOf<P>` means the actor may be spawned and resolved directly beneath a
mailbox of logical actor type `P`. Neither trait means that an instance is
currently live.

The supported declaration surface is the existing actor macro:

```rust
#[actor(singleton, root)]
pub struct ComponentHostCapability;

#[actor(
    instanced,
    child_of(ComponentHostCapability),
)]
pub struct WasmTrampoline;
```

The namespace remains where it is today: the `NAMESPACE` const lifted from the
actor's runtime or wasm actor implementation. `root` and `child_of(...)` name
types, never namespace strings.

`child_of(...)` may be repeated. The macro emits one trait implementation per
allowed parent:

```rust
impl ChildOf<FirstManager> for WindowActor {}
impl ChildOf<SecondManager> for WindowActor {}
```

An actor may also declare `root` and one or more parent permissions when both
placements are meaningful. This is why `ChildOf` means "may be a child of,"
not "must only be a child of." The APIs that perform a placement retain their
existing cardinality and resolver bounds; the first implementation admits the
current root-singleton and instanced-child cases rather than inventing a
keyless native-child resolver inside this decision.

`root` names two properties, and they are carried by two artifacts rather than
one. **Placement permission** — this identity may exist with no actor parent —
is the `Root` implementation, which the chassis spawn surfaces read as a bound;
every `root` declaration gets it. **Anchoring** — this namespace may stand
before `://` in an abbreviated address — is the `RootEntry` record §4 emits,
which the boundary resolver reads at link time. Only a singleton root anchors:
an instanced namespace identifies no single actor, so §5 has nothing for it to
resolve to. An instanced `#[actor(instanced, root)]` therefore takes the
placement and submits no record.

The anchor claim follows from cardinality rather than from a second keyword
because the mapping is total — a singleton root always anchors, an instanced
one never can — so a separate declaration could only restate what cardinality
already decides, or claim something unsatisfiable. Splitting the artifacts and
not the keyword keeps a coherent shape reachable: a family of parentless
instanced siblings, addressed by index, is placeable without making an
addressing claim it cannot honour.

The relationship is declared with the child type. That makes an implementation
legal when the child is local even if the parent comes from another crate, and
prevents a downstream crate from redefining the placement semantics of two
foreign actor identities.

### 2. Typed Rust resolution chains from an existing mailbox

Native and wasm actor mailboxes gain the same child-resolution method:

```rust
impl<'a, P: Addressable> ActorMailbox<'a, P> {
    pub fn resolve<C>(&self, name: &str) -> ActorMailbox<'a, C>
    where
        C: ChildOf<P> + Instanced,
    {
        ActorMailbox::at_in_flight(
            C::resolve(self.mailbox_id().0, name),
            self.transport_and_causal_context(),
        )
    }
}
```

The concrete native and wasm implementations preserve their existing
transport binding, sender, and in-flight causal context. The sketch above is
the shared contract, not a new transport-erasing mailbox type.

A caller writes one child type and one runtime discriminator:

```rust
let trampoline = ctx
    .actor::<ComponentHostCapability>()
    .resolve::<WasmTrampoline>("camera");
```

Each step is compile-checked by `ChildOf` and delegates address construction to
the child's existing `Addressable::Resolver`. It does not build a string,
query the registry, validate liveness, or accept an arbitrary scope.

Longer lineages compose by chaining:

```rust
let panel = ctx
    .actor::<ComponentManager>()
    .resolve::<ComponentActor>("editor")
    .resolve::<PanelActor>("inspector");
```

No tuple resolver, route object, `instance_key!`, or variadic generic is added.
The mailbox already carries the parent lineage fold, so it is the natural
typed cursor.

### 3. Spawn enforces the same permissions

Top-level composition and spawn surfaces require `A: Root`. Child spawn
surfaces require `C: ChildOf<P>` in addition to their existing runtime and
cardinality bounds.

Current post-init contexts do not carry `P` in their Rust type. Rather than
making every handler signature generic over its actor identity, the initial
API names both identities:

```rust
ctx.spawn_child::<ComponentHostCapability, WasmTrampoline>(
    Subname::Named(name),
    config,
    params,
)
```

The type check proves `WasmTrampoline:
ChildOf<ComponentHostCapability>`. The context also carries the current
logical actor-type tag at runtime and rejects a mismatched `P`. Runtime
variants that share a namespace share that logical tag, so desktop/headless
variants of one capability do not become different parents.

Wasm sibling spawn applies the same rule to exported actor identities. Dynamic
by-tag spawn validates against the relationship metadata described below
instead of bypassing the typed rule.

**Reusable Wasm module children.** Every Wasm actor is physically embedded
behind the embedding-host class through its `Embedded` or `EmbeddedMany`
resolver. That physical address rule, including its `aether.embedded` segment,
is distinct from permission to be spawned beneath another logical Wasm actor.
Exact guest relationships use `ChildOf<P>` normally. A reusable actor library
may instead declare `#[actor(composable)]`, which implements `ModuleChild` and
supplies `ChildOf<P>` for every `P: WasmActor`. The permission is confined to
actors executing from one resident module; it is not native placement,
cross-module loading, ownership, supervision, liveness, or security
isolation. This is an application-correctness contract inside one resident
module, not hostile isolation inside a shared Store.

`composable` is valid only for an instanced Wasm actor and is mutually
exclusive with explicit `child_of(...)` declarations on that actor. The Wasm
lineage section records
`ActorLineageRecord::ModuleChild { child, child_namespace }` separately from
exact `Child` edges and bumps `ACTOR_LINEAGE_SECTION_VERSION`. Typed spawn
retains the `C: ChildOf<P> + Instanced` bound and runtime parent-tag check.
Dynamic by-tag spawn validates exported membership, generated instanced
cardinality, and the generated exact-or-module-child fact before allocating an
alias or staging a detached spawn.

Placement enforcement is **guest-side and terminal**, not a host load gate.
Every check above reads the generated tables inside the resident module — the
`ChildOf<P>` bound at compile time, the exported-membership and
module-child facts at by-tag spawn. That is the correct locus for the contract
this section states: an application-correctness rule confined to actors
executing from one resident module, explicitly not hostile isolation. A host
reader that re-validated the same facts would duplicate a check the guest
already cannot evade by accident, and would gain nothing against a guest that
is not trusted in the first place — the threat model this decision disclaims.

The `aether.actor.lineage` custom section is therefore **reserved**: written by
`export!`, read by no host today, and carrying its version byte so a future
host-side *reader* — the discovery and autocomplete surface §5 contemplates,
not a permission gate — can version-check it. The substrate's manifest reader
knows `aether.kinds`, `aether.kinds.labels`, `aether.namespace`,
`aether.no_default`, and `aether.boot`; that list is complete, and a module
whose lineage section is absent or unreadable loads normally.

`Addressable::NAMESPACE` remains the logical actor type key.
`aether.embedded` remains owned by the resolver and never becomes a prefix
inside actor namespace declarations. A loaded module entry is embedded, not an
actor-tree `Root`; `export!` membership and load selection control entry
placement independently. `ModuleChild` supplies no globally exact parent edge,
so only exact `Root` and `Child` records participate in external abbreviation.

### 4. The macro emits anonymous lineage metadata

`#[actor(root)]` and `#[actor(child_of(...))]` emit discoverable records
alongside their trait implementations. A `RootEntry` is the anchor claim §1
describes, so a singleton `root` emits one and an instanced `root` emits none
while still implementing the `Root` marker. Conceptually:

```rust
RootEntry {
    actor: ActorTypeTag::of::<ComponentHostCapability>(),
    namespace: ComponentHostCapability::NAMESPACE,
}

ChildEntry {
    parent: ActorTypeTag::of::<ComponentHostCapability>(),
    child: ActorTypeTag::of::<WasmTrampoline>(),
    parent_namespace: ComponentHostCapability::NAMESPACE,
    child_namespace: WasmTrampoline::NAMESPACE,
}
```

The exact native representation may extend the existing link-time name
inventory. Wasm declarations use equivalent component-manifest metadata when
the substrate must inspect them. Both are generated from the same macro
arguments and actor-owned constants.

These records are anonymous facts, not a named topology. There is no
`actor_routes!` block, route name, duplicated relationship table, or globally
selected parent. Multiple `ChildEntry` records naturally represent branching
and multiple allowed parents. Records with the same logical actor tags and
namespaces deduplicate, which preserves shared runtime variants.

### 5. External abbreviations retain the canonical root namespace

Abbreviated paths exist only at string-addressed boundaries. Rust actor code
continues to use ZSTs and typed mailboxes.

The external grammar is:

```text
address          := canonical-path | abbreviated-path
abbreviated-path := root-namespace "://" relative-path?
relative-path    := relative-segment ( "/" relative-segment )*
relative-segment := discriminator | canonical-segment
```

`root-namespace` is the exact `NAMESPACE` of a declared `Root`. The `://`
delimiter says that the remaining segments are relative to that canonical
root; it does not introduce a URL scheme or a second actor name:

```text
aether.component://camera
aether.window://main
```

The boundary resolver walks generated lineage records from that root:

- The prefix before `://` selects a root by its canonical namespace, so no
  root-alias derivation, registration, or uniqueness check exists.
- A canonical segment (`namespace` or `namespace:discriminator`) selects that
  declared child namespace explicitly.
- A bare discriminator may omit the child namespace only when exactly one
  logical instanced-child namespace is possible at that point.
- Several concrete child types sharing one logical namespace count as one
  choice because their canonical address node is identical.
- Several distinct child namespaces make the abbreviation ambiguous. The
  caller must provide the canonical child segment.
- Expansion is iterative and retains ADR-0099's path depth and byte limits.

A declaration the index cannot use excludes its own namespace and nothing
else. Two shapes reach this: a root whose namespace is instanced, which
identifies no single actor and so anchors nothing; and a namespace carrying a
placement fact without a matching cardinality fact, or carrying two that
contradict, whose elision behaviour is undefined. Both are reachable only
through a hand-written `inventory::submit!` — the macro emits placement and
cardinality together, and withholds the record entirely for an instanced root
(§1) — and both are per-namespace rather than fatal to the
index, because rejecting the whole index would disable abbreviated addressing
process-wide over one unrelated declaration and report a namespace the caller
was not addressing. An excluded root keeps its reason, so a `://` prefix
naming one is a structured boundary error distinguishing it from a root
nothing declares; an excluded child drops its own edge while its siblings
still resolve. A malformed namespace or an actor tag disagreeing with the
namespace it claims stays fatal: the fact cannot be trusted to name what it
says it names, so there is no offending namespace to exclude.

For the first consumer:

```text
aether.component://camera
    ->
aether.component/aether.embedded:camera
```

If the component host admitted two distinct child namespaces, the short form
would fail with candidates and the explicit form would remain valid:

```text
aether.component://aether.embedded:camera
```

Raw MCP, CLI, configuration, and manifest strings cannot fail Rust
compilation, so their ambiguity is a structured boundary error. The equivalent
Rust call names the child type and therefore fails at compile time when the
relationship is absent; naming the type removes the textual ambiguity:

```rust
host.resolve::<WasmTrampoline>("camera");
```

Abbreviation expansion happens once in the shared mailbox-name resolution
seam, before the existing canonical path validation,
`mailbox_id_from_path` fold, and exact registered-name check:

```rust
let canonical = addresses.expand(input)?;
let mailbox = registry.lookup_canonical(&canonical);
```

An abbreviated spelling is never registered, stored as the mailbox name,
reverse-mapped as the canonical identity, or hashed directly. Existing
canonical inputs remain valid. MCP and other clients do not carry their own
abbreviation tables; inventory may expose the generated root and child records
for discovery and autocomplete, while the engine remains the resolver of
record.

### 6. The component host and trampoline are the first consumer

The first production adoption declares:

```rust
ComponentHostCapability: Root
WasmTrampoline: ChildOf<ComponentHostCapability>
```

Component loading spawns the trampoline through the parent-checked spawn
surface. `ComponentHostWasmExt::loaded` and
`ComponentHostNativeExt::loaded` resolve the trampoline through the typed
mailbox edge and then expose the same physical mailbox under the loaded
guest's recipient type. `resolve_embedded` remains equal by construction.

Once no production caller needs it, `resolve_peer_scoped(scope, segment)` is
removed: accepting an arbitrary scope would preserve the unchecked escape
hatch this decision replaces.

Regression coverage proves all three spellings land on the same
`MailboxId`:

```text
typed host -> trampoline resolution
canonical external path
aether.component://camera
```

The window manager and per-window actors adopt the mechanism only after this
smaller root/child path is working.

## Consequences

### Positive

- Spawn and relative resolution use one declared relationship. A type that
  cannot legally sit under a parent fails before it can mint or register a
  mailbox.
- Multiple parents and branching are represented without fixing one topology
  in the child type or in a global route manifest.
- Rust retains static, lookup-free ZST addressing. Human and declarative
  clients gain concise addresses through one engine-owned boundary resolver.
- Namespace strings remain single-owner. Root qualification and child
  expansions read `Addressable::NAMESPACE`, never copied helper constants.
- Canonical lineage and `MailboxId` do not change, so abbreviation rules can
  evolve without a wire migration.
- The same model fits component hosts, per-window actors, component trees,
  session actors, and other nested capabilities.
- Reusable Wasm actor libraries can remain below their assembly crates:
  same-module composition no longer forces reverse dependencies, copied parent
  namespaces, or fake adapter identities.

### Negative

- The actor macro and inventories gain placement metadata, including an
  equivalent representation for wasm exports that must be visible to the
  substrate.
- Existing child spawns must name and declare their logical parent as they
  adopt enforcement. Because contexts erase that type today, spawn initially
  carries two type parameters plus a runtime parent-tag check.
- Textual abbreviations are resolved at runtime. An ambiguous MCP or config
  string returns an error rather than receiving Rust's compile-time
  diagnostic.
- The initial child-resolution surface covers the instanced children supported
  by current spawn APIs. A true keyless singleton beneath a native parent
  requires a relative singleton resolver and a deliberate extension.
- A `ModuleChild` deliberately permits more parents than an exact
  `ChildOf<P>` edge. It cannot contribute a globally exact external
  abbreviation edge, and actors needing restricted topology must use exact
  declarations.

### Neutral and follow-on

- `ChildOf<P>` is permission, not ownership, monitoring, lifetime, or
  liveness. Supervision remains a separate actor relationship.
- An actor may be both `Root` and `ChildOf<P>` when its resolver and placement
  APIs support both. This ADR does not impose "children can never run at the
  root."
- Canonical names remain the durable values returned by existing APIs.
  Surfaces may additionally display a preferred abbreviation, but must not
  replace canonical identity fields without a separate compatibility
  decision.
- The implementation arc lands core traits and macro metadata, typed
  resolution/spawn enforcement, the component consumer plus abbreviation
  expansion, and then the window manager/per-window consumer.

## Alternatives considered

- **A named `actor_routes!` topology.** Rejected because it restates
  relationships in a central manifest, selects parents too early, and creates
  a second place for actor namespaces to drift.
- **A shortened root alias such as `component://`.** Rejected because retaining
  the canonical root as `aether.component://` removes the alias declaration,
  derivation, uniqueness check, and rename synchronization.
- **Use `aether.component://...` inside Rust actor code.** Rejected because
  typed ZST resolution is already stronger, allocation-free, and
  compile-checked.
- **Resolve a tuple of actor types and instance keys in one call.** Rejected
  because an existing mailbox already carries the lineage fold; chained
  `resolve::<Child>(key)` calls compose naturally and produce better local
  errors.
- **Give every child exactly one associated parent type.** Rejected because
  the same actor may be hosted by different managers or embedding systems.
  Repeated `ChildOf<P>` permissions model that directly.
- **Make `ChildOf<P>` mean the actor can never be a root.** Rejected because
  placement permissions are orthogonal; `Root` independently controls
  top-level placement.
- **Infer abbreviations from only the currently live mailbox set.** Rejected
  because an abbreviation would change meaning as actors start and stop.
  Generated relationship facts make ambiguity stable; liveness remains the
  later registry check.
- **Hash or register the abbreviated string.** Rejected because it would
  create a second identity and contradict ADR-0099's canonical lineage fold.
- **Let downstream crates implement relationships between two foreign actor
  types.** Rejected by Rust's orphan rules and by the semantic concern: that
  would patch placement policy onto identities whose owners did not declare
  it. A local adapter identity is the explicit composition escape hatch.
- **Encode embedding or parent scope inside `NAMESPACE`.** Rejected because
  `NAMESPACE` is the logical actor type key while the resolver owns physical
  placement. Combining them changes type tags across placements, breaks
  multi-parent reuse, and duplicates `EMBEDDED_SCOPE`.
- **Create adapter parent identities in every lower-level child crate.**
  Rejected because the adapters exist only to invert Cargo dependencies,
  either duplicate or forward higher-level namespaces, and turn a reusable
  module permission into fictional exact topology.
- **Treat every Wasm actor as an unrestricted module child.** Rejected because
  physical embedding is universal but logical composition is not. Exact
  application actors should still fail to compile beneath undeclared parents.

## Related

- ADR-0079 — Instanced actors as a first-class category. Existing child spawn
  cardinality remains the first supported relationship shape.
- ADR-0099 — Actor identity and addressing. This ADR supplies the static
  placement marker anticipated by §2 and realizes §4's deferred display
  abbreviation without changing lineage identity.
- ADR-0119 — Actor addressing via a Resolver strategy. Typed child resolution
  delegates to the selected resolver rather than adding another address
  algorithm.
- ADR-0122 — Split actor identity from runtime state. `Root` and `ChildOf`
  belong to the always-on identity ZST, not runtime state.
- ADR-0164 — Window actor owns native window integration. The later
  manager/per-window adoption extends that window ownership decision.
