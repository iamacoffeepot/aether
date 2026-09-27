# ADR-0242: A Kind's Reach Is Its Narrowest Field's

- **Status:** Proposed
- **Date:** 2026-09-27

Builds on [ADR-0230](0230-proven-actor-references.md) (proven references,
paths as the description that crosses),
[ADR-0231](0231-protocol-typed-references-and-reply-checks.md) (contextual
decode through `DecodeCtx`), [ADR-0233](0233-engine-only-mail.md) (the
`ActorMail` marker), and
[ADR-0139](0139-guest-reply-correlation-and-request-contexts.md) (the
request-context table). Edits ADR-0230 §1 and §2, ADR-0231 §3, and ADR-0233
in place, and amends ADR-0139 §4.

## Context

A kind says nothing about where its bytes may go, yet the code already moves
kinds across three boundaries:

- **Its own actor.** A request context is encoded by one actor and decoded
  only by that actor: the ADR-0139 table (`RequestContextTable::insert` and
  `RequestContextTable::take` in `crates/aether-actor/src/request_context.rs`),
  the native binding's store and take (`store_request_context` and
  `take_request_context` in
  `crates/aether-substrate/src/actor/native/binding/reply.rs`), and the guest
  snapshot that carries the table across `replace_component`, checked by
  `check_carried_contexts` in
  `crates/aether-component/src/trampoline/runtime/replace.rs`.
- **The engine.** In-process mail between actors, native or guest, is encoded
  by the envelope encoder (`crates/aether-substrate/src/mail/attachments/encoder.rs`)
  and decoded against the inbound's attachments and the mail registry
  (`NativeCtx::__decode_inbound` in
  `crates/aether-substrate/src/actor/native/ctx/inbound.rs`).
- **The wire.** Everything else leaves the process: a wire `Call`, an MCP
  bundle, `describe_kinds`, and the egress rewrites.

The contexts already hold values that mean something only to their own
actor. Guest contexts hold a `ReplyHandle`, an index into the instance's own
reply table, and native contexts hold a `Source`, a raw sender route. Nothing
told those kinds apart from mail: every derived kind was `ActorMail`, so any
actor could send a context, and ADR-0230 §1 gave proven references no codec
at all, so a context could not carry a reference even though its bytes never
leave the actor.

The owner's direction for this decision:

> No serialization of mailbox identifier doesn't mean no serialization of refs that are verified. the wire mode of data needs to be explicit. there are three modes, actor local, cross actor local, cross application. that much is obvious now. the types belong in those groups and they're expanding in definition. Context kinds SHOULD be able to declare references.

> Context serialization for references is something I forgot about because technically it doesnt cross any wires and stays local.

The owner named the concept **reach**, with the levels actor, engine, and
wire, and ruled that a kind's reach is the lowest reach among its fields.

## Decision

### 1. Reach is a fact about a type, with three levels

| Reach | Marker | May hold | Crosses | A narrower kind is refused at |
|---|---|---|---|---|
| Actor | neither | anything with a schema, including a `ReplyHandle`, a `Source`, or a `SourceAddr` | nothing: its bytes stay in their own actor's request-context table and replace snapshot | every typed send and reply (`ActorMail`), and every handler's kind |
| Engine | `CrossesActors` | a typed proof, once its codec lands (section 6); a `MailboxId` or a `MailId` | in-process mail, the guest FFI included | the typed wire doors (`WireMail`) |
| Wire | `CrossesActors` and `CrossesWire` | primitives, strings, the typed ids, `EngineId`, `SessionToken`, `LoadName`, paths (`ErasedActorPath`, `ActorPath<R>`, `ProtocolPath<P>`), `Blob`, and every plain data type | a wire `Call`, an MCP bundle, a file, saved state | nothing |

Nothing is declared on a kind. Reach is not hashed into `Kind::ID`, the
schema, or `describe_kinds`, so no id and no wire byte changes.

### 2. Two marker traits carry it

`aether_data::CrossesActors` means engine reach or wider, and
`aether_data::CrossesWire: CrossesActors` means wire reach
(`crates/aether-data/src/reach.rs`). Each carries a
`#[diagnostic::on_unimplemented]` that names the reach the type lacks, such as
"`Source` has actor reach: it does not cross actors".

- **Both markers:** the primitives, `bool`, `()`, `String`, `KindId`, `DagId`,
  `TransformId`, `ThreadId`, `EngineId`, `SessionToken`, `LoadName`,
  `ErasedActorPath`, `Blob`, `ReplyContract`, `ActorPath<R>`, `ProtocolPath<P>`,
  and every other plain data type, each beside its `Schema` impl.
- **The containers** `Vec<T>`, `Option<T>`, `Box<T>`, `[T; N]`, and
  `BTreeMap<K, V>` forward each marker from their element types.
- **`CrossesActors` only (engine reach):** `MailboxId` and `MailId`. A position
  is a registry key inside its engine: a core-internal kind, such as an
  engine-only notice or a trace record, may carry one, and a kind that does
  never reaches a typed wire door. `ActorRef<R>` and `ProtocolRef<P>` join
  them when their codec lands.
- **Neither (actor reach):** `ReplyHandle`, a per-instance host handle, and
  `Source` and `SourceAddr`, raw sender routes.
- **`ErasedActorRef`** gets no codec and no marker. Erased sending is being
  removed (#6895), and a peer that is sent to later is held as a
  `ProtocolRef<P>`.

A hand-written `Schema` impl adds the markers its type deserves beside it; one
that adds none gives its type actor reach, which fails closed.

### 3. The derives fold reach from the fields

The `Schema` derive, and the `Storage` derive, which emits the same schema
core, implement each marker for the type with one predicate per field of
every variant, merged with the type's own generics and where-clause
(`reach_impls` in `crates/aether-data-derive/src/lib.rs`):

```rust
// `aether-http`'s `DeferredSource { source: Source }`
impl CrossesActors for DeferredSource
where
    for<'__reach> Source: CrossesActors,
{}
```

The `for<'__reach>` binder is required. Stable rustc refuses an unsatisfied
bound over a concrete type in an impl's where-clause (E0277), so without the
binder the first kind holding an actor-reach field would fail to compile
rather than have actor reach. With it, the bound is checked where the marker
is used, and rustc's note chain names the kind ("required for
`DeferredSource` to implement `CrossesActors`") beside the leaf the diagnostic
names. A field name is not reachable from a trait diagnostic.

### 4. `ActorMail` is conditional, and a handler's kind crosses actors

`ActorMail: Kind + CrossesActors`. The `Kind` derive emits

```rust
impl ActorMail for K where for<'__reach> K: CrossesActors {}
```

and `engine_only` still withholds it outright. A typed send or reply of an
actor-reach kind is refused at the call site with the reach diagnostic. The
flat send verbs keep their `ActorMail` bounds, so no send signature changes.

`reply_marker_impl` in `crates/aether-actor-derive/src/reply_markers.rs`,
which every handler passes through on the wasm, native, and handler-set
paths, also requires the handler's kind to cross actors. So no handler
receives an actor-reach kind, and one injected through a raw door finds no
row. The request-context table keeps taking any `Kind`.

### 5. `WireMail` bounds the typed wire doors

`pub trait WireMail: Kind + CrossesWire {}` has a blanket impl. It bounds
`WasmActor`'s `Config`, which the MCP `load_component` encodes from JSON
(`crates/aether-actor/src/wasm/mod.rs`), and the FleetHarness typed sends,
which cross the real RPC wire (`crates/aether-harness-fleet/src/lib.rs`).
Journal records are `Storage` values, whose leaf lists name no proof and no
reply route, and typed saved state is serde, which neither a proof nor a
reply route implements, so both stay closed by type.

The raw doors carry only a `KindId`: the RPC `Call`, `boundary::accept`
(`crates/aether-substrate/src/mail/boundary.rs`), session and hub egress, and
the RPC reply-out. They cannot take a bound. Their refusal reads the kind's
registered schema, as the egress rewrites `inline_payload` and
`plain_payload` (`crates/aether-substrate/src/mail/attachments/mod.rs`)
already walk it, so it adds no list or inventory
([R-0026](../guide/contributing/design-rules.md#r-0026)). Section 8 stages it.

### 6. Contextual versus non-contextual is a separate axis

Reach says where bytes may go; contextual says whether their decode needs the
engine. The two are independent, and this decision leaves the contextual
axis as ADR-0231 §3 built it.

A reference leaf is contextual. It encodes its position under its own
`SchemaType::TypeId`, with no tagged-id arm, so the JSON codec refuses it
(`crates/aether-codec/src/encode.rs` refuses an unknown type id) and
`MailboxId`'s codec is never reached. Its decode proves the position again
through one new `Decoder` operation on the `PublishedRoutes` hook `DecodeCtx`
already holds. The registry answers with the read `Registry::stamped_sender`
makes (`crates/aether-substrate/src/mail/registry/mailbox/proven.rs`): a
route record stands at the position, and names are never reused. An
`ActorRef<R>` also checks the route's leaf namespace, and a `ProtocolRef<P>`
also checks its rows through `prove_route_covers`. The door this opens, an
implementor vouching for its own answer, is closed with the existing lint:
`DecodeCtx::routes` joins clippy.toml's `disallowed-methods`, with an allow
and a reason at the substrate's callers
([R-0010](../guide/contributing/design-rules.md#r-0010),
[R-0005](../guide/contributing/design-rules.md#r-0005)). A guest refuses a
reference leaf until its first guest carrier adds the arm, as it refuses a
`ProtocolPath<P>` today
([R-0009](../guide/contributing/design-rules.md#r-0009),
[R-0032](../guide/contributing/design-rules.md#r-0032)).

The request-context table decodes with an empty context today
(`RequestContextTable::take`), so a context holding a reference takes through
a contextual take, which lands with that reference's codec.

### 7. The guest surface is unchanged in shape

A guest context holding a `ReplyHandle` has actor reach by its field.
`send_with_context` and `take_context` are unchanged, and no host import is
added.

### 8. Staging

This decision lands in pieces, each with its first carrier
([R-0009](../guide/contributing/design-rules.md#r-0009),
[R-0028](../guide/contributing/design-rules.md#r-0028)).

**Built with this decision:** the two markers and their impls, the derive
fold, the conditional `ActorMail`, the handler requirement, and `WireMail` on
the two typed wire doors. The existing contexts get actor reach from their
`Source` and `ReplyHandle` fields, with no edit.

**With the first kind that carries a typed proof:** the contextual reference
leaf, the `Decoder` operation, the table's contextual take, the
`disallowed-methods` gate on `DecodeCtx::routes`, and the typed proofs'
codec.

**The raw-door refusal is a follow-up.** `MailboxId` and `MailId` are the
first engine-reach leaves. The typed wire doors refuse a kind holding either
by type, but the raw `KindId` doors (the RPC `Call`, `boundary::accept`,
session and hub egress, and the RPC reply-out) do not yet refuse one. That
refusal reads the registered schema and reuses the egress walk (section 5).
The kinds on main that it will govern:

| Kind | Where | Engine-reach leaf |
|---|---|---|
| `aether.trace.tail`, `aether.trace.tail_result`, `aether.trace.describe_tree_result`, `aether.trace.dispatch_traced_ack` | `crates/aether-kinds/src/trace.rs` | `MailId`, and `TraceEvent`'s and `MailNodeWire`'s `MailboxId` `sender` and `recipient` |
| `aether.trace.settled` (engine-only) | `crates/aether-kinds/src/trace.rs` | `MailId` |
| `aether.render.pre_settled` (engine-only) | `crates/aether-render/src/kinds.rs` | `MailId` |
| `aether.window.subscribe`, `aether.window.unsubscribe`, `aether.window.unsubscribe_all` | `crates/aether-window/src/kinds.rs` | `MailboxId` |
| `aether.http.server.register_route`, `aether.http.server.unregister_route`, `aether.http.server.unregister_routes_all` | `crates/aether-http/src/kinds.rs` | `MailboxId`; #6899 replaces them with typed handler paths |

## Consequences

- An actor-reach context is never mail: no actor can send it and no handler
  receives it, by type. A context that needs a reply route or a reference no
  longer needs a side map beside the table.
- A typed proof may become a field of an engine-reach kind once its codec
  lands, and is proven again at every decode; a wire-reach kind never carries
  one.
- A position may ride a core-internal kind inside the engine, and a kind that
  carries one cannot reach a typed wire door. The public APIs other code is
  written against still take and return no `MailboxId` (#6903).
- A kind gains no attribute and no id change. A hand-written `Schema` impl
  must add its markers, or its type has actor reach; the compiler names the
  site the first time the type is sent or handled.
- The reach diagnostic names the leaf, and rustc's note chain names each kind
  between the leaf and the bound.
- No wire format changes: reach is not in `Kind::ID`, the schema, or
  `describe_kinds`.

## Alternatives considered

- **A `#[kind]` flag naming the reach, with `MODE` and `REACH` consts checked
  by a `const` assertion.** Declined by the owner's ruling that a kind's reach
  is the lowest reach among its fields. A declared reach is a second source
  that must agree with the fields
  ([R-0027](../guide/contributing/design-rules.md#r-0027)), and the marker
  traits make the fact a bound
  ([R-0003](../guide/contributing/design-rules.md#r-0003)).
- **Plain where-clauses in the derived marker impls.** Stable rustc refuses
  an unsatisfied bound over a concrete type in an impl's where-clause (E0277),
  so the first kind with an actor-reach field would fail to compile rather
  than have actor reach.
- **Proofs attached to the envelope, ADR-0238's `Blob` shape, instead of a
  contextual re-proof.** It needs a second attachment type, a host proof per
  reference at `send_mail_p32`, and a separate store for the request-context
  table and the replace snapshot, where the registry hook already serves all
  three ([R-0026](../guide/contributing/design-rules.md#r-0026)).
- **Extend ADR-0233's link-time list to refuse narrow-reach kinds at the raw
  doors.** The handler requirement refuses actor-reach injection statically.
  For engine reach, the registry schema that the egress walk already reads
  answers the same question, with no list
  ([R-0026](../guide/contributing/design-rules.md#r-0026)).
- **Land the proof codec, the `Decoder` operation, and the raw-door refusal
  with the markers.** No kind on main carries a proof yet
  ([R-0009](../guide/contributing/design-rules.md#r-0009),
  [R-0028](../guide/contributing/design-rules.md#r-0028)).
- **Edit ADR-0233 in place instead of adding this ADR.** ADR-0233 decides one
  class, engine-only mail. Reach also rewrites ADR-0230 §1's export rule and
  ADR-0231 §3's decode.
