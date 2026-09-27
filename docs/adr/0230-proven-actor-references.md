# ADR-0230: Proven Actor References

- **Status:** Proposed
- **Date:** 2026-09-21
- **Amended:** 2026-09-23 — §5's deleted `ctx.actor::<R>()` handle is replaced at the call site by flat ctx verbs (`ctx.send::<R>(&k)`, `ctx.subscribe::<P, K>()`, `ctx.send_to(&r, &k)`) proven by `#[actor(depends(R))]`, with no optional peers ([ADR-0232](0232-flat-ctx-send-verbs.md)).
- **Amended:** 2026-09-23 — §3's declared-dependency check reaches two more births: an inline-spawnable actor's dependencies are checked when its module loads, and a native dependency on a pumped slot passes the birth check on the slot's Claim-stage reservation, with the boot failing if the pump never goes `Live`.
- **Amended:** 2026-09-24 — §3's dependency list is written as one `depends(A, B, …)` per `#[actor]`: `depends(R)` is a list of one, and a second `depends(...)` in the same attribute is a compile error that points at the list (#6557). [ADR-0232](0232-flat-ctx-send-verbs.md) §2's example is respelled to match.
- **Amended:** 2026-09-24 — §3: a wire `Call` names its recipient by `ErasedActorPath`; the engine that hosts the recipient resolves and proves it on arrival, an unresolved path is answered as not present (`RpcError::NotPresent`), and no mailbox id crosses the RPC wire as a recipient or in a reply.
- **Amended:** 2026-09-24 — §3's module-load check reaches private inline children: the types `export!` lists under `private = [..]` are read from the module's `aether.kinds.inputs.private` section and checked like the exported inline-spawnable actors (#6590).
- **Amended:** 2026-09-24 — §3's declared-dependency proof `DependsOn<R>` is a safe trait whose impl names `R`'s position in the actor's one `Declared::Depends` list, which `#[actor(depends(..))]` writes and which is the list the pre-`init` check reads on both transports: the native birth check walks it, and `export!` writes a guest's `Dependency` records from it; a hand-written impl for an undeclared `R` repeats an emitted impl (`E0119`) or names a position that holds another dependency or none (`E0277`) ([ADR-0231](0231-protocol-typed-references-and-reply-checks.md) §10; #6614, #6842, #6870).
- **Amended:** 2026-09-24 — §2: a proven reference's canonical `ErasedActorPath` is readable through the host registry (`NativeCtx::actor_path`) for diagnostics, as text only, never a position or anything sendable; the registry proves each route's name against the ADR-0166 grammar when the route is first published, so the read cannot fail for a reference it minted (#6635).
- **Amended:** 2026-09-24 — every `ErasedActorRef` is route-backed: an unwound eager spawn, or a chassis boot that fails before its spawn pass, withdraws its `Live` claim, because no reference to the claim can outlive that unwind; any other unwind retires its route to `Dropped`, which keeps its name and is never registered again. So `CancelStarting` and that withdrawal are the only edges that remove a route; and the envelope sender mints through one published-route read, `None` for a stamped position with no route (the chassis sentinel) (#6656).
- **Amended:** 2026-09-24 — an off-thread helper that must decide about a peer it holds a proof of reads an `ActorProbe` from the init ctx (`ctx.actor_probe()`); the probe answers whether that actor is `Live` now and whether it accepts a kind, takes proofs only, and sends, resolves, and enumerates nothing (#6324).
- **Amended:** 2026-09-24 — a guest host's receive surface is a declaration the substrate reads: a native actor that runs a guest implements `GuestHost`, and `NativeCtx::sync_guest` makes its accept set and cost rows match that declaration, so no actor reads its own position and no other actor writes a guest host's accept set. `NativeCtx::path`, bounded on `GuestHost`, reads a guest host's own canonical path as text only (issue 6350).
- **Amended:** 2026-09-24 — §3: an `ErasedActorPath` that arrived in a payload is proven through the ctx verb `resolve_path`: the host resolves the address and the published-route read proves it at once; a refusal names the path or its canonical path, never an id (#6324).
- **Amended:** 2026-09-25 — §3: a guest proves an `ErasedActorPath` that arrived in its config or mail through `WasmCtx::resolve_path`, the native verb's twin with the same resolution, proof, and refusals; its first consumer is the environment bootstrap script (#6786). Any loaded component can now reach any `Live` actor whose path it can spell, and its sends through the answer are unchecked by kind.
- **Amended:** 2026-09-26 — §3: a guest's doors are settled against the native ones (#6796). No ctx proves an actor's parent outside a guest's inline cluster, and a loaded component's lineage parent is the component host, not its loader; a guest proves a component it loads from the load reply's sender; a guest's detached sibling spawn yields no reference; a guest has no `resolve_live`, because no guest API takes a `MailboxId`; and a guest resolves an actor-typed path, an `ActorPath<R>`, through `WasmCtx::resolve`, whose consumer is the Bloomery bootstrap ([ADR-0240](0240-several-bloomery-journal-units-per-engine.md) D8).

Amends [ADR-0099](0099-actor-identity-and-addressing.md) (the lineage fold
stays how a position is *derived*; a derived position stops being something
a caller can *send to*), [ADR-0166](0166-typed-actor-lineage-and-abbreviated-external-addresses.md)
(its string grammar becomes `ErasedActorPath`, the one value that describes an
actor across a boundary),
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
   ([ADR-0079](0079-instanced-actors-as-a-first-class-category.md): names are never reused). The
   backwards edges are `RegistryEffect::CancelStarting`, which removes the
   route of a birth whose `init` failed, and `RegistryEffect::WithdrawClaim`,
   which removes the `Live` claim of an eager spawn or a chassis boot that
   unwound before any actor could have observed it. Every other unwind
   retires its route to `Dropped`, and a `Dropped` route is never registered
   again.

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
An `ErasedActorPath` is the one form that crosses a boundary, because it carries
names only and claims nothing, and the receiver proves it again on its own
side. The typed paths, `ActorPath<R>` (section 2) and `ProtocolPath<P>`
([ADR-0231](0231-protocol-typed-references-and-reply-checks.md) §3), are an
`ErasedActorPath` on the wire. A typed path's claim is about what the named
actor is, which a decode can prove: an `ActorPath<R>`'s decode checks its
text against `R` (section 2), and a `ProtocolPath<P>`'s checks its claim
against the engine it is decoded in (ADR-0231 §3). Liveness is proven only
by `resolve` (section 3).

**No serialized type carries a `MailboxId`.** A description that crosses a
boundary (a kind field, a config field, saved or dehydrated state, a journal
record, an MCP or RPC payload, a trace or log export) names an actor by its
path. A position stays inside the engine, where it is a registry key and a
routing input; outside, nothing can tell a registered position from a
computed one (Context). A caller-relative reference, such as a peer in the
caller's own module, is rendered absolute before it leaves the actor: the
ctx reads the actor's own canonical path and writes the peer's beneath it.
Every actor therefore needs a ctx verb for its own path, and no self verb
returns a `MailboxId` in its place. On main the only own-path read is
`NativeCtx::path`, bounded on `GuestHost`; the general verb lands with its
first consumer.

An exception is narrow, named, and justified in the decision that makes it,
never a general public API. An engine-internal ring, for example, may hold
positions in memory as long as what it exports renders them as canonical
paths.

The serialized positions on main are debt under this rule. A follow-up
removes them; this section records the rule and the list, not each fix.

| Where | Serialized position |
|---|---|
| `crates/aether-kinds/src/trace.rs` | `TraceEvent`'s and `MailNodeWire`'s `sender` and `recipient` |
| `crates/aether-kinds/src/diagnostics.rs` | `aether.mail.unresolved`'s `recipient_mailbox_id` |
| `crates/aether-kinds/src/input.rs` | `WindowId`, declared as a `MailboxId` and rendered as the tagged `mbx-…` string |
| `crates/aether-kinds/src/lib.rs` | `LogEntry.origin: Option<MailboxId>` |
| `crates/aether-data/src/mail.rs` | the mail sender schema: `MailId.sender`, and `SourceAddr`'s `EngineMailbox` and `Component` |
| `crates/aether-data/src/reference/address.rs` | `Address<R>`'s codec: `AddressForm::Beneath { parent }` and `AddressForm::Exact { id }` (section 5 deletes the type) |
| `crates/aether-window/src/kinds.rs` | `SubscribeWindow`, `UnsubscribeWindow`, and `UnsubscribeAllWindows` `.mailbox` |
| `crates/aether-http/src/kinds.rs` | `RegisterRoute`, `UnregisterRoute`, and `UnregisterRoutesAll` `.mailbox` |
| `crates/aether-data/src/schema.rs` | `MailboxDescriptor.id`, the mailbox table the engine ships to the hub |

### 2. The types

```rust
pub struct Namespace(&'static str);
pub struct ErasedActorPath(Box<str>);
pub struct ActorPath<R> { path: ErasedActorPath, _actor: PhantomData<fn() -> R> }
pub struct ActorRef<R> { id: MailboxId, _actor: PhantomData<fn() -> R> }
pub struct ErasedActorRef { id: MailboxId }
```

`ErasedActorPath` is today's `aether_data::ActorPath`. Its error keeps the
name `ActorPathError`, because the typed paths share the grammar and its one
refusal. This ADR uses the new names throughout.

The paths mirror the references:

| | Path (a description, with a codec) | Reference (a proof, no codec) |
|---|---|---|
| untyped | `ErasedActorPath` (`aether-data`) | `ErasedActorRef` |
| actor-typed | `ActorPath<R>` (`aether-actor`) | `ActorRef<R>` |
| protocol-typed | `ProtocolPath<P>` (`aether-actor`, ADR-0231 §3) | `ProtocolRef<P>` (ADR-0231 §3) |

| Type | Claims | Made by | Can |
|---|---|---|---|
| `Namespace` | the grammar is valid | `const fn new`, a compile error when invalid | compare, `Debug`, fold to an `ActorId` |
| `R::Key` | the discriminator is valid | the actor type's own fallible constructor and fallible decode | be the key segment of a path |
| `ErasedActorPath` | the text is a well-formed ADR-0166 address, canonical or short (with `:name` holes); nothing about existence or placement | its fallible constructor and fallible decode | be stored, mailed, configured, persisted: carried in a kind (`NamedMail.recipient`), name a wire `Call`'s recipient, compared, displayed; become a position only inside the engine, through the host's `resolve_address`. The only description of an actor with a wire format; it carries names only. |
| `ActorPath<R>` | the text is a well-formed canonical path whose leaf namespace is `R::NAMESPACE`, whether it was written here or decoded; nothing about existence | the type constructors `ActorPath::<R>::instance` and `ActorPath::<C>::child` below, which write it from `R`'s namespace, placement, and key; decode, which refuses a short path and a leaf namespace other than `R::NAMESPACE` | everything an `ErasedActorPath` can; narrow to a `ProtocolPath<P>` (ADR-0231 §3); be resolved to an `ActorRef<R>`. It grants no send. |
| `ActorRef<R>` | an `R` reached `Live` at this id, in this engine session | section 3 only | send, monitor, be held in actor memory, name its canonical path |
| `ErasedActorRef` | some actor reached `Live` at this id | the envelope sender, including a monitor notice's sender; the registry's liveness read over a position that arrived in a payload; an `ErasedActorPath` proven through `resolve_path`, on a native or a guest ctx | reply, monitor, be the target of an untyped send — inheriting, detached, or tracked, unchecked against a kind because the set it keys may be heterogeneous — be held in a capability's own table and keyed in an ordered set, name its canonical path |
| `MailboxId` | nothing; it is a position | the fold, decode | be a registry key inside the engine, be printed; never be serialized (section 1) |

`ActorRef::id()` is free and total. There is no function from a `MailboxId`
to anything sendable outside the registry.

`ErasedActorPath` is the text form of ADR-0166's grammar and the only
description of an actor. A typed path adds a compile-time claim and nothing
on the wire.

```rust
// aether-actor: constructors on the path type, bounded by placement
impl<R: Root + Instanced> ActorPath<R> {
    /// `R::NAMESPACE:key`.
    pub fn instance(key: &LoadName) -> Self;
}

impl<C: Instanced> ActorPath<C> {
    /// `<parent>/C::NAMESPACE:key`; refused only past the path's depth or
    /// byte cap.
    pub fn child<P: Addressable>(parent: &ActorPath<P>, key: &LoadName) -> Result<Self, ActorPathError>
    where
        C: ChildOf<P>;
}
```

A typed path is written by a constructor on its own type, and the
constructor's bounds are the check: `ActorPath::<R>::instance(&key)`
compiles only for `R: Root + Instanced`, and
`ActorPath::<C>::child(&parent, &key)` only for `C: ChildOf<P> + Instanced`,
so a path whose topology the actor types do not allow does not compile. Any
code may call them. Each takes an actor type and a key, never text, and
writes only that type's own canonical path, so no call attaches an `R` to
text that is not an `R`'s path. Nothing is declared beside a typed path: the
writer's ctx plays no part, and the target need not be live, or even
reachable by a dependency, since it may be an `Instanced` actor.

An `ActorPath<R>` is written from the actor type: each step is a type's
`NAMESPACE` and, for an instance, its key, so the path is canonical and has
no holes. Writing one reads no registry and folds nothing; the position
exists only when a receiver resolves it. Which constructor exists is decided
by `R`'s placement facts (`Root`, `ChildOf<P>`, `Singleton`, `Instanced`),
and a constructor lands with its first consumer: `instance` for a root
instance and `child` for an instanced child beneath a written path serve the
Bloomery driver and bootstrap ([ADR-0240](0240-several-bloomery-journal-units-per-engine.md) D7,
D8), and a root singleton or a singleton child comes with the first caller
that needs one. There is no caller-relative form: a peer named relative to
the caller is written absolute from the caller's own path (section 1). The
constructor from a bare `ErasedActorPath` is private to `aether-actor`,
which holds the placement traits, so no crate can attach an `R` to arbitrary
text (section 4).

On the wire an `ActorPath<R>` is the path text, with `ErasedActorPath`'s
schema and codec. Decoding validates the grammar, refuses a short path, and
refuses a path whose leaf namespace is not `R::NAMESPACE`. That check is
self-contained, a comparison with a constant, so an `ActorPath<R>` that
exists names an `R`, however it arrived. The leaf is the one segment the
type fixes: an `ActorPath<C>` does not carry its parent's type. `Debug`
prints the path, because a path is a name, not a position.

A sender is responsible for the validity of what it sends, and a value that
exists is valid. A typed path is proven where it comes into existence, by its
constructor or by its decode, and nothing downstream checks its claim again;
`resolve` proves only liveness (section 3).

A loaded component's key is its load name, a validated `LoadName`; a
window's is the name its spec gives it. There is one addressing system and
`ErasedActorPath` is its value type.

A capability keeps the envelope sender as an `ErasedActorRef`.

The per-handler handle keeps its job of carrying origin, now fed by a
reference rather than a raw id: `ctx.to(&actor_ref).send(&kind)` replaces
`actor_at::<R>(id)`, which is deleted; an erased reference sends through
`ctx.send_to` or, with a request context, `ctx.send_with_context`.

### 3. The doors: where a reference comes from

| Source | Proof | Runtime cost |
|---|---|---|
| A declared dependency of the actor | the `#[actor]` dependency list (`depends(A, B, …)`) is emitted to the wasm custom section; each entry folds to its position through its strategy — `One` at the root, `Embedded` beneath the placement's parent — and the host requires a `Live` route there before `init` (native: at chassis build, and at spawn for a spawned child). A missing dependency refuses the load and names it. | none for `One` — at depth 1 the fold is a `const`; one registry read per `Embedded` entry |
| Self, parent, inline cluster members | structural; the host supplies them at `init` and the SDK mints them. On a guest the parent is the inline one, inside the module (`WasmCtx::parent`). No ctx has a verb that proves an actor's parent across a module boundary or on a native actor; a native child reaches its parent by declaring it, when the parent is a root singleton | none |
| A child this actor spawned or loaded | A native staged spawn's completion, `TaskDone<SpawnOutcome<C>, _>`, carries `ActorRef<C>` on its `Ok` arm, minted by the registry when the birth's finalizer runs after the owner has published the child's `Live` route. A loaded component's successful `LoadResult` is delivered from the loaded actor itself: the component host hands its owed reply to the trampoline it spawned, which replies in its own name, so the requester keeps the reply's stamped sender — `ctx.sender()` for an actor, the reply event's sender for an embedder. `LoadResult::Ok` carries the canonical `ErasedActorPath` and no position. A guest's inline spawn returns the typed `InlineChild<C>` handle; a parent keeping children of several types keeps the erased form instead — `InlineChild::erase`, or `spawn_inline_child_by_tag`'s `ErasedActorRef` — a proof without the type. A guest that loads a component takes the proof from the load reply's sender the same way, through `WasmCtx::sender`. A guest's detached sibling spawn (`WasmCtx::spawn_child`, ADR-0097) yields no reference, because the birth completes after the call returns. Two embedder cases share the row: an embedder spawn's `finish` returns `ActorRef<A>` once its commit has published the route, and a chassis-composed actor's reference is recorded at boot, when the route goes `Live`, and read back by type through the chassis handle's `actor_ref::<R>()` | none for a spawn or a load; one published-route read when an embedder types a load reply's sender |
| A child beneath a reference an embedder already holds | the embedder hands the chassis handle's `child::<P, C>(parent, key)` the parent's proof and the child's key, bounded `C: ChildOf<P> + Instanced`; the door folds the key with `C`'s resolver beneath the parent's position and proves the route with the published-route read `resolve_live` takes: only `Live` mints, and `Starting`, `Dropped`, and never-registered positions refuse with `ChildRefused`, which names the key and `C::NAMESPACE`, never a position. The proof and the key are all it takes; no description type is involved. Its consumer is the substrate harness's `child`, which reaches a component's spawned child or a window capability's opened window without rendering a path | one published-route read per lookup |
| The envelope sender | the host stamps the origin at dispatch, so the SDK mints it from the host's value. A `MonitorNotice` is host-generated mail that carries one: the host stamps the departed actor, which `register_monitor` required to be `Live`, so the watcher's `ctx.sender()` is a reference to it. | one published-route read; no lock, no allocation |
| A position that arrived in mail, config, saved state, or from another process | the ctx verb `resolve_live`, over the host's liveness read of the published route view: `Live` mints, `Dropped` and `Unknown` refuse by name, and `Starting` reads as unknown (section 1). Minted once, at receipt, in the handler that received the field — never at the send. The registry method behind it is crate-private, so the verb is the only spelling a capability has. Native only: a guest has no `resolve_live`, because no guest API takes a `MailboxId` (amendment 2026-09-26). Its inputs are the serialized positions section 1 lists as debt, and it leaves with the last of them | one published-route read per proof; no lock, no allocation |
| An `ErasedActorPath` that arrived in mail or config | the ctx verb `resolve_path`, on a native ctx (`NativeCtx`) and on a guest ctx (`WasmCtx`): `Unresolved` when the address names no `Starting` or `Live` route (a dropped route included), `NotLive` when its route is still `Starting` (or drops between the two reads). A guest's call crosses one host import, `resolve_path_p32`, and the host resolves and proves the path through the same crate-private path the native verb takes; the SDK mints the `ErasedActorRef` from the host's answer, as it mints the envelope sender. Native consumers are the component host's drop, replace, load-under, and describe receipts, and the trampoline's replacement dependency check; the guest consumer is the environment bootstrap script's `wire`, which proves the journal owner and the bundle driver from its config (#6786) | one address resolution plus one published-route read; for a guest, inside one host call |
| An `ActorPath<R>` in the actor's memory or arrived in mail, config, saved state, or from another process | the ctx verb `resolve`, on a native and a guest ctx: the name this decision reserves for its typed door, one spelling for both typed paths, where the path's type decides the proof's. An `ActorPath<R>` is canonical, so it never expands: it compiles to its position by the lineage fold, and one route-table lookup then checks that the route there carries that canonical name and that it is `Live`. That is all `resolve` proves: liveness. What the path claims about `R` was proven when the path came into existence, by its constructor or by its decode's leaf-namespace check (section 2), and the rows behind it do not change under it ("Build skew" below). It mints an `ActorRef<R>`, through which every kind `R` handles is sendable, manual rows included, and refuses `NotLive`, naming the path, never a position. The guest consumer is the Bloomery bootstrap, which reaches the journal and the driver this way (ADR-0240 D8); the native arm lands with its first native caller. An untyped `ErasedActorPath` stays untyped: it goes through `resolve_path` above and then ADR-0231 §4's cast | one fold plus one route-table lookup; for a guest, inside one host call |
| A `ProtocolPath<P>` in the actor's memory or arrived the same ways | the same `resolve`, which mints a `ProtocolRef<P>` after the same fold and lookup and checks nothing more (ADR-0231 §3). A narrowed path's coverage of `P` was proven by the compiler, and a decoded one's by its contextual decode, against the engine it was decoded in; no rows are compared at receipt and no answer is kept per route. The native consumer is the Bloomery workspace's receipt of a `Run` or `Import` `source` (ADR-0240 D7); the guest arm lands with its first guest caller | the same |

**Build skew is not checked per resolve.** Within one engine a route's rows
are fixed or only grow. A native actor's rows are its binary's, fixed for the
engine's life, and a native namespace is claimed once per engine
(`try_claim_namespace`,
`crates/aether-substrate/src/actor/native/spawn/activation.rs`), so a native
`R` and its caller are one binary and an `R`'s leaf namespace names that
binary's `R`. A component's rows are republished only when ADR-0231 §5 finds
no dropped or changed row. So the rows behind a received path never change
under it, and `resolve` compares none. A protocol path's claim is proven
against the engine's published rows once, at its decode (ADR-0231 §3).

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

**Amendment (2026-09-25): guests prove paths too.** A guest can address
only actor types it can compile. The native actors a Bloomery script
mails, the journal owner and the bundle driver, are instanced roots in
native-only crates, so neither a declared dependency nor a typed send
reaches them. `WasmCtx::resolve_path(&ErasedActorPath) -> Result<ErasedActorRef,
ResolvePathError>` is the native verb's guest twin: the same resolution,
the same proof, and the same two refusals. `Unresolved` carries the
registry's diagnostic as text and `NotLive` the canonical path, never an
id. The path usually arrives in the component's config, where it crossed
the boundary as an `ErasedActorPath` and was validated on decode, so text still
becomes a position only inside the engine, through `resolve_address`.

The consequences:

- Any loaded component can reach any `Live` actor whose canonical or
  short path it can spell. That includes a native actor that grants it no
  dependency, such as the journal owner, whose writes are not
  authenticated (ADR-0226). The host already routed any position a guest
  named on `send_mail_p32`; what changes is that the SDK now offers a
  proven door by path, where before only a disallowed hand fold reached
  such an actor.
- Sends through the answer are unchecked by kind. The reference is an
  `ErasedActorRef`, because a guest cannot name a native actor's type, so
  a kind the recipient does not handle is caught only at the recipient,
  never at compile time.
- The proof lives only in the guest's memory. It is minted once, at
  `wire` or at receipt, and stored; it is never re-derived at a send.

**Amendment (2026-09-26): which doors a guest has.** A guest proves
references through the doors above, and four rows answer differently for
it (#6796). Two must not exist for a guest, one waits for a production
consumer, and the typed-path door, `resolve` over an `ActorPath<R>`, has
a guest consumer.

- **A module's own parent.** No ctx proves its actor's parent outside a
  guest's inline cluster. A native child whose parent is a root singleton
  declares it, as `FleetProxy` declares `FleetServer`, and a guest has the
  same door. The substrate hands a guest's entry actor its parent's
  position at `init`, where it is the seed an `Embedded` dependency folds
  beneath, and nothing sendable. For a loaded component that parent is the
  component host, which spawned the component's trampoline, and never the
  actor that asked for the load. A door to it would answer "who loaded me"
  with the wrong actor, and would give every loaded component an undeclared
  proof of the actor that loads, drops, and replaces components. A
  component reaches its loader as any receiver reaches an announcer: the
  loader mails it and it keeps the sender, or the loader's path arrives in
  its config and `resolve_path` proves it. The one parent a door would
  answer usefully is a detached sibling's (ADR-0097), the guest actor that
  spawned it. Guest detached spawn has no consumer and #6818 removes it;
  a consumer that brings it back brings this door with it, as an
  `ErasedActorRef` minted from the position `init` receives, which the
  trampoline proved `Live` before the birth. It is erased because a
  `child_of` list may name more than one parent.
- **A child it spawned or loaded.** A guest loads a component by sending
  `aether.component.load` to the component host, a declared dependency
  (`depends(ComponentHostCapability)`), and `LoadResult::Ok` arrives from the loaded actor, so `ctx.sender()` is the
  proof, as for a native requester. A detached sibling spawn returns
  nothing addressable, because the birth completes after the call, and no
  completion notice is added.
- **No `resolve_live`.** The verb takes a `MailboxId`, and no guest API
  takes or returns one. A guest is told where to send by an `ErasedActorPath`,
  proved through `resolve_path`, or by the envelope sender. No guest on main
  proves a position it received.
- **A typed path.** A guest resolves an `ActorPath<R>` through
  `WasmCtx::resolve`, the twin of the native verb, whose guest consumer is
  the Bloomery bootstrap
  ([ADR-0240](0240-several-bloomery-journal-units-per-engine.md) D8): the
  path compiles to its position by the lineage fold, one route-table
  lookup checks the canonical name and `Live`, and the SDK mints an
  `ActorRef<R>` from the host's answer. The path's leaf namespace was
  proven when the bootstrap wrote it with `ActorPath::<R>::instance` or
  `ActorPath::<C>::child`, and both targets are native, so no resolve
  compares rows. A guest writes an
  `ActorPath<R>` only from an actor type it compiles, so a kind-checked guest send to a native actor
  needs that actor's crate to export an always-on identity half
  (ADR-0122), as `aether-workspace` does for `WorkspaceCapability`. The
  journal owner and the bundle driver gain theirs under the same decision
  (#6823), and the bootstrap's sends to them then go through
  `send_to(ActorRef<R>, &K)`, checked by kind, including the driver's
  manually answered `Call`.

An off-thread helper that only wakes its own actor — an accept loop, a socket
reader, a timer — holds a `SelfWake<K>` from the ctx (`ctx.self_wake::<K>()`)
rather than any of these: it names no position, carries no reference, and can
send only that one wake. A helper that must also decide about a peer it holds a
proof of, like the http reader choosing a live route member, holds an
`ActorProbe` from `ctx.actor_probe()` beside its `SelfWake<K>`, and the probe
grants no send, no lookup by name or position, and no registry.

What arrived stays what it was. A position in a payload, such as
`SubscribeWindow.mailbox`, is section 1's debt: the payload-borne door
proves it at receipt and changes nothing about the wire, because a proof
cannot cross a boundary, and the rule removes the field rather than the
door making it safe. The proof it yields lives only in the receiver's
memory, from the moment of receipt until the row is dropped.

The position and the proof have different owners. A position is derived
only inside the engine: `Resolve` folds a declared dependency or a spawn
from the actor's own type, and the host's `resolve_address` compiles a
canonical path to its position by the same lineage fold, after filling a
short path's holes from the generated root and child declarations. The
host is the single authority on whether that position is occupied, and it
is the only thing that can turn one into a reference. Handing a reference
to a peer means sending a path, an `ActorPath<R>` written from the actor's
type or a `ProtocolPath<P>` narrowed from one; the peer's `resolve` is one
synchronous host call and no mail. Persisted state stores a path for the same reason, and so does state
an actor dehydrates across `replace_component`: `on_rehydrate` resolves it
again. This is enforced by the proven types having no codec rather than by
convention.

Strings exist in exactly one place: text crosses the MCP, RPC, and harness boundary as an `ErasedActorPath`, validated on construction and on decode. A wire `Call` names its recipient by that path, beside the engine that hosts it, so a malformed path fails the frame decode and never reaches resolution, and no mailbox id crosses the wire as a recipient. Nothing outside the engine computes a position for a path: not aether-mcp, not a harness, and not the hub, which relays an engine-addressed `Call` to that engine's proxy with the path as written. The engine that hosts the recipient resolves the path when the `Call` arrives, through the host's `resolve_address`, the one place an `ErasedActorPath` becomes a position, and proves the answer at once through the payload-borne door above. The proof does not leave the RPC server's handler: the server holds a deliver-only item, the same shape a bundle item takes, and delivering it is all it can do. A path that does not resolve to a `Live` actor is not present, whatever the reason: never registered, still starting, dropped, or a short path that is ambiguous or names no declared child. The call closes with `RpcError::NotPresent`, which names the path and carries the registry's diagnostic; nothing is parked or dropped, and the hub relays the refusal to the caller unchanged. An id an engine still reports, in a trace tree or a window listing, is section 1's debt; until it goes, a client asks that engine for the id's canonical path and sends by the path. A reply on the wire carries its kind and bytes and no address.
A mail bundle — the `NamedMail` list that `DispatchTraced` and `CaptureFrame`
carry — is the same boundary inside a payload: the receiving capability proves
every `ErasedActorPath` recipient once, before any item moves, and a proven item can
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

`Namespace`, `LoadName`, and `ErasedActorPath` live in `aether-data`, because kinds
in `aether-kinds` carry paths and `aether-actor` depends on that crate.
`ActorPath<R>` and `ProtocolPath<P>` live in `aether-actor`: their
constructors take an actor type and its placement facts, which are
`aether-actor` traits, and a constructor in `aether-data` would have to be
public or `#[doc(hidden)]` for `aether-actor` to call, a door that attaches an
actor type or a protocol to arbitrary text.
The public constructors, `ActorPath::<R>::instance` and
`ActorPath::<C>::child`, take an actor type and a key, never text, so they
attach `R` to no text but `R`'s own canonical path, and no other crate needs
a hidden writer.
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
`name: String` as an address to re-hash, and its `mailbox_id`; `Address<R>`
and `AddressForm` with their codec, and the helpers that build one
(`address`, `address_at`, `address_named`, `child_address`, and
`ActorRef::address`), whose one production use is the embedder's child
door, which needs only the parent's proof and the key; and the declared
links: `#[actor(links(..))]`, `LinksTo<R>`, `WasmCtx::link` and
`link_child`, `NativeCtx::link`, and the hidden `__link` writer, replaced by
section 2's type constructors.

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
- A kind never carries a reference or a position. A kind that names an
  actor to send to carries an `ErasedActorPath`, or an `ActorPath<R>` or a
  `ProtocolPath<P>` when its writer can name the actor's type; none is
  cast-shape (`Pod`), and its receiver pays one synchronous host call to
  resolve it, once. A kind with a `ProtocolPath<P>` field is contextual and
  decodes only against an engine (ADR-0231 §3). Most of the affected
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
- **A second description system beside the path (`Address<R>`).** An enum
  of a caller-relative form, a beneath-a-parent form, and an exact form,
  with its own codec and schema. Two description systems conflict: every
  boundary picks one, every receiver accepts both, and new work cannot tell
  which one it should build on. The second one also carried positions
  (`Beneath { parent }`, `Exact { id }`), which section 1 forbids in any
  serialized type. The typed paths are not a second system: `ActorPath<R>`
  and `ProtocolPath<P>` are an `ErasedActorPath` on the wire, with its schema,
  and add only a claim that exists at compile time.
- **Check rows on every resolve.** Comparing the caller's compiled rows
  with the route's published ones at each `resolve` pays per use for an
  invariant the code already holds: a native caller and a native actor are
  one binary, a route's rows only grow, and a protocol path's claim is
  proven at its decode.
- **Declared links** (`#[actor(links(R))]`, `LinksTo<R>`, `ctx.link` and
  `ctx.link_child`, a hidden `__link` writer, and link records in the
  module). A link declares that an actor writes paths naming `R`, but the
  actor never uses `R`, so nothing about the declaration can be checked at
  compile time. The type constructors check topology on the type instead.
- **Build skew as a load-time link check** against link records, whichever
  of the linker and the target loads second. Within one engine a route's
  rows are fixed or only grow, so a received path's rows never change under
  it, and there is no link record left to check against.
- **Check the route's actor-type tag at resolve**, with a tag written on the
  route record. An `ActorPath<R>`'s decode already proves its leaf
  namespace, and `resolve`'s canonical-name check proves the route carries
  that name.
- **Typed paths whose decode checks only the grammar**, carrying the
  writer's claim until `resolve` proves it. A value could exist without
  being true.
- **Serializable references with a structural check at decode.** The first
  form of this decision. Decode cannot re-establish "reached `Live` here",
  and the type cannot know where its bytes came from, so every codec impl is
  a door that skips the registry.
