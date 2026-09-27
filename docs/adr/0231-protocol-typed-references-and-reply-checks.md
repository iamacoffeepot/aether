# ADR-0231: Protocol Links and Static Reply Checks

- **Status:** Proposed
- **Date:** 2026-09-23

Actors link through contracts the compiler checks. A reference or a path
names the protocol its holder needs, and the compiler proves, where the path
is written, that the target's handlers meet it. A path that arrives is proven
by its decode, against the engine it arrives in, so a typed path that exists
is valid. The one runtime step left is proving, on receipt, that the target
is live.

[ADR-0230](0230-proven-actor-references.md) made a reference a proof of
identity: an actor of this type reached `Live` at this position, in this engine
session. This ADR makes references and paths prove the target's contract
too, meaning which kinds it handles and how it answers each one. A send reads
the contract off the reference's type, so the send site has nothing to check
beyond the types.

Amends [ADR-0075](0075-actor-typed-sender-api-and-chassis-cap-marker-split.md)
decision 1 and [ADR-0076](0076-collapse-cap-facade-pattern.md) (`HandlesKind`
gating), [ADR-0109](0109-handler-reply-contracts.md) §5 (the native manifest's reply
field), [ADR-0227](0227-reply-contracts-are-type-markers.md) (the reply
markers), [ADR-0230](0230-proven-actor-references.md)
(protocol references and protocol paths, and the end of untyped sends through
an erased reference), [ADR-0232](0232-flat-ctx-send-verbs.md) (`send_to`),
and the replace contract of
[ADR-0022](0022-drain-on-swap.md), [ADR-0038](0038-actor-per-component-dispatch.md),
[ADR-0101](0101-replace-hooks-on-ffiactor.md) and ADR-0169, and the
inline-child markers of [ADR-0114](0114-inline-child-actors.md). The full list
is under [Amendments](#amendments).

## Context

A reply is ordinary mail addressed to whoever sent the request. If the
requester has no handler for the reply's kind, the reply lands on nothing: a
strict receiver logs and drops it at the dispatch miss
(`typed_then_fallback_or_warn`,
`crates/aether-substrate/src/actor/native/slot/dispatch.rs`), and a receiver
with a `#[fallback]` swallows it. Nothing at the call site says that the
request elicits a reply at all.

The compiler already knows half of this. `#[actor]` emits, per handler, a
`HandlesKind<K>` marker and a `Contract<K>` row whose `Reply` is the reply kind
`O`, `Silent`, or `Undeclared` for a manual handler
(`crates/aether-actor/src/model/contract.rs`,
`crates/aether-actor-derive/src/reply_markers.rs`), and per actor one
`Contracts` impl carrying the same rows twice: as the type-level list
`Contracts::Rows`, which each `Contract<K>` row names its position in (§10), and
as the `Contracts::CONTRACTS` list in the manifest's `ReplyContract`
vocabulary. It also emits one `Declared` impl listing the actor's
`depends(..)` and `spawns(..)` entries, which each `DependsOn<R>` and
`Spawns<C>` impl names its position in (§10). Typed sends bound on
`HandlesKind<K>` only. No bound connects the
target's reply to the sender, so a caller can send `LoadMesh` to an actor that
replies `MeshLoadResult` from a sender with no `MeshLoadResult` handler, and it
compiles.

Three further gaps:

- **Erased references carry no contract.** An `ErasedActorRef`
  (`crates/aether-actor/src/reference/erased_actor_ref.rs`), from
  `ctx.sender()`, `resolve_path`, or a native `resolve_live`, is a `Target` for
  every kind (`crates/aether-actor/src/reference/target.rs`), and the native
  ctx also sends through one by raw `KindId` (`send_envelope_tracked_to`,
  `send_envelope_detached_to`), fans out through a set of them (`fanout`), and
  relays through one (`forward_to`), all in
  `crates/aether-substrate/src/actor/native/ctx/send.rs`. A kind the target
  does not handle is caught only at the target's dispatch miss.
- **A link between actors carries no contract.** When one actor tells another
  where a third lives, the field is an `ErasedActorPath`
  (`crates/aether-data/src/reference/actor_path.rs`), which names nothing
  about what lives there, so the receiver learns what the target handles
  only by trying it, or by a runtime check. No kind, config, or journal
  record carries the typed `Address<R>`
  (`crates/aether-data/src/reference/address.rs`); its one production use is
  the embedder's child door (`crates/aether-substrate/src/chassis/builder/built.rs`),
  which needs only the parent's proof and the key. A link typed by the
  target's actor type would also make its holder depend on an
  implementation type; a link typed by a protocol does not.
- **The rows are published, and nothing proves against them yet.** A route
  record (`RouteRecord`,
  `crates/aether-substrate/src/mail/registry/mailbox/route.rs`) carries a
  `RouteContract` on its `Live` and `Alias` lifecycles (#6844): the actor's
  `(KindId, ReplyContract)` rows, sorted, and whether it has a `#[fallback]`
  (`crates/aether-substrate/src/mail/registry/contract.rs`), written by the
  same registry apply that changes the lifecycle, so one route-table lookup
  answers both "is it `Live`" and "what does it cover". No door reads them
  yet: there is no cast, and no decode proves a protocol path against them. The DAG
  validator's `CapabilityRegistry`
  (`crates/aether-substrate/src/mail/capability.rs`) still keeps its own
  copy, written under its own lock on its own schedule.

The owner, on typed links:

> It'd be nice if we could have compile time actor path checks like this path is going to 100% be the protocol you need it to be, as verified. It'd solve a LOT of headache. Liveness is a different issue.

> It could essentially prevent runtime casts and actually define contracts that actors have to meet when linking together.

On what a link is made of:

> actor paths are the only way we should ever, ever, communicate about external actors. address conflicts and is poisoning the road about what needs to exist.

The protocol-typed link:

> should be around an actor path and is meant as a compile time assurance.

And on erased sends:

> Honestly with this we could remove generic sends except to actors that have fallback

Out of scope: responder-side delivery faults (a reply handle answering the
wrong requester across a replace, a lost deferred reply, a panicking blocking
worker) are fixed where they occur. Runtime delivery and liveness stay best
effort, with monitors as the way to observe a peer's death. This ADR closes the
caller-side class, a reply the requester cannot receive, the untyped send
through an erased reference, and the untyped link.

## Decision

The rules:

1. A typed send compiles only when the sender handles the target's reply for
   the sent kind (§1).
2. A protocol is a zero-sized type naming contract rows; the trait solver
   decides whether a target covers it (§2).
3. A link is typed by what its holder needs: a protocol, as `ProtocolRef<P>`
   (the proof) and `ProtocolPath<P>` (the description that crosses a
   boundary), or a whole actor type, as ADR-0230's `ActorRef<R>` and
   `ActorPath<R>`. A protocol path is made by narrowing an actor path the
   compiler checked against `P`, or by a decode that proves `P` against the
   engine's published rows. A typed path that exists is valid, and receipt
   proves only liveness (§3).
4. Every route publishes its contract rows and whether it has a `#[fallback]`
   when it goes `Live`. An `ErasedActorRef` has no send verb: a send goes
   through a typed reference, or through a cast of the erased reference over
   the published rows. The only untyped targets are fallback actors (§4).
5. A replace may add rows and may not drop or change one (§5).
6. A manual row declares no reply kind and covers no protocol row (§6).
7. Every ctx `#[actor]` hands out is typed by its actor (§7).
8. A subscriber's or watcher's handler for the event is silent or manual (§8).
9. Relays forward through typed references; the reply they pass through is not
   checked (§9).
10. A contract row, a declared dependency, a declared inline child, and a
    module listing exist only at a position of their actor's or module's one
    declaration list (§10).

| § | Decision | On main |
|---|---|---|
| 1 | Static reply check on typed sends | not built |
| 2 | `#[protocol]` and `CoveredBy` | built: `Row`, `RowReply`, `RowSet`, `CoversRows`, `Protocol`, `CoveredBy` (`crates/aether-actor/src/model/protocol.rs`) and `#[protocol]` (`crates/aether-actor-derive/src/protocol.rs`), over the per-handler `Contract<K>` rows and per-actor `Contracts::CONTRACTS`; `includes` and protocol-to-protocol coverage are not built |
| 3 | `ProtocolRef<P>`, `ProtocolPath<P>`, contextual decode, `resolve` | `ProtocolPath<P>` and `ActorPath::narrow` built (`crates/aether-actor/src/path/`), with the path text as their only wire form. `ProtocolPath<P>`'s decode proves coverage against the live route at its path through `Kind::decode_with` and `DecodeCtx` (`crates/aether-data/src/wire/context.rs`, over the registry's `PublishedRoutes` answer in `crates/aether-substrate/src/mail/registry/mailbox/resolve.rs`), and it has no `Deserialize`; `ActorPath<R>`'s decode checks its leaf namespace, and the type constructors `ActorPath::<R>::instance` and `ActorPath::<C>::child` replace the declared links. `ProtocolRef<P>` built (`crates/aether-actor/src/reference/protocol_ref.rs`), a `Target` for each kind `P` lists through a row index the compiler infers (`RowAt`, `crates/aether-actor/src/model/protocol.rs`), with the native liveness-only `resolve` over a `ProtocolPath<P>` (`Registry::resolve_protocol`, `crates/aether-substrate/src/mail/registry/mailbox/proven.rs`). Not built: the guest's published-routes answer (ADR-0241), so a guest refuses a `ProtocolPath<P>` at decode; reference narrowing |
| 4 | Published rows, no erased send verb, the cast | published rows built on the route record for both transports (`RouteContract`, `crates/aether-substrate/src/mail/registry/contract.rs`); the native cast built for its `Subscriber<K>` arm (`ctx.cast`, `CastTarget`; `Registry::cast` in `crates/aether-substrate/src/mail/registry/mailbox/proven.rs`), with the window's and the lifecycle capability's typed subscriber fan-out; the erased send verb's removal (#6895), the cast's protocol and `AnyKind` arms, and a guest cast are not |
| 5 | Replace preserves contracts | built, the fallback rule included: `crates/aether-data/src/contract.rs`, `crates/aether-substrate/src/mail/registry/contract.rs`, `crates/aether-component/src/trampoline/runtime/contract.rs` |
| 6 | Manual rows | built: `Undeclared` row, `ReplyContract::Manual` on both manifests |
| 7 | Ctx typed by its actor | built, every ctx on both transports |
| 8 | Silent subscribers and watchers | built for the wasm `subscribe` (`crates/aether-actor/src/wasm/ctx/subscribe.rs`) and for the window's and the lifecycle capability's subscriber references (`ProtocolRef<Subscriber<K>>`, `crates/aether-window/src/runtime/subscribers.rs`, `crates/aether-lifecycle/src/subscribers.rs`); the `monitor` bound is not |
| 9 | Relays | `forward_to` and `DeferredReply::hand_off` exist; the typed target is not built |
| 10 | Markers exist only at a declared position | built: `Here`, `There<I>`, `Gap`, `ListIndex`, `RowIndex`, `Declared` (`crates/aether-actor/src/model/declared.rs`), `Contracts::Rows` and the `Index` of `Contract<K>`, `DependsOn<R>`, `Spawns<C>`, and `Rebuildable<M>`, emitted by `#[actor]` (`crates/aether-actor-derive/src/reply_markers.rs`, `wasm_expand.rs`, `native_expand.rs`, `handler_set.rs`) and `export!` (`crates/aether-actor/src/wasm/mod.rs`). The checks read the declaration lists: the native birth check (`crates/aether-substrate/src/actor/native/dependencies.rs`) and `export!`'s guest `Dependency` records read `Declared::Depends` through `DependencyList`, and `export!`'s inline-child coverage check reads `Declared::Spawns` through `ListedIn`, so a hand-written `Declared` is checked as an emitted one is. A type no `#[actor]` built still writes its own `Contracts` and dispatch (#6887, #6888) |

### 1. The static reply check

A typed send of `K` to a target whose row for `K` replies `O` (single `-> O`,
deferred `-> Pending<O>`) requires the sending actor `A: HandlesKind<O>`.
Otherwise it does not compile. A silent row (`-> ()`) needs nothing from the
sender, and neither does a manual row, whose replier picks the kind at run
time (§6).

```rust
/// `A` can receive what a row sends back.
pub trait ReplyHandledBy<A> {}
impl<A> ReplyHandledBy<A> for Silent {}
impl<A> ReplyHandledBy<A> for Undeclared {}
impl<A: HandlesKind<O>, O: ActorMail> ReplyHandledBy<A> for O {}

impl<A, M: ReplyMode> WasmCtx<'_, A, M> {
    pub fn send<R, K: ActorMail>(&mut self, payload: &K)
    where
        A: DependsOn<R>,
        R: Contract<K>,
        <R as Contract<K>>::Reply: ReplyHandledBy<A>;

    pub fn send_to<K: ActorMail, T: Target<K>>(&mut self, target: T, payload: &K)
    where
        T::Reply: ReplyHandledBy<A>;

    /// The one written opt-out: send a request whose reply this actor does
    /// not handle. `send_to_ignoring_reply` is its held-reference twin.
    pub fn send_ignoring_reply<R, K: ActorMail>(&mut self, payload: &K)
    where
        A: DependsOn<R>,
        R: Contract<K>;
}
```

`Target<K>` (ADR-0232 §1) gains `type Reply: ReplyShape`, which is
`<R as Contract<K>>::Reply` for an `ActorRef<R>`. A protocol implements no
`Contract<K>` (§2), so a send through a `ProtocolRef<P>` states its bound over
the row set instead: it finds `K`'s row in `P::Rows` with a method-level
bound whose row index is inferred at the call site, and that row's reply
carries the same `ReplyHandledBy<A>` bound. `send_detached`, `send_tracked`, `send_many`, and the context-carrying sends
carry the same bound, on the native ctx as on the guest's. `Silent` and
`Undeclared` never implement `Kind`, which keeps the three impls disjoint, as
`ReplyShape`'s three impls already are.

A failing check surfaces as the nested obligation `A: HandlesKind<O>`, so
`HandlesKind` gains a `#[diagnostic::on_unimplemented]` message that names the
missing handler and points at `send_ignoring_reply`.

What the check does not change:

- Replies stay ordinary mail delivered to the sender's typed handler. Runtime
  reply delivery, settlement, correlation, and request contexts are unchanged.
- Several outstanding requests of the same kind need nothing special. The check
  is per type, and correlation stays the runtime's job.
- `#[fallback]` does not count as handling. It emits no `HandlesKind` and no
  `Contract` row, so it satisfies no reply check on the sender.
- Non-actor senders are exempt: the MCP tools, an RPC `Call`, the substrate
  harness's `send_and_settle` / `send_and_await_reply`, chassis threads, and an
  embedder's `PassiveChassis` sends. They have no handler table, and each
  already receives any reply kind as data.
- `send_ignoring_reply` delivers exactly as `send` does. The reply still
  arrives at the sender and takes the dispatch-miss path. The verb exists so
  the discard is written, and the compiler enumerates every site that needs it.
- A reply that is itself a request is unchecked on its return hop. When `A`'s handler
  for the reply `O` itself replies `P`, `A`'s call site is checked and the `P`
  that returns to the responder is not, because a reply is not a send site. An
  exchange that needs a second round trip spells it as a second typed send.

### 2. Protocols are zero-sized types composing contract rows

A protocol names a set of contract rows under a stable type, independent of any
implementation. It is written as an attribute on a real trait item whose method
signatures mirror a handler list, never as a bang macro wrapping a DSL.

```rust
#[protocol]
pub trait MeshLoader {
    fn load(mail: LoadMesh) -> MeshLoadResult;
    fn ping(mail: Ping) -> Pong;
    fn set_mode(mail: SetMode);
}
```

A signature with no return type is a silent row and `-> O` is a single row; a
target's deferred `-> Pending<O>` handler has the row `O` and covers it, and
`-> Pending<O>` in a protocol is refused. There is no spelling for a manual row
(§6). The method name labels the row in the protocol's rustdoc; a target
matches rows by kind, never by method name. Each parameter needs a name or `_`,
because an attribute's input must parse.

The attribute replaces the trait with a unit struct, so the trait never exists
as a trait object and a protocol costs nothing at run time. The struct's docs
list each row under its method name. The expansion is only a declaration of the
rows as a type:

```rust
pub struct MeshLoader;

impl Protocol for MeshLoader {
    type Rows = (Row<LoadMesh, MeshLoadResult>, Row<Ping, Pong>, Row<SetMode, Silent>);
}
```

Everything else is computed from `Rows` in `aether-actor`
(`crates/aether-actor/src/model/protocol.rs`), through sealed traits:

```rust
/// One row: kind `K` answered with `O`. A type only, never constructed.
pub struct Row<K, O>(PhantomData<fn() -> (K, O)>);

/// A reply kind or `Silent`. `Undeclared` is left out (§6).
pub trait RowReply: ReplyShape + Sealed {}

/// Tuples of one to 16 rows.
pub trait RowSet: Sealed {
    const CONTRACTS: &'static [(KindId, ReplyContract)];
}

/// One blanket per tuple arity, the only place coverage is computed.
pub trait CoversRows<Rows>: Sealed<Rows> {}
impl<T, K1: Kind, O1: RowReply, /* … */> CoversRows<(Row<K1, O1>, /* … */)> for T
where
    T: Contract<K1, Reply = O1>, /* … */
{
}

pub trait Protocol {
    type Rows: RowSet;
}

/// Any target whose rows cover the protocol narrows to it.
pub trait CoveredBy<R>: Sealed<R> {}
impl<P: Protocol, R: CoversRows<P::Rows>> CoveredBy<R> for P {}
```

`Protocol` is safe to implement by hand, because an impl only declares rows:
the rows' list and who covers them follow from the declaration, so no impl can
state either one differently. `CoveredBy` has only its blanket: a hand-written
impl on a type that is not a protocol fails the private seal, and on a protocol
it collides with the blanket.

The trait solver decides coverage. A target covers a row only with the same
kind and the exact reply type: a silent row is covered only by a silent
handler, and a row `O` only by a handler that replies `O`. A kind the target
handles only through `#[fallback]` has no row and never covers. `CoveredBy<R>`
holds for any actor whose `#[actor]` rows match. `RowSet::CONTRACTS` maps each
row as `#[actor]`'s `Contracts::CONTRACTS` does, `One(O::ID)` for a row `O` and
`None` for a silent row, so the protocol's list and a live target's published
rows (§4) compare in one vocabulary.

A protocol implements no `Contract<K>`. Deriving one from `Rows` needs a
type-level lookup of `K` in the tuple, and a blanket impl cannot express it:
positional impls overlap (`E0119`) when two rows could share a kind, and an
inferred row index is an unconstrained impl parameter (`E0207`). Two places
need a protocol's row for `K`: a `ProtocolRef<P>` send, and narrowing one
protocol reference to another, which needs one protocol covered by another
(§3). Both find the row with a
method-level bound on `P::Rows` whose row index is inferred at the call site.

**Composition.** `#[protocol(includes(Pingable, Describable))]` concatenates the
included protocols' rows into `Rows`, so the `CONTRACTS` list and coverage
follow from the concatenation. A proc macro cannot read another item, so every
`#[protocol]` also emits a hidden `macro_rules!` bridge carrying its rows, and
`includes` invokes it, the technique ADR-0169 uses to paste a handler set's
markers. The bridge carries the protocol's own path beside its rows, and the
expansion dedupes by protocol, so a protocol reached twice through a diamond of
`includes` contributes its rows once. Two distinct protocols that list the same
kind are refused, as a kind listed twice in one protocol is.

### 3. Protocol links

A link is typed by what its holder needs, not by what the target is. The
paths and references come in three matching pairs:

| | Path (a description, with a codec) | Reference (a proof, no codec) |
|---|---|---|
| untyped | `ErasedActorPath` | `ErasedActorRef` |
| actor-typed | `ActorPath<R>` (ADR-0230 §2) | `ActorRef<R>` |
| protocol-typed | `ProtocolPath<P>` | `ProtocolRef<P>` |

ADR-0230 owns the actor-typed pair. This section adds the protocol-typed
pair, made from something the compiler already checked against `P`, or
decoded against an engine whose published rows cover `P`.
The references are unchanged by the paths; whether the three references
should become one type is a separate question, not decided here.

| Type | Claims | Made by | Can |
|---|---|---|---|
| `ProtocolRef<P>` | an actor whose rows cover `P` reached `Live` here, in this session | narrowing an `ActorRef<R>` or a `ProtocolRef<Q>`; `ctx.resolve` of a `ProtocolPath<P>`; the guard cast (§4) | send the kinds `P` lists, with §1's reply check; monitor; name its canonical path; be held in actor memory. No codec. |
| `ProtocolPath<P>` | the text is a well-formed canonical path naming an actor whose rows cover `P`: narrowed, because the compiler proved the source actor type covers `P`; decoded, because the decode proved the engine's published rows for the live route at the path cover `P`. Nothing about existence after the decode. | narrowing an `ActorPath<R>`; a contextual decode (`decode_with`) against the engine's context | be a kind field, whose kind is then declared `no_serde`; be compared and displayed; be resolved. It grants no send. |

#### `ProtocolRef<P>`

```rust
pub struct ProtocolRef<P> {
    target: ErasedActorRef,
    _protocol: PhantomData<fn() -> P>,
}

impl<R> ActorRef<R> {
    pub fn narrow<P: CoveredBy<R>>(self) -> ProtocolRef<P>;
}
impl<P> ProtocolRef<P> {
    pub fn narrow<Q: CoveredBy<P>>(self) -> ProtocolRef<Q>;
}
```

`ProtocolRef<P>` is the existing proven target plus a phantom protocol. Sends
through it monomorphize against `P`'s rows: no vtable, no per-send lookup,
nothing added to the runtime send path. It is a `Target<K>` for each kind `P`
lists, so `ctx.send_to(&loader, &LoadMesh { path })` takes it as it takes an
`ActorRef<R>`. A narrowed reference is a capability view: its holder may send
only what `P` lists, whatever else the target handles. Like every proven type
it has no codec (ADR-0230 §1).

#### `ProtocolPath<P>`

```rust
// aether-actor
pub struct ProtocolPath<P> {
    path: ErasedActorPath,
    _protocol: PhantomData<fn() -> P>,
}

impl<R> ActorPath<R> {
    /// The same text under a narrower claim: type-level only, infallible.
    pub fn narrow<P: CoveredBy<R>>(&self) -> ProtocolPath<P>;
}
```

A `ProtocolPath<P>` is an `ErasedActorPath` and a phantom protocol. In code it
is made only by narrowing an `ActorPath<R>`, as a `ProtocolRef<P>` is narrowed
from an `ActorRef<R>`, and `P: CoveredBy<R>` is the compile-time check, over
the sealed coverage §2 defines. Narrowing keeps the text the actor path was
written with (ADR-0230 §2: each step a type's `NAMESPACE` and its key), so a
protocol path is canonical and has no holes; it reads no registry, folds
nothing, and cannot fail. The one other way a `ProtocolPath<P>` comes into
existence is its contextual decode, below.

`ProtocolPath<P>` lives in `aether-actor`, beside `Protocol`, `CoveredBy`, and
`ActorPath<R>`, and not in `aether-data` beside `ErasedActorPath`. Its
constructor from a bare `ErasedActorPath` is private to that crate. In
`aether-data` the constructor would have to be public or `#[doc(hidden)]` for
`aether-actor` to call it, and either door would let any crate attach a `P`
claim to arbitrary text.

On the wire a `ProtocolPath<P>` is the path text only, with
`ErasedActorPath`'s schema and codec, implemented in `aether-actor`. A typed
path adds no schema node, so the JSON codec, MCP, and the hub see a path.
`Debug` prints the path, because a path is a name, not a position.

#### Contextual decode

A `ProtocolPath<P>` claims that the actor it names publishes rows covering
`P`. That is a fact about an engine, not about the text, so no decode of the
text alone can prove it. A `ProtocolPath<P>` is therefore a contextual type:
its decode proves the claim against a context the engine supplies, and
refuses without one.

- Every kind decodes through one body, `Kind::decode_with(bytes, &mut ctx)`,
  which returns the value or a named `wire::Error`. The `DecodeCtx` is built
  from what the decoding ctx already holds: its inbound mail's blob
  attachments and its mail registry. It keeps both in private fields and
  offers a leaf only the two decode operations: resolve a blob hash, and
  prove that the live route at a path publishes a set of rows. Neither the
  resolver nor the registry is reachable through it.
- A `ProtocolPath<P>` leaf checks the grammar and canonical form, as every
  typed path's decode does, then asks the context to prove that the `Live`
  route standing under exactly its path publishes every row of
  `<P::Rows as RowSet>::CONTRACTS`, compared as the cast compares them (§4).
  The context's answer is the mail registry's published route contracts
  (§4). The leaf refuses `ProtocolPathUnchecked` when the context has no
  registry, `ProtocolPathUnpublished` when no `Live` route stands at the
  path, and `UncoveredProtocolPath` naming the path and the first row that
  is missing or different.
- The plain shorthand (`decode_from_bytes`, `wire::decode_from_slice`)
  decodes with an empty context, so it refuses every `ProtocolPath<P>`.
  Serde carries no context, so `ProtocolPath<P>` has no `Deserialize`, and a
  kind carrying one is declared `no_serde`.
- Native dispatch decodes each typed arm with the inbound's attachments and
  the registry, and logs a refusal at warn, naming the kind and the error,
  before treating the mail as a miss. A guest's context carries its blob
  holds only, so a guest decode of a `ProtocolPath<P>` refuses until
  [ADR-0241](0241-code-is-published-not-loaded.md) gives it a published-routes
  answer.

So a `ProtocolPath<P>` that exists is valid by construction: narrowed where
the compiler proved `P: CoveredBy<R>`, or decoded against the engine it is
in. Bytes from another engine, a config file, a journal record, or an
operator's MCP call become a `ProtocolPath<P>` only in an engine where a live
route at the path covers `P`. A sender is responsible for the validity of
what it sends, and a value that exists is valid: the claim is proven once,
where the value comes into existence, and nothing downstream checks it again,
neither the RPC door nor `resolve`.

`ProtocolPath<P>` is the class [ADR-0238](0238-engine-blob-store.md)
decision 6 records for `Blob`: a type whose meaning depends on where it is.
A shared (tag-1) `Blob` field resolves through the same context, and a
resolved blob always decodes as a reference to the store entry; inline
(tag-0) bytes mean the same bytes anywhere, so they need no context and
decode owned. A protocol path's claim means nothing apart from an engine, so
its decode always consults one.

A path names a slot by name, so the same text names the same slot in any
session. That is why a typed path may sit in a config or in saved state: the
engine that reads it back proves its claim at the decode, and whether
anything is live there, the receipt proves. A reader that decodes with the
plain shorthand or serde, as config and saved-state decodes do today, has no
context, so it refuses a `ProtocolPath<P>`.

#### Why two typed paths

An actor-typed path and a protocol-typed path claim different things, as the
two references do. `resolve` of an `ActorPath<R>` yields an `ActorRef<R>`,
through which every kind `R` handles is sendable, its manual rows included.
A protocol names single and silent rows only, and a manual row covers no
protocol row (§6), so no protocol-typed link can carry a request to a manual
handler. The Bloomery bootstrap sends `aether.bloomery.driver.call` to the
driver, which answers it from a manual handler (`on_call`,
`crates/aether-bloomery-driver/src/actor/mod.rs`), so it names the driver
by `ActorPath<BundleDriver>` (ADR-0240 D8). A holder that needs only some
rows, and should not depend on the implementation type, takes a
`ProtocolPath<P>` narrowed by whoever can name it, as the Bloomery workspace
takes its storage source (ADR-0240 D7).

#### Receipt

```rust
ctx.resolve(&path) // &ProtocolPath<P> -> Result<ProtocolRef<P>, ResolveError>
                   // &ActorPath<R>    -> Result<ActorRef<R>, ResolveError>   (ADR-0230 §3)
```

`resolve` is ADR-0230 §3's verb for a typed path, held or arrived in mail,
config, or saved state: one spelling, the name ADR-0230 reserves, and the
path's type decides the proof's. It runs once, in the handler that holds the
path. A typed path is canonical, so it never expands; it compiles to its
position by the lineage fold, pure computation over its segments with no
lookup, and one route-table lookup (§4) then checks that a route stands at
that position under that canonical name and is `Live`, or refuses
`ResolveError::NotLive` (never registered, still `Starting`, or `Dropped`);
the name check guards a fold collision.

That is the whole receipt: `resolve` proves liveness, the one claim a path
cannot carry. Everything else was proven when the path came into existence:
an `ActorPath<R>`'s leaf namespace by its constructor or its decode
(ADR-0230 §2), and a `ProtocolPath<P>`'s coverage by narrowing or by its
contextual decode. Within one engine the rows behind a path do not change
under it (§4, "Build skew"), so no row is compared at receipt and no answer
is kept per route. A replace never makes a resolve fail, because §5 refuses
a replace that drops or changes a row; a resolve fails on liveness only.
Every refusal names the path, never a position.

A path in a kind or config that its receiver will send to is a typed path,
an `ActorPath<R>` or a `ProtocolPath<P>` by what the holder needs, and it
arrives typed; it is never an `ErasedActorPath`. An `ErasedActorPath` at the
untyped boundary, an MCP tool argument or an RPC `Call` recipient, is
delivered through the boundary's stand-in (§4). An `ErasedActorPath` that
only names an actor is proven, where a proof is needed, by `resolve_path`,
after filling a short path's holes from the generated root and child
declarations, a static inventory rather than the live tree; the
`ErasedActorRef` it returns serves identity and monitoring, and sends
nothing.

Each arm lands with its consumer. The native arm over a `ProtocolPath<P>`
serves the Bloomery workspace's receipt of a request's storage source and
the window's and the lifecycle capability's explicit subscribe and
unsubscribe receipts, and the guest arm over an `ActorPath<R>` serves the Bloomery bootstrap resolving
its unit's journal and driver
([ADR-0240](0240-several-bloomery-journal-units-per-engine.md) D7, D8). The
other two arms come with their first callers. A guest's call is one host
call, as `resolve_path`'s is.

**Consumer.** The Bloomery workspace's `Run` and `Import` carry
`source: ProtocolPath<ArtifactStorage>`, which each unit's driver narrows
from its journal's actor path,
`ActorPath::<JournalActor>::instance(&key).narrow::<ArtifactStorage>()`.
`Run` and `Import` are therefore contextual kinds: the workspace's dispatch
decodes each against the engine, which proves the journal's rows cover
`ArtifactStorage`, and the workspace resolves the source on receipt
(ADR-0240 D7). The window's `aether.window.subscribe` and `unsubscribe` and
the lifecycle capability's `aether.lifecycle.subscribe` and `unsubscribe`
carry their subscriber as a `ProtocolPath<Subscriber<K>>` (§8), inside a
subscription enum with one variant per published kind (`WindowSubscription`,
`LifecycleSubscription`), so the sender chooses the event and the path's
protocol is fixed by the variant. Each publisher decodes the request against
the engine and resolves the path on receipt.

### 4. Published rows, typed sends, and the guard cast

#### Rows are published when a route goes `Live`

A route publishes its contract rows, `(KindId, ReplyContract)` pairs, and
whether it has a `#[fallback]`, on its `RouteRecord` when it goes `Live`, or,
for an inline child's `Alias` route, when the alias is staged. The cast and
the engine's context for a contextual decode (§3) read them there, so one
published-route read answers both "is it `Live`" and "what does it cover".
The context of a `decode_with` comes from the receiving ctx's own mail
registry and attachments, the machinery `resolve` uses: the registry answers
the rows the `Live` route standing under exactly the path published. A path
with no published `Live` route, such as one to a type that is linked but not
spawned or to a route still `Starting`, is therefore refused at decode until
ADR-0241's namespace publication table answers for namespaces that are
published but not yet spawned.

| Route | Rows come from |
|---|---|
| Native actor | its static `Contracts::CONTRACTS` and its inputs manifest's fallback record |
| Wasm component | the trampoline's `ActorInputs` for the hosted type (`capabilities.handlers`, `capabilities.fallback`) |
| Inline child (`RouteLifecycle::Alias`) | the child type's `ActorInputs` from the same module map, private children from `aether.kinds.inputs.private` |
| After `replace_component` | the replacement's `ActorInputs`, republished where the trampoline refreshes `actor_caps` (`crates/aether-component/src/trampoline/runtime/replace.rs`); §5 makes it a superset |

`CapabilityRegistry` keeps serving the DAG validator and `describe_component`.
It is not the proof source, because its writes are not ordered with the
route's lifecycle.

#### An erased reference has no send verb

An `ErasedActorRef` proves only that some actor reached `Live` there. It keeps
every use that is not a send: reply, monitor, be a table key, name its
canonical path, and be cast. It is not a `Target`, and the ctx has no verb that
sends through it. A send goes through:

- an `ActorRef<R>` or a `ProtocolRef<P>`, checked by kind at compile time
  (§1, §3); or
- a `ProtocolRef<AnyKind>`, which sends any kind, made only by the cast below
  and only for a target whose published route has a `#[fallback]`.

The only untyped targets are fallback actors. `AnyKind` is a built-in marker
with `Contract<K, Reply = Undeclared>` for every kind; the published fallback
flag is its coverage, the way `RowSet::CONTRACTS` is a protocol's.

Replies are not sends through a reference: `-> O`, `ctx.reply`, and a reply
handle answer the requester, whose own call site was checked (§1).

#### The guard cast

```rust
ctx.cast::<T>(reference) // ErasedActorRef -> Option<ProtocolRef<T>>, T: CastTarget
```

The cast is the fallback for references that arrive untyped: the envelope
sender (`ctx.sender()`) and a native `resolve_live` answer. It runs once,
at receipt, in the handler that received the reference,
and reads the same published rows the contextual decode reads.

It is a native ctx verb over a sealed `CastTarget`, beside `resolve`, and the
rule each target admits is a method of that sealed trait. It is not a method
on `ErasedActorRef` over a public `ProveCtx` trait: any crate could implement
such a trait and hand the mint rows of its own choosing, so the cast reads
the registry's published rows through the ctx and nothing else (R-0005).
`Subscriber<K>` is today's only `CastTarget`; the protocol and `AnyKind`
arms land with their consumers, as does a guest cast.

| `T` | Succeeds when the published rows show |
|---|---|
| a protocol `P` | every row of `<P::Rows as RowSet>::CONTRACTS`, kind and `ReplyContract` alike |
| `AnyKind` | a `#[fallback]` |
| `Subscriber<K>`, the built-in one-row marker for a published kind (§8) | a row for `K` that is `None` or `Manual` |

A cast that fails returns `None`. The holder decides: refuse the request that
carried the reference, or drop the row. Nothing is parked and no mail is sent.
The native mint lives beside the other mints in
`crates/aether-substrate/src/mail/registry/mailbox/proven.rs`, and
`scripts/check-reference-mint.py` extends its pattern to it without widening
its path allowlist. The guest SDK mints its own from the host's answer.

A typed link (§3) replaces a cast wherever the link is made by actors that can
name the protocol. The cast remains for what arrives untyped.

#### Ingress bridges: a private stand-in

A bridge that delivers bytes whose kind is chosen at run time cannot name a
kind at compile time. It holds its delivery target as a reference typed by a
**stand-in**: a type private to the bridge's crate that declares itself
accepting any kind, as `AnyKind` does, without the fallback claim.

- The stand-in has no public constructor and no re-export. No other crate may
  ever define one or obtain one. Its mint is gated under ADR-0230 §4's path
  allowlist, naming the bridge module.
- It claims nothing about the target's handlers. The target's receipt stays as
  it is: a kind it has no handler for takes its dispatch miss, which a strict
  receiver warns and drops, deduplicated per kind, and a `#[fallback]`
  receives.
- It is private so the claim cannot leave the bridge: no signature outside the
  crate can name it, so no actor can hold, store, or pass one on.

The bridges today:

| Bridge | Runtime-chosen kind | Stand-in lives in |
|---|---|---|
| RPC `Call` receipt (`crates/aether-rpc/src/server/runtime.rs`) and the bundle doors `DispatchTraced` (`crates/aether-trace/src/runtime.rs`) and `CaptureFrame` (`crates/aether-render/src/runtime/mod.rs`) | the `Call`'s or bundle item's kind | `aether-substrate`'s boundary module, which proves the path and mints the deliver-only `BoundaryMail` (`crates/aether-substrate/src/mail/boundary.rs`); the item's recipient becomes the stand-in reference, and the RPC server, render, and trace hold only the item |
| The http server's routed request (`dispatch_prepared`, `crates/aether-http/src/server/runtime/state.rs`) | the kind the route holder registered in `RegisterRoute` | `aether-http`'s server runtime |

The http server's stream and websocket deliveries
(`crates/aether-http/src/server/runtime/streaming.rs`, `websocket.rs`) send
kinds fixed at compile time (`HttpRequestStreamOpen`, `HttpRequestChunk`,
`HttpRequestStreamEnd`, `HttpStreamCredit`, `WebSocketMessage`,
`WebSocketClose`), so they are not bridges: the handler is held as a
`ProtocolRef` over those rows, cast from the registrant at registration.

#### Where today's erased sends go

| Group | Sites | Becomes |
|---|---|---|
| Subscriber fan-out | `fanout` to the one consumer in `crates/aether-tcp/src/session/runtime.rs`; the window (`crates/aether-window/src/runtime/subscribers.rs`) and the lifecycle capability (`crates/aether-lifecycle/src/subscribers.rs`) already fan out through typed references | each subscriber held as `ProtocolRef<Subscriber<K>>` (§8): an explicit request carries a `ProtocolPath<Subscriber<K>>`, which decodes against the exact silent row and is resolved at receipt, and a reflexive request's sender is cast at receipt, which also admits a manual row, so a manual-row subscriber subscribes through the reflexive form; a runtime `KindId`, from a reflexive request or the synthetic window's `InjectWindowEvent`, dispatches to the typed table of the published kind it names and any other is refused |
| Relays | `forward_to` in `crates/aether-window/src/runtime/manager.rs` and `crates/aether-component/src/component/runtime/mod.rs` | a typed target for the forwarded kind (§9) |
| Ingress bridges | above | the private stand-in |
| A witness that takes every kind | the render harness observer (`observe`, `crates/aether-render/src/runtime/mod.rs`) | `ProtocolRef<AnyKind>`, cast at `wire`; the observer's route publishes a fallback |
| Any other held erased peer | e.g. `RetireWindow` to a window's children (`crates/aether-window/src/runtime/desktop/mod.rs`, `synthetic/mod.rs`) | the typed reference from the door that minted it, or a cast |

The public `send_envelope_tracked_to` and `send_envelope_detached_to` go: no
generic send remains for authors.

#### Build skew

`#[actor]` emits the same `CONTRACTS` list for an actor `R` that `#[protocol]`
emits for a protocol. Within one engine those rows are never compared with a
loaded target's published rows: not per send, not per resolve, and not when a
door mints an `ActorRef<R>`. A route's rows are fixed or only grow. A native
actor's are its binary's, and a native `R` and its caller are one binary; a
component's are republished only when §5 finds no dropped or changed row. So
the rows behind a reference or a path never change under it, and a protocol
path's claim was proven against the engine's published rows when it was
decoded (§3).

### 5. Replace preserves contracts

A `replace_component` whose replacement drops or changes any contract row its
predecessor declared is refused before the swap, and the old module keeps
running. The refusal names the actor and the kind:
`"<actor> replacement changes its contract for <kind>"`
(`contract_refusal`, `crates/aether-component/src/trampoline/runtime/contract.rs`).
Added rows are allowed. Config, documentation, cost, and assets do not count.
A `#[fallback]` counts like a row, because a peer may hold a
`ProtocolRef<AnyKind>` (§4): a replacement that drops its predecessor's
fallback is refused, and one that adds a fallback is allowed.

The comparison (`first_contract_break`, `crates/aether-data/src/contract.rs`)
runs over the rows the cast reads. A row changes when its `ReplyContract`
changes, including silent to replying, since a caller checked against a silent
row does not handle the new reply. `KindId` hashes a kind's schema, so a
changed input schema is a dropped row. A predecessor `Manual` row is kept by
any successor row (`One(O)`, `None`, or `Manual`), because no caller was
checked against an undeclared reply; a declared row that becomes `Manual` is a
break.

This makes a contract monotone, the property ADR-0230 §1 requires of anything a
reference claims. "The target handles at least these rows" can only become more
true across replaces, so no `ProtocolRef`, no `ProtocolPath`, and no static
assumption compiled into any peer goes stale, and neither the cast nor a
contextual decode's answer is invalidated by a replace. Native capabilities are not replaced at run
time; their rows are fixed at link time.

### 6. Manual rows

A `#[handler::manual]` handler replies with any kind, or none, through
`ctx.reply` or a reply handle, so it declares no reply kind:

- Its `Contract` row is `Undeclared` and its manifest row
  `ReplyContract::Manual`, on both the wasm inputs manifest and the native
  `HandlerEntry`. Both are permanent. It gets no `Replies` marker, and reply
  handles stay untyped.
- A send to a manual row carries no reply bound (§1).
- A protocol names single or silent rows only; `#[protocol]` has no spelling
  for a manual row.
- A manual row covers no protocol row, single or silent. Statically,
  `R: Contract<K, Reply = O>` and `R: Contract<K, Reply = Silent>` both fail
  for `Reply = Undeclared`; at run time the cast and the contextual decode compare
  `ReplyContract` exactly, and `Manual` equals neither `One(O)` nor `None`. A
  target that serves a protocol declares that row single, deferred, or silent.
- The one place a manual row passes as silent is §8, the subscriber and watcher
  bound, and its runtime twin `Subscriber<K>`.
- Across a replace, a manual row may become declared and a declared row may not
  become manual (§5).

### 7. The ctx is typed by its actor

The reply check needs the sender's type. Every ctx `#[actor]` hands out is
typed by its actor, on both transports: handlers, `#[fallback]`s,
`#[handler(task)]` completions, `wire`, `unwire`, and `on_rehydrate`. A ctx
that omits its actor reads as `Self` (`WasmCtx<'_>` is `WasmCtx<'_, Self>`,
`NativeCtx<'_>` is `NativeCtx<'_, Self>`, reply mode second:
`WasmCtx<'_, Self, Manual>`). One that spells `Erased` receives the erased
view, which has no typed send (ADR-0232). A `#[handler_set]` member is typed by
the adopting actor, and the set states what its default bodies reach as
supertraits (`WidgetDefaults: DependsOn<TextCapability>`,
`WindowEndpoint: DependsOn<WindowCapability>`).

A helper generic over the ctx takes `A` and threads the bound:

```rust
fn request_mesh<A>(ctx: &mut WasmCtx<'_, A>, loader: &ProtocolRef<MeshLoader>, path: MeshPath)
where
    A: HandlesKind<MeshLoadResult>,
{
    ctx.send_to(loader, &LoadMesh { path });
}
```

### 8. Subscriptions and monitors require a silent handler

A published event and a monitor notice arrive as ordinary mail from the
publisher or the host, and nobody at the other end is waiting for an answer.
An event handler reached by subscription or monitoring must therefore be
silent or manual.

- `ctx.subscribe::<P, K>()` requires the publisher `P: Publishes<K>` and the
  subscriber's own row for `K` to be one, `<A as Contract<K>>::Reply: SilentRow`
  (`SilentRow` is sealed to `Silent` and `Undeclared`). A subscriber with no
  handler for `K`, or with one that replies, is a compile error at the
  `subscribe` call site.
- `ctx.monitor(target)` requires the same of the watcher's row for
  `MonitorNotice`.
- The publisher holds each subscriber as `ProtocolRef<Subscriber<K>>`, cast
  from the subscribe request's sender (§4). The cast is the runtime twin of
  the `subscribe` bound, for a request from a non-actor or an erased sender,
  and fan-out sends through those references. A subscriber named explicitly
  in a request, rather than by its sender, is a typed path (§3), never an
  `ErasedActorPath`: a `ProtocolPath<Subscriber<K>>`, whose decode requires
  the exact silent row `(K, None)`. A manual row covers no protocol (§6), so
  a manual-row subscriber, such as a widget root's manual `on_tick`,
  subscribes through the reflexive form, whose cast admits it.

### 9. Relays

A relay passes the original reply target through, so the reply lands on the
original caller. Today's relays are the native `forward_to`
(`crates/aether-substrate/src/actor/native/ctx/send.rs`), whose consumers are
the component host's `DropComponent` forward and the window root's forward to
the sole live window, and `DeferredReply::hand_off`
(`crates/aether-substrate/src/actor/native/offload/blocking.rs`), whose
consumer is the component host's load reply. All run from manual handlers.

- A relay's target is typed: `forward_to` takes a `Target<K>` for the forwarded
  kind, as `hand_off` already takes `ActorRef<R>` with `R: HandlesKind<K>` (§4).
- The reply the target sends back is not checked. The relaying handler's row is
  `Manual`, so its caller was checked against no reply kind (§1), and the
  target's reply reaches that caller as any manual reply does.
- A relay from a declared row, bounding the target's reply for the forwarded
  kind to equal the relaying handler's `O`, is not decided here. No consumer
  needs it.

### 10. Markers exist only at a declared position

Four per-actor markers stand for facts only a macro expansion establishes:
`Contract<K>` (a handler for `K` exists, §1, §2), `DependsOn<R>` (the birth
checked that `R` was `Live`, ADR-0230 §3), `Spawns<C>` (the spawner declared
`C`, ADR-0114 §5), and `Rebuildable<M>` (the module's `export!` lists the
type). Sealing cannot close them, because `#[actor]` and `export!` expand in
the author's crate and any path they name the author can name too, and
`unsafe` marks undefined behaviour, not a logic rule. They are closed by
coherence instead: a trait has one impl per type, and a hand-written impl
cannot add to it.

`#[actor]` emits one `Contracts` impl and one `Declared` impl per actor, and
`export!` one `ListedModule` impl for its module type, each carrying its
entries as a type-level list `(E1, (E2, (…, ())))` in declaration order. Each
marker names its entry's position in that list:

```rust
pub trait Contracts {
    type Rows; // (Row<K1, O1>, (Row<K2, O2>, (…, ()))), one entry per handler
    const CONTRACTS: &'static [(KindId, ReplyContract)];
}

pub trait Declared {
    type Depends: DependencyList; // (R1, (R2, ())) from depends(..)
    type Spawns;                  // (C1, (C2, ())) from spawns(..); () on native
}

/// Sealed; `()`, and `(R, Tail)` when `R: Singleton + CallerAddressable`,
/// `R::Resolver: DependencyResolver`, and `Tail: DependencyList`.
pub trait DependencyList {
    #[doc(hidden)]
    const FIRST: Option<&'static DependencyLink>;
}

pub trait Contract<K: Kind>: Contracts {
    type Reply: ReplyShape;
    #[doc(hidden)]
    type Index: RowIndex<<Self as Contracts>::Rows, K, Reply = Self::Reply>;
}

pub trait DependsOn<R: Singleton + CallerAddressable>: Addressable + Declared
where
    R::Resolver: DependencyResolver,
{
    #[doc(hidden)]
    type Index: ListIndex<<Self as Declared>::Depends, R>;
}

pub trait Spawns<C>: Declared {
    #[doc(hidden)]
    type Index: ListIndex<<Self as Declared>::Spawns, C>;
}

pub trait Rebuildable<M: ListedModule> {
    #[doc(hidden)]
    type Index: ListIndex<<M as ListedModule>::Listed, Self>;
}
```

A position is `Here` (the list's head) or `There<I>` (position `I` of the
tail). `ListIndex<L, T>` and `RowIndex<L, K>` are sealed and implemented
structurally in `aether-actor` only: `Here: ListIndex<(T, Tail), T>`, and
`There<I>: ListIndex<(H, Tail), T>` when `I: ListIndex<Tail, T>`; `RowIndex`
is the same walk over `Row<K, O>` entries and carries the row's reply. The
expansion knows each entry's position and writes it (`type Index =
There<Here>;`). A hand-written marker either repeats an impl the expansion
emitted (`E0119`) or names a position that holds a different kind,
dependency, or type, or no entry at all (`E0277` on the `Index` bound). A
marker that type-checks is backed by a declaration, and none of the four
traits is `unsafe`. Consumers keep their bounds (`T: Contract<K, Reply = O>`,
`A: DependsOn<R>`, `P: Spawns<C>`, `C: Rebuildable<M>`), and no turbofish
names an index.

- **Gated handlers.** A `#[cfg]`-gated handler keeps its slot: a pair of
  `#[cfg]`-ed type aliases beside the `Contracts` impl picks its `Row<K, O>`
  when its predicates hold and `Gap` when they do not. `Gap` holds no row, so
  only `There` steps over it, and every other handler's position is the same
  in every configuration.
- **Handler sets.** A native set's marker bridge has an `@rows` arm, the set's
  rows as a list, each gated row picked through the set's own gate, so the
  definer's features decide as ADR-0183 requires. The adopter's `Rows` ends
  with that list in place of `()`, and its bridge invocation passes the
  position just past its own rows; the bridge writes set row `j`'s position
  as `j` `There` steps around it. A wasm set emits no per-kind `Contract` row
  and adds nothing to `Rows`; its rows reach `CONTRACTS` as before.
- **`export!` owns its module's list.** The `@listed` arm implements the
  doc-hidden `aether_actor::wasm::ListedModule` for its `__AetherModule`
  (`type Listed = (T1, (T2, ()))`) and writes each `Rebuildable` impl at its
  position through a recursive accumulator. A type listed twice still
  collides (`E0119`). `__AetherModule` is private: `Listed` may name a bundle
  generator's private type, and only the invoking crate names the module
  type.
- **Declared entries are `pub`.** `Rows`, `Depends`, and `Spawns` are
  associated types of public-trait impls for an actor, so for a public actor
  each type they name must be nominally `pub`, or rustc refuses the impl
  with `E0446` (private type in public interface). A handled kind, a declared
  dependency, and a declared inline child are declared `pub`, and may live in
  a private module to stay out of other crates' reach; `#[actor]` enforces
  this through `E0446`, with no check of its own. `RetireWindow`
  (`crates/aether-window/src/kinds.rs`), the window manager's order to a
  child it retires, is the worked case: a `pub struct` in a private
  `mod internal`, re-exported `pub(crate)`, so it enters the public window
  instance actors' `Rows` while no other crate has a path to it.
- **The checks read the lists.** `Declared` is a supertrait of `NativeActor`
  and `WasmActor`, so every birth site has it. `DependencyList` is sealed,
  and its `FIRST` is a chain of opaque `DependencyLink`s, each a resolver tag
  and a namespace, built only by the `(R, Tail)` impl. The native birth check
  walks `<A as Declared>::Depends` through `declared_dependencies::<A>()`.
  `export!` writes a guest's `InputsRecord::Dependency` records at compile
  time from each listed type's `Declared::Depends`, after that type's
  manifest records, and the host reads them as before. `export!`'s coverage
  check requires each listed type's `Declared::Spawns` to be
  `ListedIn<__AetherModule>`, a sealed trait that holds for `()` and for
  `(C, Tail)` when `C: Rebuildable<__AetherModule>`; it is sealed so the
  invoking crate cannot implement it for a list of its own unlisted
  children. `CONTRACTS` stays emitted beside `Rows`. The native
  `DependencyEntry` inventory, the dependency records `#[actor]` wrote into
  the inherent manifest, and the hidden `__aether_listed_children` are gone,
  and a generic native actor may declare `depends(..)`, since no monomorphic
  inventory entry is needed.

The boundary is the declaration impl. A type that no `#[actor]` expansion
built writes its own `Contracts` / `Declared` impl, as it writes its own
`Dispatch` or `WasmDispatch`. Its dependencies and inline children are closed:
what its `Declared` impl lists is exactly what the birth check and the
coverage check read. Its rows are not yet: a hand-written `Contracts::Rows`,
`HandlesKind<K>`, or `Replies<K>` can claim a kind its hand-written dispatch
does not serve. Deriving dispatch from the row list is #6887, and reading
`HandlesKind<K>` and `Replies<K>` off the rows is #6888.

## Scenario sweep

"Compiles" and "compile error" describe the send site. "Runtime guard" means a
cast, a contextual decode, a receipt, or the replace refusal decides. "Exempt" means the check does
not apply.

### A. The target's row for `K`

Sender: an actor `A` with a typed ctx; target typed (`ActorRef<R>` or `ProtocolRef<P>`).

| Target's handler for `K` | Contract row | Outcome |
|---|---|---|
| silent `#[handler::single] -> ()` | `Silent` | compiles |
| single `-> O` | `O` | compiles if `A: HandlesKind<O>`, else compile error naming the missing handler |
| deferred `-> Pending<O>` | `O` | as single |
| enum reply `-> O`, `O` an enum kind | `O` | compiles if `A` handles `O`; one handler matches the variants |
| `#[handler::manual]` | `Undeclared` | compiles with no reply bound; the replier picks the kind |
| no handler | none | compile error (`T: Contract<K>` unsatisfied) |
| `#[fallback]` only | none | compile error through a typed reference; through `ProtocolRef<AnyKind>`, compiles with no reply bound |

### B. Links and paths

| Reference or path | Outcome |
|---|---|
| typed `ActorRef<R>` | static check over `R`'s rows; no row comparison at the door that minted it |
| `ProtocolRef<P>` | static check over `P`'s rows; a kind outside `P` is a compile error even when the target handles it |
| `ActorRef<R>` narrowed to `ProtocolRef<P>` | compiles if `P: CoveredBy<R>`, else compile error |
| `ActorPath::<R>::instance(&key)`, `ActorPath::<C>::child(&parent, &key)` | compiles if `R: Root + Instanced`, or `C: ChildOf<P> + Instanced`, else compile error; `R`'s own canonical path, with no registry read and no position |
| `ActorPath<R>` narrowed to `ProtocolPath<P>` | compiles if `P: CoveredBy<R>`, else compile error; the same text, with no registry read and no position |
| `ActorPath<R>` decoded from mail, config, or saved state | the decode refuses a short path or a leaf namespace other than `R::NAMESPACE` |
| `ActorPath<R>` held or received | `ctx.resolve`: `NotLive` refuses, else an `ActorRef<R>`, which sends every kind `R` handles, manual rows included; no row comparison |
| `ProtocolPath<P>` decoded from mail, config, or saved state | a contextual decode (`decode_with`) against the engine: refused unless the rows the live route at the path published cover `P`; a decode without a context is refused at decode |
| `ProtocolPath<P>` held or received, narrowed or decoded | `ctx.resolve`: `NotLive` refuses, else a `ProtocolRef<P>`; no row comparison |
| `ErasedActorPath` received untyped (config, MCP, RPC) | a field its receiver sends to is never one: it is a typed path (§3); at the MCP/RPC boundary, delivered through the stand-in (§4); otherwise `resolve_path` to an `ErasedActorRef` for naming, identity, or monitoring |
| `ErasedActorRef` (`ctx.sender()`, `resolve_path`, `resolve_live`) | no send; reply, monitor, key, or `ctx.cast::<T>(reference)` first |
| by path over the wire (MCP, RPC `Call`, `NamedMail` bundles) | exempt from §1; the boundary proves the path (ADR-0230 §3) and delivers through its stand-in |

### C. Sender

| Sender | Outcome |
|---|---|
| typed ctx, `WasmCtx<'_, Self>` / `NativeCtx<'_, Self>` | check applies with `A = Self` |
| `WireCtx<'_, '_, Self>` | check applies; subscriptions and first requests made in `wire` are checked |
| init ctx (`WasmInitCtx`) | not applicable; it sends nothing |
| generic helper over `A` | compiles when the helper states `A: HandlesKind<O>` and every caller satisfies it |
| erased ctx (`Erased`) | no typed send (ADR-0232) |
| non-actor (MCP, RPC `Call`, harness, chassis threads, embedder) | exempt |

### D. Verbs

| Verb | Outcome |
|---|---|
| `send`, `send_detached`, `send_tracked`, `send_many` | check applies |
| `send_with_context`, `send_to_with_context` | check applies; the reply handler recovers the context as today |
| `send_to` | check applies over the reference's rows; not callable with an `ErasedActorRef` |
| `send_ignoring_reply`, `send_to_ignoring_reply` | compiles for any row the target has; the reply takes the sender's dispatch-miss path, which logs |
| capability facade methods | check applies; the facade shim is a send |
| relay (`forward_to`, `DeferredReply::hand_off`) | typed target for the forwarded kind; the passed-through reply is not checked |
| `subscribe::<P, K>()` | compiles if `P: Publishes<K>` and the subscriber's row for `K` is `SilentRow` |
| `monitor(target)` | compiles if the watcher's row for `MonitorNotice` is `SilentRow` |
| publish to subscribers | through each subscriber's `ProtocolRef<Subscriber<K>>` |
| `-> O`, `ctx.reply`, reply handle | reply path; nothing to check at the responder |

### E. Edge cases

| Case | Outcome |
|---|---|
| send to self | compiles if `A` handles its own reply kind |
| sender handles `O` for another reason (subscribes to it, or serves it as a request) | compiles; one handler receives replies and other `O` mail alike and tells them apart by correlation or context |
| the reply kind is itself a request at the sender | each hop is checked at its own send; the second reply returns to the responder unchecked |
| a subscriber's or watcher's handler replies | compile error at the `subscribe` or `monitor` call site; runtime refusal at the publisher's cast for a request from an erased sender |
| wasm and native parity | same markers and bounds from the same macro; rows published from `ActorInputs` for wasm and `Contracts::CONTRACTS` for native |
| replace that drops or changes a row | runtime refusal; the old module keeps running |
| replace that drops the `#[fallback]` | runtime refusal, as for a dropped row |
| replace that adds a row | allowed; the route republishes the superset |
| a `ProtocolPath<P>` arriving in an engine whose live route at the path does not cover `P`, or where no live route stands at the path | refused at decode, naming the path and, when a route stands there, the first kind whose row is missing or different |
| a kind with a `ProtocolPath<P>` field decoded without a context | refused at decode: the empty context has no registry |
| a `ProtocolPath<P>` whose target is dead or not yet started | `NotLive` at receipt |
| a decoded `ActorPath<R>` whose leaf namespace is not `R::NAMESPACE` | refused at decode |
| cast failure | `None`; the holder refuses or drops, nothing parked |
| a peer compiled against a different build of `R` | refused at the registry-consulting `ActorRef<R>` door |
| a protocol reached twice through `includes` | rows appear once |
| a manual row where a protocol expects a single or silent row | not covered, statically and at run time |
| a hand-written `Contract<K>`, `DependsOn<R>`, `Spawns<C>`, or `Rebuildable<M>` for an actor `#[actor]` built or a module `export!` built | compile error: `E0119` when it repeats an emitted impl, else `E0277` on its `Index` bound (§10) |
| a public actor handling a crate-private kind, or declaring a crate-private dependency or inline child | compile error `E0446` at the `#[actor]`; declare the type `pub` inside a private module (§10) |
| a type no `#[actor]` built, whose hand-written `Declared::Depends` lists `R` | refused at birth, before `init`, while `R` is not `Live` (§10) |
| a type no `#[actor]` built, whose hand-written `Declared::Spawns` lists a child the `export!` that lists the type does not list | compile error at the `export!`, naming `private = [..]` (§10) |
| a type no `#[actor]` built, whose hand-written `Contracts::Rows` lists a row no dispatch arm serves | not yet closed (#6887) |

## Consequences

### Positive

- A link between actors is checked when it is made. The actor that narrows
  an actor path proves at compile time that the target's type covers the
  protocol, and a path that arrives is proven by its decode; the receiver
  gets a typed reference from one fold and one route-table lookup, with no
  cast and no row comparison.
- A typed path that exists is valid: nothing downstream of its constructor
  or its decode checks its claim again.
- Every link is an actor path on the wire, readable in a config, a log, or an
  MCP call, and no link carries a position.
- A request whose reply the requester cannot receive does not compile, and
  every deliberate discard is written as `send_ignoring_reply`.
- No actor sends an untyped kind to a target that cannot take it. The only
  untyped targets are fallback actors, and an ingress bridge's stand-in cannot
  leave its crate.
- A holder is handed exactly the rows it may use, and a peer depends on a
  protocol rather than an implementation type.
- Contracts are monotone across replace, so proofs and protocol paths never
  need revalidating because of a replace.
- A published event or a monitor notice never elicits a reply that lands on
  the publisher or the host.
- A peer built against a different build of an actor is refused at the door
  that mints its reference.

### Negative

- Erased sends migrate: fan-out tables become typed per published kind, relays
  and held erased peers take typed references or casts, and two ingress
  bridges gain a stand-in. Each cast is one registry read at receipt per
  reference.
- The compiler enumerates the sites that need `send_ignoring_reply`.
- A replace that changes a contract is refused. Changing a reply kind means
  loading the component under a new name, or adding a new request kind beside
  the old one.
- The route record grows per-route rows and a fallback flag, written at `Live`
  and at replace.
- A handler that serves a protocol row cannot be manual; it declares its row
  single, deferred, or silent.
- A typed path needs its writer to name the target's actor type and its
  placement. A writer that cannot name them sends an untyped
  `ErasedActorPath`, and the receiver pays the cast at receipt; a
  caller-relative peer is written absolute from the writer's own path first
  (ADR-0230 §1).
- A kind that carries a `ProtocolPath<P>` is contextual: it decodes only
  inside an engine, through `decode_with` against the engine's registry, and
  it has no serde decode. A guest refuses one until ADR-0241. A reader
  outside an engine sees the field through its schema, as an
  `ErasedActorPath`'s text.
- `send_ignoring_reply` still takes the sender's dispatch-miss path, which logs
  each discarded reply.

### Neutral

- Runtime reply delivery, settlement, correlation, request contexts, and
  liveness are unchanged.
- No new mail and no per-send cost. A resolve is one fold and one route-table
  lookup; a cast adds a slice comparison, and so does a contextual decode.

## Alternatives considered

- **Carry a typed actor description (`Address<R>`) in the kind and resolve by
  `R`.** The receiver must name `R`, often an implementation type from a
  crate it should not depend on, and learns only identity, not the contract
  it needs. It is also a second description system beside `ErasedActorPath`, which
  ADR-0230 rejects.
- **Carry an untyped `ErasedActorPath` and cast on receipt.** A runtime check at
  every receipt for a fact the sender's compiler already knew, and a mismatch
  found in production. It stays the form for writers that cannot name the
  target's type.
- **A protocol link that carries a folded position** (`ProtocolAddress<P>`,
  narrowed from a typed address). A position is a `MailboxId` on the wire,
  which ADR-0230 §1 forbids in any serialized type, and it reads as nothing
  in a config or a log. A path is written from `Addressable::NAMESPACE`, a
  `&'static str`, inside `aether-actor`, so the text form needs nothing from
  `Namespace`, which ADR-0230 §4 keeps textless.
- **Check a protocol path's coverage at resolve**, against the route's
  published rows, with the answer kept per route and protocol. Redundant for
  a narrowed path, whose coverage the compiler proved, and paid per use; the
  claim is proven once, where a decoded value comes into existence.
- **Compare `R`'s rows once per mint**, at each door that already consults
  the registry when it mints an `ActorRef<R>`. Within one engine a route's
  rows are fixed or only grow (§4, "Build skew"), so the comparison could
  never fail for an actor the engine admitted.
- **Check typed paths at the RPC door, with schema nodes for them.** A typed
  path is erased to its text at the RPC boundary, so the door would need a
  schema node per typed path to find the fields to check. The erasure is
  handled where the value is decoded instead, which covers every source,
  not only RPC.
- **A sender-trusted `ProtocolPath<P>`**, whose decode checks only the
  grammar and carries the writer's claim until something proves it. A value
  could exist without being true.
- **Declared links** (`#[actor(links(R))]`, `LinksTo<R>`, `ctx.link` and
  `ctx.link_child`, a hidden `__link` writer, link records, and a load-time
  link check against a target's rows). A link declares that an actor writes
  paths naming `R`, but the actor never uses `R`, so nothing about it can be
  checked at compile time. ADR-0230 §2's type constructors check topology on
  the type, and build skew needs no load-time check (§4).
- **Make a `ProtocolPath<P>` any way but narrowing an `ActorPath<R>` or a
  contextual decode**, such as from a held reference or by narrowing a
  received path. A reference yields its path through a registry read, not
  from a type, and a received path's claim was checked by another crate's
  compiler, so either would attach a claim this compiler did not check.
- **One typed path, protocol-typed only, with every actor also a protocol.**
  An actor's protocol could only be a projection of it: its single and
  silent rows, without its placement, its `#[fallback]`, or its manual rows,
  since a manual row covers no protocol row (§6). A path typed by the
  driver's protocol could not carry the bootstrap's `Call`, which the driver
  answers manually. Two typed paths mirror the two typed references, and an
  `ActorPath<R>` resolves to the `ActorRef<R>` that sends every kind `R`
  handles.
- **A crate-private engine door that checks an arriving `KindId` against the
  target's rows before delivery.** A second mechanism beside the reference
  types; the stand-in keeps one rule, untyped sends only to fallback targets,
  and expresses the ingress case as a type.
- **Keep untyped sends through erased references for publishing.** Replaced by
  typed subscriber references: the publisher proves each subscriber once and
  every later send is typed.
- **A declared manual reply (`Manual<O>` in the ctx).** A manual handler
  replies with any kind, so a declared kind would be a claim the handler does
  not keep.
- **`#[protocol(of = R)]`, deriving a protocol from one actor's rows.**
  `ActorRef<R>` and `ActorPath<R>` already carry `R`'s full contract.
- **Protocols as `dyn` trait objects.** A vtable per send and a boxed or
  borrowed object per reference. Marker types give the same checks at zero
  cost.
- **Runtime-only reply checks.** Finds the bug in production, pays a lookup per
  reply, and leaves the call site unchanged.
- **A runtime reply-obligation system** (reply tokens injected into handlers,
  stored, carried across replace). This ADR checks that a reply can be
  received, not that one is delivered.
- **Tracking which protocols were cast against each target**, to allow
  contract-changing replaces that no holder depends on. Per-target bookkeeping
  for a refusal that is simpler as a rule.
- **Failing stale sends at run time after a contract-changing replace.** Turns
  a replace into a runtime fault in every peer instead of one refused operation.
- **Keep `unsafe trait` on `DependsOn`, `Spawns`, and `Rebuildable`, and add
  it to `Contract`.** `unsafe` marks undefined behaviour, and a logic rule
  behind it stays open to anyone who writes the keyword (§10).
- **Seal each marker with a supertrait the macro implements through a
  doc-hidden path.** The macro expands in the author's crate, so the author
  can name any path it names.
- **An unnameable per-actor rows type in an anonymous `const _` block, with
  per-kind impls on it.** The projection `<A as Contracts>::Rows` names it,
  and rustc accepts an impl whose self type is a concrete projection.
- **A blanket `impl<T, K, I> Contract<K> for T where T::Rows: RowAt<K, I>`.**
  `I` is unconstrained (`E0207`), as §2 records for protocols. Moving the
  index into the trait (`Contract<K, I>`) makes `ctx.subscribe::<P, K>()` and
  `narrow` spell `_`, and the `CoversRows` blankets hit `E0207` again.
- **A const assertion that `K` is in `CONTRACTS`, evaluated where a row is
  used.** It runs after monomorphization, so `cargo check` never evaluates it,
  and the trait solver that decides `CoveredBy` cannot see it.
- **Flat row tuples that reuse the protocol `RowAt<K, At<N>>` impls.** Capped
  at 16 rows; the render runtime has 21 handlers and the widget panel 32.
- **Leave a cfg-disabled handler out of the rows list.** Every later position
  would then depend on the enabled features.
- **Make `RetireWindow` `pub` at the kinds root.** `aether-window`'s
  `pub use kinds::*` then exports it, and any crate could send a window child
  the manager's shut-down order, skipping `CloseWindow` and the manager's
  bookkeeping.
- **Leave crate-private kinds out of `Rows`.** The handler keeps its
  `CONTRACTS` entry but loses its `Contract<RetireWindow>` row, so the rows
  stop matching the handlers, and once §1 moves typed sends onto
  `Contract<K>` the manager's own typed `ctx.send_to(child, &RetireWindow)`
  stops compiling.
- **Derive dispatch from the row list** (each row a handler entry type whose
  `handle` a sealed dispatcher calls, the actor-framework `Handler<M>`
  shape). The only shape that also closes a type with no `#[actor]`, but it
  rewrites dispatch on both transports and every hand-written test actor;
  #6887.
- **Keep the `DependencyEntry` inventory and check that it agrees with
  `Declared`.** Two coupled mechanisms for one fact, and a link-time
  inventory answering an engine check. A hand-written type submits no entry,
  so the agreement check would itself need the list.
- **An `A: Declared` bound at each birth site instead of a supertrait.**
  Every generic caller up the chain (builder, harness, spawner, pumped slot)
  would repeat it, and a test actor pays the same one impl either way.
- **Have `#[actor]` compute the guest's dependency records from
  `Declared::Depends` into the inherent manifest.** The inherent manifest is
  itself hand-writable, so the section would still carry whatever the type
  chose to write. `export!` reading the trait list is the check reading the
  list.
- **Check a guest's dependencies from inside the guest at `init`, through a
  host import.** It adds guest ABI for a fact the host already reads without
  running the guest (ADR-0230 §3), and it moves the refusal after
  instantiation.
- **Leave `ListedIn` unsealed.** The invoking crate could then implement it
  for a list of its own unlisted children, and the coverage check would pass.
- **Keep rejecting generic native `depends(..)`.** Once the inventory is gone,
  the rule has no reason left and only blocks a shape the traits support.

## Amendments

- **ADR-0075 decision 1 / ADR-0076.** `HandlesKind` stays the handler marker and
  gains an `on_unimplemented` message. Typed sends bound on `Contract<K>` plus
  the sender's `ReplyHandledBy`, not on `HandlesKind<K>` alone.
- **ADR-0109 §5.** The native `HandlerEntry.reply` is a `ReplyContract`
  (`None`, `One(O)`, `Manual`), the wasm manifest's vocabulary. The ban on a
  reply annotation stands: a manual handler states no reply kind.
- **ADR-0227.** `Replies` stays the narrower bound for helpers; a send through
  it carries the same sender bound. Decision 3 stands: a manual handler has no
  `Replies` marker.
- **ADR-0230.** `ProtocolRef<P>` joins the proven types with no codec, and
  `ProtocolPath<P>`, narrowed from ADR-0230's `ActorPath<R>`, is an
  `ErasedActorPath` with a compile-time claim, carried with its codec. The
  cast
  is a new door for erased references, minted in `proven.rs` under the existing
  gate. §2's `ErasedActorRef` row loses "be the target of an untyped send", and
  `send_envelope_tracked_to` / `send_envelope_detached_to` leave the public
  surface. §3's reserved `resolve` verb takes a `ProtocolPath<P>` as well as
  an `ActorPath<R>`, and proves liveness only; a `ProtocolPath<P>` decodes
  only against the engine's context. No door compares `R`'s rows when it
  mints an `ActorRef<R>`.
  `ctx.monitor` requires a silent `MonitorNotice` handler.
- **ADR-0232.** `send_to` takes an `ActorRef<R>` or a `ProtocolRef<P>`; an
  `ErasedActorRef` is not a `Target`.
- **ADR-0022 / ADR-0038 / ADR-0101.** `replace_component` refuses a replacement
  that drops or changes a contract row, before `on_dehydrate` runs.
- **ADR-0101 decision 1.** `WasmActor::on_rehydrate` takes `WasmCtx<'_, Self>`.
  An override that writes `WasmCtx<'_>` is typed by the macro, and one that
  writes `WasmCtx<'_, Erased>` receives the erased view.
- **ADR-0169.** A handler-set member is typed by its adopter: `WasmCtx<'_>` in a
  set reads as `WasmCtx<'_, Self>`. A set bounds what its default bodies reach
  with supertraits (`DependsOn<R>`), and `#[handler_set]` adds `Sized` to them.
  An override is a plain trait-method impl, so it spells the typed signature
  `WasmCtx<'_, Self>`. A native set's bridge also carries the set's rows as a
  list an adopter's `Contracts::Rows` ends with (§10).
- **ADR-0230 §3.** `DependsOn<R>` is a safe trait whose impl names `R`'s
  position in the actor's `Declared::Depends` list; a hand-written impl is
  refused with `E0119` or `E0277` rather than `E0200`. The pre-`init` check
  reads that list on both transports: the native birth check walks it, and
  `export!` writes a guest's `Dependency` records from it (§10).
- **ADR-0114 §5.** `Rebuildable<M>` and `Spawns<C>` are safe traits whose
  impls name a position in `export!`'s module list and the spawner's
  `spawns(..)` list, and `export!`'s module type is private. The coverage
  check reads each listed type's `Declared::Spawns` list through the sealed
  `ListedIn` (§10).
