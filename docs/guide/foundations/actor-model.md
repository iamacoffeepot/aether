# The actor model

> **Governing ADR:** [ADR-0074](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0074-unified-actor-model-for-substrate-and-guests.md) (the unified actor model — capabilities and
> components are one model, not two) with [ADR-0079](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0079-instanced-actors-as-a-first-class-category.md) (the lifecycle stages)
> and [ADR-0033](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0033-handler-driven-inputs-manifest.md) (the `#[actor]` macro), extended by [ADR-0096](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0096-multi-actor-wasm-modules.md) (a wasm module exports several
> actor types), [ADR-0097](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0097-wasm-sibling-spawn.md) (a component spawns its siblings), and [ADR-0099](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0099-actor-identity-and-addressing.md) (actor
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
actor and ask to shut itself down. A component can spawn its **sibling** types — the
other actors its own module exports
([ADR-0097](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0097-wasm-sibling-spawn.md), and the
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
#[actor]
impl WasmActor for Hello {
    const NAMESPACE: &'static str = "example.hello";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Hello)
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
    }

    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        ctx.actor::<RenderCapability>().send(&TRIANGLE);   // draw every tick
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

You address peers **by type** — `ctx.actor::<RenderCapability>().send(&payload)`
compiles only if that actor actually handles the payload's kind, and both the
mailbox id and kind id resolve at compile time. The handler takes the decoded mail
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

The mailbox chain selects which permitted lineage is meant. An accepted
session resolves through its named listener, while an outbound session resolves
directly beneath the capability:

```rust
let accepted_session = tcp
    .resolve::<TcpListenerActor>("game")
    .resolve::<TcpSessionActor>("shared");
let outbound_session = tcp.resolve::<TcpSessionActor>("shared");

assert_ne!(
    accepted_session.mailbox_id(),
    outbound_session.mailbox_id(),
);
```

`ChildOf` therefore does not choose one global parent for an actor type. Each
typed `resolve` step checks one declared direct edge, and the mailbox carried
from the previous step supplies the canonical lineage and identity fold.

### External actor addresses

Rust actor code continues to resolve peers by type. String-addressed
boundaries such as MCP, configuration, and harness calls may additionally use
an ADR-0166 abbreviation rooted in an actor namespace:

```text
aether.component://camera
```

The substrate expands that spelling from the generated `Root` and `ChildOf`
inventory before it performs the ordinary canonical registry lookup. With the
component host's one instanced child family, the address above expands to:

```text
aether.component/aether.embedded:camera
```

An explicit child namespace is always accepted when the declared edge and
cardinality match:

```text
aether.component://aether.embedded:camera
```

A bare discriminator is allowed only when the current actor has exactly one
logical instanced-child namespace. If several instanced child namespaces are
possible, resolution returns a deterministic ambiguity error listing the
explicit segments the caller can use. Exact singleton child namespaces take
precedence over discriminator elision.

Abbreviations are boundary input, never actor identity. The registry expands
them before hashing and stores, lists, and reverse-reports only the canonical
path. Unknown roots, illegal segments, ambiguous children, path-limit
violations, and a valid expansion with no live mailbox remain distinct
resolution errors.

## Reply classes

A handler declares how it answers through its class marker
([ADR-0112](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0112-handler-reply-classes.md),
[ADR-0134](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0134-multi-reply-class-and-explicit-handler-classes.md)).
The **single** class (`#[handler::single]`) answers 0-or-1 through
its return value — `-> R` sends `R` back, `-> ()` is fire-and-forget. The
**manual** class (`#[handler::manual]`) takes a `Manual` ctx and issues its own
replies by hand (`ctx.reply` / `ctx.reply_to`), for a reply it can't compute this
turn.

The **multi** class (`#[handler::multi]`) answers one dispatch with *several*
mails. Its ctx is `Multi<K>` and it emits 0..n mails of the declared kind `K`
through `ctx.emit`, returning `()` — the emissions are the reply:

```rust
#[handler::multi]
fn on_query(&mut self, ctx: &mut WasmCtx<'_, Multi<Row>>, q: Query) {
    for row in self.rows_matching(&q) {
        ctx.emit(&row);            // one Row mail per match
    }
}
```

Each `emit` is a **detached chain root addressed at the dispatch source** — the
mail goes back to whoever sent the query, correlated by its payload, on a fresh
causal chain rather than the request's. So the request chain settles promptly on
the handler's return instead of staying open for the stream, and every emission
has the same chain shape regardless of when the producer sends it. A dispatch with
no routable source (session / broadcast mail) drops the emission with a warning.
The `#[actor]` macro reads `K` off the `Multi<K>` marker, so
`describe_component` reports the real `ReplyContract::Multi(K)` element kind.

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

#[actor(instanced, composable, handler_set(WidgetDefaults))]
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

Set handlers reach the `aether.kinds.inputs` manifest exactly as local ones do,
so `describe_component` reports an adopter's full receive surface and input
subscription covers inherited kinds with no extra wiring. A set is wasm or
native throughout, uses one authoring shape throughout, does not nest, and an
actor adopts at most one — a family that outgrows one set wants a second set,
not a chain.

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
    fn on_unsubscribe_all(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: UnsubscribeAllWindows) {
        Self::subscribers(state).unsubscribe_all(mail.mailbox);
    }
}
```

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

A native set's kinds carry `HandlesKind` markers, so typed sends to an adopter
(`ctx.actor::<DesktopWindowInstance>().send(&k)`) compile for inherited kinds
too. The markers travel through a `macro_rules!` bridge the set generates, which
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
#[actor]
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
`aether.window`), and `ctx.actor::<AudioCapability>()` resolves to it as a
compile-time const with no runtime lookup.

For a **component** the `NAMESPACE` is the *default load name*, and the loaded
actor runs under its runtime parent. With the root component host as parent,
the lineage is `aether.component` followed by the component as an instance
under the embedding-host class (`aether.embedded`), rendered with one `/` per
node as `aether.component/aether.embedded:<name>` — the canonical rendered
address `LoadResult.name` hands back. A nested host contributes its own lineage
instead. The string is a display rendering of the lineage; the `MailboxId` is
the fold over the nodes (`mailbox_id_from_path` on the string side), never a
hash of the joined string.

Typed component-peer addressing selects the logical-parent mailbox retained by
the runtime. `ctx.actor::<Camera>()` and `peer::<Camera>()` fold the default
`Camera::NAMESPACE` beneath that parent; `peer_named::<Camera>(load_name)` uses
an explicit runtime load name beneath the same parent. Moving the caller under
a nested or replacement host therefore moves both peer routes without a host
lookup or call-site change. These paths accept only
`Addressable<Resolver = Embedded>` recipients; root (`One`), caller-relative
(`Many`), and spawned embedded (`EmbeddedMany`) actor types describe different
placements. Replicas such as `camera-0` and `camera-1` have no default-named
instance and are reached with `peer_named`.

The explicit component-host route remains useful when code already holds that
host mailbox: `loaded::<Camera>(load_name)` folds from the held host regardless
of the caller's own parent. Keep `LoadResult.mailbox_id` for direct by-id
addressing. `LoadResult.name` is the canonical rendered address for
external/string addressing; do not pass it to `loaded`, `peer_named`,
`resolve_actor`, or `send_to_named`.

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
`aether.kit.camera` actor. The dash has no addressing semantics — it makes
neither actor a child of the other, and the full `NAMESPACE` still yields the
`ActorId` before lineage yields the `MailboxId`. Do not use a dash merely to
spell a multi-word segment: `aether.kit.terra` is the bare Terra actor even
though its implementation type is `TerraEditor`, not
`aether.kit.terra-editor`.

A capability can also dress up its mail surface with **extension-trait helpers** —
typed methods on the mailbox handle that stand in for raw kind sends.
`ctx.actor::<WindowCapability>().subscribe::<Key>(WindowSelector::All)` is one
(from `WindowManagerMailboxExt`), and
`ctx.actor::<ComponentHostCapability>().loaded::<Camera>("camera")` is the
loaded-component lookup just mentioned (from `ComponentHostWasmExt` in a
component, `ComponentHostNativeExt` in a capability). In a component,
`ctx.peer::<Camera>()` and `ctx.peer_named::<Camera>("camera")` offer the same
embedded-only route for default and explicit loads. They resolve from the
component ctx's runtime parent and return the physical trampoline mailbox typed
as `Camera`; the trampoline and its loaded guest share one mailbox, while the
guest type supplies the compile-time mail-handling surface. By contrast,
`ctx.actor::<ComponentHostCapability>().loaded::<Camera>("camera")` is the
explicit root-host form and follows the declared host-to-trampoline edge from
the handle it was called on.

## One or many: cardinality

An actor type is either **singleton** or **instanced**, marked by the `Singleton` or
`Instanced` trait, and the choice sets whether its `NAMESPACE` is a whole name or a
prefix.

A **singleton** is one of a kind: at most one instance under a given parent, and
its `ActorId` is the plain `hash(NAMESPACE)`. Every capability is a root
singleton — its one-node lineage makes its `NAMESPACE` the whole address, so you
address it straight by type, `ctx.actor::<R>()`.

An **instanced** actor is one of many sharing a prefix. Its `NAMESPACE` is that
prefix, and each live instance gets its own `ActorId` by folding a runtime
discriminator in — `hash(NAMESPACE:subname)`, rendered `aether.net.session:42` —
with its `MailboxId` folding that ActorId under the parent's lineage, so two
instances under one parent differ by subname. The case that drives this is
sockets: a singleton listener accepts connections and spawns a session actor per
connection with `ctx.spawn_child`
([ADR-0079](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0079-instanced-actors-as-a-first-class-category.md)), then reaches a specific one by subname,
`ctx.resolve_actor::<SessionActor>("42")`.

That `resolve_actor::<R>(key)` spelling is typed keyed addressing, not a flat
string escape hatch. It accepts only an instanced, caller-addressable `R`, asks
`R::Resolver` whether to select the ctx's current, root, or logical-parent
mailbox, and calls `R::resolve(selected_mailbox.0, key)`. The built-in `Many`
resolver selects the current actor, so a child instance resolves beneath its
caller; another keyed resolver can deliberately select a different declared
scope. By contrast, `send_to_named(name, payload)` has no recipient type or
resolver: it hashes `name` as one flat mailbox name. Use that only for an
actually flat registered name, never for a rendered lineage path or as a
substitute for keyed typed resolution.

`ctx.spawn_child` works on both hosts. A native capability names only the child
type, and can spawn an `Instanced` native actor when that child declares
`ChildOf<Parent>` for the actor doing the spawning:
`ctx.spawn_child::<TcpSessionActor>(subname, config, params)`. The parent comes
from the ctx. A handler opts into the call by naming its own actor in its ctx
signature — `ctx: &mut NativeCtx<'_, Single, Self>`, or
`NativeCtx<'_, Manual, Self>` for a manual-reply handler — and the `#[actor]`
macro hands such a handler a ctx typed by the actor being dispatched. Every
other handler keeps the plain `NativeCtx<'_>` and reaches no spawn surface, so
a birth cannot be placed under a parent other than the one running.

A native birth **stages** during the handler turn and commits afterward. The
handler chains any
`after_init` bootstrap mail and ends with `.stage()` (or `.stage_with(context)`
to carry your own value forward), which does the local half of the work — the
permission and subname checks, `A::init`, the transport — and appends one
ordered prepared birth to the parent's buffer
([ADR-0165](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0165-handlers-read-views-emit-effects.md)). Nothing in the shared
registry moves while the handler runs, so no spawn takes a global lock
mid-turn. What comes back is a `SpawnReceipt`: the child's `mailbox_id` and
`canonical_name`, both derived from the parent's identity and usable
immediately as a send target, plus a `completion` `DispatchId`.

```rust
let Ok(receipt) = ctx
    .spawn_child::<TcpSessionActor>(Subname::Counter, config, params)
    .after_init(Hello)
    .stage()
else {
    return;
};
ctx.send_envelope_tracked(receipt.mailbox_id, Frame::ID, &frame.encode_into_bytes());
```

The receipt says the birth was accepted locally; the registry owner applies it
after the handler returns, and *that* result is authoritative. It arrives back
at the spawner through the ordinary task-completion path as
`TaskDone<SpawnOutcome, C>`, keyed by `receipt.completion` — so an apply-time
conflict (a name another actor won first, say) surfaces as one typed failure
rather than a silent half-spawn. A `SpawnOutcome` names itself on both arms:

```rust
struct SpawnOutcome {
    mailbox_id: MailboxId,
    canonical_name: Arc<str>,
    result: Result<(), SpawnError>,
}
```

so a handler correlates the completion with the birth it staged straight off the
outcome, and `C` stays `()` unless there is something the spawn genuinely does
not know — a peer address, a channel, which leg of a multi-step plan this birth
belongs to. A handler that must know the child is live before it reports success
waits for that completion; one that only needs somewhere to send mail can use
the receipt directly. Synchronous commit still exists, but only at the
boot/embedder boundary — `BuiltChassis::spawn_actor` /
`PassiveChassis::spawn_actor` and their `.finish()` terminal, which block until
the birth is live and hand back the `MailboxId` you can immediately address.

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
    Ok(receipt) => { /* the successor now owns the reply */ }
    Err((error, done)) => done.resolve_err(ctx, &Failed { error: format!("{error:?}") }),
}
```

A handler with no `TaskDone` in hand — one that parks a caller across a worker
thread, say — mints the same debt from its ctx with `ctx.defer_reply_to(target)`
and passes that to `continue_from` instead. Dropping a `DeferredReply` without
replying releases its hold (settlement is never wedged) and trips a
`debug_assert`, because a lost reply strands the caller forever.

Wasm enforces
the same `ChildOf` permission, and names both types to do it: a component spawns
its own **sibling** types — `Instanced` actors its module also exports
([ADR-0097](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0097-wasm-sibling-spawn.md)) — only when the child declares the exact
`child_of(Parent)` relationship or is an instanced `composable` module child.
One wasm crate can export several
actor types (`export!(RootManager, Panel, …)`), and a running instance stands up a
sibling just as the listener stands up a session:
`ctx.spawn_child::<RootManager, Panel>(Subname::Counter, &config)`. `WasmCtx` is
addressed by tag rather than by Rust type, so the parent is named at the call
and the SDK checks it against the ctx's registry-backed actor tag before
encoding config or calling the host; writing a different parent type earns an
error rather than bypassing the declared edge. A component spawns within the
module it was built from; a foreign module comes in through `load_component`, which
carries its own code and kinds — the boundary is covered in
[Components & lifecycle](../systems/components.md).

A component can also run as several instances of one type: load the same wasm under
different names and each is an independent actor at its own
`aether.component/aether.embedded:<name>`. The loader in fact hosts every component behind
an instanced trampoline actor, spawned once per load — so even a single loaded
component is, underneath, one instance of an instanced host.

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
