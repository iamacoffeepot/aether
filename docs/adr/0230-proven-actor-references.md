# ADR-0230: Proven Actor References

- **Status:** Proposed
- **Date:** 2026-09-21
- **Amended:** 2026-09-23 — §5's deleted `ctx.actor::<R>()` handle is replaced at the call site by flat ctx verbs (`ctx.send::<R>(&k)`, `ctx.subscribe::<P, K>()`, `ctx.send_to(&r, &k)`) proven by `#[actor(depends(R))]`, with no optional peers ([ADR-0232](0232-flat-ctx-send-verbs.md)).
- **Amended:** 2026-09-23 — §3's declared-dependency check reaches two more births: an inline-spawnable actor's dependencies are checked when its module loads, and a native dependency on a pumped slot passes the birth check on the slot's Claim-stage reservation, with the boot failing if the pump never goes `Live`.
- **Amended:** 2026-09-24 — §3's dependency list is written as one `depends(A, B, …)` per `#[actor]`: `depends(R)` is a list of one, and a second `depends(...)` in the same attribute is a compile error that points at the list (#6557). [ADR-0232](0232-flat-ctx-send-verbs.md) §2's example is respelled to match.
- **Amended:** 2026-09-24 — §3: a wire `Call` names its recipient by `ActorPath`; the engine that hosts the recipient resolves and proves it on arrival, an unresolved path is answered as not present (`RpcError::NotPresent`), and no mailbox id crosses the RPC wire as a recipient or in a reply.
- **Amended:** 2026-09-24 — §3's module-load check reaches private inline children: the types `export!` lists under `private = [..]` are read from the module's `aether.kinds.inputs.private` section and checked like the exported inline-spawnable actors (#6590).
- **Amended:** 2026-09-24 — §3's declared-dependency proof `DependsOn<R>` is an `unsafe trait` that only `#[actor(depends(..))]` implements; its safety contract is that the macro also records the dependency entry the pre-`init` check reads, and a hand-written safe impl is refused with `E0200` (#6614).
- **Amended:** 2026-09-24 — §2: a proven reference's canonical `ActorPath` is readable through the host registry (`NativeCtx::actor_path`) for diagnostics, as text only, never a position or anything sendable; the registry proves each route's name against the ADR-0166 grammar when the route is first published, so the read cannot fail for a reference it minted (#6635).

Amends [ADR-0099](0099-actor-identity-and-addressing.md) (the lineage fold
stays how a position is *derived*; a derived position stops being something
a caller can *send to*), [ADR-0166](0166-typed-actor-lineage-and-abbreviated-external-addresses.md)
(its string grammar becomes the text form of one typed value),
[ADR-0075](0075-actor-typed-sender-api-and-chassis-cap-marker-split.md)
(`HandlesKind<K>` gains a stored, kind-typed reference), and
[ADR-0133](0133-reply-based-stream-handles-for-the-http-server-data-phase.md) (its
`send_detached_to(MailboxId)` recipient and its
`{ counterparty: MailboxId, stream_id }` handle shape become the proven
forms — an `ErasedActorRef` constructed from `ctx.sender()`, so a stream
handle cannot exist without the proof it sends to).

## Context

The decision splits one type into a position, a description, and a proof.
Everything below follows from keeping those three apart.

A `MailboxId` is a position. `fold_lineage` and `mailbox_id_from_path`
(`crates/aether-data/src/hash.rs`) derive it deterministically from a
lineage, so an id exists for every lineage anyone can spell, registered or
not. The type is `pub struct MailboxId(pub u64)`, `Pod` and `Default`, and
its wire decode is `u64::decode(cursor).map(Self)`
(`crates/aether-data/src/wire/leaf.rs`). Every integer is a well-formed
`MailboxId`, and nothing distinguishes one the substrate registered from one
a caller computed.

That is the root of the project's most frequent defect class. Two
derivations of "the same" id agree only at depth 1, so a non-root receiver
is missed and the miss is quiet: `route_mail`
(`crates/aether-substrate/src/mail/mailer.rs`) parks mail for an unknown
recipient through `Registry::park_or_drop`, and an absent one ends in the
unknown-recipient policy (bubble to the hub, otherwise warn-drop). The host
function `send_mail_p32` validates `from` and never `recipient`. Four
successive passes narrowed *which function* may compute an id
(`clippy.toml` `disallowed-methods`); 184 `#[allow(clippy::disallowed_methods)]`
sites show how that escape is used, and tuple construction `MailboxId(x)` was
never covered — roughly 600 non-test sites.

Four further facts shape the decision:

1. **Typed handles cannot be stored.** `WasmActorMailbox<'a, R>` and
   `NativeActorMailbox<'a, R>` borrow the ctx so that a send's origin is
   always the executing actor's. Anything kept in actor state or carried in a
   kind therefore degrades to a raw `MailboxId` — 229 field declarations,
   among them `SubscribeWindow.mailbox`, `RegisterRoute.mailbox`, and
   `BindListener.consumer`.
2. **The guest can derive a position and cannot know whether it is
   occupied.** `Resolve` (`crates/aether-actor/src/model/mod.rs`) is the one
   derivation: each strategy (`One`, `Many`, `Embedded`, `EmbeddedMany`)
   declares a `CallerScope`, and the runtime retains the caller's logical
   parent so a co-hosted peer folds under the shared host rather than under
   the caller (`scope_mailbox` on the guest's inline registry and on the native
   binding). The fold is correct and total, so
   it answers for a peer that was never loaded exactly as it answers for one
   that was. Only the host's registry knows which.
3. **The authoritative resolver already exists.** `Registry::resolve_address`
   and `lookup_canonical`
   (`crates/aether-substrate/src/mail/registry/mailbox/resolve.rs`) expand an
   ADR-0166 address through `AddressIndex::expand_segment`, require a
   `Starting` or `Live` route, and return a structured
   `AddressResolutionError`. Only the MCP and inventory edge reach it.
4. **Registration is already recorded, and already nearly monotone.** A route
   record (`RouteLifecycle::{Starting, Live, Alias, Dropped}`) is created at
   reservation. Retiring an actor leaves its route in place and records the
   id in `ActorRegistry::tombstones`
   ([ADR-0079](0079-instanced-actors-as-a-first-class-category.md): names are never reused). The one
   backwards edge is `RegistryEffect::CancelStarting`, which removes the
   route of a birth whose `init` failed.

`NAMESPACE` is the other raw string in the flow. Outside the SDK it is
consumed as path assembly (`format!("{}/{}:{name}", Host::NAMESPACE,
Trampoline::NAMESPACE)`), as a direct hash input
(`mailbox_id_from_name(X::NAMESPACE)`), and as a recipient string handed to
harness operations.

## Decision

### 1. The invariant: a reference is a lower bound on a monotone state

A registered actor's lifecycle only moves forward: `Live` then `Dead`, and
ADR-0079 forbids the name ever being registered again. A reference type may
claim exactly what stays true under that order. "This actor reached `Live`"
stays true forever, so it is a type. "This actor is alive" can be falsified
between any two instructions, so it is never a type. Death is observed through
`monitor`: the host delivers the fieldless `MonitorNotice` with the departed
actor stamped as its envelope sender, so the watcher reads the departed actor
from `ctx.sender()` as the same reference it monitored and drops its entries by
keyed lookup. The notice carries no position. A reference never updates
itself from a notice: it goes on claiming only that its actor reached `Live`,
which stays true after the actor dies. "This actor is dead" is terminal too,
but no held state needs a value that says so, so it is not a type.

A reference is issued only once its target is `Live`. `Starting` stays
internal to the registry, so the `CancelStarting` edge never invalidates a
reference, and a load that failed `init` can be retried under the same name.

Because the claim is monotone it is discharged once. Nothing revalidates a
reference, no mail is spent keeping one valid, and no generation counter is
added to the id.

The claim is relative to one engine in one session, and that decides what may
be exported. A type that cannot uphold its invariant on the far side of a
boundary is not exportable. A structural check at decode (tag bits, non-zero)
is not the invariant: the same bytes arrive from saved state, a config file,
a save written last session, an MCP parameter, or another engine, where the
same id is a well-formed position that may hold a different actor or nothing.
So the proven types implement no `Serialize`, `Deserialize`, `WireEncode`,
`WireDecode`, or `Schema`. They cannot be a kind field, a config field, or
saved state; they exist only in the memory of the context that proved them.
`Address<R>` is the one form that crosses a boundary, because it claims
nothing, and the receiver proves it again on its own side.

### 2. The types

```rust
pub struct Namespace(&'static str);
pub enum Address<R> { Scoped { key }, Beneath { parent, key }, Exact { id } }
pub struct ActorRef<R> { id: MailboxId, _actor: PhantomData<fn() -> R> }
pub struct ErasedActorRef { id: MailboxId }
```

| Type | Claims | Made by | Can |
|---|---|---|---|
| `Namespace` | the grammar is valid | `const fn new`, a compile error when invalid | compare, `Debug`, fold to an `ActorId` |
| `R::Key` | the discriminator is valid | the actor type's own fallible constructor and fallible decode | build an `Address` |
| `Address<R>` | the description is well-formed; nothing about existence | `R::address()`, `R::address_at(key)`, `parent.child::<C>(key)`, `reference.address()` | be stored, mailed, configured, persisted; be resolved. The only reference form with a wire format. |
| `ActorPath` | the text is a well-formed ADR-0166 address, canonical or short (with `:name` holes); nothing about existence or placement | its fallible constructor and fallible decode | be carried in a kind (`NamedMail.recipient`) and name a wire `Call`'s recipient, compared, displayed; become a position only inside the engine, through the host's `resolve_address` |
| `ActorRef<R>` | an `R` reached `Live` at this id, in this engine session | section 3 only | send, monitor, be held in actor memory, yield its `Address`, name its canonical path |
| `ErasedActorRef` | some actor reached `Live` at this id | the envelope sender, including a monitor notice's sender; the registry's liveness read over a position that arrived in a payload | reply, monitor, be the target of an untyped send — inheriting, detached, or tracked, unchecked against a kind because the set it keys may be heterogeneous — be held in a capability's own table and keyed in an ordered set, name its canonical path |
| `MailboxId` | nothing; it is a position | the fold, decode | be a registry key, be printed |

`ActorRef::id()` is free and total. There is no function from a `MailboxId`
to anything sendable outside the registry.

`ActorPath` is the text form of ADR-0166's grammar, and `Address<R>` is its
typed description. Which `Address<R>` constructor exists is decided by `R`'s placement facts (`Root`, `ChildOf<P>`,
`Singleton`, `Instanced`): a child address needs a proven parent. A loaded
component's key is its load name, a validated `LoadName`; a window's is its
window id. There is one addressing system and this is its value type.

A capability keeps the envelope sender as an `ErasedActorRef`.

The per-handler handle keeps its job of carrying origin, now fed by a
reference rather than a raw id: `ctx.to(&actor_ref).send(&kind)` replaces
`actor_at::<R>(id)`, which is deleted; an erased reference sends through
`ctx.send_to` or, with a request context, `ctx.send_with_context`.

### 3. The doors: where a reference comes from

| Source | Proof | Runtime cost |
|---|---|---|
| A declared dependency of the actor | the `#[actor]` dependency list (`depends(A, B, …)`) is emitted to the wasm custom section; each entry folds to its position through its strategy — `One` at the root, `Embedded` beneath the placement's parent — and the host requires a `Live` route there before `init` (native: at chassis build, and at spawn for a spawned child). A missing dependency refuses the load and names it. | none for `One` — at depth 1 the fold is a `const`; one registry read per `Embedded` entry |
| Self, parent, inline cluster members | structural; the host supplies them at `init` and the SDK mints them | none |
| A child this actor spawned or loaded | A native staged spawn's completion, `TaskDone<SpawnOutcome<C>, _>`, carries `ActorRef<C>` on its `Ok` arm, minted by the registry when the birth's finalizer runs after the owner has published the child's `Live` route. A loaded component's successful `LoadResult` is delivered from the loaded actor itself: the component host hands its owed reply to the trampoline it spawned, which replies in its own name, so the requester keeps the reply's stamped sender — `ctx.sender()` for an actor, the reply event's sender for an embedder. `LoadResult::Ok` carries the canonical `ActorPath` and no position. A guest's inline spawn returns the typed `InlineChild<C>` handle; a parent keeping children of several types keeps the erased form instead — `InlineChild::erase`, or `spawn_inline_child_by_tag`'s `ErasedActorRef` — a proof without the type. Two embedder cases share the row: an embedder spawn's `finish` returns `ActorRef<A>` once its commit has published the route, and a chassis-composed actor's reference is recorded at boot, when the route goes `Live`, and read back by type through the chassis handle's `actor_ref::<R>()` | none for a spawn or a load; one published-route read when an embedder types a load reply's sender |
| A child beneath a reference an embedder already holds | the embedder builds the child's `Address<C>` from the parent's proof and the child's key — `child_address::<P, C>(parent, key)`, bounded `C: ChildOf<P> + Instanced` — and the chassis handle's `child::<P, C>` folds it with `C`'s resolver beneath the parent's position and proves the route with the published-route read `resolve_live` takes: only `Live` mints, and `Starting`, `Dropped`, and never-registered positions refuse with `ChildRefused`, which names the key and `C::NAMESPACE`, never a position. This is the first provider of the `Address<R>` form, fed by a held proof rather than a foreign address; its consumer is the substrate harness's `child`, which reaches a component's spawned child or a window capability's opened window without rendering an address | one published-route read per lookup |
| The envelope sender | the host stamps the origin at dispatch, so the SDK mints it from the host's value. A `MonitorNotice` is host-generated mail that carries one: the host stamps the departed actor, which `register_monitor` required to be `Live`, so the watcher's `ctx.sender()` is a reference to it. | none |
| A position that arrived in mail, config, saved state, or from another process | the ctx verb `resolve_live`, over the host's liveness read of the published route view: `Live` mints, `Dropped` and `Unknown` refuse by name, and `Starting` reads as unknown (section 1). Minted once, at receipt, in the handler that received the field — never at the send. The registry method behind it is crate-private, so the verb is the only spelling a capability has. | one published-route read per proof; no lock, no allocation |
| An `Address<R>` that arrived in mail, config, saved state, or from another process | not yet provided: no door turns a foreign address into a reference. It lands against the first migrated site that holds one. The first such site — the editor shell's `RegionSpec.target`, issue #6306 — dropped the field instead, so the region announces itself and the shell keeps the envelope sender; the door stays unprovided. | — |

**Amendment (2026-09-23): two births the declared-dependency check did not
reach.** The first row checked dependencies only at component load and
boot-plan load (wasm) and at each native birth site. Migrating call sites to
ADR-0232's flat verbs exposed two gaps:

- **Inline-spawnable actors.** A composable actor that a guest spawns inline
  through `spawn_inline_child_by_tag` (the kit-widget types, the behavior
  host's children) runs before the host sees it, and the trampoline stages its
  alias only afterwards. A `depends(R)` on such an actor compiled a `DependsOn`
  proof that nothing checked. When the host loads a module, it now also checks
  the declared dependencies of every inline-spawnable actor in that module, with
  the same refusal naming the dependency, before anything in the module runs.
  Private inline children, which the module rebuilds but does not export, are
  read from the module's `aether.kinds.inputs.private` section and checked the
  same way (#6590).
- **A native dependency on a pumped slot.** A pumped actor goes `Live` after
  the passive actors' `init`: the desktop driver boots render from its
  Claim-stage reservation, and the harness chassis leaves render to the
  embedder after build. A passive that declares it, such as text declaring
  render, would be refused on every such boot. A native declared dependency
  whose target is a pumped slot reserved at the Claim stage passes the birth
  check, and the boot fails if that pump never goes `Live`. This is the one
  dependency accepted before its target is `Live`, and only for the length of
  the boot: once a boot completes, every declared dependency is `Live`.

An off-thread helper that only wakes its own actor — an accept loop, a socket
reader, a timer — holds a `SelfWake<K>` from the ctx (`ctx.self_wake::<K>()`)
rather than any of these: it names no position, carries no reference, and can
send only that one wake.

What arrived stays a position. The payload-borne door changes nothing about
the wire: `SubscribeWindow.mailbox` is still a `MailboxId` and still decodes
as one, because a proof cannot cross a boundary (section 1). The proof it
yields lives only in the receiver's memory, from the moment of receipt until
the row is dropped.

The position and the proof have different owners. `Resolve` stays the single
derivation of a position, fed by an `Address<R>` and never by text. The host
is the single authority on whether that position is occupied, and it is the
only thing that can turn one into a reference. Handing a reference to a peer
means sending `reference.address()`; the peer's `resolve` is one synchronous
host call and no mail. Persisted state stores an `Address` for the same
reason, and this is enforced by the types having no codec rather than by
convention.

Strings exist in exactly one place: text crosses the MCP, RPC, and harness boundary as an `ActorPath`, validated on construction and on decode. A wire `Call` names its recipient by that path, beside the engine that hosts it, so a malformed path fails the frame decode and never reaches resolution, and no mailbox id crosses the wire as a recipient. Nothing outside the engine computes a position for a path: not aether-mcp, not a harness, and not the hub, which relays an engine-addressed `Call` to that engine's proxy with the path as written. The engine that hosts the recipient resolves the path when the `Call` arrives, through the host's `resolve_address`, the one place an `ActorPath` becomes a position, and proves the answer at once through the payload-borne door above. The proof does not leave the RPC server's handler: the server holds a deliver-only item, the same shape a bundle item takes, and delivering it is all it can do. A path that does not resolve to a `Live` actor is not present, whatever the reason: never registered, still starting, dropped, or a short path that is ambiguous or names no declared child. The call closes with `RpcError::NotPresent`, which names the path and carries the registry's diagnostic; nothing is parked or dropped, and the hub relays the refusal to the caller unchanged. A client holding an id an engine reported, from a trace tree or a window listing, asks that engine for the id's canonical path and sends by the path. A reply on the wire carries its kind and bytes and no address.
A mail bundle — the `NamedMail` list that `DispatchTraced` and `CaptureFrame`
carry — is the same boundary inside a payload: the receiving capability proves
every `ActorPath` recipient once, before any item moves, and a proven item can
only be delivered, with the bytes the boundary encoded.

### 4. Gate the eliminators, not the constructors

A validated `Namespace` grants no ability to send, so its constructor is
public. What is removed is every sink that turns text into something
sendable, and every way to read the text back out: `Namespace` has no
`as_str`, no `Deref`, and no `Display`. `format!("{}/{}", …)` over two
namespaces stops compiling; `{:?}` renders `Namespace("aether.render")`,
fine in a log and useless as an address. The registry renders canonical
names from the macro-emitted inventory records, which never pass through the
type.

`Namespace`, `LoadName`, `Address<R>`, and `ActorPath` live in `aether-data`, because kinds
in `aether-kinds` carry addresses and `aether-actor` depends on that crate.
The proven types live beside `Addressable` in `aether-actor` with
crate-private constructors: nothing serializable names them, so no kind crate
needs them, and the guest SDK mints its own from the host's answers without
any public door.

Rust visibility is crate-granular and the native registry in
`aether-substrate` also has to mint, so there is one `#[doc(hidden)]` mint
function. It is guarded by a gate with a path allowlist naming the registry
module and with no in-source `#[allow]` escape: widening it means editing the
gate in a reviewed diff.

### 5. What is deleted

`MailSender::send_to_named` and `send_detached_to_named`; `resolve_mailbox`
and `Mailbox<K>`; public access to `mailbox_id_from_name`,
`mailbox_id_from_name_pair`, `mailbox_id_from_path`, and
`MailboxId::from_name`; the public field, `Pod`, and `Default` on
`MailboxId` (`MailboxId::NONE` becomes `Option`); the unproven handle:
`ctx.actor::<R>()` and `actor_at::<R>(id)` returning something sendable
straight from a fold or a raw id (`actor_at` is gone from both native ctxs),
and `resolve_actor::<R>(&str)` keyed by text; `LoadResult`'s rendered
`name: String` as an address to re-hash, and its `mailbox_id`.

## Consequences

- The dominant defect class is unrepresentable: `Resolve` is the only
  derivation left and text never reaches it, so two derivations cannot
  disagree. A position nothing registered resolves to `None` at the one
  place the dependent asked, instead of absorbing mail.
- A missing chassis dependency is a refused load naming the dependency,
  rather than a warn-drop on every tick.
- Steady-state sends get cheaper. A stored reference or a constant replaces a
  fold recomputed per `ctx.actor::<R>()` call. No mail is added anywhere on
  the send path.
- A kind never carries a reference. A kind that names an actor to send to
  carries an `Address<R>`, which is not cast-shape (`Pod`), and its receiver
  pays one synchronous host call to resolve it, once. Most of the affected
  families are subscribe and register shapes, and many can drop the field in
  favor of the envelope sender, as `SubscribeWindowSelf` already does.
- Mail addressed to a position nobody has registered is no longer expressible
  from an actor, so parking stops being the mechanism for boot-order
  independence between separately loaded peers. A dependent resolves its peer
  in `wire` and treats absence as its own error; the boot manifest carries
  ordering. A registration notification (`await_registered`, one mail, once,
  replacing the parked mail) is a possible later addition and is not part of
  this decision.
- The work lands as independent issues in three groups. *Expand*: the types;
  host `resolve` and the mint gate; declared dependencies with the load-time
  check; references on spawn results, load results, and the envelope sender;
  the typed boundary for MCP, RPC, and the harness. *Migrate*: one change per
  crate converting its kinds and stored state. *Contract*: one deletion per
  item in section 5, each landable only when its call-site count is zero.
  During expand and migrate, a change that raises the count of old-door call
  sites or of `#[allow(clippy::disallowed_methods)]` lines does not land, and
  the rule is `scripts/check-raw-mailbox-ratchet.py` against the counts in
  `scripts/raw-mailbox-baseline.json` — a required check rather than a step
  in each issue's plan.
- The inline-child alias (`RouteLifecycle::Alias`) remains a second id for
  one actor. A reference to an alias is valid under this decision; unifying
  the alias with its trampoline for monitoring (issue 4202) is separate work.
  An alias departs under its own reference: the host sends one notice per
  departing alias with the alias as its sender, the identity the inline
  child's own sends stamp (ADR-0114 §4), so a capability finds the child's
  rows under the reference it filed them by.

## Alternatives considered

- **Keep narrowing the computing functions by lint.** Four passes did not
  close it: tuple construction stays open and `#[allow]` is an in-source
  escape.
- **Keep references in kinds and have the host validate them at delivery.**
  A schema walk and a registry lookup per reference field per mail, paid by
  every delivery whether or not the receiver uses the reference, and the
  guest-side decode stays open to any bytes.
- **Liveness in the type.** Not monotone; holding it true would cost mail.
- **Generational ids.** Already rejected by ADR-0079 as a wire change, and
  unnecessary while names are never reused.
- **Issue references at `Starting` and tombstone failed births.** Makes the
  order total, but burns the name of every load whose `init` failed, so a
  corrected retry under the same name is refused.
- **One type with a state parameter (`ActorRef<R, S>`).** The states carry
  different data (a description, an id, an unsendable id), so it is three
  structs under one name and a longer spelling at every use.
- **Serializable references with a structural check at decode.** The first
  form of this decision. Decode cannot re-establish "reached `Live` here",
  and the type cannot know where its bytes came from, so every codec impl is
  a door that skips the registry.
