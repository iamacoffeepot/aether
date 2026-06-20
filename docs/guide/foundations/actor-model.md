# The actor model

> **Governing ADR:** [ADR-0074](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0074-unified-actor-model-for-substrate-and-guests.md) (the unified actor model — capabilities and
> components are one model, not two) with [ADR-0079](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0079-instanced-actors-as-a-first-class-category.md) (the lifecycle stages)
> and [ADR-0033](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0033-handler-driven-inputs-manifest.md) (the `#[actor]` macro), extended by [ADR-0096](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0096-multi-actor-wasm-modules.md) (a wasm module exports several
> actor types), [ADR-0097](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0097-wasm-sibling-spawn.md) (a component spawns its siblings), and [ADR-0099](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0099-actor-identity-and-addressing.md) (actor
> identity and addressing). This model is **stable**; it's the
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
`WasmCtx` in a component, `NativeCtx` in a capability — but you write handlers against a
shared set of capability traits (`Resolver`, `MailSender`, and friends), so the same
body works on either.

## Authoring an actor

You declare the receive side with the **`#[actor]`** attribute on one `impl`
block, and each **`#[handler]`** method *is* a handler — the macro infers the kind
it handles from the method's **third parameter**:

```rust
#[actor]
impl WasmActor for Hello {
    const NAMESPACE: &'static str = "hello";

    fn init<C: Resolver>(_ctx: &mut C) -> Result<Self, ActorInitError> {
        Ok(Hello)
    }

    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
    }

    #[handler]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        ctx.actor::<RenderCapability>().send(&TRIANGLE);   // draw every tick
    }

    #[handler::manual]
    fn on_ping(&mut self, ctx: &mut WasmCtx<'_, Manual>, ping: Ping) {
        if let Some(sender) = ctx.reply_target() {
            ctx.reply_to(sender, &Pong { seq: ping.seq });
        }
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

## Configuring an actor

An actor can take typed **boot configuration**. Declare a `Config` associated type
and the chassis threads a decoded value into `init` as its leading argument:

```rust
#[actor]
impl WasmActor for ProbeWithConfig {
    type Config = ProbeConfig;
    const NAMESPACE: &'static str = "probe_with_config";

    fn init<C>(config: ProbeConfig, ctx: &mut C) -> Result<Self, ActorInitError>
    where C: Resolver { … }
}
```

Most actors need none. Omit `Config` and the `#[actor]` macro synthesizes `()` and
injects the unused argument, so a no-config `init` stays the terse
`fn init<C>(ctx: &mut C)` from the examples above — there's no `type Config = ()` to
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
`aether.input`), and `ctx.actor::<AudioCapability>()` resolves to it as a
compile-time const with no runtime lookup.

For a **component** the `NAMESPACE` is the *default load name*, and the loaded
actor runs under the component host: its lineage is the `aether.component` host,
then itself as an instance under the embedding-host class (`aether.embedded`),
rendered with one `/` per node as
`aether.component/aether.embedded:<name>` — the name `LoadResult` hands back
(the `NAMESPACE` unless the load overrode it). The string is a display rendering
of the lineage; the `MailboxId` is the fold over the nodes
(`mailbox_id_from_path` on the string side), never a hash of the joined string.
You reach a loaded component through its lineage: pass `LoadResult.name` to
`resolve_actor`, use the `loaded` helper below, or address it by bare type —
`ctx.actor::<Camera>()` on an embeddable component type resolves by folding the
component's own name under the embedding-host class, landing on the same hosted
mailbox.

Because the lineage is the address, two actors collide exactly when they would
occupy the same position — same parent, same name. The substrate enforces one
claimant per position **at registration**: a second capability claiming a taken
root name fails to boot, and a component loaded under a name already in use
comes back as a load error. This is not a compile-time check — two types can
declare the same `NAMESPACE` string and compile cleanly; the collision only
surfaces when the second one tries to register. For an instanced actor (below)
the colliding unit is the full `NAMESPACE:subname` under one parent, not the
shared prefix.

A capability can also dress up its mail surface with **extension-trait helpers** —
typed methods on the mailbox handle that stand in for raw kind sends.
`ctx.actor::<InputCapability>().subscribe::<Key>()` is one (from
`InputMailboxExt`), and `ctx.actor::<ComponentHostCapability>().loaded::<Camera>("camera")`
is the loaded-component lookup just mentioned (from `ComponentHostWasmExt` in a
component, `ComponentHostNativeExt` in a capability). Each helper is available on
both the component and the capability handle, so the same call reads the same
whichever host you write from.

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

`ctx.spawn_child` works on both hosts. A native capability spawns any `Instanced`
native actor; a wasm component spawns its own **sibling** types — `Instanced` actors
its module also exports ([ADR-0097](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0097-wasm-sibling-spawn.md)). One wasm crate can export several
actor types (`export!(RootManager, Panel, …)`), and a running instance stands up a
sibling just as the listener stands up a session:
`ctx.spawn_child::<Panel>(Subname::Counter, &config)`. A component spawns within the
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

The two traits are deliberately mirror images. Both sit on the shared `Actor`
super-trait (which owns only `NAMESPACE`). Both have a `type Config`, the same
`init` / `wire` / `unwire` lifecycle with identical semantics, and the same
`#[actor]` / `#[handler]` authoring shape. The *only* differences are the context
type — `NativeCtx<'_>` instead of `WasmCtx<'_>` — and the host machinery underneath.
A native capability's `wire` looks like a component's `wire`:

```rust
#[actor]
impl NativeActor for AudioCapability {
    type Config = AudioConfig;
    const NAMESPACE: &'static str = "aether.audio";

    fn init(config: AudioConfig, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> { … }

    #[handler]
    fn on_note_on(&mut self, ctx: &mut NativeCtx<'_>, note: NoteOn) { … }
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
