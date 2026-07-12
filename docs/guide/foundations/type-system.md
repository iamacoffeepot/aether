# The type system

> **Governing ADRs:** [ADR-0005](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0005-mail-typing-system.md) (mail typing), [ADR-0019](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0019-unified-mail-encoding.md) (unified encoding),
> [ADR-0029](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0029-name-derived-mailbox-ids.md)/[ADR-0030](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0030-hashed-kind-ids.md) (name- and schema-derived ids), [ADR-0099](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0099-actor-identity-and-addressing.md) (actor identity and addressing), [ADR-0031](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0031-const-constructible-schema-representation.md)/[ADR-0032](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0032-canonical-schema-bytes-and-labels-sidecar.md) (const
> schema + canonical bytes / labels sidecar), [ADR-0048](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0048-transforms-and-content-addressed-handles.md) (transforms),
> [ADR-0064](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0064-type-tagged-opaque-ids-on-the-mcp-wire.md)/[ADR-0065](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0065-typed-id-newtypes-and-first-class-type-ids-in-the-schema.md) (type-tagged wire ids +
> first-class id types). The type vocabulary is **stable** — the wire format
> depends on it.

Everything the engine moves is typed, and the vocabulary is small. Three kinds of
thing carry types — **kinds** (payloads), **mailboxes** (addresses), and
**transforms** (pure functions) — and each is named by a **typed id**. This page
is the tour: what each one is, how it's identified, and how they compose.

The reason to care isn't compiler ergonomics. A typed thing here is
*self-describing*: it carries enough to encode itself from JSON, decode itself
without a shared header, and appear in the appropriate inventory. That lets an
agent inspect live engine kinds/components while separately reading the static
transform set linked into `aether-mcp`. Typing is the substrate of
observability, not paperwork.

## Kinds — typed payloads

A **kind** is a named, sendable mail payload — a Rust type carrying a name, a
stable id, and a wire encoding. You declare one with two derives and a name:

```rust
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "aether.audio.note_on")]
struct NoteOn { pitch: u8, velocity: u8, instrument_id: u8 }
```

**Two derives, because they describe two different things.**

- **`Schema`** gives `const SCHEMA: SchemaType` — a description of the type's
  byte *layout* ([ADR-0031](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0031-const-constructible-schema-representation.md)). It's compositional: every type that can sit inside a
  payload implements it — the primitives, `String`, `Vec<T>`, `Option<T>`,
  arrays, and your own structs — so a struct's schema is assembled from its
  fields'. A type that only ever appears as a *field* of a kind derives `Schema`
  alone.
- **`Kind`** marks the type as a top-level, addressable payload: `const NAME`
  (the declared name), `const ID: KindId`, and the encode/decode bodies. `Kind`
  is *layered on* `Schema` — the id is `hash(name + canonical(SCHEMA))`, so the
  derive reads the type's schema to compute it. A kind is always a schema; a
  schema is not always a kind.

So you write `#[derive(Kind, Schema)]` on a message you send, and
`#[derive(Schema)]` alone on a helper struct that only appears as a field of one.
That gives a kind three compile-time constants — `NAME` and `ID` from `Kind`,
`SCHEMA` from `Schema`. In practice `Schema` is derived on far more types than
`Kind`: every field-only struct has one, but isn't independently sendable.

Why not just fold `Schema` into `Kind`, so one derive does everything? Because a
kind's schema is *composed* from its fields' schemas by trait dispatch — the
derive emits `<Self as Schema>::SCHEMA`, which recurses into
`<FieldType as Schema>::SCHEMA` for each field. For that to resolve, every field
type — including nested structs that are never mailed — has to implement `Schema`
on its own, so `Schema` must be a standalone derive regardless. Given that,
`Kind` *reads* the schema rather than recomputing it; merging the two would
duplicate the schema walk that [ADR-0031](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0031-const-constructible-schema-representation.md)/[ADR-0032](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0032-canonical-schema-bytes-and-labels-sidecar.md) deliberately collapsed into one
trait-dispatched path. (It's the `Serialize` / `Deserialize` split: one trait,
one derive.)

Because `ID` and `SCHEMA` are `const`, there is no host round-trip to learn a
kind's identity or shape — `Kind::ID` is a compile-time value. And because the
schema travels with the type, the wire layer can encode a kind from JSON and a
recipient can decode it without a shared header ([ADR-0019](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0019-unified-mail-encoding.md)). On the wire a
`#[repr(C)]` plain-data kind rides as a raw byte cast; everything else as a
structured encoding — the derive autodetects from the type's layout, so a single
`send` / `reply` call site handles both.

**What feeds the id — and what doesn't.** The `KindId` hash takes `name +
schema`, where `name` is the declared kind name and `schema` is the *structural*
shape — field types and positions. All nominal information (the Rust type name,
field names, variant names) is erased from the hashed bytes and carried in a
parallel labels sidecar instead ([ADR-0031](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0031-const-constructible-schema-representation.md)/[ADR-0032](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0032-canonical-schema-bytes-and-labels-sidecar.md)). So:

| edit | id changes? | why |
|---|---|---|
| add / remove a field | **yes** | shape changed |
| change a field's type | **yes** | shape changed |
| reorder fields | **yes** | positions are structural |
| rename a field (same type) | no | field names are erased |
| rename the Rust `struct` | no | type names are erased |
| change `#[kind(name = "…")]` | **yes** | the *name* half of the hash |

This is what makes schema drift fail loud rather than silently garbage-decode —
a mismatched producer and consumer compute *different* ids and the mail lands on
"kind not found." The full contract is in
[Invariants & guarantees](invariants.md).

For the end-to-end walkthrough of declaring a new substrate kind — the derives,
the self-registering descriptor, the handler, and the rebuild rule when an edit
moves the id — see the [Adding a substrate
kind](../recipes/adding-a-substrate-kind.md) recipe.

## The schema vocabulary — `SchemaType`

A kind's shape is a tree of `SchemaType`. The leaves and containers are what
you'd expect — `Bool`, `Scalar` (the primitives), `String`, `Bytes`, `Option`,
`Vec`, `Array` (fixed-length `[T; N]`; `Vec` is the variable-length form),
`Struct`, `Enum`, and `Map` (a keyed lookup table). The arms that carry rules
worth knowing beyond "it's a type tree":

- **Two wire shapes, and what picks them.** A struct encodes as a raw
  `#[repr(C)]` byte cast (`repr_c = true`) *only if* it is `#[repr(C)]` **and**
  every field is cast-eligible, recursively; otherwise it encodes as structured.
  Cast-eligible means a scalar primitive, a typed-id newtype (`MailboxId` and
  friends are `#[repr(transparent)]` over `u64`), a fixed `[T; N]` array of
  cast-eligible elements, or a nested all-cast-eligible `#[repr(C)]` struct. A
  single `String`, `Bytes`, `Vec`, `Option`, `Map`, `Enum`, or `Ref` field
  anywhere short-circuits the whole struct to structured. You don't choose this —
  the derive computes it at compile time (`CastEligible::ELIGIBLE` ANDs every
  field) — but it's why two similar-looking kinds can have different wire
  encodings, and it's the `encode` vs `encode_struct` split in the SDK.
- **`Map` keys are restricted.** A map key may only be a `String`, an integer
  scalar, or `Bool` — the `BTreeMap<K: Ord, V>` bound rules out `f32`/`f64`/
  `Vec`/`Option` at the type level and the codec rejects them defensively.
  Entries serialize in key-sorted order, and a map (being variable-length)
  always forces its parent struct onto the structured path.
- **The canonical bytes are positional-only.** When a schema is serialized for
  hashing and for the `aether.kinds` manifest, field and variant names are
  dropped; a separate labels sidecar (`aether.kinds.labels`) carries them for
  consumers that want human-readable reconstruction, like `describe_kinds`
  ([ADR-0032](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0032-canonical-schema-bytes-and-labels-sidecar.md)). The wire never carries names — structured fields are positional.
- **`TypeId` is a schema arm.** A kind can have a field that *is* an id — a
  `MailboxId` or `KindId` as a first-class typed reference, not just a bare
  `u64` ([ADR-0065](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0065-typed-id-newtypes-and-first-class-type-ids-in-the-schema.md)). These encode as a tagged string on JSON and a varint
  on the wire.

### What counts as the same kind

The canonical schema is a *positional* encoding: each arm becomes a type tag plus
its structural content — field and variant *names* are dropped, but field
*order*, array *lengths*, enum *discriminants*, and the `repr_c` flag are all
kept. Two kinds with the same declared name share a `KindId` exactly when those
bytes match. The "which edits move the id" matrix above lists the basics; the
cases that surprise people live at the schema-arm level. Holding the
`#[kind(name = …)]` fixed:

**Still the same kind** — the id doesn't move:

- renaming a field but keeping its type — `{ x: u32 }` and `{ count: u32 }` are
  identical;
- renaming the Rust `struct`, or renaming an enum variant while keeping its
  discriminant and payload shape.

**A different kind, despite looking alike:**

- **Reordering fields** — `{ a: u32, b: u8 }` ≠ `{ b: u8, a: u32 }`; positions
  are structural.
- **A same-size type swap** — `{ a: u32 }` ≠ `{ a: i32 }`, and `{ a: u64 }` ≠
  `{ a: MailboxId }` even though both are eight wire bytes: a typed-id field is a
  distinct `TypeId` arm, not a `Scalar`.
- **Flipping `#[repr(C)]`** when it changes the cast/structured choice — `repr_c`
  is part of the canonical bytes, so the same fields under a different wire
  format are a different kind.
- **Wrapping a field** — `{ a: u32 }` and `{ a: Option<u32> }` are distinct
  kinds; likewise `[u32; 3]` ≠ `[u32; 4]` ≠ `Vec<u32>`.
- **Changing an enum discriminant** — discriminants are encoded; variant names
  are not.

## Mailboxes — typed addresses

A **mailbox** is an address: where mail goes. Addressing rests on two ids, one
per question ([ADR-0099](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0099-actor-identity-and-addressing.md)):

- An **`ActorId`** answers *which actor*. It is a 64-bit hash of the actor's
  `NAMESPACE` alone — `hash(NAMESPACE)` for a singleton actor,
  `hash(NAMESPACE:subname)` for an instanced one — with no schema in the hash,
  because an actor's identity is its name ([ADR-0029](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0029-name-derived-mailbox-ids.md)). It names the actor
  wherever it is hosted, and it reverse-maps to that name for introspection.
- A **`MailboxId`** answers *where in the tree*. An actor sits somewhere — at
  the substrate root, or hosted under a parent — and its **lineage** is the
  ordered list of ActorIds from the root down to it. The `MailboxId` is a hash
  chain over that lineage (`fold_lineage`, one fold step per node), and mail
  routes to it.

For a root actor — every chassis capability — the lineage is a single node,
and the fold of one node is that node: `MailboxId == ActorId == hash(name)`,
so the name hash of [ADR-0029](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0029-name-derived-mailbox-ids.md) is the depth-1 case of the fold. A hosted
actor — a loaded component, a spawned child — folds its ActorId onto its
parent's, so the same code under two different parents is two different
mailboxes. The `/`-rendered addresses you see
(`aether.component/aether.embedded:camera`) are a display rendering of the
lineage, one segment per ActorId; a written path resolves by parsing it into
segments and re-folding (`mailbox_id_from_path`), never by hashing the joined
string.

Both ids are computed from compile-time constants with no registry lookup, so
every process that holds the same names and the same lineage computes the same
ids — and a component hot-swap, which changes neither, keeps the address
valid. In a component you rarely touch either id directly: you address a peer
by type (`ctx.actor::<RenderCapability>()`) or hold a `Mailbox<K>` token. The
mailbox-vs-kind distinction — why an address and a payload shape are different
things even when they share a name prefix — is the
[Mail, kinds & scheduling](../systems/mail-and-kinds.md) page's subject.

## Transforms — typed pure functions

A **transform** is a pure function over typed values — `#[transform] fn
foo(input: A) -> B`, registered at build time with a `TransformId` and typed by
its input and output kinds ([ADR-0048](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0048-transforms-and-content-addressed-handles.md)). Where a kind is a value and a mailbox is
an address, a transform is a typed *edge*: it takes a value of one kind and
produces a value of another. The `aether.fs` `fetch` verb runs a fold chain of
them over a file's bytes; inspect the current MCP build's linked set with
`describe_transforms`.

## Typed ids — the naming layer

Every typed thing is named by a newtype over a hash, not a bare integer:

| id | wraps | derived from |
|---|---|---|
| `KindId` | `u64` | `name + schema` ([ADR-0030](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0030-hashed-kind-ids.md)) |
| `ActorId` | `u64` | the actor's `NAMESPACE` (plus `:subname` when instanced) ([ADR-0099](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0099-actor-identity-and-addressing.md)) |
| `MailboxId` | `u64` | the actor's lineage — a hash chain of `ActorId`s, root → leaf; a root actor's equals its name hash ([ADR-0029](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0029-name-derived-mailbox-ids.md)/[ADR-0099](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0099-actor-identity-and-addressing.md)) |
| `TransformId` | `u64` | the transform's identity ([ADR-0048](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0048-transforms-and-content-addressed-handles.md)) |
| `EngineId`, `SessionToken` | `Uuid` | wire identity of an engine / session |

Two properties keep these from being foot-guns:

- **Disjoint hash domains.** A kind id and a mailbox id are hashed with
  different domain prefixes, so the *same* name produces *different* ids in the
  two spaces — the id spaces don't collide even when names are shared ([ADR-0030](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0030-hashed-kind-ids.md)).
  This is the mechanical reason a kind name and a mailbox name can look alike yet
  address different things. (`ActorId` deliberately shares the mailbox domain —
  a root actor's `MailboxId` *is* its `ActorId`, which is what keeps every
  chassis capability's id equal to its name hash.)
- **Tagged strings on the MCP wire.** Across the agent boundary, ids encode as
  `<tag>-XXXX-XXXX-XXXX` — `mbx-…` for a mailbox, `knd-…` for a kind
  ([ADR-0064](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0064-type-tagged-opaque-ids-on-the-mcp-wire.md)). The tag makes an id self-identifying, so a mailbox id and
  a kind id can't be silently swapped in a tool call. **Hand these back
  verbatim** — they're opaque tokens, not numbers to parse.

## How it composes

The pieces stack: a **kind** describes some bytes; a **schema** describes the
kind; a **mailbox** is where a kind is sent; a **transform** turns one kind into
another; and a **typed id** names each so the owning inventory can report what
exists. Because every one of them is self-describing, the system is inspectable
from the outside — with engine-scoped and MCP-build-static views kept distinct.

## Where to read more

- The contracts these types enforce — [Invariants & guarantees](invariants.md).
- Addresses vs payloads, in depth — [Mail, kinds & scheduling](../systems/mail-and-kinds.md).
