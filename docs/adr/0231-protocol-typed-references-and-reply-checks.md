# ADR-0231: Protocol-Typed References and Static Reply Checks

- **Status:** Accepted
- **Date:** 2026-09-23
- **Amended (#6440):** 2026-09-23 — with the multi class removed (ADR-0134 amendment), a contract row's reply is `Silent` or a kind `O` (`One(O)` on the manifest). `Multi<O>` leaves `Contract`, `ReplyHandledBy` and `#[protocol]` signatures; the multi rows of the scenario sweep (A: multi `Multi<O>`; D: `emit` on the reply path; E: a multi request passed on by a relay) and the multi sentence of §9 no longer apply; the §4 reason for widening `HandlerEntry.reply` (telling a multi row from a single one) is gone, and whether the widening stays is left to the ADR-0231 implementation.
- **Amended (#6486 closed):** 2026-09-24 — a manual handler replies with any kind, so `Manual<O>` is dropped and §6 no longer applies. The manual reply mode stays unparameterized: its manifest row stays `ReplyContract::Manual`, its `Contract` row is `Undeclared` for good (no migration waves, and `Undeclared` and the bare `Manual` spelling are not deleted), it gets no `Replies` marker, and reply handles stay untyped. Consequences: §1 checks single and deferred rows only, and a send to a manual row compiles with no sender bound, because the replier picks the kind at run time; §5 casts match a protocol's manual row against `ReplyContract::Manual` (a manual row no longer matches nothing); §8 lets a manual row pass the silent-handler bound, as it did during the old migration; §9's `forward`, `forward_to` and `hand_off` sit on the manual ctx with no reply-equality bound; the scenario rows for `Manual<O>` and `Manual<Silent>`, the migration bullet under Negative, the Positive bullet on manual reply kinds, and the `Manual<O>` parts of the ADR-0109, ADR-0134 and ADR-0227 amendments no longer apply. §5's replace rule is unchanged: a manual row may become `One(O)` or `None`, and a declared row may not become manual.

References are proofs of what is handled. [ADR-0230](0230-proven-actor-references.md)
made a reference a proof of identity: an actor of this type reached `Live` at
this position, in this engine session. This ADR extends the same reference to
prove the target's contract too, meaning which kinds it handles and how it
answers each one. A send reads the contract off the reference's type, so the
send site has nothing to check beyond the types.

Amends [ADR-0075](0075-actor-typed-sender-api-and-chassis-cap-marker-split.md)
decision 1 and [ADR-0076](0076-collapse-cap-facade-pattern.md) (`HandlesKind`
gating), [ADR-0109](0109-handler-reply-contracts.md) and
[ADR-0227](0227-reply-contracts-are-type-markers.md) (the reply markers and the
manual class's missing reply kind), [ADR-0134](0134-multi-reply-class-and-explicit-handler-classes.md)
(manual declares its reply), [ADR-0230](0230-proven-actor-references.md)
(`ProtocolRef<P>` and the cast mint), and the replace contract of
[ADR-0022](0022-drain-on-swap.md), [ADR-0038](0038-actor-per-component-dispatch.md)
and [ADR-0101](0101-replace-hooks-on-ffiactor.md). The full list is under
[Amendments](#amendments).

## Context

A reply is ordinary mail addressed to whoever sent the request. If the
requester has no handler for the reply's kind, the reply lands on nothing: a
strict receiver logs and drops it at the dispatch miss, and a receiver with a
`#[fallback]` swallows it. Nothing at the call site says that the request
elicits a reply at all.

The compiler already knows half of this. `R: HandlesKind<K>`
(`crates/aether-actor/src/model/mod.rs`) gates every typed send, so sending a
kind the target does not handle is an `E0277`. ADR-0227 added the other half on
the target: `Replies<K, Reply = O>` for a single or deferred handler and
`Streams<K, Item = O>` for a multi handler, emitted by `#[actor]` from the
signature. No bound connects the two. A caller can send `LoadMesh` to an actor
that replies `MeshLoadResult` from a sender that has no `MeshLoadResult`
handler, and it compiles.

Two further gaps widen the hole:

- **Manual handlers declare nothing.** A `#[handler::manual]` handler replies
  whatever `ctx.reply` is handed, so its inputs-manifest row is
  `ReplyContract::Manual` (`crates/aether-data/src/schema.rs`) and it has no
  `Replies` marker (ADR-0227 §3). There are 149 manual handler sites in the
  tree. A caller of one cannot bound on its reply.
- **Erased references carry no contract at all.** An `ErasedActorRef`
  (`crates/aether-actor/src/reference/erased_actor_ref.rs`), from
  `ctx.sender()` or a payload proven through `resolve_live`, sends any kind
  through `ctx.send_to` unchecked (ADR-0230 §2), and the native relay
  `NativeCtx::forward_to` (`crates/aether-substrate/src/actor/native/ctx/send.rs`)
  forwards unchecked with the reply target pinned.

The week's reply bugs are what prompted the survey: #6409 (a reply handle
carried across `replace_component` answered the wrong requester), #6411 (the
puppet lost its owed load reply on a second load or a replace) and #6419 (the
substrate-harness `advance` answered through a path that dropped component
senders). Those three were responder-side delivery faults and were fixed
locally, as are the open silent-drop fixes of the same inventory (#6420 for an
evicted request context, #6421 for a panicking blocking worker). This ADR does
not address them. Runtime delivery and liveness stay best effort, with monitors
as the way to observe a peer's death. What this ADR closes is the caller-side
class, a reply the requester cannot receive, and the untyped request through
an erased reference.

## Decision

The thesis is the opening paragraph: a reference proves its target's contract.
`ActorRef<R>` proves `R`'s full contract. `ProtocolRef<P>` proves the contract
`P` names. The guard cast is the one place an erased reference is proven into a
contract. Everything below is how those three facts are built and kept true.

### 1. The static reply check

A typed send of `K` to a target whose contract for `K` is a reply `O` (single
`-> O`, deferred `-> Pending<O>`, or a declared manual reply) or a multi of `O`
requires the sending actor `A: HandlesKind<O>`. Otherwise it does not
compile. A silent target (`-> ()`) needs nothing from the sender.

```rust
/// The contract row a target (an actor or a protocol) has for `K`.
pub trait Contract<K: Kind> {
    /// The reply kind `O`, [`Silent`], or [`Multi<O>`].
    type Reply: ReplyShape;
}

pub struct Silent;
// `Multi<O>` is the existing ADR-0134 reply-mode marker, reused as the row shape.

/// `A` can receive what a contract sends back.
pub trait ReplyHandledBy<A> {}
impl<A> ReplyHandledBy<A> for Silent {}
impl<A: HandlesKind<O>, O: Kind> ReplyHandledBy<A> for O {}
impl<A: HandlesKind<O>, O: Kind> ReplyHandledBy<A> for Multi<O> {}

impl<T, A> WasmActorMailbox<'_, T, A> {
    pub fn send<K: Kind>(&self, payload: &K)
    where
        T: Contract<K>,
        <T as Contract<K>>::Reply: ReplyHandledBy<A>,
    { /* unchanged runtime path */ }

    /// The one written opt-out: send a request whose reply this actor does
    /// not handle.
    pub fn send_ignoring_reply<K: Kind>(&self, payload: &K)
    where
        T: Contract<K>,
    { /* unchanged runtime path */ }
}
```

`Silent` and `Multi<O>` are `aether-actor` types that never implement `Kind`,
which keeps the three blanket impls disjoint across the crate boundary with
`aether-data` (a minimal two-crate reproduction compiles on edition 2024).
The mailbox handle gains the sender type `A` from the ctx that produced it;
`T` is the target type an `ActorRef<R>` (`T = R`) or `ProtocolRef<P>` (`T = P`)
carries. `#[actor]` emits one `Contract<K>` row per handler beside the
`HandlesKind` / `Replies` / `Streams` markers it already emits, from the same
signature.

A failing check surfaces as the nested obligation `A: HandlesKind<O>`, so
`HandlesKind` gains a `#[diagnostic::on_unimplemented]` message that names the
missing handler and, for the reply case, points at `send_ignoring_reply`.

What the check does not change:

- Replies stay ordinary mail delivered to the sender's typed handler. Runtime
  reply delivery, settlement, correlation, and request contexts are unchanged.
- Several outstanding requests of the same kind need nothing special. The check
  is per type, and correlation stays the runtime's job.
- `#[fallback]` does not count as handling. It emits no `HandlesKind`, so it
  satisfies no reply check on the sender and no `Contract` row on a target.
- Non-actor senders are exempt: the MCP tools, an RPC `Call`, the substrate
  harness's `send_and_settle` / `send_and_await_reply`, chassis threads, and an
  embedder's `PassiveChassis` sends. They have no handler table, and each
  already receives any reply kind as data.
- `send_ignoring_reply` delivers exactly as `send` does. The reply still
  arrives at the sender and takes the dispatch-miss path. The verb exists so
  the discard is written, and the compiler enumerates every site that needs it.
- A reply that is itself a request is a documented caveat. Each hop is checked
  at its own send, and a reply is not a send site. When the reply kind `O` is a
  request at the sender (`A`'s handler for `O` replies `P`), the first request
  is checked at `A`'s call site, but the `P` that returns to the responder is
  not checked against the responder. No new machinery covers this; an exchange
  that needs a second round trip spells it as a second typed send.

### 2. Protocols are zero-cost marker types composing contracts

A protocol names a set of contract rows under a stable type, independent of any
implementation. The primary authoring form is an attribute on a real trait item
whose method signatures mirror a handler list. It is an attribute on a real
item, never a bang macro wrapping a DSL.

```rust
#[protocol]
pub trait MeshLoader {
    fn load(mail: LoadMesh) -> MeshLoadResult;
    fn ping(mail: Ping) -> Pong;
    fn set_mode(mail: SetMode);
    fn watch(mail: Watch) -> Multi<Frame>;
}
```

A signature with no return type is a silent row, `-> O` is a single reply, and
`-> Multi<O>` is a multi row, spelled with the same marker a multi handler's ctx
carries (ADR-0134). The method name labels the row in rustdoc and
diagnostics; a target matches rows by kind, never by method name. Each
parameter needs a name or `_`, because anonymous trait parameters stopped
parsing in the 2018 edition and an attribute's input must parse.

The attribute replaces the trait with a unit struct, so the trait never exists
as a trait object and a protocol costs nothing at run time. It expands to:

```rust
pub struct MeshLoader;

impl Contract<LoadMesh> for MeshLoader { type Reply = MeshLoadResult; }
impl Contract<Ping> for MeshLoader { type Reply = Pong; }
impl Contract<SetMode> for MeshLoader { type Reply = Silent; }
impl Contract<Watch> for MeshLoader { type Reply = Multi<Frame>; }

impl Protocol for MeshLoader {
    const CONTRACTS: &'static [(KindId, ReplyContract)] = &[
        (LoadMesh::ID, ReplyContract::One(MeshLoadResult::ID)),
        (Ping::ID, ReplyContract::One(Pong::ID)),
        (SetMode::ID, ReplyContract::None),
        (Watch::ID, ReplyContract::Multi(Frame::ID)),
    ];
}

/// Any target whose rows cover this protocol narrows to it for free.
impl<T> CoveredBy<T> for MeshLoader
where
    T: Contract<LoadMesh, Reply = MeshLoadResult>
        + Contract<Ping, Reply = Pong>
        + Contract<SetMode, Reply = Silent>
        + Contract<Watch, Reply = Multi<Frame>>,
{
}
```

The `impl Contract<K> for P` rows are the underlying form. The attribute emits
them together with the const list and the narrowing impl, which a hand-written
protocol would otherwise have to keep in step with its rows by hand.
`CONTRACTS` reuses the manifest's own `ReplyContract`, so the const list and the
live handler rows compare in one vocabulary. `CoveredBy` holds for any actor
whose `#[actor]` rows match, and for any protocol that includes this one.

**Composition.** `#[protocol(includes(Pingable, Describable))]` adds the
included protocols' rows to the `Contract` impls, the `CONTRACTS` list, and the
`CoveredBy` where-clause. A proc macro cannot read another item, so every
`#[protocol]` also emits a hidden `macro_rules!` bridge carrying its rows, and
`includes` invokes it, the technique ADR-0169 uses to paste a handler set's
markers. The bridge carries the protocol's own path beside its rows, and the
expansion dedupes by protocol: a protocol reached twice through a diamond of
`includes` contributes its rows once. Rows are keyed by kind, so two distinct
protocols that both list the same kind are still a conflicting-impl error.

### 3. Protocol-typed references

```rust
pub struct ProtocolRef<P> {
    target: ErasedActorRef,
    _protocol: PhantomData<fn() -> P>,
}
```

`ProtocolRef<P>` is the existing proven target plus a phantom protocol. Sends
through it monomorphize against `P`'s `Contract` rows: there is no vtable, no
per-send lookup, and nothing added to the runtime send path. Like every proven
type, it implements no codec and cannot cross a boundary (ADR-0230 §1).

`ActorRef<R>` for a known concrete actor already carries `R`'s rows, so it
proves `R`'s full contract. Narrowing is static and free:

```rust
impl<R> ActorRef<R> {
    pub fn narrow<P: CoveredBy<R>>(&self) -> ProtocolRef<P>;
}
impl<P> ProtocolRef<P> {
    pub fn narrow<Q: CoveredBy<P>>(&self) -> ProtocolRef<Q>;
}
```

`ctx.to` accepts either reference through a `Target<T>` trait both implement,
so one send surface serves both. A narrowed reference is a capability view. Its holder may send only what `P`
lists, whatever else the target handles.

### 4. The guard cast is the single runtime proof point

Widening happens only through the guard:

```rust
impl ErasedActorRef {
    pub fn cast<P: Protocol>(&self, ctx: &impl ProveCtx) -> Option<ProtocolRef<P>>;
}
```

The cast checks `P::CONTRACTS` against the target's handler rows: every row of
`P` must appear, kind and `ReplyContract` alike. Undeclared manual rows match
nothing. It runs once, at receipt, in the handler that received the reference,
exactly as ADR-0230 proves a position once where it arrived. The rows come from
the wasm component's decoded `ActorInputs`
(`crates/aether-substrate/src/actor/wasm/kind_manifest.rs`) and, for a native
capability, from the link-time handler manifest (`HandlerEntry`,
`crates/aether-data/src/name_inventory.rs`). The registry records a target's
rows when its route goes `Live`, so a cast is one published-route read and a
slice comparison. `HandlerEntry.reply` widens from `Option<KindId>` to
`ReplyContract` so a native row can tell a multi handler from a single one.

The native mint lives beside the other mints in
`crates/aether-substrate/src/mail/registry/mailbox/proven.rs`, and
`scripts/check-reference-mint.py` extends its pattern to the protocol mint
without widening its path allowlist. The guest SDK mints its own from the host's
answer, as ADR-0230 §4 already does for `resolve_live`.

A cast that fails returns `None`. The holder decides: refuse the request that
carried the reference, or drop the row. Nothing is parked and no mail is sent.

**What an erased reference may do without a cast.** An erased reference carries
no contract, so no send through it can be checked. Two uses are allowed without
a cast:

1. Replies. Answering through the reply path (`-> O`, `ctx.reply`, a multi
   `emit`) addresses the requester, whose call site was already checked.
2. Publishing. Sending a kind the holder publishes (`Self: Publishes<K>`) to a
   row a subscription filled is allowed, because each subscriber's `subscribe`
   call site already proved it handles `K` silently (§8).

Every other send through an erased target, request or notification, needs a
cast first. `ctx.send_to` narrows to publishing.

**Build skew.** `#[actor]` emits the same `CONTRACTS` list for an actor `R`
that `#[protocol]` emits for a protocol, so `R`'s compiled rows compare against
a loaded target's rows in the manifest vocabulary. Every `ActorRef<R>` door of
ADR-0230 §3 that already consults the registry (the dependency check before
`init`, the spawn and load mints, the embedder's typed read of a load reply,
`child::<P, C>`, and the chassis handle's `actor_ref::<R>()`) runs the same
contract-row check the cast runs, over the rows the peer was compiled against.
A peer built against a different build of `R` than the one loaded is refused
at the door, naming the actor and the missing or changed kind, rather than
holding an `ActorRef<R>` whose static checks describe some other build. Rows
the loaded build adds pass, as they do for a cast. A door that reads the
registry already pays that read; the check adds a slice comparison.

### 5. Replace preserves contracts

A `replace_component` whose replacement drops or changes any contract row its
predecessor declared is refused before the swap, and the old module keeps
running. The refusal has the shape of the `#[actor(depends(R))]` refusal (the
component host's `dependencies.rs`), naming the actor and the kind:
`"<actor> replacement changes its contract for <kind>"`. Added rows are
allowed. Config, documentation, cost, and fallback presence do not count,
because no peer holds a proof about them.

The comparison runs over the same rows the cast reads. A row changes when its
reply shape or reply kind changes, including silent to replying, since a caller
checked against a silent row does not handle the new reply. `KindId` hashes a
kind's schema, so a changed input schema is a dropped row.

This makes a contract monotone, the property ADR-0230 §1 requires of anything a
reference claims. "The target handles at least these rows" can only become more
true across replaces, so no `ProtocolRef` and no static assumption compiled into
any peer goes stale, and neither the cast nor the reference ever needs
revalidating. The principle behind it is the owner's: carrying anything across a
replace is sound only if it cannot degrade state others rely on. Native
capabilities are not replaced at run time; their rows are fixed at link time.

### 6. Manual handlers declare their reply kind

*No longer applies (2026-09-24 amendment): a manual handler replies with any kind. See the amendment line at the top.*

```rust
#[handler::manual]
fn on_load(&mut self, ctx: &mut WasmCtx<'_, Self, Manual<MeshLoadResult>>, load: LoadMesh) { /* … */ }
```

The ctx type is the one statement of the contract, following the precedent
ADR-0134 set for `Multi<O>`: the `#[actor]` macro reads `O` off the
signature's `Manual<O>` marker, records `ReplyContract::One(O)` on the manifest
row, and emits a `Contract<K, Reply = O>` row and a `Replies<K, Reply = O>`
marker. There is no side declaration that can drift from the type the compiler
enforces. A caller sees a declared manual handler exactly as it sees a single
one.

The binding covers every reply the handler can issue, including stored ones.
`ctx.reply` through a `Manual<O>` ctx accepts only `O`, and a reply handle taken
from that ctx is typed by the same `O` (`ReplyHandle<O>`, and on the native
side `DeferredReply<O>`), so a handle kept in actor state and answered from a
later handler can still only send `O`. A manual handler whose ctx names
`Manual<Silent>` declares a silent row; `reply` exists only for a `Kind`, so
calling it there does not compile.

This lands in two steps, because every one of the 149 manual sites changes.
While manual handlers migrate, a bare `Manual` ctx keeps today's
`ReplyContract::Manual` and a `Contract<K, Reply = Undeclared>` row that every
sender accepts. The last migration change deletes `Undeclared` and the bare
`Manual` spelling, after which every manual ctx names `Manual<O>` or
`Manual<Silent>`, and an undeclared manual reply is a compile error.

### 7. The handler ctx is typed by its actor by default

The reply check needs the sender's type. When a handler's ctx omits the actor
parameter, `#[actor]` fills it with `Self`, so a handler receives
`WasmCtx<'_, Self>` / `NativeCtx<'_, Self>` and `wire` receives
`WireCtx<'_, '_, Self>`. The `Erased` default stays for code outside a handler.
A helper generic over the ctx takes `A` and threads the bound:

```rust
fn request_mesh<A>(ctx: &mut WasmCtx<'_, A>, loader: &ProtocolRef<MeshLoader>, path: MeshPath)
where
    A: HandlesKind<MeshLoadResult>,
{
    ctx.to(loader).send(&LoadMesh { path });
}
```

`Erased` handles nothing, so an erased ctx (`ctx.erase()`) can send only to
silent rows, or through `send_ignoring_reply`. Erasing the ctx is not a way
around the check.

### 8. Subscriptions and monitors require a silent handler

A published event and a monitor notice arrive as ordinary mail from the
publisher or the host, and nobody at the other end is waiting for an answer.
An event handler reached by subscription or monitoring must therefore be
silent. `subscribe::<K>()` already requires the publisher `Cap: Publishes<K>`;
it additionally requires the subscriber's own row for `K` to be silent,
`A: Contract<K, Reply = Silent>`. `ctx.monitor(target)` requires the same of the
watcher's row for `MonitorNotice`. A subscriber with no handler for `K`, or with
one that replies, is a compile error at the `subscribe` or `monitor` call site,
so the publisher's sends to its subscriber rows (§4) never elicit a reply that
lands on the publisher. While manual handlers migrate (§6), an `Undeclared` row
passes this bound as it passes every other. The facade traits that carry
`subscribe` (`LifecycleMailboxExt`, the window facade) gain the sender type from
the handle they already sit on.

### 9. Relays

A relay passes the original reply target through, so the reply lands on the
original caller, whose own call site was already checked. The relay itself does
not need to handle the reply. It is sound only if its own handler for the
inbound kind declares the same reply as the peer it forwards to:

```rust
impl<A, O: Kind> WasmCtx<'_, A, Manual<O>> {
    pub fn forward<T, K: Kind>(&mut self, target: &impl Target<T>, payload: &K)
    where
        T: Contract<K, Reply = O>;
}
```

The native `forward_to` and `DeferredReply::hand_off` take the same bound: the
handler's declared reply `O` must equal the target's reply for the forwarded
kind. A relay whose target is erased casts it first.

Relays are defined for single contracts only: a reply `O`, whether the target
answers from a single, deferred, or declared manual handler. `O: Kind` in the
bound excludes a `Multi<O>` row by construction. A handler that passes a multi
request on does so with an ordinary send, so the emissions go to the relay,
which is checked as their sender (`A: HandlesKind<O>`) and re-emits or folds
them itself.

## Scenario sweep

"Compiles" and "compile error" describe the send site. "Runtime guard" means the
cast or the replace refusal decides. "Exempt" means the check does not apply.

### A. The target's contract for `K`

Sender: an actor `A` with a typed ctx, target typed (`ActorRef<R>` or `ProtocolRef<P>`).

| Target's handler for `K` | Contract row | Outcome |
|---|---|---|
| silent `#[handler::single] -> ()` | `Silent` | compiles |
| single `-> O` | `O` | compiles if `A: HandlesKind<O>`, else compile error naming the missing handler |
| deferred `-> Pending<O>` | `O` | as single; when the reply arrives stays ADR-0109 / ADR-0093 |
| multi `Multi<O>` | `Multi<O>` | compiles if `A: HandlesKind<O>`; emissions are detached roots at `A` |
| enum reply `-> O`, `O` an enum kind | `O` | compiles if `A` handles `O`; one handler matches the variants |
| manual, ctx `Manual<O>` | `O` | as single |
| manual, ctx `Manual<Silent>` | `Silent` | compiles; `ctx.reply` inside that handler is a compile error |
| manual, bare `Manual` ctx during migration | `Undeclared` | compiles, unchecked; the bare spelling is deleted when migration ends |
| no handler | none | compile error (`T: Contract<K>` unsatisfied), as `HandlesKind` today |
| `#[fallback]` only | none | compile error; a cast that lists `K` fails at run time |

### B. Addressing

| Reference | Outcome |
|---|---|
| typed `ActorRef<R>`, or `ctx.actor::<R>()` while it survives ADR-0230's contract phase | static check over `R`'s rows; the door that minted the reference checked those rows against the loaded build |
| `ProtocolRef<P>` | static check over `P`'s rows; a kind outside `P` is a compile error even when the target handles it |
| `ActorRef<R>` narrowed to `ProtocolRef<P>` | compiles if `P: CoveredBy<R>`, else compile error |
| `ErasedActorRef` (`ctx.sender()`, an erased child, `resolve_live`) | replies allowed; publishing a kind `Self: Publishes<K>` to already-checked subscriber rows allowed; every other send needs `cast::<P>()` first, a runtime guard |
| by name over the wire (MCP, RPC `Call`, `NamedMail` bundles) | exempt; the boundary proves the position (ADR-0230 §3) and the reply returns to the wire caller |

### C. Sender

| Sender | Outcome |
|---|---|
| typed handler ctx, `WasmCtx<'_, Self>` / `NativeCtx<'_, Self>` (the default after §7) | check applies with `A = Self` |
| `WireCtx<'_, '_, Self>` | check applies; subscriptions and first requests made in `wire` are checked |
| init ctx (`WasmInitCtx`) | not applicable; it resolves addresses and sends nothing |
| generic helper over `A` | compiles when the helper states `A: HandlesKind<O>` and every caller satisfies it |
| erased ctx (`ctx.erase()`, a helper over `Erased`) | silent rows only; a request needs `send_ignoring_reply` |
| non-actor (MCP, RPC `Call`, harness, chassis threads, embedder) | exempt |

### D. Verbs

| Verb | Outcome |
|---|---|
| `send` | check applies |
| `send_detached` | check applies; only the chain differs, and the reply still returns to the sender |
| `send_tracked` | check applies |
| `with_context(..).send`, `send_with_context` | check applies; the reply handler recovers the context as today |
| capability facade methods (`MailboxForward::forward`) | check applies; the facade shim is a send |
| relay (`forward`, native `forward_to`, `DeferredReply::hand_off`) | single contracts only; compiles if the target's reply for the forwarded kind equals the relay handler's declared reply; erased targets cast first |
| `send_ignoring_reply` | compiles for any row the target has; the reply takes the sender's dispatch-miss path, which logs |
| `subscribe::<K>()` | compiles if `Cap: Publishes<K>` and `A: Contract<K, Reply = Silent>` |
| `monitor(target)` | compiles if `A: Contract<MonitorNotice, Reply = Silent>` |
| publish to subscriber rows | compiles if `Self: Publishes<K>`; each subscriber's `subscribe` check proved a silent handler |
| `-> O`, `ctx.reply`, `emit` | reply path; nothing to check at the responder |

### E. Edge cases

| Case | Outcome |
|---|---|
| send to self | compiles if `A` handles its own reply kind |
| sender handles `O` for another reason (it subscribes to `O`, or serves `O` as a request) | compiles; the one handler receives replies and other `O` mail alike and tells them apart by correlation or context |
| the reply kind is itself a request at the sender (`A`'s handler for `O` replies `P`) | documented caveat: each hop is checked at its own send; `P` returns to the responder unchecked, and no machinery is added |
| a multi request passed on by a relay | an ordinary send; the emissions go to the relay, which must handle `O` |
| a subscriber's handler for a published kind, or a watcher's for `MonitorNotice`, replies | compile error at the `subscribe` or `monitor` call site |
| wasm and native parity | same markers and bounds from the same macro; the cast reads `ActorInputs` for wasm and `HandlerEntry` for native |
| replace that drops or changes a row | runtime refusal; the old module keeps running |
| replace that adds a row | allowed |
| cast failure | `None`; the holder refuses or drops, nothing parked |
| a peer compiled against a different build of `R` | runtime guard: the registry-consulting `ActorRef<R>` door runs the cast's contract-row check and refuses a missing or changed row |
| a protocol reached twice through `includes` | the macro dedupes by protocol; its rows appear once |

## Consequences

### Positive

- A request whose reply the requester cannot receive does not compile. The
  caller-side "reply to nothing" class closes, and every deliberate discard is
  written as `send_ignoring_reply`.
- An erased reference cannot carry an untyped request. Widening happens at one
  runtime point, once per reference, in the handler that received it.
- A holder can be handed exactly the rows it may use (`ProtocolRef<P>`), and a
  peer can depend on a protocol rather than an implementation type.
- Manual handlers join the manifest's reply contract, so `describe_component`
  and `describe_handlers` report a reply kind for them.
- Contracts become monotone across replace, so the proofs never need
  revalidating.
- A published event or a monitor notice can never elicit a reply that lands on
  the publisher or the host, because the subscriber's handler is proven silent
  at the `subscribe` or `monitor` call site.
- A peer built against a different build of an actor is refused at the door
  that mints its reference, so a typed reference's static checks always describe
  the loaded build.

### Negative

- Migration. Every manual handler names `Manual<O>` or `Manual<Silent>` in its
  ctx, and stored reply handles gain the reply kind as a type parameter.
  Erased requests gain casts, which is one registry read at receipt per
  reference. The compiler enumerates the sites that need `send_ignoring_reply`.
  Handler and wire ctxs become typed by their actor, so helpers that took the
  erased or `Manual` ctx become generic over `A`.
- A replace that changes a contract is refused. Changing a reply kind now means
  dropping and loading the component under a new name, or adding a new request
  kind beside the old one.
- The registry grows per-route contract rows, and the native manifest widens its
  reply field.
- A subscriber or watcher whose event handler replies must be split into a
  silent handler and a separate request kind before it can subscribe or
  monitor.
- `send_ignoring_reply` still takes the sender's dispatch-miss path, which logs
  each discarded reply. Silencing that path would be a runtime delivery change
  and is out of scope for this ADR.
- A reply that is itself a request stays unchecked on its return hop (§1). The
  caveat is documented rather than closed.

### Neutral

- Runtime reply delivery, settlement, correlation, request contexts, and
  liveness are unchanged. A reply to a dead requester, a lost deferred reply, and
  a crashed responder stay runtime concerns, observed through monitors and fixed
  where they occur.
- No new mail, no new host round trip on the send path, and no per-send cost.
- The multi / stream naming is to be revisited; the owner flagged that 'stream'
  may mislead. This ADR spells the multi row `Multi<O>`, matching the ADR-0134
  class, marker, and manifest variant, and leaves ADR-0227's `Streams` bound
  named as it is.

## Alternatives considered

- **`#[protocol(of = R)]`**, deriving a protocol from one actor's rows. Dropped:
  `ActorRef<R>` already proves `R`'s full protocol, so the derived protocol would
  be a second name for rows the reference type already carries.
- **A `reply = O` argument on `#[handler::manual]`.** Dropped for the `Manual<O>`
  ctx form: it would be a side declaration beside the ctx type, while ADR-0134
  already reads a multi handler's element kind off `Multi<O>` in the signature.
- **Protocols as `dyn` trait objects.** A vtable per send and a boxed or
  borrowed object per reference. Rejected: slow, and the owner judged it gross.
  Marker types give the same checks at zero cost.
- **Runtime-only checks.** Checking at delivery whether the sender handles the
  reply finds the bug in production, pays a lookup per reply, and leaves the
  call site unchanged. Rejected: the facts are all known at compile time.
- **A runtime reply-obligation system** (reply tokens injected into handlers,
  stored in actor state, carried across replace, failing fast when dropped).
  Rejected: this ADR checks that a reply can be received, not that one is
  delivered, and awaiting nothing needs no runtime state.
- **Async handlers.** Rejected for the same reason: the check does not await a
  reply, it verifies that the caller handles the reply it would get.
- **Tracking which protocols were cast against each target**, to allow
  contract-changing replaces that no holder depends on. Rejected: per-target
  bookkeeping for a refusal that is simpler as a rule.
- **Failing stale sends at run time after a contract-changing replace.**
  Rejected: it turns a replace into a runtime fault in every peer instead of one
  refused operation.

## Amendments

- **ADR-0075 decision 1 / ADR-0076.** `HandlesKind` stays the handler marker and
  gains an `on_unimplemented` message. Typed sends bound on `Contract<K>` plus
  the sender's `ReplyHandledBy`, not on `HandlesKind<K>` alone.
- **ADR-0109.** The ban on a reply annotation stands: a manual handler states
  its reply kind in its ctx type, `Manual<O>`, which is its only statement of
  it.
- **ADR-0227.** `Replies` / `Streams` stay the narrower bounds for helpers; a
  send through them carries the same sender bound. Decision 3 reverses: a manual
  handler with a declared reply gets `Replies<K, Reply = O>` and a `Contract`
  row.
- **ADR-0230.** `ProtocolRef<P>` joins the proven types with no codec. The cast
  is a new door for erased references, minted in `proven.rs` under the existing
  gate. `ctx.send_to` narrows to publishing. The `ActorRef<R>` doors that
  consult the registry also check `R`'s compiled contract rows against the
  loaded target's rows. `ctx.monitor` requires a silent `MonitorNotice`
  handler.
- **ADR-0134.** The manual reply mode becomes `Manual<O>`, written in the ctx
  and read by the macro, mirroring `Multi<K>`; reply handles taken from it are
  typed by `O`. `Multi<O>` also serves as the multi row shape in `Contract` and
  in `#[protocol]` signatures.
- **ADR-0022 / ADR-0038 / ADR-0101.** `replace_component` refuses a replacement
  that drops or changes a contract row, before `on_dehydrate` runs.
