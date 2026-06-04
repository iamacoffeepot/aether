# The type system

Everything the engine moves is typed, and the vocabulary is small. Four kinds of
thing carry types — **kinds** (payloads), **mailboxes** (addresses), **handles**
(references to stored values), and **transforms** (pure functions) — and each is
named by a **typed id**. This page is the tour: what each one is, how it's
identified, and how they compose.

The reason to care isn't compiler ergonomics. A typed thing here is
*self-describing*: it carries enough to encode itself from JSON, decode itself
without a shared header, and answer "what are you?" to a live engine. That's
what lets the agent driving the engine introspect it — `describe_kinds`,
`describe_component`, `describe_handles`, `describe_transforms` are all just
"read the types." Typing is the substrate of observability, not paperwork.

> Governing ADRs: **ADR-0005** (mail typing), **ADR-0019** (unified encoding),
> **ADR-0029/0030** (name- and schema-derived ids), **ADR-0031/0032** (const
> schema + canonical bytes / labels sidecar), **ADR-0045/0048/0049** (handles,
> transforms, the handle store), **ADR-0064/0065** (type-tagged wire ids +
> first-class id types). The type vocabulary is **stable** — the wire format
> depends on it. The DAG composition surface that handles and transforms feed is
> **shipped and settling** (its 0.4 stack merged).

## Kinds — typed payloads

A **kind** is a named, sendable mail payload — a Rust type carrying a name, a
stable id, and a wire encoding. You declare one with two derives and a name:

```rust
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "aether.audio.note_on")]
struct NoteOn { instrument: u8, pitch: u8, velocity: f32 }
```

**Two derives, because they describe two different things.**

- **`Schema`** gives `const SCHEMA: SchemaType` — a description of the type's
  byte *layout* (ADR-0031). It's compositional: every type that can sit inside a
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
`SCHEMA` from `Schema`.

Because `ID` and `SCHEMA` are `const`, there is no host round-trip to learn a
kind's identity or shape — `Kind::ID` is a compile-time value. And because the
schema travels with the type, the wire layer can encode a kind from JSON and a
recipient can decode it without a shared header (ADR-0019). On the wire a
`#[repr(C)]` plain-data kind rides as a raw byte cast; everything else as
postcard — the derive autodetects from the type's layout, so a single
`send` / `reply` call site handles both.

**What feeds the id — and what doesn't.** The `KindId` hash takes `name +
schema`, where `name` is the declared kind name and `schema` is the *structural*
shape — field types and positions. All nominal information (the Rust type name,
field names, variant names) is erased from the hashed bytes and carried in a
parallel labels sidecar instead (ADR-0031/0032). So:

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

## The schema vocabulary — `SchemaType`

A kind's shape is a tree of `SchemaType`. The leaves and containers are what
you'd expect — `Bool`, `Scalar` (the primitives), `String`, `Bytes`, `Option`,
`Vec`, `Array` (fixed-length `[T; N]`; `Vec` is the variable-length form),
`Struct`, `Enum`, and `Map` (a keyed lookup table). The arms that carry rules
worth knowing beyond "it's a type tree":

- **Two wire shapes, and what picks them.** A struct encodes as a raw
  `#[repr(C)]` byte cast (`repr_c = true`) *only if* it is `#[repr(C)]` **and**
  every field is cast-eligible, recursively; otherwise it encodes as postcard.
  Cast-eligible means a scalar primitive, a typed-id newtype (`MailboxId` and
  friends are `#[repr(transparent)]` over `u64`), a fixed `[T; N]` array of
  cast-eligible elements, or a nested all-cast-eligible `#[repr(C)]` struct. A
  single `String`, `Bytes`, `Vec`, `Option`, `Map`, `Enum`, or `Ref` field
  anywhere short-circuits the whole struct to postcard. You don't choose this —
  the derive computes it at compile time (`CastEligible::ELIGIBLE` ANDs every
  field) — but it's why two similar-looking kinds can have different wire
  encodings, and it's the `encode` vs `encode_struct` split in the SDK.
- **`Map` keys are restricted.** A map key may only be a `String`, an integer
  scalar, or `Bool` — the `BTreeMap<K: Ord, V>` bound rules out `f32`/`f64`/
  `Vec`/`Option` at the type level and the codec rejects them defensively.
  Entries serialize in key-sorted order, and a map (being variable-length)
  always forces its parent struct onto the postcard path.
- **The canonical bytes are positional-only.** When a schema is serialized for
  hashing and for the `aether.kinds` manifest, field and variant names are
  dropped; a separate labels sidecar (`aether.kinds.labels`) carries them for
  consumers that want human-readable reconstruction, like `describe_kinds`
  (ADR-0032). The wire never carries names — postcard fields are positional.
- **`Ref` is a schema arm.** A field whose type is `Ref<K>` is a *handle-or-
  inline* slot — see *Handles* below. The
  schema wraps the inner kind's shape, so a recipient decodes the resolved value
  the same way whether it arrived inline or via the store.
- **`TypeId` is a schema arm.** A kind can have a field that *is* an id — a
  `MailboxId`, `KindId`, or `HandleId` as a first-class typed reference, not just
  a bare `u64` (ADR-0065). These encode as a tagged string on JSON and a varint
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
- **Flipping `#[repr(C)]`** when it changes the cast/postcard choice — `repr_c`
  is part of the canonical bytes, so the same fields under a different wire
  format are a different kind.
- **Wrapping a field** — `{ a: u32 }`, `{ a: Option<u32> }`, and `{ a: Ref<u32> }`
  are three distinct kinds; likewise `[u32; 3]` ≠ `[u32; 4]` ≠ `Vec<u32>`.
- **Changing an enum discriminant** — discriminants are encoded; variant names
  are not.

## Mailboxes — typed addresses

A **mailbox** is an address: where mail goes. Its `MailboxId` is a 64-bit hash
of the mailbox *name* alone (no schema — a mailbox is a pure address, ADR-0029),
so it's the same id in every process that hashes the same name, and it survives
a component hot-swap. In a component you rarely touch the id directly: you
address a peer by type (`ctx.actor::<RenderCapability>()`) or hold a `Mailbox<K>`
token. The mailbox-vs-kind distinction — why an address and a payload shape are
different things even when they share a name prefix — is the
[Mail, kinds & scheduling](../systems/mail-and-kinds.md) page's subject.

## Handles — references to stored values

Some values are too big to put on the wire (a decoded texture, a model's
output), or are produced asynchronously by a pipeline. Those live in the
substrate's **handle store** and travel as a *reference* instead of inline bytes.
The unifying type is `Ref<K>`:

```rust
pub enum Ref<K> {
    Inline(K),                          // the whole value is on the wire
    Handle { id: u64, kind_id: u64 },   // a reference into the handle store
}
```

A kind field typed `Ref<K>` accepts **either** form. A caller that has the value
passes `Ref::inline(v)`; a caller pointing at a stored value passes
`Ref::handle(id)`, which stamps `kind_id` from `K::ID` so it can't disagree with
the type. The substrate resolves a `Handle` to its stored value *before* the
recipient's handler runs — validating that the stored entry's kind matches
`K::ID` — so a handler decodes a resolved `K` identically either way and never
has to know which form arrived.

Handle ids are **content-addressed** (the id is derived from the bytes, so two
producers of the same value get the same handle and the store deduplicates,
ADR-0048), and the store is **persistent** with a disk budget (ADR-0049 —
inspect it with `describe_handles`). Handles are how the computation DAG passes
values between steps without round-tripping them through an actor's memory; see
[The computation DAG & handles]().

## Transforms — typed pure functions

A **transform** is a pure function the DAG runs between steps — `#[transform] fn
foo(input: A) -> B`, registered at build time with a `TransformId` and typed by
its input and output kinds (ADR-0048). Where a kind is a value and a mailbox is
an address, a transform is a typed *edge*: it takes a resolved handle of one kind
and writes a new handle of another. Inspect the linked set with
`describe_transforms`. The DAG that wires sources, transforms, and outputs into a
job is its own subject — [The computation DAG & handles]().

## Typed ids — the naming layer

Every typed thing is named by a newtype over a hash, not a bare integer:

| id | wraps | derived from |
|---|---|---|
| `KindId` | `u64` | `name + schema` (ADR-0030) |
| `MailboxId` | `u64` | mailbox name (ADR-0029) |
| `HandleId` | `u64` | the stored bytes (content-addressed, ADR-0048) |
| `TransformId` | `u64` | the transform's identity (ADR-0048) |
| `DagId` | `u64` | an in-flight DAG job |
| `EngineId`, `SessionToken` | `Uuid` | wire identity of an engine / session |

Two properties keep these from being foot-guns:

- **Disjoint hash domains.** A kind id and a mailbox id are hashed with
  different domain prefixes, so the *same* name produces *different* ids in the
  two spaces — the id spaces don't collide even when names are shared (ADR-0030).
  This is the mechanical reason a kind name and a mailbox name can look alike yet
  address different things.
- **Tagged strings on the MCP wire.** Across the agent boundary, ids encode as
  `<tag>-XXXX-XXXX-XXXX` — `mbx-…` for a mailbox, `knd-…` for a kind, `hdl-…` for
  a handle (ADR-0064). The tag makes an id self-identifying, so a mailbox id and
  a kind id can't be silently swapped in a tool call. **Hand these back
  verbatim** — they're opaque tokens, not numbers to parse.

## How it composes

The pieces stack: a **kind** describes some bytes; a **schema** describes the
kind; a **mailbox** is where a kind is sent; a **handle** (`Ref<K>`) lets a kind
travel by reference when it's large or async; a **transform** turns one kind's
handle into another's; and a **typed id** names each so a live engine can be
asked what exists. Because every one of them is self-describing, the whole system
is introspectable from the outside — which is the property the agent harness is
built on.

## Where to read more

- The contracts these types enforce — [Invariants & guarantees](invariants.md).
- Addresses vs payloads, in depth — [Mail, kinds & scheduling](../systems/mail-and-kinds.md).
- Handles and transforms in motion — [The computation DAG & handles]().
