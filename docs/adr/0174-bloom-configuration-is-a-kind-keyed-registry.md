# ADR-0174: Bloom Configuration Is a Kind-Keyed Registry

- **Status:** Accepted
- **Date:** 2026-08-07

## Context

A sealed bloom addresses its configuration through bespoke, single-purpose digest fields. `BloomDraft` seals three of them alongside the base it lands against:

```rust
pub base: Digest,           // the source the bloom seals against
pub stage_catalog: Digest,
pub toolchain: Digest,
pub policy: Digest,
```

`base` is resolved everywhere it matters — it is the subject of the compare-and-swap that lands the bloom. The other three are attested and inert. `toolchain` has no read site outside its own accessor. `policy` has none at all; the pre-seal gate loads a policy *file* at API init and never consults the sealed digest. `stage_catalog` is unresolved, and #4587 exists to change that.

Each inert field is a latent instance of the divergence #4588 closed for the per-member scope revision: the receipt names a digest and the run ignores it, so an auditor reading the receipt and an operator watching the run can disagree with nothing to reconcile them. Closing that gap for one field took a store table, an authoring route, and a resolution site. Three fields remain, and the board already holds two of those repetitions.

The shape of the repetition is what matters. Every one of these fields is the same thing: a digest naming content the host must fetch and hand to a process. They differ only in which content. Modelling that difference as a Rust field name means the sealed value type has to change every time a new kind of configuration becomes worth attesting, and a sealed value type is the most expensive thing in the system to change — it re-digests every spec and every membership that carries it.

The machinery to key on the content's *type* instead already exists in full:

- `inventory::collect!(DescriptorEntry)` in `aether-data` gives every native binary a name → `KindDescriptor { name, schema }` map, materialized by `aether_kinds::descriptors::all()`. The MCP `describe_kinds` tool resolves through it.
- `aether_codec::encode_schema` / `decode_schema` convert JSON to canonical wire bytes and back over a `SchemaType`, the same path `send_mail` encodes params through.
- `Kind::NAME` is a compile-time const on every `#[derive(Kind)]` type, and it is the same string the descriptor inventory keys on, so a type and a JSON request name a configuration identically.
- The `scope_revision` table added in #4588 is already a content-addressed blob store under a digest key.

The SDK also already resolves by type without a string key at the call site. `ctx.actor::<RenderCapability>()` names a mailbox by the capability's type; a config lookup that reads `configs.resolve::<K>()` is the same move against a sealed table.

## Decision

A bloom's configuration is a sealed, kind-keyed registry, and the bespoke digest fields are removed.

```rust
pub type ConfigRegistry = BTreeMap<String, Digest>;   // keyed by `Kind::NAME`

pub struct BloomDraft  { pub configs: ConfigRegistry, .. }
pub struct Membership  { pub configs: ConfigRegistry, .. }
```

Five properties define it.

**At most one entry per kind.** This is what makes the lookup typed rather than named — `configs.resolve::<K>()` needs no key argument, because `K::NAME` is the key. A caller that needs two configurations of the same underlying shape declares a newtype kind for the second; the wrapper is the disambiguator, so the registry never needs a name-plus-type composite key.

**Resolution walks a scope chain, and registries layer rather than nest.** A lookup starts at the member's registry, falls through to the bloom's, and ends at the caller-supplied default:

```rust
let toolchain = resolve_config::<Toolchain>(store, ConfigScopes::member_of(member, bloom))?.unwrap_or_default();
```

This is the fall-through `ModelOverride::resolve` already performs across the fields of one value, lifted one level to operate across scopes. Nesting a per-member map inside the bloom's registry would express the same thing while making every sealed value type know about the scope hierarchy; layering keeps each registry flat and puts the hierarchy in the resolver.

**Whoever resolves an entry must already hold its content.** The reducer seals the registry, orders it canonically, and hands it on; the host resolves an entry at the point of use, through the same store the scope revision resolves through. The ADR-0149 boundary is unchanged.

The reducer resolves too, when a configuration is one *it* has to read — a retry budget decides re-dispatch versus wedge, and that decision happens inside `reduce`. It cannot fetch, so its content arrives as an argument (`ResolvedConfigs`), filled by the caller before it reduces. Decoding was never the obstacle: a config kind is a type `aether-bloomery` declares, so `from_bytes::<K>` is all a resolution needs once the bytes are in hand. Reaching a store is the obstacle, and it is a property of *where the code runs*, not of what it can decode.

**Authoring is generic.** One route replaces the per-config routes:

```
POST /configs  { "kind": "aether.bloomery.toolchain", "value": { .. } }
```

The host resolves the kind name to its schema through the descriptor inventory, encodes the JSON to canonical bytes with `encode_schema`, digests them, stores them, and answers with the address. The route defers on the store reply, so a `200` is a durability claim — the #4588 precedent, and load-bearing for the same reason: a lost config reintroduces the divergence the registry exists to close. `POST /scope-revisions` becomes one instance of this rather than a special case.

**A sealed key the host cannot resolve is a loud failure.** If a registry names a kind with no descriptor in the resolving binary, or a digest with no stored content, the dispatch fails rather than proceeding on a default. Silently defaulting past a sealed entry would attest a configuration that never applied, which is the failure mode this whole decision exists to remove. Absence is a valid state and resolves to the default; a present-but-unresolvable entry is not.

### What stays outside the registry

`base` stays a field. It is not configuration injected into a process — it is the bloom's subject, the source revision the land compare-and-swaps against.

`Membership.scope_revision` also stays a field, and this is the sharper call. Two distinct things are fused in it today: the identity of the workpiece's approved scope, which the member's `approval` evidence is bound to, and a carrier for the per-workpiece `ModelOverride`, which was retrofitted onto that identity because it needed somewhere attested to live. Only the second is configuration. The override moves into the member's registry as a `ModelOverride` entry, and `Membership.scope_revision` goes back to being a bare digest naming approved scope content — the `ScopeRevision` *type* is deleted outright, since removing the override left nothing in it.

That split moves model choice out from under the approval binding, which today covers it by accident — changing the override changes the revision digest and invalidates the approval. Losing that would be a real regression, so the approval's binding widens from `scope_revision` alone to the member's configuration set as well. An operator still cannot change which model runs for an approved workpiece without re-approval.

### What this does not decide

Per-stage granularity within a single configuration is a separate question and stays with #4601. The scope chain answers *where* a lookup finds a value; it says nothing about how finely one value discriminates. A `ModelOverride` that distinguishes Construct from Refine keeps the whole model decision in one attested value, which is better for a system whose product is a receipt, and the registry neither helps nor hinders it.

The wrapper-kind route is available if that map ever proves awkward — `ConstructModel` and `RefineModel` as distinct registry entries — but it splits one decision across two attested values with a fallback the sealed bytes do not record, so it is the fallback rather than the plan.

## Consequences

- Attestation improves outright. The spec's digest covers the registry, so it covers exactly which configurations were sealed and no others, and each entry's digest covers its content. A receipt attests a complete configuration set rather than a fixed handful of fields. A configuration nobody sealed is absent, rather than defaulted and unrecorded.
- Adding an attestable configuration stops touching sealed value types. It becomes a new kind, an entry, and a resolution site — no re-digest, no migration.
- `stage_catalog`, `toolchain`, and `policy` resolve through the registry at their points of use, or are deleted where they have no consumer. No field stays sealed and inert either way, which is the outcome that matters more than which of the two each field gets.
- The canonical member order in `BloomDraft::seal` re-keys. Sorting currently leads on `scope_revision`; with configuration alongside it the sort leads on `workpiece` and carries the registry and approval as tiebreakers, which stays a total order over the member set and so keeps the bloom id a stable function of that set rather than of its input order.
- This re-digests every spec and every membership. The migration is free today because no bloom has sealed a configuration the registry would have to carry forward, and it will not stay free — which is the argument for deciding now rather than after three more bespoke fields.
- A string key costs more bytes per entry than a fixed-width id would, and it is the one string in a structure whose other identities are typed. Sealed registry bytes are legible to anyone reading them without the binaries that produced them, which is the compensating property.
- Follow-on work, each its own change: the registry types and their sealing (#4602); the generic `POST /configs` route (#4602); migrating the scope revision's override into the member registry and widening the approval binding (#4606); resolving or deleting each of the three inert fields (#4607 deleted `toolchain` and `policy`, #4587 resolved `stage_catalog`).
- One follow-on this record did not anticipate: **a configuration the reducer must read needs the content handed to it** (#4618). The retry budgets in the stage catalog are read inside `reduce`, which has no store handle, so `reduce` takes a `ResolvedConfigs` the caller fills — at boot from a bulk read, at runtime from a deferred re-read when an admit names an address the control core has not seen. The seal door refuses a spec naming content it was not given, since a sealed address is immutable and would otherwise fail later at a dispatch that parks.
- `policy` was deleted rather than resolved, and reinstating it as a registry entry the pre-seal gate resolves is #4616. It is a design change, not a wiring one: the gate is synchronous over in-memory state and runs pre-seal, and whether a member may seal the policy that admits it is an open question.

## Alternatives considered

- **Wire the three inert fields one at a time, as #4588 did for the scope revision.** The straightforward path, and the one the board is already on. Rejected because it pays the same store-table-plus-route-plus-resolution cost three more times and leaves the next configuration paying it again, with a sealed-value-type change each time.
- **Key the registry by `KindId`.** Compact, fixed-width, and the workspace's existing currency for type-keyed lookup, so it was this record's original choice. Rejected during implementation on a durability finding: the `Kind` derive folds the kind's *schema* into its id (`fnv1a_64_prefixed(KIND_DOMAIN, canonical(name, schema))`), so adding a field to a configuration kind moves its id and orphans that entry in every bloom already sealed. A key inside an immutable record has to survive its type growing a field, and a name does. The secondary benefit is that a `KindId` collision could have let two kinds address one slot; keying on the name it hashes removes the question.
- **Nest per-member registries inside the bloom's registry.** One table instead of two. Rejected because the scope hierarchy then lives inside the sealed value types, so a new scope level is a re-digest; layering puts it in the resolver where it costs nothing.
- **Allow multiple entries per kind under a name-plus-type key.** Removes the need for wrapper newtypes. Rejected because it removes the typed lookup with it — every call site would carry a string, which is the ambient string-keyed configuration ADR-0162 rejected one layer down.
- **Do nothing until a third configuration kind is genuinely wanted.** The honest counter-argument: only `ScopeRevision` and `StageCatalog` are actively wanted, and a registry for two entries is over-built. It does not survive the field count — there are already four, three inert — and the intent is that a process can be handed configuration generally rather than through a fixed list.
