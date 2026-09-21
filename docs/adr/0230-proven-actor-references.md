# ADR-0230: Proven Actor References

- **Status:** Proposed
- **Date:** 2026-09-21

Amends [ADR-0099](0099-actor-identity-and-addressing.md) (the lineage fold
stays how a `MailboxId` is *assigned*; it stops being how an actor is
*looked up*), [ADR-0166](0166-typed-actor-lineage-and-abbreviated-external-addresses.md)
(its string grammar becomes the text form of one typed value), and
[ADR-0075](0075-actor-typed-sender-api-and-chassis-cap-marker-split.md)
(`HandlesKind<K>` gains a stored, kind-typed reference).

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
2. **The guest cannot answer scoped questions.** It holds a rolling `u64`
   carry, which is a fold and is not invertible. `ctx.actor::<Peer>()`
   between two co-hosted components folds the peer under the *caller*, a
   position nothing registers. The host holds the real tree.
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

### 2. The types

```rust
pub struct Namespace(&'static str);
pub struct Address<R: Addressable> { /* anchor + typed keys */ }
pub struct ActorRef<R: Addressable> { id: MailboxId, _actor: PhantomData<fn() -> R> }
pub struct Recipient<K: Kind> { id: MailboxId, _kind: PhantomData<fn(K)> }
pub struct AnyActorRef { id: MailboxId }
pub struct Tombstone<R: Addressable> { id: MailboxId, _actor: PhantomData<fn() -> R> }
```

| Type | Claims | Made by | Can |
|---|---|---|---|
| `Namespace` | the grammar is valid | `const fn new`, a compile error when invalid | compare, `Debug`, fold to an `ActorId` |
| `R::Key` | the discriminator is valid | the actor type's own fallible constructor and fallible decode | build an `Address` |
| `Address<R>` | the description is well-formed; nothing about existence | `R::address()`, `R::address_at(key)`, `parent.child::<C>(key)`, the boundary parser | be stored, mailed, configured, persisted; be resolved |
| `ActorRef<R>` | an `R` reached `Live` at this id | section 3 only | send, monitor, store, ride in mail |
| `Recipient<K>` | an actor that handles `K` reached `Live` at this id | `ctx.me().recipient::<K>()`, bounded on `HandlesKind<K>`; host-checked narrowing of an `AnyActorRef` | send `K`, monitor, store, ride in mail |
| `AnyActorRef` | some actor reached `Live` at this id | the envelope sender | reply, monitor, narrow |
| `Tombstone<R>` | that actor is dead | exchanging a reference on its `MonitorNotice` | key cleanup of held state |
| `MailboxId` | nothing; it is a position | the fold, decode | be a registry key, be printed |

`ActorRef::id()` is free and total. There is no function from a `MailboxId`
to anything sendable outside the registry.

`Address<R>` is the typed form of ADR-0166's grammar, and which constructor
exists is decided by `R`'s placement facts (`Root`, `ChildOf<P>`,
`Singleton`, `Instanced`): a child address needs a proven parent. A loaded
component's key is its load name, a validated `LoadName`; a window's is its
window id. There is one addressing system and this is its value type.

`Recipient<K>` replaces `Mailbox<K>` and the string-taking `resolve_mailbox`.
Subscriber and consumer fields are this type: the window capability knows its
subscriber handles `Key` and nothing else about it.

The per-handler handle keeps its job of carrying origin, now fed by a
reference rather than a raw id: `ctx.to(&actor_ref).send(&kind)` replaces
`actor_at::<R>(id)`.

### 3. The doors: where a reference comes from

| Source | Proof | Runtime cost |
|---|---|---|
| A declared root singleton of the chassis | the `#[actor]` dependency list is emitted to the wasm custom section and checked against the chassis's linked inventory before `init` (native: at chassis build). A missing dependency refuses the load and names it. | none; at depth 1 the fold is a `const` |
| Self, parent, inline cluster members | structural | none; handed over at `init` |
| A child this actor spawned or loaded | the spawn or load result carries the reference | none beyond the spawn |
| The envelope sender; a reference field in mail | the type carries the proof between in-engine actors | none |
| `ctx.resolve(&address) -> Option<ActorRef<R>>` | one registry lookup on the host, synchronous, no mail | one lookup per reference, ever |
| Bytes from another process (RPC ingress) | resolved against the registry at decode; a miss is an error reply | one lookup per reference field, at ingress only |

Resolution runs on the host for every case that is not a compile-time
constant, because only the host holds the tree. References do not go to
disk; persisted state stores the `Address` and resolves it on load.

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

`ActorRef` and its siblings live beside `Addressable` in `aether-actor` with
crate-private constructors. Rust visibility is crate-granular and the native
registry in `aether-substrate` also has to mint, so there is one
`#[doc(hidden)]` mint function. It is guarded by an xtask gate with a path
allowlist naming the registry module and with no in-source `#[allow]`
escape: widening it means editing the gate in a reviewed diff.

### 5. What is deleted

`MailSender::send_to_named` and `send_detached_to_named`; `resolve_mailbox`
and `Mailbox<K>`; public access to `mailbox_id_from_name`,
`mailbox_id_from_name_pair`, `mailbox_id_from_path`, and
`MailboxId::from_name`; the public field, `Pod`, and `Default` on
`MailboxId` (`MailboxId::NONE` becomes `Option`); the guest-side fold behind
`ctx.actor::<R>()` and `resolve_actor::<R>(&str)`; `LoadResult`'s rendered
`name: String` as an address to re-hash.

## Consequences

- The dominant defect class is unrepresentable: no code outside the registry
  derives an id, so two derivations cannot disagree. A wrong-anchor
  resolution (issue 4471) cannot occur because the guest no longer resolves.
- A missing chassis dependency is a refused load naming the dependency,
  rather than a warn-drop on every tick.
- Steady-state sends get cheaper. A stored reference or a constant replaces a
  fold recomputed per `ctx.actor::<R>()` call. No mail is added anywhere on
  the send path.
- Kinds that carry a reference leave the cast-shape (`Pod`) class, since a
  cast from `u64` would forge one. Most of the affected families are
  subscribe and register shapes, and many can drop the field in favor of the
  envelope sender, as `SubscribeWindowSelf` already does.
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
- **Validate reference fields on every send.** A registry lookup per field
  per mail to defend against forgery, when the defects are safe-Rust mistakes
  that a missing constructor already prevents. Validation belongs only where
  bytes arrive from another process.
- **Liveness in the type.** Not monotone; holding it true would cost mail.
- **Generational ids.** Already rejected by ADR-0079 as a wire change, and
  unnecessary while names are never reused.
- **Issue references at `Starting` and tombstone failed births.** Makes the
  order total, but burns the name of every load whose `init` failed, so a
  corrected retry under the same name is refused.
- **One type with a state parameter (`ActorRef<R, S>`).** The states carry
  different data (a description, an id, an unsendable id), so it is three
  structs under one name and a longer spelling in every kind field.
- **Decode as the only door, with the native registry encoding bytes to mint
  from them.** No hidden function, at the price of a ceremony that proves
  nothing; one gated function is the honest form.
