# ADR-0231: Protocol Links and Static Reply Checks

- **Status:** Proposed
- **Date:** 2026-09-23

Actors link through contracts the compiler checks. A reference or a path
names the protocol its holder needs, the compiler proves at the link that the
target's handlers meet it, and the one runtime step left is proving, on
receipt, that the target is live and is still the build the link was checked
against.

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
[ADR-0101](0101-replace-hooks-on-ffiactor.md) and ADR-0169. The full list is
under [Amendments](#amendments).

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
`crates/aether-actor-derive/src/reply_markers.rs`), and per actor a
`Contracts::CONTRACTS` list of the same rows in the manifest's `ReplyContract`
vocabulary. Typed sends bound on `HandlesKind<K>` only. No bound connects the
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
  yet: there is no cast, no receipt check, and no build-skew check. The DAG
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
   `ActorPath<R>`. A protocol path is made only by narrowing an actor path
   the compiler checked against `P`, and every typed path is proven again on
   receipt (§3).
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

| § | Decision | On main |
|---|---|---|
| 1 | Static reply check on typed sends | not built |
| 2 | `#[protocol]` and `CoveredBy` | built: `Row`, `RowReply`, `RowSet`, `CoversRows`, `Protocol`, `CoveredBy` (`crates/aether-actor/src/model/protocol.rs`) and `#[protocol]` (`crates/aether-actor-derive/src/protocol.rs`), over the per-handler `Contract<K>` rows and per-actor `Contracts::CONTRACTS`; `includes` and protocol-to-protocol coverage are not built |
| 3 | `ProtocolRef<P>`, `ProtocolPath<P>`, `resolve` | `ProtocolPath<P>` and `ActorPath::narrow` built (`crates/aether-actor/src/path/`), with the path text as their only wire and serde form; `ProtocolRef<P>`, reference narrowing, `resolve`, and the protocol path's in-memory `source` tag are not |
| 4 | Published rows, no erased send verb, the cast, build skew as a load-time link check | published rows built on the route record for both transports (`RouteContract`, `crates/aether-substrate/src/mail/registry/contract.rs`); the erased send verb's removal, the cast, and the link check are not |
| 5 | Replace preserves contracts | built, the fallback rule included: `crates/aether-data/src/contract.rs`, `crates/aether-substrate/src/mail/registry/contract.rs`, `crates/aether-component/src/trampoline/runtime/contract.rs` |
| 6 | Manual rows | built: `Undeclared` row, `ReplyContract::Manual` on both manifests |
| 7 | Ctx typed by its actor | built, every ctx on both transports |
| 8 | Silent subscribers and watchers | built for the wasm `subscribe` (`crates/aether-actor/src/wasm/ctx/subscribe.rs`); the `monitor` bound and subscriber references are not |
| 9 | Relays | `forward_to` and `DeferredReply::hand_off` exist; the typed target is not built |

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
pair, made only from something the compiler already checked against `P`.
The references are unchanged by the paths; whether the three references
should become one type is a separate question, not decided here.

| Type | Claims | Made by | Can |
|---|---|---|---|
| `ProtocolRef<P>` | an actor whose rows cover `P` reached `Live` here, in this session | narrowing an `ActorRef<R>` or a `ProtocolRef<Q>`; `ctx.resolve` of a `ProtocolPath<P>`; the guard cast (§4) | send the kinds `P` lists, with §1's reply check; monitor; name its canonical path; be held in actor memory. No codec. |
| `ProtocolPath<P>` | narrowed here: the path was written from an actor type the compiler proved covers `P`. Decoded: only that the text is a well-formed canonical path; `P` is the writer's claim until `resolve` proves it. Nothing about existence either way. | narrowing an `ActorPath<R>`, only; decoding yields one that carries the writer's claim | be a kind field, a config field, or saved state; be compared and displayed; be resolved. It grants no send. |

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
    source: Option<ActorTypeTag>, // memory only: the actor type it was narrowed from; `None` when decoded
    _protocol: PhantomData<fn() -> P>,
}

impl<R> ActorPath<R> {
    /// The same text under a narrower claim: type-level only, infallible.
    pub fn narrow<P: CoveredBy<R>>(&self) -> ProtocolPath<P>;
}
```

A `ProtocolPath<P>` is an `ErasedActorPath` and a phantom protocol, and it is
made only by narrowing an `ActorPath<R>`, as a `ProtocolRef<P>` is narrowed
from an `ActorRef<R>`. `P: CoveredBy<R>` is the compile-time link check, over
the sealed coverage §2 defines. Narrowing keeps the text the actor path was
written with (ADR-0230 §2: each step a type's `NAMESPACE` and its key), so a
protocol path is canonical and has no holes; it reads no registry, folds
nothing, and cannot fail. It also keeps, in memory only, the actor-type tag of
the `R` it was narrowed from, which the receipt below uses; the tag is not
part of the wire form, so a decoded protocol path has none.

`ProtocolPath<P>` lives in `aether-actor`, beside `Protocol`, `CoveredBy`, and
`ActorPath<R>`, and not in `aether-data` beside `ErasedActorPath`. Its
constructor from a bare `ErasedActorPath` is private to that crate. In
`aether-data` the constructor would have to be public or `#[doc(hidden)]` for
`aether-actor` to call it, and either door would let any crate attach a `P`
claim to arbitrary text.

On the wire a `ProtocolPath<P>` is the path text only, with
`ErasedActorPath`'s schema and codec, implemented in `aether-actor`. Decoding
validates the grammar and claims nothing about `P`: bytes from another
engine, a config file, or an operator's MCP call decode as the same type, and
only `resolve` turns one into anything that sends. `Debug` prints the path,
because a path is a name, not a position.

A path names a slot by name, so the same text names the same slot in any
session. That is why a typed path may sit in a config or in saved state: what
it cannot know about the far side, whether anything is live there and what
build it is, the receipt proves.

#### Why two typed paths

An actor-typed path and a protocol-typed path claim different things, as the
two references do. `resolve` of an `ActorPath<R>` yields an `ActorRef<R>`,
through which every kind `R` handles is sendable, its manual rows included.
A protocol names single and silent rows only, and a manual row covers no
protocol row (§6), so no protocol-typed link can carry a request to a manual
handler. The Bloomery bootstrap sends `aether.bloomery.driver.call` to the
driver, which answers it from a manual handler (`on_call`,
`crates/aether-bloomery-driver/src/actor/mod.rs`), so it links to the driver
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
lookup, and one route-table lookup (§4) then decides.

Every typed path checks that a route stands at that position under that
canonical name and is `Live`, or refuses `ResolveError::NotLive` (never
registered, still `Starting`, or `Dropped`); the name check guards a fold
collision. What else is checked depends on what the code already holds:

| Path | Also checked | Why this is enough |
|---|---|---|
| `ActorPath<R>` | the route's actor-type tag is `R`'s, or `OtherActor` (ADR-0230 §3) | a native `R` and its caller are one binary; a guest caller's compiled `R` was checked once, at load, under its declared link (§4) |
| `ProtocolPath<P>` narrowed in this binary | the route's tag is the source actor's, or `OtherActor` | `P: CoveredBy<R>` was proven when it was narrowed, and `R` is identified as above |
| `ProtocolPath<P>` decoded at a boundary | the route's published rows cover `<P::Rows as RowSet>::CONTRACTS`, compared as the cast compares them (§4), or `Uncovered`, naming the first kind whose row is missing or different | the path crossed a boundary (ADR-0230 §1) and its text could have come from anyone, so the published rows are the one thing that proves `P` |

The row comparison for a decoded protocol path is the only one left at run
time, and it is the boundary's, not each resolve's. Published rows only grow:
the registry republishes a route's contract only when `first_break` finds no
dropped or changed row (#6844, §5). So a positive answer is kept per route and
protocol and never compared again, and a path built in code and resolved in
the same binary never pays it. Every refusal names the path, never a
position. A replace never makes a resolve fail, because §5 refuses a replace
that drops or changes a row; a resolve fails on liveness, or when a
different actor type, or for a decoded path a different build, now answers
at the path.

An untyped `ErasedActorPath` (a config field, an MCP tool argument, an RPC
`Call`) stays untyped: `resolve_path` proves it to an `ErasedActorRef`,
after filling a short path's holes from the generated root and child
declarations, a static inventory rather than the live tree, and the cast
(§4) types it.

Each arm lands with its consumer. The native arm over a `ProtocolPath<P>`
serves the Bloomery workspace's receipt of a request's storage source, and
the guest arm over an `ActorPath<R>` serves the Bloomery bootstrap resolving
its unit's journal and driver
([ADR-0240](0240-several-bloomery-journal-units-per-engine.md) D7, D8). The
other two arms come with their first callers. A guest's call is one host
call, as `resolve_path`'s is.

**Consumer.** The Bloomery workspace's `Run` and `Import` carry
`source: ProtocolPath<ArtifactStorage>`, which each unit's driver narrows
from its journal's actor path,
`ctx.link::<JournalActor>(&key).narrow::<ArtifactStorage>()`, and the
workspace resolves on receipt (ADR-0240 D7).

### 4. Published rows, typed sends, and the guard cast

#### Rows are published when a route goes `Live`

A route publishes its contract rows, `(KindId, ReplyContract)` pairs, and
whether it has a `#[fallback]`, on its `RouteRecord` when it goes `Live`, or,
for an inline child's `Alias` route, when the alias is staged. The
decoded-path receipt check (§3), the cast, and the load-time link check read them there, so one
published-route read answers both "is it `Live`" and "what does it cover".

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
impl ErasedActorRef {
    pub fn cast<T: CastTarget>(&self, ctx: &impl ProveCtx) -> Option<ProtocolRef<T>>;
}
```

The cast is the fallback for references that arrive untyped: the envelope
sender (`ctx.sender()`), a path proven through `resolve_path`, including one
an MCP tool or an RPC `Call` put in a payload, and a native `resolve_live`
answer. It runs once, at receipt, in the handler that received the reference,
and reads the same published rows the receipt check reads.

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
| Subscriber fan-out | `fanout` in `crates/aether-window/src/runtime/desktop/mod.rs`, `synthetic/mod.rs`, and to the one consumer in `crates/aether-tcp/src/session/runtime.rs`; `send_envelope_tracked_to` in `crates/aether-lifecycle/src/subscribers.rs` and the synthetic window's `InjectWindowEvent` | each subscriber held as `ProtocolRef<Subscriber<K>>`, cast at the subscribe request (§8); a table keyed by a runtime `KindId` dispatches the id to the typed table of the published kind it names and refuses any other |
| Relays | `forward_to` in `crates/aether-window/src/runtime/manager.rs` and `crates/aether-component/src/component/runtime/mod.rs` | a typed target for the forwarded kind (§9) |
| Ingress bridges | above | the private stand-in |
| A witness that takes every kind | the render harness observer (`observe`, `crates/aether-render/src/runtime/mod.rs`) | `ProtocolRef<AnyKind>`, cast at `wire`; the observer's route publishes a fallback |
| Any other held erased peer | e.g. `RetireWindow` to a window's children (`crates/aether-window/src/runtime/desktop/mod.rs`, `synthetic/mod.rs`) | the typed reference from the door that minted it, or a cast |

The public `send_envelope_tracked_to` and `send_envelope_detached_to` go: no
generic send remains for authors.

#### Build skew is a load-time link check

`#[actor]` emits the same `CONTRACTS` list for an actor `R` that `#[protocol]`
emits for a protocol, so the rows a peer was compiled against for `R` compare
with a loaded target's published rows. That comparison runs when the link is
made, never per send and never per resolve.

- A guest that names `R` by path declares `#[actor(links(R))]`, and the
  loader checks its compiled rows for `R` once, before `init`, whichever of
  the two loads second (ADR-0230 §3, "Declared links"). The load that would
  run skewed is refused, naming the linking actor, `R`, and the first kind
  whose row is missing or different; rows the loaded build adds pass.
- The doors that already consult the registry when they mint an
  `ActorRef<R>` (the dependency check before `init`, the load mints, the
  embedder's typed read of a load reply, `child::<P, C>`, and the chassis
  handle's `actor_ref::<R>()`) compare the same rows in the read they
  already make, once per mint.
- `resolve` of an `ActorPath<R>` compares no rows: its tag check stands on
  the link check, or on one binary for a native `R`. Only a
  `ProtocolPath<P>` decoded at a boundary is compared at run time (§3), once
  per route and protocol.

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
receipt is invalidated by a replace. Native capabilities are not replaced at run
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
  for `Reply = Undeclared`; at run time the cast and the receipt compare
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
  and fan-out sends through those references.

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

## Scenario sweep

"Compiles" and "compile error" describe the send site. "Runtime guard" means a
cast, a receipt, or the replace refusal decides. "Exempt" means the check does
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
| typed `ActorRef<R>` | static check over `R`'s rows; the door that minted it checked those rows against the loaded build |
| `ProtocolRef<P>` | static check over `P`'s rows; a kind outside `P` is a compile error even when the target handles it |
| `ActorRef<R>` narrowed to `ProtocolRef<P>` | compiles if `P: CoveredBy<R>`, else compile error |
| `ActorPath<R>` narrowed to `ProtocolPath<P>` | compiles if `P: CoveredBy<R>`, else compile error; the same text, with no registry read and no position |
| `ActorPath<R>` held or received in mail, config, or saved state | `ctx.resolve`, only for an actor that declares `links(R)`: `NotLive` or `OtherActor` refuse, else an `ActorRef<R>`, which sends every kind `R` handles, manual rows included; no row comparison |
| `ProtocolPath<P>` narrowed in this binary | `ctx.resolve`: `NotLive` or `OtherActor` refuse, else a `ProtocolRef<P>`; no row comparison |
| `ProtocolPath<P>` decoded from mail, config, or saved state | `ctx.resolve` at receipt: `NotLive` or `Uncovered` refuse, else a `ProtocolRef<P>`; the row comparison runs once per route and protocol |
| `ErasedActorPath` received untyped (config, MCP, RPC) | `resolve_path` to an `ErasedActorRef`, then `cast::<T>()` |
| `ErasedActorRef` (`ctx.sender()`, `resolve_path`, `resolve_live`) | no send; reply, monitor, key, or `cast::<T>()` first |
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
| a decoded `ProtocolPath<P>` whose path now names a different build or actor type | `Uncovered` at receipt |
| a guest module that links `R` against rows a live `R` does not publish, or an `R` loaded against a live link it breaks | refused at that load, before `init` |
| a `ProtocolPath<P>` whose target is dead or not yet started | `NotLive` at receipt |
| an `ActorPath<R>` whose path now names another actor type | `OtherActor` at receipt |
| cast failure | `None`; the holder refuses or drops, nothing parked |
| a peer compiled against a different build of `R` | refused at the registry-consulting `ActorRef<R>` door |
| a protocol reached twice through `includes` | rows appear once |
| a manual row where a protocol expects a single or silent row | not covered, statically and at run time |

## Consequences

### Positive

- A link between actors is checked when it is made. The actor that narrows
  an actor path proves at compile time that the target's type covers the
  protocol; the receiver gets a typed reference from one fold and one
  route-table lookup, with no cast.
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
- Resolving a typed path needs the route record to carry its actor type,
  the load-time link check needs the registry to keep live links by tag,
  and a decoded protocol path's answer needs a per-route cache. None exists
  on main (ADR-0230 §3).
- A guest that names an actor by typed path declares the link, and a load
  that would run against a skewed build of that actor is refused.
- `send_ignoring_reply` still takes the sender's dispatch-miss path, which logs
  each discarded reply.

### Neutral

- Runtime reply delivery, settlement, correlation, request contexts, and
  liveness are unchanged.
- No new mail and no per-send cost. A resolve is one fold and one route-table
  lookup; a cast, and the receipt of a decoded protocol path, add a slice
  comparison, the latter once per route and protocol.

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
- **Check rows on every resolve.** A row comparison at each `resolve` pays
  at run time for an invariant the code already holds: a native caller and
  actor are one binary, and a guest's skew is refused once, at load, under
  its declared link. Only a path decoded at a boundary is compared, once per
  route and protocol.
- **Make a `ProtocolPath<P>` any way but narrowing an `ActorPath<R>`**, such
  as from a held reference or by narrowing a received path. A reference
  yields its path through a registry read, not from a type, and a received
  path's claim was checked by another crate's compiler, so either would
  attach a claim this compiler did not check.
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
  an `ActorPath<R>`. `#[actor(links(R))]` joins `depends(...)`, and a
  guest's link is checked against `R`'s rows at load; the
  registry-consulting `ActorRef<R>` mints compare `R`'s compiled rows in the
  read they already make. `ctx.monitor` requires a silent `MonitorNotice`
  handler.
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
  `WasmCtx<'_, Self>`.
