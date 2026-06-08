# ADR-0099: Actor identity and addressing

- **Status:** Proposed
- **Date:** 2026-06-07

## Context

`MailboxId = hash(name)` (ADR-0029): an actor's wire identity is the FNV-1a hash of its mailbox name, resolved client-side with no registry lookup. That one id answers two questions at once — *which actor is this* and *where does it sit in the tree* — and the two come apart as the tree gains depth.

The split is live in iamacoffeepot/aether#1364. A loaded component declares a `NAMESPACE` and a cardinality (ADR-0079) but runs under the component host's trampoline, registered at `aether.component.trampoline:camera` (ADR-0096 §3). A peer addressing it by bare type — `ctx.actor::<Camera>()` — hashes the bare `NAMESPACE` and reaches nothing, because the actor's id is the hash of its *hosted* name. The bare type names what the actor is; the hosted name also encodes where it lives; one flat hash cannot carry both, so the bare-type call lands on an empty id and warn-drops.

The same conflation caps how deep the tree can go. Multi-actor modules and sibling spawn (ADR-0096/0097) put actors under other actors, and per-scope structure — one settings actor per open document, one player-state per session — nests further. A flat `hash(name)` can encode a position only by baking the whole path into one string and hashing it, which discards the constituent identities: you cannot read back what sits at each level, and extending a path means rehashing the whole string.

The fix is to give an actor two identities — an **ActorId** for which actor it is, and a **MailboxId** for where it sits, derived from its lineage. The rest of this ADR defines them, how the lineage produces the second, and how a name renders both.

Constraints carried in:

- **ADR-0029.** `MailboxId` is a 64-bit FNV-1a hash, computed identically on substrate and guest, no registry lookup. Width, domain prefix, and no-lookup resolution stand; only the hash input changes for nested actors.
- **ADR-0096.** Each exported actor type has a stable actor-type tag, `mailbox_id_from_name(NAMESPACE)`, used at module init to select which type to instantiate.
- **ADR-0079.** `Singleton` / `Instanced` cardinality; an instanced actor's discriminator joins its `NAMESPACE` with the `:` separator.
- **ADR-0097.** Wasm sibling spawn — a running actor spawns one of its sibling types, today registered flat under the trampoline.
- **ADR-0064.** Every id carries a 4-bit type tag in its high nibble; `with_tag(Tag::Mailbox, _)` stamps it, overwriting any prior tag.

This ADR **supersedes ADR-0098** (scoped singletons), which framed the same #1364 gap and proposed a flat `{scope}:{segment}` name join with an open question about where the runtime scope name lives. The per-scope-singleton concept it introduced is carried forward here on the identity model; its addressing mechanism is replaced by the lineage fold below.

## Decision

Five sub-decisions.

### 1. Two identities: ActorId (which actor) and MailboxId (where in the tree)

An actor carries two ids, one per question the single `MailboxId` conflated.

**ActorId — which actor.** A `NAMESPACE` is universally unique, so its hash names the actor unambiguously across the whole binary. The ActorId names a node in isolation: binary-unique, reverse-mappable to the actor it identifies (via the name the registry retains, ADR-0029), and independent of where the node is hosted. Cardinality (ADR-0079) sets how it is computed:

- A **singleton** node's ActorId is its actor-type tag (ADR-0096), `mailbox_id_from_name(NAMESPACE)`.
- An **instanced** node's ActorId is `mailbox_id_from_name_pair(NAMESPACE, subname)` — `hash(NAMESPACE:subname)`, the same namespace with the runtime discriminator folded in by the `:` cardinality separator.

These are exactly the flat ids the engine computes today. Today's per-actor mailbox id, read as "which actor (which instance)," is the ActorId.

**MailboxId — where in the tree.** A function of the actor's whole lineage, not of its leaf node alone. Two actors of the same code under different parents are different mailboxes; two of the same code under the same parent differ by the discriminator in their ActorId. Mail routes to the MailboxId.

### 2. Lineage is a runtime array of ActorIds, carried and extended at spawn

An actor's lineage is the ordered list of ActorIds from the root down to the actor — one per node on its path. It is a runtime value: an actor receives its lineage when it is created, holds it, and extends it by one node when it spawns a child. Position is not encoded in the type.

The single static, type-level fact about an actor's **position** is whether it is **pinned to the root** or **may run as a child**. A root chassis capability (`aether.render`, `aether.fs`, the component host) exists once, at the root; a loaded or spawned actor always has a parent. That marker (iamacoffeepot/aether#1423) is the only thing about an actor's position the compiler holds; everything else about lineage is runtime data threaded through spawn. (How a peer *resolves* an actor — statically or through its host — is decided by the actor's type; §5.)

This expresses the per-scope cardinality ADR-0098 introduced on the identity model: a singleton is "exactly one under this parent," enforced because its lineage — and therefore its MailboxId — is unique. A second instance under the same parent folds to the same id and collides at registration (ADR-0029's collision guard). The substrate-global chassis cap is the depth-1 case of the same rule.

The runtime carries the lineage as a single rolling value rather than a growing array (§3), but the model is an array of ActorIds, root → leaf.

### 3. MailboxId is a hash chain over the lineage; root caps are the fixed point

The MailboxId is the chained hash of the lineage's ActorIds — each node folded onto the running hash of its ancestors:

```text
state = lineage[0].0                                    // root ActorId, verbatim
for node in lineage[1..]:
    state = fnv1a_64_fold(state, node.0.to_le_bytes())  // chain each ActorId onto the prior state
MailboxId = with_tag(Tag::Mailbox, state)
```

`fnv1a_64_fold` and `with_tag` are the existing `aether-data::hash` primitives; the fold reuses the same FNV-1a step the id helpers already share. This is a hash chain — each ActorId folded into the running hash of its ancestors, a hash of hashes — rather than a flat hash of the joined path string, and the distinction earns its keep twice:

- **The nodes stay recoverable.** The lineage is an array of ActorIds, each reverse-mapping to a name, so a path reads back to what sits at every level — a flat `hash("a/b:7/c")` throws that away.
- **The fold is incremental.** Extending a lineage is one more `fnv1a_64_fold` step on the running state, so an actor carries its lineage as a single `u64` — the fold state — and a spawn extends it in O(1): `child_state = fnv1a_64_fold(parent_state, child_actor_id.0.to_le_bytes())`. That `u64` is the rolling carry on the actor's runtime binding. "Carry your lineage, pass it forward" is one integer, extended one step per spawn — no growing string, no trait value, no per-spawn rehash of a path.

**Root caps are the depth-1 fixed point.** A lineage of one node folds to that node verbatim: the loop never runs, so `MailboxId = with_tag(Tag::Mailbox, lineage[0].0)`. Because `with_tag` overwrites the tag nibble, re-tagging an already-`Mailbox`-tagged value is identity, and an ActorId is already `Mailbox`-tagged — so `MailboxId == ActorId == hash(NAMESPACE)`, the id the cap has today. Only actors at depth ≥ 2 — everything loaded, spawned, or nested, each now carrying a real parent — get a new id. The root vocabulary every chassis and component already targets is frozen; the wire break is confined to the hosted layer.

Lineage depth and rendered-path length stay bounded by the existing `MAX_SCOPE_PATH_DEPTH` (8 nodes) and `MAX_SCOPE_PATH_BYTES` (4096) caps (`validate_scope_path`), so a runaway spawn chain is rejected rather than folded into an unbounded key.

### 4. The string path is a display rendering, not the identity

The canonical identity is the lineage array and the MailboxId it folds to. The dotted-and-slashed string renders that lineage for humans, the CLI, and tools.

```text
path     := segment ( "/" segment )*
segment  := atom ( ":" discriminator )?
atom     := ident ( "." ident )*
```

- **`/`** separates nodes — one segment per ActorId in the lineage, root → leaf.
- **`:`** carries an instanced node's discriminator; the segment around it names that node's ActorId, `hash(NAMESPACE:subname)`.
- **`.`** is cosmetic, within a single namespace ident — `aether.component.trampoline` is one segment.

The loaded camera renders as `aether.component/aether.component.trampoline:camera`: root host, child trampoline scope, instance. Because the string is a rendering, a MailboxId is never computed by hashing it — a written path is resolved by parsing it into segments, mapping each segment to its ActorId, and chain-folding (§3). Type addressing never touches the string at all: `ctx.actor::<R>()` resolves R to its id by R's resolution mode (§5), not by the rendered path. The parse is the cold path, paid only by string-addressed callers — MCP, the CLI, `actor_logs`.

Display spellings can therefore vary — collapsing a repeated root, abbreviating a namespace — without touching the hash, because the lineage array is the single source of truth and the string never feeds resolution. There is no second authoritative form to disagree with the first.

#1364 closes on this model, by way of §5. An embeddable component is resolved through its host class: `ctx.actor::<Camera>()` reaches the hosted mailbox because Camera's type folds its own name under the embedding-host class, rather than hashing the bare `NAMESPACE` and missing. The footgun ADR-0098 patched in prose is gone because the type carries how it must be resolved.

### 5. Resolution is static; an embeddable actor resolves through its host class

A peer resolves another actor's MailboxId from the peer's *type*, not the caller. The call site is uniform — `ctx.actor::<R>()` asks for R's identifier; how it is found is R's concern — and it is **static in every case**: the id is computed from compile-time identities, never looked up and never round-tripped.

Two strategies, selected by R's category:

- **Reconstructible from constants.** A root-pinned cap resolves to its ActorId (depth-1, §3); an actor that is the caller's own child folds its ActorId onto the caller's lineage carry. Every actor whose position the caller can reconstruct from what it already holds.
- **Embedded through its host class.** An embeddable actor — an FFI/wasm component — carries no position of its own; it runs inside an embedding host. It resolves by folding its own `NAMESPACE` as an instance under the reserved embedding-host namespace `aether.embedded`, onto the embedding host cap's carry (the depth-1 `aether.component` host today): `with_tag(Mailbox, fold(host_carry, ActorId(aether.embedded:<name>)))`. R names only its own `NAMESPACE`; the host class supplies the rest. Each node's namespace is read only inside the actor that owns it — R reads `R::NAMESPACE`, the host class reads `aether.embedded` — so no actor's name is written anywhere but on that actor.

The caller does not choose; R's type does. A plain type resolves from constants; an embeddable type resolves through its host class. Resolve the name — the type decides how.

**The embedding host is a class, not a single type.** One reserved namespace `aether.embedded` is shared by every concrete embedding host — `WasmTrampoline` today, a `DylibTrampoline` (a native dynamic library loaded with `libloading`) tomorrow — forward-fed into each, declared on none. An embeddable actor's id therefore depends on its own name and the host *class*, never on the embedding mechanism: the same logical actor delivered as wasm or as a dynamic library resolves to the same MailboxId. Identity is what the code is, not how it is hosted.

**No lookup, and no need for one.** Embeddability is intrinsic and the host class is universal: an FFI actor exists only embedded in a host, so its position is always "an instance, by its own name, under the embedding-host class." The caller never has to discover where it sits — the class fixes it, so a peer (native or wasm) computes the identical id with no transport branch, and ADR-0029's client-side no-lookup holds. The one value still handed over is the component's own id, which the host computes at spawn and passes into the guest's `init` — a component loaded under a non-default name cannot otherwise know its own name. A non-default name is supplied by the caller: `loaded::<R>("cam2")` folds the same lineage with the given name.

**Liveness is separate from address.** `ctx.actor::<R>()` resolves to where R *would* sit under the host class; if no instance is loaded at that name the mail drops on delivery, as for any address whose actor is not up. The id is always right; whether the actor exists is a runtime fact, handled by delivery, not resolution. The wrong-*address* footgun (#1364) is gone — the fold lands on the registered id.

The native actor graph is the whole graph: an embedding host is a native actor, the FFI module is the payload embedded into it, and resolution composes native ActorIds without inspecting the embed format — the format only selects which host class an actor belongs to. #1364 closes.

### 6. The embedding-host namespace is shared across host types, unique per instance

`aether.embedded` is claimed by more than one type — every concrete embedding host — which a per-namespace uniqueness check reads as a collision. It is not. The hosts are instanced (one per embedded component) and partition the name space by the embedded actor's own name: `aether.embedded:camera` and `aether.embedded:widget` are distinct instances, and two host *types* registering under the shared class namespace collide only if they would instantiate the same logical actor — the intended single identity, not a conflict.

This is a second kind of namespace sharing. The chassis render caps share `aether.render` across **mutually exclusive** variants — exactly one is composed, a double-claim is a build error. Embedding hosts instead **coexist**: a substrate may host wasm and dynamic-library components at once, so several host types legitimately claim `aether.embedded` together. Uniqueness for this class is per-instance (per subname), enforced at registration, not per-type at composition. The reserved namespace is shared by design.

## Consequences

### Positive

- **Two ids, each answering one question.** ActorId reverse-maps to the actor (introspection, `actor_logs`, "which actor is this"); MailboxId routes mail and encodes position. #1364 closes because where-in-the-tree is no longer crammed into which-actor.
- **The root vocabulary is frozen.** Depth-1 identity keeps every chassis cap's id exactly as today; the wire break touches only the hosted layer.
- **The tree is recoverable and parseable.** A path reads back to per-level ActorIds, each reverse-mappable to a name, so tooling splits a lineage into named nodes instead of guessing at separators.
- **Lineage is O(1) to carry and extend.** One `u64`, one fold step per spawn — no growing path string, no per-spawn rehash.
- **Display is free to evolve.** The string renders the lineage; collapse and abbreviation rules can change without touching identity.
- **The "exactly one" guarantee survives nesting.** A per-scope singleton (one player-state per session) is enforced by the same id-collision check as a substrate-global cap, with no new mechanism — ADR-0098's scoped-singleton goal carried onto the fold.

### Negative

- **Wire break at depth ≥ 2.** Every hosted, spawned, or nested MailboxId moves from `hash(name)` to the lineage fold: trampoline registration, `LoadResult.name`, route caches, `actor_logs` addressing, and the loaded / spawn-child composition. Root caps are unchanged. Pre-1.0, with no external consumers, a contained one-time migration.
- **A runtime field on the binding.** Each actor binding gains the rolling fold state (`u64`) and threads it through spawn — new lifecycle plumbing, small but load-bearing.
- **ADR-0029 generalizes.** `MailboxId = hash(name)` survives as the depth-1 case; the general id is the fold over the lineage. The hashing, width, domain prefix, and no-lookup resolution are untouched.
- **Sibling spawn nests.** A spawned sibling's lineage extends its spawner's, revising ADR-0097's flat `trampoline:<name>` addressing: the sibling's id folds the spawner's carry with the sibling's ActorId. The id still returns synchronously — one fold step on a carry the trampoline already holds — so ADR-0097's sync-id / async-failure contract stands; only the value changes.
- **String addressing pays a parse.** MCP / CLI / string callers parse → per-segment ActorId → fold; static type addressing stays const. A cold path, but non-zero.
- **An embeddable actor resolves to its default-name id; liveness is separate.** `ctx.actor::<R>()` on an embeddable component resolves the default-name instance under the host class; if nothing is loaded at that name the mail drops on delivery, as for any chassis cap that is not up. The wrong-*address* footgun (#1364) is gone — the fold is the right id; a wrong *liveness* assumption drops as for every static address. A non-default name is reached by the caller via `loaded::<R>("cam2")`.
- **The host *node* is still mechanism-specific.** An embeddable actor's id is mechanism-independent down to the `aether.embedded` class node; the node above it (`aether.component`, the wasm component host) is not. A second embedding mechanism placed under a different host cap would re-introduce a mechanism-specific node, so full-prefix mechanism-independence — one host cap for the whole embeddable class, or collapsing host and class into one node — is deferred; collapsing loses lineage, and there is one host today (§5, §6).

### Neutral

- **ActorId is ADR-0096's actor-type tag, named.** No new hash for per-actor identity — a singleton's ActorId *is* the tag, and an instanced node's ActorId is that tag's namespace with the discriminator folded in.
- **Scope lineage is not mail lineage.** The lineage here is the static spawn/scope tree that determines identity; the causal mail lineage (ADR-0080 `ReplyTo`) is a separate runtime relationship and is untouched. A reply still routes by `ReplyTo`, now a MailboxId derived from the replier's lineage.
- **`mailbox_id_from_name_pair` keeps its meaning, narrowed.** It computes an instanced node's ActorId (`hash(NAMESPACE:subname)`) — one node, the `:` cardinality discriminator. The lineage fold composes those node ActorIds; the `/`-scope join is the fold, not a second string-hash.

### Follow-on

- **Implementation** is scoped on iamacoffeepot/aether#1420 and split into PRs: the lineage fold + carry in `aether-data` and the actor binding, with the trampoline re-spell; static resolution, with an embeddable component delegating to its host class (§5) under the reserved `aether.embedded` namespace; and the migration of name-carrying surfaces.
- **Display-layer ergonomics** — collapsing a repeated namespace root in the rendered path — are free to land later, since the string never feeds the hash.

## Alternatives considered

- **One flat hash for both questions** (the status quo, `MailboxId = hash(name)`). Rejected: it conflates which-actor with where-in-the-tree, which is #1364, and it cannot encode depth without baking the whole path into one string and losing the constituent identities.
- **A flat hash of the joined path string for the MailboxId** (`hash("a/b:7/c")`). Rejected: the result is not reverse-mappable to its constituent nodes (you cannot recover what sits at each level), and it cannot be carried incrementally — extending a path means rehashing the whole growing string per spawn. The hash chain gives both reverse-mappable nodes and O(1) extension.
- **A static scope marker on the child type** (`const SCOPE_ROOT`, `type Scope = Parent`). Rejected: lineage is runtime data — which session, which parent instance — and cannot be a compile-time const; bolting the parent onto a type couples code that must stay scope-agnostic so the same component loads under different parents. §5's embedded resolution is a different thing — an embeddable type delegates to its host *class*, naming no parent and reading no harness namespace, so the type stays scope-agnostic.
- **A macro-emitted const fold of the harness lineage on the embeddable type.** Have `export!` / `#[actor]` emit a resolver that folds the literal harness lineage — `hash(aether.component)`, `hash(aether.component.trampoline:<name>)` — into the id. Rejected: the macro emitting those harness namespaces writes them somewhere other than the actors that own them, a copy of a name outside its owner, which the read-from-owner rule (§5) forbids. Delegating to the host *class*, which reads its own namespace, keeps every name on its owner — the same id, sourced legally.
- **Encode the depth or level in the fold.** Rejected as redundant: the fold is sequential and non-commutative over fixed-width (8-byte) node ids, so position is already encoded; there are no cycles and no variable-length node boundary to disambiguate.
- **A flat `{scope}:{segment}` name join** (ADR-0098, superseded). Rejected: it kept one flat hash, so it inherited the not-reverse-mappable and not-incremental problems, and it left open where the runtime scope name lived (ADR-0098 §7). The rolling-`u64` carry answers that — the lineage rides as the fold state, neither a heavy name on the handle nor a registry round-trip.
- **Encode spawn lineage only in `ReplyTo`, off the address.** Rejected: who-spawned-whom is exactly where-in-the-tree, which is what a MailboxId must encode. Keeping it only in the causal `ReplyTo` chain is what leaves bare-type addressing landing on the wrong id.

## Related

- ADR-0098 — Scoped singletons. **Superseded by this ADR.** Its per-scope-singleton concept is carried forward on the identity model (§2); its flat `{scope}:{segment}` join and its open §7 (where the runtime scope name lives) are replaced by the lineage fold and the rolling carry.
- ADR-0029 — `MailboxId = hash(name)`. Generalized: the name-hash is the depth-1 case; the general id is the fold over the lineage. Width and domain prefix unchanged; client-side no-lookup resolution holds for embeddable actors too — they delegate to a host class whose lineage is static (§5), so a peer computes the id with no round-trip.
- ADR-0096 — Multi-actor wasm modules. The actor-type tag it defines is a node's ActorId.
- ADR-0097 — Wasm sibling spawn. Its spawn mechanism (stage-and-drain, `ReplyTo`, sibling-only, no cascade) stands; this revises a spawned sibling's addressing from flat to nested under the spawner's lineage. The guest's mailbox being the trampoline's puts every embeddable actor under the same static embedding-host lineage — the basis for the static class-delegated resolution (§5).
- ADR-0079 — Instanced actors and cardinality. An instanced node's discriminator is folded into its ActorId by the `:` separator.
- ADR-0064 — Type-tagged opaque ids. The depth-1 fixed point relies on `with_tag` being idempotent for a repeated tag.
- ADR-0080 — Mail tracing and settlement. The causal mail lineage (`ReplyTo`) is a separate lineage, unaffected.
- iamacoffeepot/aether#1364 — the gap this closes; #1420 — the implementation; #1423 — static resolution (an embeddable actor delegates to its host class; no registry, no round-trip).
