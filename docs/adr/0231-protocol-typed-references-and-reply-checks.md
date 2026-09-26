# ADR-0231: Protocol Links and Static Reply Checks

- **Status:** Proposed
- **Date:** 2026-09-23

Actors link through contracts the compiler checks. A reference or an address
names the protocol its holder needs, the compiler proves at the link that the
target's handlers meet it, and the one runtime step left is proving, on
receipt, that the target is live and is still the build the link was checked
against.

[ADR-0230](0230-proven-actor-references.md) made a reference a proof of
identity: an actor of this type reached `Live` at this position, in this engine
session. This ADR makes references and addresses prove the target's contract
too, meaning which kinds it handles and how it answers each one. A send reads
the contract off the reference's type, so the send site has nothing to check
beyond the types.

Amends [ADR-0075](0075-actor-typed-sender-api-and-chassis-cap-marker-split.md)
decision 1 and [ADR-0076](0076-collapse-cap-facade-pattern.md) (`HandlesKind`
gating), [ADR-0109](0109-handler-reply-contracts.md) §5 (the native manifest's reply
field), [ADR-0227](0227-reply-contracts-are-type-markers.md) (the reply
markers), [ADR-0230](0230-proven-actor-references.md)
(protocol references and addresses, and the end of untyped sends through an
erased reference), [ADR-0232](0232-flat-ctx-send-verbs.md) (`send_to`),
[ADR-0240](0240-several-bloomery-journal-units-per-engine.md) D8 (the `Root`
address form narrows), and the replace contract of
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
  where a third lives, the field is an `Address<R>`
  (`crates/aether-data/src/reference/address.rs`) or an `ActorPath`. An
  `Address<R>` needs the receiver to name `R`, which is often an
  implementation type the receiver does not and should not know; an
  `ActorPath` names nothing about what lives there. Either way the receiver
  learns what the target handles only by trying it, or by a runtime check.
- **The manifest's rows are not published where a proof can read them.** The
  route record (`RouteRecord`,
  `crates/aether-substrate/src/mail/registry/mailbox/route.rs`) carries a
  canonical name and a lifecycle and no rows. The rows exist in the native
  handler manifest (`HandlerEntry.reply`,
  `crates/aether-data/src/name_inventory.rs`), in each wasm module's decoded
  `ActorInputs` (`crates/aether-substrate/src/actor/wasm/kind_manifest.rs`,
  kept on the trampoline as `actor_caps`,
  `crates/aether-component/src/trampoline/runtime/config.rs`), and in the DAG
  validator's `CapabilityRegistry`
  (`crates/aether-substrate/src/mail/capability.rs`), which also records
  whether a mailbox has a `#[fallback]`. That registry is written under its own
  lock on its own schedule (a guest's through `NativeCtx::sync_guest`), apart
  from the route's lifecycle.

The owner, on typed links:

> It'd be nice if we could have compile time actor path checks like this path is going to 100% be the protocol you need it to be, as verified. It'd solve a LOT of headache. Liveness is a different issue.

> It could essentially prevent runtime casts and actually define contracts that actors have to meet when linking together.

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
3. A link is typed by the protocol the holder needs. `ProtocolRef<P>` is the
   proof; `ProtocolAddress<P>` is the description that crosses a boundary,
   made only by narrowing a typed address or reference the compiler checked
   against `P`, and proven again on receipt (§3).
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
| 2 | `#[protocol]` and `CoveredBy` | not built; per-handler `Contract<K>` rows and per-actor `Contracts::CONTRACTS` are built |
| 3 | `ProtocolRef<P>`, `ProtocolAddress<P>`, narrowing, receipt | not built |
| 4 | Published rows, no erased send verb, the cast, build skew | not built |
| 5 | Replace preserves contracts | built for handler rows: `crates/aether-data/src/contract.rs`, `crates/aether-component/src/trampoline/runtime/contract.rs`; the fallback rule is not |
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

`Target<K>` (ADR-0232 §1) gains `type Reply: ReplyShape`: `<R as Contract<K>>::Reply`
for an `ActorRef<R>`, `<P as Contract<K>>::Reply` for a `ProtocolRef<P>`.
`send_detached`, `send_tracked`, `send_many`, and the context-carrying sends
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
target's deferred `-> Pending<O>` handler has the row `O` and covers it. There
is no spelling for a manual row (§6). The method name labels the row in rustdoc
and diagnostics; a target matches rows by kind, never by method name. Each
parameter needs a name or `_`, because an attribute's input must parse.

The attribute replaces the trait with a unit struct, so the trait never exists
as a trait object and a protocol costs nothing at run time. It expands to:

```rust
pub struct MeshLoader;

impl Contract<LoadMesh> for MeshLoader { type Reply = MeshLoadResult; }
impl Contract<Ping> for MeshLoader { type Reply = Pong; }
impl Contract<SetMode> for MeshLoader { type Reply = Silent; }

impl Protocol for MeshLoader {
    const CONTRACTS: &'static [(KindId, ReplyContract)] = &[
        (LoadMesh::ID, ReplyContract::One(MeshLoadResult::ID)),
        (Ping::ID, ReplyContract::One(Pong::ID)),
        (SetMode::ID, ReplyContract::None),
    ];
}

/// Any target whose rows cover this protocol narrows to it.
impl<R> CoveredBy<R> for MeshLoader
where
    R: Contract<LoadMesh, Reply = MeshLoadResult>
        + Contract<Ping, Reply = Pong>
        + Contract<SetMode, Reply = Silent>,
{
}
```

The trait solver decides coverage. A target covers a row only with the same
kind and the exact reply type: a silent row is covered only by a silent
handler, and a row `O` only by a handler that replies `O`. A kind the target
handles only through `#[fallback]` has no row and never covers. `CoveredBy<R>`
holds for any actor whose `#[actor]` rows match and for any protocol that
includes this one. `CONTRACTS` reuses the manifest's `ReplyContract`, so the
const list and a live target's published rows (§4) compare in one vocabulary,
exactly as `#[actor]`'s `Contracts::CONTRACTS` already do.

**Composition.** `#[protocol(includes(Pingable, Describable))]` adds the
included protocols' rows to the `Contract` impls, the `CONTRACTS` list, and the
`CoveredBy` where-clause. A proc macro cannot read another item, so every
`#[protocol]` also emits a hidden `macro_rules!` bridge carrying its rows, and
`includes` invokes it, the technique ADR-0169 uses to paste a handler set's
markers. The bridge carries the protocol's own path beside its rows, and the
expansion dedupes by protocol, so a protocol reached twice through a diamond of
`includes` contributes its rows once. Two distinct protocols that list the same
kind are a conflicting-impl error.

### 3. Protocol links

A link is typed by what its holder needs, not by what the target is. The proof
is `ProtocolRef<P>`; the description that crosses a boundary is
`ProtocolAddress<P>`. Both are made only from something the compiler already
checked against `P`.

| Type | Claims | Made by | Can |
|---|---|---|---|
| `ProtocolRef<P>` | an actor whose rows cover `P` reached `Live` here, in this session | narrowing an `ActorRef<R>` or a `ProtocolRef<Q>`; `ctx.resolve` of a `ProtocolAddress<P>`; the guard cast (§4) | send the kinds `P` lists, with §1's reply check; monitor; yield its `ProtocolAddress<P>`; be held in actor memory. No codec. |
| `ProtocolAddress<P>` | this position was folded from a type the compiler proved covers `P`; nothing about existence | narrowing only | be a kind field, a config field, or saved state; be resolved. No public constructor. |

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
    pub fn address(self) -> ProtocolAddress<P>;
}
```

`ProtocolRef<P>` is the existing proven target plus a phantom protocol. Sends
through it monomorphize against `P`'s `Contract` rows: no vtable, no per-send
lookup, nothing added to the runtime send path. It is a `Target<K>` for each
kind `P` lists, so `ctx.send_to(&loader, &LoadMesh { path })` takes it as it
takes an `ActorRef<R>`. A narrowed reference is a capability view: its holder
may send only what `P` lists, whatever else the target handles. Like every
proven type it has no codec (ADR-0230 §1).

#### `ProtocolAddress<P>`

`ProtocolAddress<P>` is the protocol-typed twin of `Address<R>`: a description
with a codec, claiming nothing about existence. It lives in `aether-data`
beside `Address<R>`, because kinds carry it, with the same codec, `Schema`,
and cast-ineligibility. It cannot carry `R`, so narrowing resolves everything
`R` contributes at that moment: it folds the address with `R`'s resolver to a
position, and the position is what the protocol address carries.

It has no public constructor, and no public API reads a `MailboxId` out of it
or builds one from a `MailboxId`. Its one constructor is `#[doc(hidden)]` and
gated like the reference mints (ADR-0230 §4), with the narrowing code in
`aether-actor` as its only caller. On the wire it carries the position as
`Address<R>`'s `Exact` form does.

A position is a fold of a lineage, so the same bytes name the same slot in any
session. That is why a protocol address may sit in a config or in saved state:
what it cannot know about the far side, whether anything is live there and what
build it is, the receipt proves.

#### Narrowing

```rust
impl<R: Addressable> Address<R> {
    pub fn narrow<P: CoveredBy<R>>(&self) -> Result<ProtocolAddress<P>, NarrowError>;
}
impl<Q> ProtocolAddress<Q> {
    pub fn narrow<P: CoveredBy<Q>>(&self) -> ProtocolAddress<P>;
}
```

`P: CoveredBy<R>` is the compile-time link check. Which form narrows is decided
by whether `R`'s resolver determines the position without a caller:

| `Address<R>` form | Narrows | How |
|---|---|---|
| `Root { key }` (ADR-0240 D8, bounded `R: Root + Instanced`) | yes, without a ctx | folds `key` with `R`'s resolver at the root |
| `Beneath { parent, key }` | yes, without a ctx | folds `key` with `R`'s resolver beneath `parent` |
| `Scoped { key }` | through the caller's ctx | its position depends on the caller, so `Address::narrow` refuses it (`NarrowError::CallerRelative`); the caller resolves it (`ctx.resolve`, ADR-0230 §3) and narrows the proof: `ctx.resolve(&address)?.narrow::<P>().address()` |
| `Exact { id }` | only through a proof | `R`'s resolver played no part in the position, so `P: CoveredBy<R>` would check a type the position was never folded from; `Address::narrow` refuses it (`NarrowError::Exact`). A held `ActorRef<R>` narrows as `r.narrow::<P>().address()` |

A key `R`'s resolver cannot fold (a key on a keyless actor, none on a keyed
one) is `NarrowError::Key`, the resolver's own `candidate` answering `None`.
A held proof narrows without any of these cases: `ActorRef<R>::narrow` and
`ProtocolRef<Q>::narrow` are total, and `ProtocolRef<P>::address` reads the
position the proof already holds.

#### Receipt

```rust
ctx.resolve(&address) // &ProtocolAddress<P> -> Result<ProtocolRef<P>, ResolveError>
```

`resolve` is the ADR-0230 §3 verb for a description that arrived in mail,
config, or saved state; for a `ProtocolAddress<P>` it mints a
`ProtocolRef<P>`. It runs once, in the handler that received the address, and
makes one published-route read (§4):

- the route at the position is `Live`, or `ResolveError::NotLive` (never
  registered, still `Starting`, or `Dropped`);
- the route's published rows cover `P::CONTRACTS`, compared as the cast
  compares them (§4), or `ResolveError::Uncovered`, naming the first kind whose
  row is missing or different.

Neither refusal carries a position. The rows check re-proves on the receiving
side what narrowing checked on the sending side, because the address crossed a
boundary (ADR-0230 §1): it guards forged bytes and build skew. For an address
made by narrowing, a replace never causes it to fail, because §5 refuses a
replace that drops or changes a row. It fails on liveness, or when a different
build or a different actor type now sits at the position.

`resolve` lands per ctx with its first caller: the native arm for a
`ProtocolAddress<P>` with the consumer below; the guest arm with the first
guest that receives one. ADR-0240 D8's guest `resolve::<R>` for an
`Address<R>` is the same verb over the other description.

**Consumer.** The Bloomery workspace's `Run` and `Import` carry
`source: ProtocolAddress<ArtifactStorage>`, built by each unit's driver
narrowing its journal's `Root` address, and the workspace resolves the source
on receipt. This is planned in ADR-0240 by draft PR #6833.

### 4. Published rows, typed sends, and the guard cast

#### Rows are published when a route goes `Live`

A route publishes its contract rows, `(KindId, ReplyContract)` pairs, and
whether it has a `#[fallback]`, on its `RouteRecord` when it goes `Live`, or,
for an inline child's `Alias` route, when the alias is staged. The
receipt check (§3), the cast, and the build-skew check read them there, so one
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
flag is its coverage, the way `CONTRACTS` is a protocol's.

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
| a protocol `P` | every row of `P::CONTRACTS`, kind and `ReplyContract` alike |
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

#### Build skew at the `ActorRef<R>` doors

`#[actor]` emits the same `CONTRACTS` list for an actor `R` that `#[protocol]`
emits for a protocol, so `R`'s compiled rows compare against a loaded target's
published rows. Every `ActorRef<R>` door of ADR-0230 §3 that already consults
the registry (the dependency check before `init`, the spawn and load mints, the
embedder's typed read of a load reply, `child::<P, C>`, the chassis handle's
`actor_ref::<R>()`, and `resolve::<R>`) runs the cast's rows check over the
rows the peer was compiled against. A peer built against a different build of
`R` than the one loaded is refused at the door, naming the actor and the
missing or changed kind. Rows the loaded build adds pass. A door that reads the
registry already pays that read; the check adds a slice comparison.

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
true across replaces, so no `ProtocolRef`, no `ProtocolAddress`, and no static
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

### B. Links and addressing

| Reference or address | Outcome |
|---|---|
| typed `ActorRef<R>` | static check over `R`'s rows; the door that minted it checked those rows against the loaded build |
| `ProtocolRef<P>` | static check over `P`'s rows; a kind outside `P` is a compile error even when the target handles it |
| `ActorRef<R>` narrowed to `ProtocolRef<P>` | compiles if `P: CoveredBy<R>`, else compile error |
| `Address<R>` (`Root`, `Beneath`) narrowed to `ProtocolAddress<P>` | compiles if `P: CoveredBy<R>`; folds to a position at once |
| `Address<R>` (`Scoped`, `Exact`) narrowed | refused by form; resolve it, or use the proof, and narrow the proof |
| `ProtocolAddress<P>` received in mail, config, or saved state | `ctx.resolve` at receipt: `NotLive` or `Uncovered` refuse, else a `ProtocolRef<P>` |
| `ErasedActorRef` (`ctx.sender()`, `resolve_path`, `resolve_live`) | no send; reply, monitor, key, or `cast::<T>()` first |
| by name over the wire (MCP, RPC `Call`, `NamedMail` bundles) | exempt from §1; the boundary proves the position (ADR-0230 §3) and delivers through its stand-in |

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
| a `ProtocolAddress<P>` whose position now holds a different build or actor type | `Uncovered` at receipt |
| a `ProtocolAddress<P>` whose target is dead or not yet started | `NotLive` at receipt |
| cast failure | `None`; the holder refuses or drops, nothing parked |
| a peer compiled against a different build of `R` | refused at the registry-consulting `ActorRef<R>` door |
| a protocol reached twice through `includes` | rows appear once |
| a manual row where a protocol expects a single or silent row | not covered, statically and at run time |

## Consequences

### Positive

- A link between actors is checked when it is made. The actor that narrows an
  address proves at compile time that the target covers the protocol; the
  receiver gets a typed reference from one route read, with no cast.
- A request whose reply the requester cannot receive does not compile, and
  every deliberate discard is written as `send_ignoring_reply`.
- No actor sends an untyped kind to a target that cannot take it. The only
  untyped targets are fallback actors, and an ingress bridge's stand-in cannot
  leave its crate.
- A holder is handed exactly the rows it may use, and a peer depends on a
  protocol rather than an implementation type.
- Contracts are monotone across replace, so proofs and protocol addresses never
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
- `Scoped` and `Exact` addresses narrow only through a proof, so the target
  must be `Live` when the link is made.
- `send_ignoring_reply` still takes the sender's dispatch-miss path, which logs
  each discarded reply.

### Neutral

- Runtime reply delivery, settlement, correlation, request contexts, and
  liveness are unchanged.
- No new mail and no per-send cost. A receipt or a cast is one published-route
  read and a slice comparison.

## Alternatives considered

- **Carry `Address<R>` in the kind and resolve by `R`.** The receiver must name
  `R`, often an implementation type from a crate it should not depend on, and
  learns only identity, not the contract it needs.
- **Carry an `ActorPath` or an erased address and cast on receipt.** A runtime
  check at every receipt for a fact the sender's compiler already knew, and a
  mismatch found in production.
- **`ProtocolAddress<P>` as canonical text.** Rendering `R`'s path needs its
  namespace as text, which ADR-0230 §4 withholds from `Namespace`; a position
  is the fold's own output.
- **Narrow an `Exact` address.** Its position was not folded from `R`, so the
  static check would describe a type the position has no tie to.
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
  `ActorRef<R>` already proves `R`'s full contract.
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
  `ProtocolAddress<P>` joins `Address<R>` as a description with one. The cast
  is a new door for erased references, minted in `proven.rs` under the existing
  gate. §2's `ErasedActorRef` row loses "be the target of an untyped send", and
  `send_envelope_tracked_to` / `send_envelope_detached_to` leave the public
  surface. §3's `resolve` verb takes a `ProtocolAddress<P>` too. The
  registry-consulting `ActorRef<R>` doors also check `R`'s compiled rows
  against the published rows. `ctx.monitor` requires a silent `MonitorNotice`
  handler.
- **ADR-0232.** `send_to` takes an `ActorRef<R>` or a `ProtocolRef<P>`; an
  `ErasedActorRef` is not a `Target`.
- **ADR-0240 D8.** An `Address<R>` in the `Root` form narrows to a
  `ProtocolAddress<P>` without a ctx.
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
