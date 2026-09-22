# ADR-0230: Proven Actor References

- **Status:** Proposed
- **Date:** 2026-09-21

Amends [ADR-0099](0099-actor-identity-and-addressing.md) (the lineage fold
stays how a position is *derived*; a derived position stops being something
a caller can *send to*), [ADR-0166](0166-typed-actor-lineage-and-abbreviated-external-addresses.md)
(its string grammar becomes the text form of one typed value),
[ADR-0075](0075-actor-typed-sender-api-and-chassis-cap-marker-split.md)
(`HandlesKind<K>` gains a stored, kind-typed reference), and
[ADR-0133](0133-reply-based-stream-handles-for-the-http-server-data-phase.md) (its
`send_detached_to(MailboxId)` recipient and its
`{ counterparty: MailboxId, stream_id }` handle shape become the proven
forms — an `AnyActorRef` constructed from `ctx.sender()`, so a stream
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
stays true forever, so it is a type. "This actor is dead" is terminal, so it
is a type. "This actor is alive" can be falsified between any two
instructions, so it is never a type; death is observed through `monitor` and
`MonitorNotice`.

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
pub struct AnyActorRef { id: MailboxId }
pub struct Tombstone<R> { id: MailboxId, _actor: PhantomData<fn() -> R> }
```

| Type | Claims | Made by | Can |
|---|---|---|---|
| `Namespace` | the grammar is valid | `const fn new`, a compile error when invalid | compare, `Debug`, fold to an `ActorId` |
| `R::Key` | the discriminator is valid | the actor type's own fallible constructor and fallible decode | build an `Address` |
| `Address<R>` | the description is well-formed; nothing about existence | `R::address()`, `R::address_at(key)`, `parent.child::<C>(key)`, `reference.address()`, the boundary parser | be stored, mailed, configured, persisted; be resolved. The only reference form with a wire format. |
| `ActorRef<R>` | an `R` reached `Live` at this id, in this engine session | section 3 only | send, monitor, be held in actor memory, yield its `Address` |
| `AnyActorRef` | some actor reached `Live` at this id | the envelope sender; the registry's liveness read over a position that arrived in a payload | reply, monitor, be the target of an untyped send, detached or tracked, be held in a capability's own table and keyed in an ordered set |
| `Tombstone<R>` | that actor is dead | exchanging a reference on its `MonitorNotice` | key cleanup of held state |
| `MailboxId` | nothing; it is a position | the fold, decode | be a registry key, be printed |

`ActorRef::id()` is free and total. There is no function from a `MailboxId`
to anything sendable outside the registry.

`Address<R>` is the typed form of ADR-0166's grammar, and which constructor
exists is decided by `R`'s placement facts (`Root`, `ChildOf<P>`,
`Singleton`, `Instanced`): a child address needs a proven parent. A loaded
component's key is its load name, a validated `LoadName`; a window's is its
window id. There is one addressing system and this is its value type.

A capability keeps the envelope sender as an `AnyActorRef`.

The per-handler handle keeps its job of carrying origin, now fed by a
reference rather than a raw id: `ctx.to(&actor_ref).send(&kind)` replaces
`actor_at::<R>(id)`.

### 3. The doors: where a reference comes from

| Source | Proof | Runtime cost |
|---|---|---|
| A declared dependency of the actor | the `#[actor]` dependency list is emitted to the wasm custom section; each entry folds to its position through its strategy — `One` at the root, `Embedded` beneath the placement's parent — and the host requires a `Live` route there before `init` (native: at chassis build, and at spawn for a spawned child). A missing dependency refuses the load and names it. | none for `One` — at depth 1 the fold is a `const`; one registry read per `Embedded` entry |
| Self, parent, inline cluster members | structural; the host supplies them at `init` and the SDK mints them | none |
| A child this actor spawned or loaded | the result mail carries the child's exact `Address`; the parent resolves it | one lookup per child |
| The envelope sender | the host stamps the origin at dispatch, so the SDK mints it from the host's value | none |
| A position that arrived in mail, config, saved state, or from another process | the ctx verb `resolve_live`, over the host's liveness read of the published route view: `Live` mints, `Dropped` and `Unknown` refuse by name, and `Starting` reads as unknown (section 1). Minted once, at receipt, in the handler that received the field — never at the send. The registry method behind it is crate-private, so the verb is the only spelling a capability has. | one published-route read per proof; no lock, no allocation |

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

Strings exist in exactly one place: the host's `resolve_address` parser
behind the MCP, RPC, and harness boundary, which yields an `Address` and then
takes the same `resolve` as everything else.

### 4. Gate the eliminators, not the constructors

A validated `Namespace` grants no ability to send, so its constructor is
public. What is removed is every sink that turns text into something
sendable, and every way to read the text back out: `Namespace` has no
`as_str`, no `Deref`, and no `Display`. `format!("{}/{}", …)` over two
namespaces stops compiling; `{:?}` renders `Namespace("aether.render")`,
fine in a log and useless as an address. The registry renders canonical
names from the macro-emitted inventory records, which never pass through the
type.

`Namespace`, `LoadName`, and `Address<R>` live in `aether-data`, because kinds
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
straight from a fold or a raw id, and `resolve_actor::<R>(&str)` keyed by
text; `LoadResult`'s rendered
`name: String` as an address to re-hash.

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
  sites or of `#[allow(clippy::disallowed_methods)]` lines does not land.
- The inline-child alias (`RouteLifecycle::Alias`) remains a second id for
  one actor. A reference to an alias is valid under this decision; unifying
  the alias with its trampoline for monitoring (issue 4202) is separate work.

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
