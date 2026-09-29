# The actor model

> **Governing ADR:** [ADR-0074](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0074-unified-actor-model-for-substrate-and-guests.md) (the unified actor model — capabilities and
> components are one model, not two) with [ADR-0079](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0079-instanced-actors-as-a-first-class-category.md) (the lifecycle stages)
> and [ADR-0033](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0033-handler-driven-inputs-manifest.md) (the `#[actor]` macro), extended by [ADR-0096](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0096-multi-actor-wasm-modules.md) (a wasm module exports several
> actor types), [ADR-0114](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0114-inline-child-actors.md) (a component spawns inline children), and [ADR-0099](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0099-actor-identity-and-addressing.md) (actor
> identity and addressing), plus [ADR-0166](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0166-typed-actor-lineage-and-abbreviated-external-addresses.md) (declared placement permissions). This model is **stable**; it's the
> spine everything else hangs off. Signatures here were read from the current SDK
> (`aether-actor`) and runtime (`aether-substrate`).

The engine is built from one kind of thing: the **actor**. The renderer, the audio
mixer, the filesystem, a component you load — all of them are actors, with no
privileged class of "system object" sitting above them. Everything in the substrate
is an actor, and the only way actors ever interact is by **mail**.

If you understand actors, most of the engine follows: every subsystem in this guide
is some actor (or a handful) doing a job. This page covers the actor itself — what
it is, the lifecycle it moves through, how you write one — and the two *hosts* it
runs under, native **capabilities** and wasm **components**.

## What an actor is

An actor is some **private state** paired with a set of **typed handlers**. It sits
idle until mail arrives; when an envelope lands, the handler registered for that
kind runs with exclusive `&mut` access to the state, updating it and sending mail
of its own. Nothing happens except in response to a message.

Two properties make it tractable to reason about:

- **Actors communicate *only* by mail.** No actor holds a reference into another
  actor's memory, calls another's methods, or shares a lock with it. The only way
  to affect another actor is to send it a kind it handles. This is what lets the
  same model span an in-process capability and a sandboxed wasm component
  without either knowing which it's talking to — mail is the only coupling, so the
  *host* is an implementation detail. (See [capability = reachability](invariants.md)
  for the security consequence.)
- **An actor only ever runs on one thread at a time.** The scheduler guarantees no
  two threads run an actor's handlers at once, so an actor can freely mutate the
  data on its own struct — its state is **plain fields**, no `Mutex`, no `RefCell`,
  no atomics, just ordinary sequential Rust. (How the scheduler enforces this — the
  run-token — is the [concurrency](../systems/concurrency.md) page; here, take it as
  a guarantee you can build on.)

What flows between actors — the kinds, their ids, the wire encoding — is the
[type system](type-system.md). How it routes and in what order — mailboxes, FIFO,
fire-and-forget — is [mail & scheduling](../systems/mail-and-kinds.md). The rest of
this page stays with the actor on the receiving end: how it's built and how it runs.

## The lifecycle

Every actor — regardless of host — moves through the same three authored stages
([ADR-0079](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0079-instanced-actors-as-a-first-class-category.md)). Each stage gets a different context, and the context *is* the
contract: it's exactly what you're permitted to do at that point.

| stage | when | ctx allows | use it for |
|---|---|---|---|
| **`init`** | once, at boot | resolve only — **no mail** | build and return the initial state |
| **`wire`** | after `init`, mailbox now published | full send + resolve | subscribe to input, announce yourself, kick off a self-poll |
| handlers | steady state, one call per inbound kind | full send + resolve + reply | the actor's actual behavior |
| **`unwire`** | after the inbox drains, before drop | full send + resolve | final broadcast, signal monitors, flush state |

The three stages exist because constructing an actor and letting it participate in
the mail system are different moments, and only the second is safe to send from.
`init` runs while the actor is still being built: its mailbox isn't published yet,
peers may not have booted, and it returns `Result<Self, ActorInitError>` so a failure
aborts the load cleanly. Sending mail from there would mean announcing yourself
before you're addressable, or mailing a peer that doesn't exist yet. So `init` stays
a pure synchronous constructor — resolve kind ids and mailbox addresses, assemble
state, return it (or fail with an `ActorInitError` that surfaces instead of leaving a
half-built actor behind).

`wire` is the first point where sending is safe. It runs once `init` has succeeded,
the mailbox is live, and the chassis is past its boot barrier, so peers are
addressable and replies can route back. That's why mail-driven setup lives here:
subscribing to the tick or input streams, announcing yourself to a peer, starting a
poll loop by mailing yourself. An actor that needs to subscribe at startup would have
nowhere safe to do it if `init` were the only hook.

`unwire` is the mirror at the other end, and it exists for the same reason in
reverse — teardown often needs to send, whether that's a closing broadcast, a signal
to monitors, or a final flush to a peer, and Rust's `Drop` can't reach cleanly into
the mail system. It runs after the inbox has drained but before the actor drops, so
its sends still land in live peers (mail to one that's already gone warn-drops). It
absorbs what used to be a separate `on_drop` hook.

Both `wire` and `unwire` default to no-ops; override them only when you have
mail-driven setup or teardown to do.

## The context

Every lifecycle method and handler is handed a **context** (`ctx`) — the actor's
only handle to the world outside its own state. Through it the actor resolves
addresses, sends mail, and replies to whoever sent the current message; depending on
where it's running it can also spawn a child actor, persist state for a successor, or
ask to shut down. Anything that reaches past the actor's own fields goes through the
context, and you never construct one — the runtime passes it in for the duration of a
call and takes it back when the call returns, so an actor touches the world only
while a handler is running, never through a stashed handle.

There's more than one context *type* because what an actor is allowed to do changes
from stage to stage, and the type is how that's enforced. The context handed to
`init` can resolve addresses but has no `send` method at all — so "init can't mail"
isn't a rule you have to remember, it simply won't compile. `wire`, handlers, and
`unwire` get a context that can send and reply; the hot-swap hooks get one that can
persist state. That's what "the context is the contract" means literally: the method
you're in determines which context type you hold, and that type determines what
compiles.

Host matters as well as stage. Resolving, sending, and replying are common to both;
a few operations are host-specific. A native capability can spawn any instanced child
actor and ask to shut itself down. A component creates children **inline** — instances
of the other actors its own module exports, co-located in its wasm instance
([ADR-0114](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0114-inline-child-actors.md), and the
[cardinality](#one-or-many-cardinality) section below) — while its own load, drop, and
replace are driven from outside. The concrete context types differ by host too —
`WasmInitCtx`/`WasmCtx` in a component and `NativeInitCtx`/`NativeCtx` in a
capability. Lower-level resolver/sender traits support shared helpers, but actor
lifecycle signatures use the concrete context for their host and stage.

## Authoring an actor

You declare the receive side with the **`#[actor]`** attribute on one `impl`
block, and each **`#[handler::<class>]`** method *is* a handler — the macro infers
the kind it handles from the method's **third parameter**:

```rust
#[actor(root, depends(LifecycleCapability, RenderCapability))]
impl WasmActor for Hello {
    const NAMESPACE: &'static str = "example.hello";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Hello)
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        ctx.subscribe::<LifecycleCapability, Tick>();
    }

    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        ctx.send::<RenderCapability>(&TRIANGLE);   // draw every tick
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, ping: Ping) -> Pong {
        Pong { seq: ping.seq }
    }
}
```

`Ping` is the kind `on_ping` handles; `Tick` is the kind `on_tick` handles. The
macro reads those parameter types and generates the dispatch table that routes an
inbound envelope to the right handler by matching its kind id — a **compile-time
const** (`K::ID`), so there's no runtime registration and no host round-trip to
resolve an address. A handler with no match falls through to an optional
**`#[fallback]`** (taking the raw `Mail<'_>`); omit the fallback and the actor is
a *strict receiver* — unhandled kinds are reported, not silently dropped.

You address peers **by type** — `ctx.send::<RenderCapability>(&payload)`, on a
ctx whose actor declares `depends(RenderCapability)`, compiles only if that actor
actually handles the payload's kind, and both the mailbox id and kind id resolve
at compile time. The handler takes the decoded mail
**by value** and gets `&mut self` because nothing else can touch the state
concurrently.

### Declaring placement

Actor identities declare where they may legally appear with `root` and
`child_of(...)` arguments on the same `#[actor]` attribute:

```rust
#[actor(singleton, root)]
pub struct ComponentManager;

#[actor(
    instanced,
    child_of(ComponentManager),
    child_of(TestComponentManager),
)]
pub struct ComponentWorker;
```

`root` implements the `Root` marker: the actor may be placed without an actor
parent. Each `child_of(Parent)` implements `ChildOf<Parent>` and records one
permitted direct parent edge. The argument may be repeated for distinct parent
types, and an actor may be both a root and a permitted child.

Parentless native entry points consume that permission as a compile-time bound.
Chassis composition (`Builder::with_actor` and `with_actor_configured`),
chassis-level instanced spawn, pumped-actor boot, and the matching test and
harness adapters all require `Root` beside their existing native actor bounds.
A child-only actor therefore cannot cross one of those root placement surfaces,
but remains valid through typed child placement such as
`NativeCtx::spawn_child`, where its declared `ChildOf<Parent>` edge is checked
instead. No runtime metadata lookup is involved in either check.

A wasm actor declares placement with the same arguments — `root`,
`instanced, root`, `instanced, child_of(P)`, or `instanced, root, child_of(P)`
([ADR-0241](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0241-code-is-published-not-loaded.md)
§5) — but the permission is checked at run time, from the module's
`aether.actor.lineage` section. `root` writes a `Root` record for every
cardinality, and it is the permission the component host checks when it loads
the type at itself, boots it as its module's boot type, or replaces an actor
with a module whose boot type it is: each is refused with the operation's
`Err`, naming the type and the placements it does declare, before the module
publishes or anything is staged. A `child_of(P)` or `composable` record never
satisfies it. A `load_under` is checked the same way against a `child_of(P)`
record naming the proven parent's type. A loaded guest is named as a native
actor is — `NS`, `NS:key`, or `parent/NS:key` — so a wasm `root` emits the same
`Root` marker impl a native root does.

These are placement permissions, not runtime facts. They do not say that an
instance is live, that a parent owns or supervises a child, or that the actor
has singleton versus instanced cardinality. The actor's own
`Addressable::NAMESPACE` remains the only namespace declaration; generated
native inventory and wasm manifest records derive names and type tags from the
actor types rather than copying string literals.

TCP uses multiple parent permissions because accepted and outbound sessions
have different real lineages:

```rust
#[actor(singleton, root)]
pub struct TcpCapability;

#[actor(instanced, child_of(TcpCapability))]
pub struct TcpListenerActor;

#[actor(
    instanced,
    child_of(TcpCapability),
    child_of(TcpListenerActor),
)]
pub struct TcpSessionActor;
```

The two permitted lineages give an accepted session and an outbound session
with the same name distinct identities. The lineage is chosen where the session
is spawned: the listener spawns accepted sessions beneath itself, and the
capability spawns dialed sessions directly beneath itself. A consumer reaches
either one through the stamped sender of the session's own mail, never by
naming it.

`ChildOf` therefore does not choose one global parent for an actor type. It is
checked where a child is placed (`spawn_child`) and where an embedder looks one
up (`child::<P, C>`), and the parent at that point supplies the canonical
lineage and identity fold.

### External actor addresses

Rust actor code continues to resolve peers by type. String-addressed
boundaries such as MCP, configuration, and harness calls may additionally use
an ADR-0166 short path rooted in an actor namespace:

```text
aether.window/:main
```

The substrate expands that spelling from the generated `Root` and `ChildOf`
inventory before it performs the ordinary canonical registry lookup. With the
window capability's one instanced child family, the address above expands to:

```text
aether.window/aether.window.instance:main
```

The same expansion reads every published module's lineage (ADR-0241 §5): its
`#[actor(root)]` exports, each exported and private type's cardinality, its
`child_of(..)` edges, and a `composable` type as a child of every type the
module declares. The index is rebuilt in the registry-owner apply that
publishes a module, and a module's facts about a native namespace are ignored,
so a module never changes a native short path.

A loaded component needs no short path to reach it: it is named by its own
namespace, so its canonical address is already short (`aether.kit.camera`, or
`aether.widget:panel` for an instanced one). Its children are reached by a hole
beneath it: `game.world/:north/:gate` for an inline child and its own inline
child, or `game.world/:k` for a guest a `load_under` placed at
`game.world/NS:k`. A `composable` type is a candidate beneath every parent in
its module, so a hole beneath a parent that declares another instanced child
as well is ambiguous.

A path is `/`-separated steps. After the root, a bare step always names a
singleton child, `namespace:discriminator` names an instance of that instanced
child, and `:discriminator` is a hole naming an instance of the one instanced
child declared under the current actor. A path with no hole is canonical and
never consults the declarations; a path with a hole is short, and its first
step must be a bare root namespace. A hole resolves only when the current actor
has exactly one logical instanced-child namespace. If several are possible,
resolution returns a deterministic ambiguity error listing the explicit
`namespace:discriminator` steps the caller can use. The older `://` spelling
is refused with an error naming the hole form.

Short paths are boundary input, never actor identity. The registry expands
them before hashing and stores, lists, and reverse-reports only the canonical
path. Unknown roots, illegal segments, ambiguous children, path-limit
violations, and a valid expansion with no live mailbox remain distinct
resolution errors.

At the boundary the text becomes an `aether_data::ErasedActorPath`. Its grammar is
checked when it is built or decoded, so a malformed address fails there rather
than in the engine; it is stored as written, holes included; and it becomes a
position only in the engine's `resolve_address`, which fills holes and checks
liveness.

## Reply classes

A handler declares how it answers through its class marker
([ADR-0112](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0112-handler-reply-classes.md),
[ADR-0134](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0134-multi-reply-class-and-explicit-handler-classes.md)).
The **single** class (`#[handler::single]`) answers 0-or-1 through
its return value — `-> R` sends `R` back, `-> ()` is fire-and-forget. The
**manual** class (`#[handler::manual]`) takes a `Manual` ctx and issues its own
replies by hand (`ctx.reply` / `ctx.reply_to`), for a reply it can't compute this
turn.

A single handler that answers one exact kind in a later turn returns
`-> Pending<R>`. The offload dispatch calls mint that receipt for work a worker
finishes; for a reply no worker produces, `ctx.hold::<R>()` returns the
`Pending<R>` with a move-only `Held<R>` ticket, which the actor keeps in state
and answers with exactly one `R` through `Held::answer`, from any later handler
([ADR-0243](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0243-typed-held-replies.md)).

A bounded many-item answer is one reply whose kind carries a list, as the log,
trace, and cost tails do. Incremental or unbounded delivery publishes to
subscribers (`Publishes<K>` / `subscribe`).

`#[actor]` records each handler's answer as a type-level **contract row**,
`impl Contract<K> for A { type Reply = …; type Index = … }`: the reply kind `O`
for `-> O` or `-> Pending<O>`, `Silent` for `-> ()`, and `Undeclared` for a
manual handler. Each row names its position in the actor's one type-level
row list, `Contracts::Rows`, so a row exists only where a handler does: a
hand-written row for a kind the actor does not handle does not compile, and a
handled kind of a public actor is declared `pub` (ADR-0231 §10). It also emits
`Contracts::CONTRACTS`, the same rows as `(KindId, ReplyContract)` pairs in the
vocabulary the inputs manifest and the native handler inventory report, with an
adopted handler set's rows appended. A `#[fallback]` contributes neither. The reply checks
[ADR-0231](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0231-protocol-typed-references-and-reply-checks.md)
specifies are built on them. A route publishes its actor's rows, and whether it
has a `#[fallback]`, on its route record when it goes `Live`: a native actor its
own, a wasm component its guest's (republished on replace), and an inline
child's alias its own type's.

A **protocol** names a set of contract rows under a stable type, independent of
any implementation (ADR-0231 §2). `#[protocol]` on a trait of signatures
declares one: each method is a row, `-> O` single and no return silent, and the
explicit return `-> Undeclared` manual. The trait becomes a unit struct whose
`impl Protocol` lists the rows as
`type Rows = (Row<K, O>, …)`. `MeshLoader: CoveredBy<R>` holds when the target
`R` has a contract row for every kind with the exact reply. Rows match by kind,
never by method name; a `#[fallback]` has no row and a manual handler's
`Undeclared` row covers only an explicit manual protocol row. That row promises
the target handles the kind without imposing a reply-handler obligation on the
sender. Coverage is sealed: `aether-actor` computes it
from `Rows`, and a hand-written `CoveredBy` impl does not compile.
`RowSet::CONTRACTS` on the rows is their list in the same `(KindId,
ReplyContract)` vocabulary as `Contracts::CONTRACTS`.

```rust
#[protocol]
pub trait MeshLoader {
    fn load(mail: LoadMesh) -> MeshLoadResult;
    fn set_mode(mail: SetMode);
    fn forward(mail: Forward) -> Undeclared;
}

// is the declaration
pub struct MeshLoader;

impl Protocol for MeshLoader {
    type Rows = (
        Row<LoadMesh, MeshLoadResult>,
        Row<SetMode, Silent>,
        Row<Forward, Undeclared>,
    );
}
```

### Typed paths

An actor names another actor by type in what it stores or sends through a typed
path: `ActorPath<R>` names an `R`, and `ProtocolPath<P>` names an actor covering
the protocol `P`
([ADR-0230](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0230-proven-actor-references.md)
§2, ADR-0231 §3). An actor writes an `ActorPath<R>` with one of two type
constructors, whose bounds check the topology at compile time.
`ActorPath::<R>::instance(&key)` compiles for a root instanced `R`
(`R: Root + Instanced`) and writes `R::NAMESPACE:key`.
`ActorPath::<C>::child(&parent, &key)` compiles for an instanced `C` declared
beneath the parent's actor (`C: ChildOf<P> + Instanced`) and writes
`<parent>/C::NAMESPACE:key`; it refuses only a path past the depth or byte cap.
Each step is a type's `NAMESPACE` and its key, so the text is the canonical
name the registry gives that instance, and writing it reads no registry.
`path.narrow::<P>()` keeps the text under the narrower claim and compiles only
for `P: CoveredBy<R>`.

Both paths are kind fields, carried as the path text with `ErasedActorPath`'s
schema. Decoding either accepts only a well-formed canonical path, and decoding
an `ActorPath<R>` also refuses a path whose leaf namespace is not
`R::NAMESPACE`, so an `ActorPath<R>` that exists names an `R`. A decoded
`ProtocolPath<P>` is checked against the mail registry's published contract
for the live route at its path: the decode refuses a path with no live route,
or whose route does not publish every row of `P`, so a `ProtocolPath<P>` that
exists names an actor covering `P`. Native dispatch decodes with the registry;
a guest's decode has none, so a guest refuses a `ProtocolPath<P>` until
[ADR-0241](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0241-code-is-published-not-loaded.md).
Neither path grants a send: the route can leave after the decode, so its
receiver's `resolve` proves that a live actor stands at the path. The first
consumer, a Bloomery unit's driver, is to write its journal's storage source
this way (ADR-0240 D7):

```rust
let source: ProtocolPath<ArtifactStorage> =
    ActorPath::<JournalActor>::instance(&unit_key).narrow::<ArtifactStorage>();
```

A native actor that receives a `ProtocolPath<P>` proves it with
`ctx.resolve(&path)`, which returns a `ProtocolRef<P>` or a `ResolveError`
(ADR-0231 §3). `resolve` folds the canonical text and reads the route table
once: a route must stand under exactly that name and be `Live`, or it refuses
`NotLive`, naming the path, never a position. It compares no rows: `P` was
proven when the path was narrowed or decoded.
`ctx.send_to(reference, &kind)` and its context-carrying siblings take the
`ProtocolRef<P>` only for a kind `P` lists; any other kind is a compile error,
whatever else the target handles:

```rust
// `run.source: ProtocolPath<ArtifactStorage>`
match ctx.resolve(&run.source) {
    Ok(storage) => ctx.send_to(storage, &read),
    Err(error) => tracing::warn!(%error, "the run's source is refused"),
}
```

The guest arm, `resolve` over an `ActorPath<R>`, lands with the Bloomery
bootstrap (#6829).

A kind or config field naming an actor its receiver will later send to, such
as a subscriber, a handler, a callback, or a source, is a typed path, never an
`ErasedActorPath`
([design rule R-0040](../contributing/design-rules.md#r-0040)). It is an
`ActorPath<R>` when the holder needs one concrete actor type, and a
`ProtocolPath<P>` when it needs only a protocol, such as a subscriber the
publisher cannot name. The receiver proves it once on receipt, by its decode,
and stores the typed proof `resolve` returns. Sending through an erased
reference is being removed (#6895), so an `ErasedActorPath` is left to name,
render, compare, or monitor an actor, and to name a recipient at the untyped
MCP and RPC boundary.

### Helpers that only send

The class marker rides on the context type — a single handler's `WasmCtx<'_>`
is `WasmCtx<'_, Self, Single>` and a manual handler holds
`WasmCtx<'_, Self, Manual>` — which is what makes a stray `ctx.reply` in a
single handler a compile error. The actor is the first parameter, the reply
mode the second, and a ctx that omits its actor is typed by it: `#[actor]`
fills in `Self`, so the ctx reaches only the actors the handler's actor
declares with `depends(R)`. Spelling `Erased` in that slot
(`WasmCtx<'_, Erased>`) asks for the untyped view. One call deeper the class
buys nothing: a helper you factor out of a handler to *send* something never
touches the reply channel,
yet pinning one class makes it uncallable from the others and staying generic
means carrying an `M: ReplyMode` parameter it doesn't read. `ctx.sends()` hands
out `Sends<'_, A>`, typed by the handler's actor — the outbound verbs that take
a proof (`send_to`, plus `send_detached_to` through `MailSender`) with the
marker dropped — so the helper takes `&mut Sends<'_, A>` and every handler
class can call it. The view sends only through a reference it is handed: the
actor mints one with `ctx.actor_ref::<RenderCapability>()` in `wire` and keeps
it in a field, because minting it in the same call as `ctx.sends()` would not
borrow-check. `Sends<'_>` alone names the erased view:

```rust
fn announce<A>(sends: &mut Sends<'_, A>, renderer: ActorRef<RenderCapability>, frame: &Frame) {
    sends.send_to(renderer, frame);
}

#[handler::single]
fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _t: Tick) {
    announce(&mut ctx.sends(), self.renderer, &self.frame);        // Single
}

#[handler::manual]
fn on_redraw(&mut self, ctx: &mut WasmCtx<'_, Self, Manual>, _r: Redraw) {
    announce(&mut ctx.sends(), self.renderer, &self.frame);        // Manual — same helper
    ctx.reply(&Acknowledged);                       // reply stays on the ctx
}
```

`reply` / `reply_to` / `emit` — and `send_with_context`, whose stashed context
is recovered on the reply — stay on `WasmCtx<'_, A, M>`, so a helper that needs
those still states which class it belongs to. That's the line: the reply class
is load-bearing exactly where the reply is.

## Sharing handlers across a family

A family of similar actors — the widgets in a set, the per-platform runtimes of
one capability — tends to carry the same block of handlers. A **handler set**
declares that block once
([ADR-0169](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0169-shared-handler-sets-via-dispatch-miss-delegation.md)).
`#[handler_set]` sits on a trait whose `#[handler::<class>]` methods carry the
shared bodies as trait defaults, and whose required methods are the accessors
those bodies reach through:

```rust
#[handler_set]
pub trait WidgetDefaults {
    fn widget_frame(&mut self) -> &mut WidgetFrame;
    fn widget_state(&mut self) -> &mut InteractionState;

    /// Release any half-finished interaction — an armed press, a live drag.
    fn cancel_activation(&mut self);

    #[handler::single]
    fn on_frame(&mut self, _ctx: &mut WasmCtx<'_>, frame: WidgetFrame) {
        *self.widget_frame() = frame;
    }

    #[handler::single]
    fn on_focus_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        self.widget_state().lose_focus();
        self.cancel_activation();
    }
}
```

An actor adopts the set by naming it in `#[actor]` and implementing the trait:

```rust
impl WidgetDefaults for ToggleWidget {
    fn widget_frame(&mut self) -> &mut WidgetFrame { &mut self.frame }
    fn widget_state(&mut self) -> &mut InteractionState { &mut self.state }
    fn cancel_activation(&mut self) { self.arms.clear(); }
}

#[actor(instanced, root, composable, handler_set(WidgetDefaults))]
impl WasmActor for ToggleWidget {
    // only toggle-specific handlers here
}
```

Dispatch tries the actor's own handlers first and consults the set on a miss,
so an actor's local declarations stay authoritative over anything inherited:

```text
match local arms  ->  DISPATCH_HANDLED
else set dispatch ->  DISPATCH_HANDLED
else #[fallback] / DISPATCH_UNKNOWN_KIND
```

A member that differs for one adopter is **overridden the ordinary Rust way** —
by implementing that trait method — which keeps the kind owned by the set: one
dispatch arm, one manifest record. Re-declaring the same kind as a local
`#[handler]` instead is a coherence error, not a second definition.

A set member's ctx is typed by the adopting actor: `WasmCtx<'_>` in a set reads
as `WasmCtx<'_, Self>`, where `Self` is whichever actor adopts the set. A
default body that reaches another actor therefore states that reach on the
trait, as a supertrait — the real `WidgetDefaults` is
`pub trait WidgetDefaults: WidgetChrome + DependsOn<TextCapability>`, because
its theme handler measures fonts through the text capability — and every
adopter must declare the dependency. `#[handler_set]` adds `Sized` to the
supertraits as well. An override is a plain trait-method impl that no macro
rewrites, so it spells the typed signature itself: `WasmCtx<'_, Self>`.

Set handlers reach the `aether.kinds.inputs` manifest exactly as local ones do,
so `describe_component` reports an adopter's full receive surface and input
subscription covers inherited kinds with no extra wiring. A set is wasm or
native throughout, uses one authoring shape throughout, does not nest, and an
actor adopts at most one — a family that outgrows one set wants a second set,
not a chain.

A set's handlers are contract rows of each adopter too, at positions past the
adopter's own, so an adopter covers a protocol that lists a set kind
(ADR-0231 §2). The rows travel through a bridge macro the set emits and the
adopter's `#[actor]` expands in the adopter's own module, so a set spells its
kind types with paths that resolve from every adopter (`crate::WidgetFrame`),
and a wasm set, whose bridge is crate-local, is adopted within the crate that
defines it.

A native capability adopts a set the same way, with two differences that follow
from how native actors are authored. Handlers are written against `NativeCtx`,
and a capability with a `type State` writes them in the split shape — the state
arrives as the first parameter rather than as a `self` receiver, so the
accessors are associated functions over `Self::State`:

```rust
#[handler_set]
pub trait WindowManagerSurface {
    fn subscribers(state: &mut Self::State) -> &mut WindowSubscribers;

    #[handler::single]
    fn on_unsubscribe(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: UnsubscribeWindow,
    ) -> SubscribeWindowResult {
        match Self::subscribers(state).unsubscribe_path(ctx, mail.selector, &mail.subscription) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => SubscribeWindowResult::Err { error: error.to_string() },
        }
    }
}
```

`unsubscribe_path` matches the subscription's variant, proves its
`ProtocolPath<Subscriber<K>>` live with `ctx.resolve`, and removes that
reference's key from the kind's typed set.

And under the [split identity / runtime shape](../capability-anatomy.md) the
adoption is declared on `#[runtime]`, in the runtime file where the dispatch
table is emitted — the capability struct's `#[actor]` reads it back off that
attribute when it harvests the file, so the set is named once:

```rust
#[runtime(handler_set(WindowManagerSurface))]
impl NativeActor for DesktopWindowCapability {
    // only desktop-specific handlers here
}

impl WindowManagerSurface for DesktopWindowCapability {
    type State = DesktopWindowCapabilityState;

    fn subscribers(state: &mut Self::State) -> &mut WindowSubscribers {
        &mut state.subscribers
    }
}
```

A native set's kinds carry `HandlesKind` markers, so kind-checked sends to an
adopter (`ctx.send_to(&window, &k)` through an `ActorRef<DesktopWindowInstance>`)
compile for inherited kinds too. The markers travel through a `macro_rules!` bridge the set generates, which
means a set's kind types need spellings that resolve at each adopter's `#[actor]`
— for a capability crate, the names re-exported at its crate root.

A `#[cfg]` on a set handler is resolved by the crate that **defines** the set, and
that answer reaches every artifact the set produces, the markers included. An
adopter inherits a surface that is already fixed: enabling a feature of its own,
even one sharing the set's spelling, never changes which handlers it inherits.
That is what keeps a set's dispatch chain and its markers from disagreeing about
which kinds it handles — a marker for a kind the chain would not answer is a send
that compiles and gets dropped at run time. When one adopter genuinely needs a
handler the others do not, declare it locally in that adopter's own `#[actor]`
block, where `#[cfg]` already means the adopter's configuration.

Put in a set only what is genuinely uniform. When bodies disagree on something
load-bearing — the widgets' `SetWidgetState` handlers disagree about which
predicate cancels an activation — a shared body has to pick one reading and
silently change the rest, which is worse than the repetition it removes.

## Configuring an actor

An actor can take typed **boot configuration**. Declare a `Config` associated type
and the chassis threads a decoded value into `init` as its leading argument:

```rust
#[actor(root)]
impl WasmActor for ProbeWithConfig {
    type Config = ProbeConfig;
    const NAMESPACE: &'static str = "probe_with_config";

    fn init(config: ProbeConfig, ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> { … }
}
```

Most actors need none. Omit `Config` and the `#[actor]` macro synthesizes `()` and
injects the unused argument, so a no-config `init` stays the terse
`fn init(ctx: &mut WasmInitCtx<'_>)` from the examples above — there's no `type Config = ()` to
write by hand.

The two hosts differ in one way, and it follows from how the config reaches them. A
capability's config is built in-process by the chassis, so it can be any
`Send + 'static` type. A component's config has to cross the wasm boundary as bytes,
so it must be a `Kind` — encoded at the load edge, decoded on the way in. That seam
aside, the authoring shape is identical. (How a component's config rides the load
call, and how a chassis assembles its own layered config, are the
[components](../systems/components.md) and configuration pages.)

## Names and addressing

The `NAMESPACE` const on the `Actor` trait is the name an actor claims — the
`"hello"`, `"camera"`, `"aether.audio"` in the examples above. From the name and
the actor's place in the runtime tree come two ids, two distinct moments
([ADR-0099](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0099-actor-identity-and-addressing.md)):

- **`NAMESPACE` → `ActorId`, at compile time.** The hash of the `NAMESPACE`
  names *which actor* this is — binary-unique, the same wherever the actor is
  hosted. An instanced actor (below) folds its runtime discriminator in:
  `hash(NAMESPACE:subname)`.
- **Lineage → `MailboxId`, at creation.** *Where* the actor sits is its
  **lineage** — the ordered ActorIds from the substrate root down to it, fixed
  when the actor is created. Its `MailboxId` is a hash chain over the lineage,
  one fold step per node, and mail routes to that.

For a **capability** the two coincide. It sits at the root, so its lineage is
one node and the fold of one node is that node: `MailboxId == ActorId`, the
`NAMESPACE` is the whole address (`aether.audio`, `aether.render`,
`aether.window`), and `ctx.send::<AudioCapability>(..)` resolves to it as a
compile-time const with no runtime lookup.

For a **component** the name is the one its module publishes for the type
([ADR-0241](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0241-code-is-published-not-loaded.md)
§5): a singleton sits at the root at `NS`, exactly as a capability does, an
instanced type at `NS:key`, and one loaded beneath a parent at `parent/NS:key`
— the canonical rendered address `LoadResult.path` hands back. The string is a
display rendering of the lineage; the `MailboxId` is the fold over the nodes
(the host registry parses a written path and folds it node by node), never a
hash of the joined string.

There is **one addressing verb**: you address a type, and the type declares
where it lives. `ctx.send::<Camera>(..)` routes through
`ctx.actor_ref::<Camera>()`, which reads the resolver `Camera` declares and
selects the routing seed from it — the root for a capability or a loaded root
singleton — so the send site says who it is talking to and never where that
peer sits. A declared dependency is always a root singleton, so moving the
caller changes nothing about the route.

A load name is the one thing the type cannot declare, because it is a runtime
fact. Replica 0 of a `replicas` fan-out claims the bare base name, so the
bare-type spelling reaches it when the base is the type's own namespace; a
component loaded under any other name is reached through the reference its load
proved, never by folding the name at the send site.

`LoadResult.path` is an `ErasedActorPath`: the host's `resolve_address` parser, at
the MCP, RPC, and harness boundary, is the one place an `ErasedActorPath` becomes a
position
([ADR-0230](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0230-proven-actor-references.md)).
The reply carries no position. A successful load reply is sent by the loaded
actor itself, so a native requester keeps `ctx.sender()` as its reference and
an embedder types the reply event's stamped sender.

Because the lineage is the address, two actors collide exactly when they would
occupy the same position — same parent, same name. The substrate enforces one
claimant per position **at registration**: a second capability claiming a taken
root name fails to boot, and a component loaded under a name already in use
comes back as a load error. This is not a compile-time check — two types can
declare the same `NAMESPACE` string and compile cleanly; the collision only
surfaces when the second one tries to register. For an instanced actor (below)
the colliding unit is the full `NAMESPACE:subname` under one parent, not the
shared prefix.

A dash in a namespace is a naming convention, not addressing grammar. Use it
only for a genuine adjacent sibling of an existing bare base:
`aether.kit.camera-controller` is the controller actor beside the bare
`aether.kit.camera` actor. The dash has no addressing semantics — it makes neither actor a child of the other, and the full
`NAMESPACE` still yields the `ActorId` before lineage yields the `MailboxId`. Do
not use a dash merely to spell a multi-word segment; that is what an underscore
is for, as in `aether.widget.menu_bar` and `aether.widget.text_field`.

`ctx.actor_ref::<Camera>()` returns an `ActorRef<Camera>` for the physical
trampoline mailbox: the trampoline and its loaded guest share one mailbox, while
the guest type supplies the compile-time mail-handling surface. A `const` beside the call
site holding what `Camera::NAMESPACE` already declares would be a second naming
authority nothing checks against the first, and the bare-type spelling exists so
it has nothing to hold.

## One or many: cardinality

An actor type is either **singleton** or **instanced**, marked by the `Singleton` or
`Instanced` trait, and the choice sets whether its `NAMESPACE` is a whole name or a
prefix.

A **singleton** is one of a kind: at most one instance under a given parent, and
its `ActorId` is the plain `hash(NAMESPACE)`. Every capability is a root
singleton — its one-node lineage makes its `NAMESPACE` the whole address, so you
address it straight by type, `ctx.send::<R>(..)`.

An **instanced** actor is one of many sharing a prefix. Its `NAMESPACE` is that
prefix, and each live instance gets its own `ActorId` by folding a runtime
discriminator in — `hash(NAMESPACE:subname)`, rendered `aether.tcp.session:42` —
with its `MailboxId` folding that ActorId under the parent's lineage, so two
instances under one parent differ by subname. The case that drives this is
sockets: a singleton listener accepts connections and spawns a session actor per
connection with `ctx.spawn_child`
([ADR-0079](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0079-instanced-actors-as-a-first-class-category.md)), then reaches a specific one through
the reference that spawn returned, kept in its own child map. A subname is
never folded at a send site: an instance is reached through the reference its
spawn returned or through a `child` / `child_as` relative.

`ctx.spawn_child` is the native verb. A native capability names only the child
type, and can spawn an `Instanced` native actor when that child declares
`ChildOf<Parent>` for the actor doing the spawning:
`ctx.spawn_child::<TcpSessionActor>(subname, config, params)`. The parent comes
from the ctx. A handler opts into the call by naming its own actor in its ctx
signature — `ctx: &mut NativeCtx<'_, Self, Single>`, or
`NativeCtx<'_, Self, Manual>` for a manual-reply handler — and the `#[actor]`
macro hands such a handler a ctx typed by the actor being dispatched. Every
other handler keeps the plain `NativeCtx<'_>` and reaches no spawn surface, so
a birth cannot be placed under a parent other than the one running.

A native birth **stages** during the handler turn and commits afterward. The
handler chains any
`after_init` bootstrap mail and ends with `.stage()` (or `.stage_with(context)`
to carry a context kind forward), which does the local half of the work — the
permission and subname checks, `A::init`, the transport — and appends one
ordered prepared birth to the parent's buffer
([ADR-0165](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0165-handlers-read-views-emit-effects.md)). Nothing in the shared
registry moves while the handler runs, so no spawn takes a global lock
mid-turn. What comes back is a `SpawnReceipt`: the child's `canonical_name`,
an `ErasedActorPath` derived from the parent's identity and proven against the
ADR-0166 address grammar on the spot, which names the child for correlation,
plus the birth's `request` id. A lineage too deep or too long for that
grammar never gets a receipt: `.stage()` itself returns
`SpawnError::PathInvalid`, and `.stage_with` hands its context back beside the
error. Neither field is a send target: the child is not
`Live` yet, so nothing can prove it. Mail the child must see first
rides `after_init` on the birth itself, and later mail goes through
`ctx.send_to(&child, &k)` once the `Ok` completion hands back its reference.

```rust
let Ok(receipt) = ctx
    .spawn_child::<TcpSessionActor>(Subname::Counter, config, params)
    .after_init(Hello)
    .stage()
else {
    return;
};
```

The receipt says the birth was accepted locally; the registry owner applies it
after the handler returns, and *that* result is authoritative. It arrives back
at the spawner through the ordinary task-completion path as
`TaskDone<SpawnOutcome<A>>` for a child of type `A`, correlated to
`receipt.request`, and owes nothing
([ADR-0243](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0243-typed-held-replies.md)
§9) — so an apply-time conflict (a name another actor won
first, say) surfaces as one typed failure rather than a silent half-spawn. A
`SpawnOutcome` names itself on both arms:

```rust
struct SpawnOutcome<A> {
    canonical_name: ErasedActorPath,
    result: Result<ActorRef<A>, SpawnError>,
}
```

so a handler correlates the completion with the birth it staged by
`canonical_name`, straight off the outcome, and stages with no context unless
there is something the spawn genuinely does not know — a peer address, which
leg of a multi-step plan this birth belongs to. A handler whose correlation key
is not the child's name stages with that key as a context kind and takes it in
the completion with `ctx.take_context()`; a live value the completion needs,
such as a channel or a held reply, waits in actor state under that key. A handler that keeps or mails its child holds the `Ok` reference
and sends through `ctx.send_to(&child, &k)`, so a handler that mails its child after the
bootstrap waits for that completion the same way one that must know the child
is live before it reports success does. Synchronous commit still
exists, but only at the boot/embedder boundary — `BuiltChassis::spawn_actor` /
`PassiveChassis::spawn_actor` and their `.finish()` terminal, which block until
the birth is live and hand back its `ActorRef<A>`, a proof you can immediately
send through. The actors the chassis composed itself — each singleton
capability and pumped actor — are recorded the same way when their routes go
live, and `actor_ref::<R>()` on either chassis handle reads one back by type.

That terminal spans the chassis's one authority boundary, the **registry
authority seal**. A chassis seals once boot is over: a built chassis after its
driver's `Start` stage returns successfully, a passive chassis immediately
before the `PassiveChassis` reaches you. Before the seal, boot writes the
registry directly — it wants synchronous apply and read-your-writes with no
scheduler thread in the picture yet. After it, there is no direct writer left to
name, so an embedder's `.finish()` submits the birth to the owner and waits for
it exactly the way a handler's staged birth is applied: `Starting`, `wire` at
the actor's execution home, then `Live`. Every birth in a running engine follows
that one protocol, whoever asked for it.

Waiting there is safe because of who waits: an embedder thread is not a pool
worker, so it can never be the worker the owner needs to make progress. That is
also why a *handler* has no such terminal — a handler blocking on the owner
could be the last worker, so it stages instead.

When the handler that receives a completion owes a reply of its own and answers
it by staging *another* birth, it hands that debt straight on with
`.continue_from(done, context)`. The reply the caller is waiting for is a `DeferredReply` — who is waiting, plus
the [settlement hold](../systems/tracing-and-settlement.md) keeping their chain
open — and it rides inside the `TaskDone` the handler already holds, so the
successor stage inherits one continuously-held chain instead of closing and
reopening it. Every synchronous failure in
`continue_from` hands the value back untouched, so the terminal error still goes
out exactly once:

```rust
match ctx.spawn_child::<Worker>(Subname::Named(&name), config, ()).continue_from(done, plan) {
    Ok(_) => { /* the successor now owns the reply */ }
    Err((error, done)) => done.resolve_err(ctx, &Failed { error: format!("{error:?}") }),
}
```

A handler with no `TaskDone` in hand — one that parks a caller across a worker
thread, say — mints the same debt from its ctx with `ctx.defer_reply_to(target)`
and passes that to `continue_from` instead. A typed `Held<R>` from `ctx.hold` is
also an `IntoDeferredReply`, so `continue_from` takes it the same way and the
chain stays held across the hand-off. Dropping a `DeferredReply` without
replying releases its hold (settlement is never wedged) and then panics, in every
build, which the scheduler escalates through the chassis aborter, because a lost
reply strands the caller forever. An actor that closes with debts still owed
while the engine keeps running answers them: the engine sends each `Held<R>`
its `R::unanswered()` before releasing its hold ([ADR-0243](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0243-typed-held-replies.md) §1), and an actor that keeps a
`DeferredReply` in state answers it in `unwire` with the terminal it knows. An
engine teardown settles held debts silently instead, because every requester
is closing with it. `abandon_for_actor_close`, which releases a debt with no reply, survives only
for the component host's boot waiters until #7008 removes it.

Wasm enforces the same `ChildOf` permission when a component creates a child
inline ([ADR-0114](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0114-inline-child-actors.md)): the child is an `Instanced` actor its module also
exports (or lists under `private = [..]`), co-located in the parent's wasm
instance, and the spawn compiles only when the child declares the exact
`child_of(Parent)` relationship or is an instanced `composable` module child.
One wasm crate can export several actor types
(`export!(public = [RootManager, Panel, …])`), and a running instance stands up a
child just as the listener stands up a session:
`ctx.spawn_inline::<Panel>(Subname::Named("body"), &config)` names only the child
type and reads the parent from the ctx as the native side does. Its two-type
counterpart `ctx.spawn_inline_child::<RootManager, Panel>(Subname::Counter, &config)`
stays for the per-parent `child_of(Parent)` edge: `WasmCtx` is addressed by tag
rather than by Rust type, so the parent is named at the call and the SDK checks it
against the ctx's registry-backed actor tag before allocating the child's alias;
writing a different parent type earns an error rather than bypassing the declared
edge. Either hands back an `InlineChild<Panel>`, whose `send` is checked against
`Panel`'s handler set. A parent keeping children of several types in one table
narrows each handle with `narrow::<P>()` to a `ProtocolRef<P>` for a protocol the
child covers, which compiles only where it does (ADR-0231 §3), and keeps the
erased `erase()` reference as the table's identity key. A component spawns within the
module it was built from; a foreign module comes in through `load_component`, which
carries its own code and kinds — the boundary is covered in
[Components & lifecycle](../systems/components.md).

An inline child ends by closing, as any actor does
([ADR-0241](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0241-code-is-published-not-loaded.md) §8):
`ctx.despawn_inline_child(child)` closes it, and so does its parent's close.
Each watcher gets a `MonitorNotice` sent from the child, and the child's name
tombstones. The name is spent: a later `monitor` of it is refused with
`TargetTombstoned`, and spawning the same key beneath the same parent fails with
`SpawnError::AliasAllocationFailed`. A parent that wants a fresh child after a
despawn spawns it under a new key, such as `Subname::Counter`.

A component can also run as several instances of one type: an `instanced` type
loaded under different keys is an independent actor at each `NS:key`. The loader
hosts every component in a native trampoline actor, spawned once per load, but
the actor is named by the guest's published namespace, never the trampoline's.

## One model, two hosts

Here's the part that ties the engine together. There aren't two actor systems —
there's **one model with two hosts**, differing in where the actor's code lives and
how it reaches the outside world ([ADR-0074](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0074-unified-actor-model-for-substrate-and-guests.md)):

- A **native capability** is an actor compiled *into* the substrate, implementing
  `NativeActor` and linked at build time. It's the host an actor takes when what it
  does needs native Rust APIs or raw performance — the GPU through wgpu, the audio
  device through cpal, the filesystem, the OS input loop. The renderer, the audio
  mixer, the filesystem, the input streams, and the component-loader itself are all
  capabilities; together they're the chassis.
- A **component** is an actor *loaded at runtime* as a wasm module, run sandboxed
  behind the wasm wall and reaching the outside world only by mailing capabilities.
  It implements `WasmActor`, and the substrate drives it through an FFI
  **trampoline**. This is the agent-facing extension path: new behavior with no
  substrate rebuild.

The two hosts preserve one actor model, but native capabilities also split their
always-addressable identity from runtime state. Both have configuration and the
same lifecycle intent; native handler signatures receive `&mut Self::State`
while wasm handlers receive `&mut self`. The host contexts and machinery differ,
but mail contracts remain symmetric. A current native capability looks like:

```rust
#[actor(singleton)]
pub struct AudioCapability;

pub struct AudioCapabilityState { /* native resources */ }

#[runtime]
impl NativeActor for AudioCapability {
    type State = AudioCapabilityState;
    type Config = AudioConfig;
    const NAMESPACE: &'static str = "aether.audio";

    fn init(config: AudioConfig, ctx: &mut NativeInitCtx<'_>)
        -> Result<Self::State, BootError> { … }

    #[handler::single]
    fn on_note_on(state: &mut Self::State, ctx: &mut NativeCtx<'_>, note: NoteOn) { … }
}
```

Because the only coupling is mail, an actor can't tell whether the mailbox it
sends to is backed by native Rust or sandboxed wasm — and doesn't need to. A
component sends `aether.render` a `DrawTriangle` exactly as one capability sends
another. This symmetry is the point of [ADR-0074](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0074-unified-actor-model-for-substrate-and-guests.md): one mental model, one macro, one
lifecycle, and components get to reuse every pattern capabilities use.

So **start here, with the actor**, and the two host pages are just specializations:

- The wasm/FFI host — the trampoline, `export!`, loading, hot-swap — is
  [Components & lifecycle](../systems/components.md), and the empty-crate-to-loaded
  walkthrough is the [Writing a component](../recipes/writing-a-component.md) recipe.
- Adding a native capability is a recipe ([Adding a chassis capability](../recipes/adding-a-chassis-capability.md));
  it's the same `#[actor]` shape against `NativeActor`.

## Where to read more

- What flows between actors — [The type system](type-system.md).
- The rules the model guarantees (ordering, fire-and-forget, capability =
  reachability, single-threaded) — [Invariants & guarantees](invariants.md).
- How mail routes and in what order — [Mail, kinds & scheduling](../systems/mail-and-kinds.md).
- How the scheduler keeps an actor single-threaded, and what to do instead of
  blocking — [Concurrency & blocking](../systems/concurrency.md).
- The wasm host in depth — [Components & lifecycle](../systems/components.md).
