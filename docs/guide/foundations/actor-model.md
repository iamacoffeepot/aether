# The actor model

The engine is built from one kind of thing: the **actor**. The renderer, the audio
mixer, the filesystem, a component you load — all of them are actors, with no
privileged class of "system object" sitting above them. Everything in the substrate
is an actor, and the only way actors ever interact is by **mail**.

If you understand actors, most of the engine follows: every subsystem in this guide
is some actor (or a handful) doing a job. This page covers the actor itself — what
it is, the lifecycle it moves through, how you write one — and the two *hosts* it
runs under, native **capabilities** and wasm **components**.

> Governing ADR: **ADR-0074** (the unified actor model — capabilities and
> components are one model, not two) with **ADR-0079** (the lifecycle stages)
> and **ADR-0033** (the `#[actor]` macro). This model is **stable**; it's the
> spine everything else hangs off. Signatures here were read from the current SDK
> (`aether-actor`) and runtime (`aether-substrate`).

## What an actor is

An actor is some **private state** paired with a set of **typed handlers**. It sits
idle until mail arrives; when an envelope lands, the handler registered for that
kind runs with exclusive `&mut` access to the state, updating it and sending mail
of its own. Nothing happens except in response to a message.

Two properties make it tractable to reason about:

- **Actors communicate *only* by mail.** No actor holds a reference into another
  actor's memory, calls another's methods, or shares a lock with it. The only way
  to affect another actor is to send it a kind it handles. This is what lets the
  same model span a trusted in-process capability and a sandboxed wasm component
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
(ADR-0079). Each stage gets a different context, and the context *is* the
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
peers may not have booted, and it returns `Result<Self, BootError>` so a failure
aborts the load cleanly. Sending mail from there would mean announcing yourself
before you're addressable, or mailing a peer that doesn't exist yet. So `init` stays
a pure synchronous constructor — resolve kind ids and mailbox addresses, assemble
state, return it (or fail with a `BootError` that surfaces instead of leaving a
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

Host matters as well as stage. Resolving, sending, and replying are common to both,
but a few operations live on one side only: a native capability can spawn child
actors and shut itself down, while a wasm component currently can't — its lifetime is
driven from outside, by load, drop, and replace. The concrete context types differ by
host too — `FfiCtx` in a component, `NativeCtx` in a capability — but you write
handlers against a shared set of capability traits (`Resolver`, `MailSender`, and
friends), so the same body works on either.

## Authoring an actor

You declare the receive side with the **`#[actor]`** attribute on one `impl`
block, and each **`#[handler]`** method *is* a handler — the macro infers the kind
it handles from the method's **third parameter**:

```rust
#[actor]
impl FfiActor for Hello {
    const NAMESPACE: &'static str = "hello";

    fn init<C>(ctx: &mut C) -> Result<Self, BootError>
    where C: Resolver {
        Ok(Hello { pong: ctx.resolve::<Pong>() })
    }

    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        ctx.actor::<InputCapability>().subscribe(Tick::ID, MailboxId(ctx.mailbox_id()));
    }

    #[handler]
    fn on_tick(&mut self, ctx: &mut FfiCtx<'_>, _tick: Tick) {
        ctx.actor::<RenderCapability>().send(&TRIANGLE);   // draw every tick
    }

    #[handler]
    fn on_ping(&mut self, ctx: &mut FfiCtx<'_>, ping: Ping) {
        if let Some(sender) = ctx.reply_target() {
            ctx.reply_kind(sender, self.pong, &Pong { seq: ping.seq });
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
impl FfiActor for ProbeWithConfig {
    type Config = ProbeConfig;
    const NAMESPACE: &'static str = "probe_with_config";

    fn init<C>(config: ProbeConfig, ctx: &mut C) -> Result<Self, BootError>
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
`"hello"`, `"camera"`, `"aether.audio"` in the examples above. Names are how actors
get reached: a mailbox id is just a compile-time hash of the name
(`mailbox_id_from_name`), so `ctx.actor::<RenderCapability>()` resolves to an address
with no runtime lookup — the type carries its `NAMESPACE`, the hash is a const, and
the send lands at the right mailbox.

What the name maps to depends on the host. For a **capability** the `NAMESPACE` *is*
the mailbox — `aether.audio`, `aether.render`, `aether.input` — claimed by the
chassis at boot, so addressing one by type reaches it directly. For a **component**
the `NAMESPACE` is only the *default load name*: once loaded, the actor is registered
at `aether.component.trampoline:<name>` (the name `LoadResult` hands back, which is
the `NAMESPACE` unless the load overrode it). You reach a loaded component through
that resolved name — `ctx.loaded::<Camera>("camera")`, or the `LoadResult.name`
string — not by hashing its bare `NAMESPACE`.

## One or many: cardinality

An actor type is either **singleton** or **instanced**, marked by the `Singleton` or
`Instanced` trait, and that choice decides how its name and addressing work.

A **singleton** is one of a kind: at most one instance per `NAMESPACE`, and the
`NAMESPACE` is its whole name. Capabilities are always singletons, and a component
loaded at its default name is one too. You address it by type, `ctx.actor::<R>()`.

An **instanced** actor is one of many sharing a prefix. Its `NAMESPACE` is that
prefix, and each live instance has a full name `NAMESPACE:subname` — for example
`aether.net.session:42`. The case that drives this is sockets: a singleton listener
accepts connections and, for each one, spawns a session actor with `ctx.spawn_child`
(ADR-0079); you then reach a specific instance by its subname,
`ctx.resolve_actor::<SessionActor>("42")`. Spawning children this way is a native
capability's job — a wasm component is itself a singleton, loaded rather than spawned.

## One model, two hosts

Here's the part that ties the engine together. There aren't two actor systems —
there's **one model with two hosts**, differing only in where the actor's code
lives and what it's trusted with (ADR-0074):

- A **native capability** is an actor compiled *into* the substrate. It implements
  `NativeActor`, it's linked at build time, and it's trusted with raw I/O — the
  renderer, the audio mixer, the filesystem, the input streams, the
  component-loader itself are all capabilities. This is the chassis.
- A **component** is an actor *loaded at runtime* as a wasm module, run sandboxed
  behind the wasm wall and reaching the outside world only by mailing capabilities.
  It implements `FfiActor`, and the substrate drives it through an FFI
  **trampoline**. This is the agent-facing extension path: new behavior with no
  substrate rebuild.

The two traits are deliberately mirror images. Both sit on the shared `Actor`
super-trait (which owns only `NAMESPACE`). Both have a `type Config`, the same
`init` / `wire` / `unwire` lifecycle with identical semantics, and the same
`#[actor]` / `#[handler]` authoring shape. The *only* differences are the context
type — `NativeCtx<'_>` instead of `FfiCtx<'_>` — and the host machinery underneath.
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
another. This symmetry is the point of ADR-0074: one mental model, one macro, one
lifecycle, and components get to reuse every pattern capabilities use.

So **start here, with the actor**, and the two host pages are just specializations:

- The wasm/FFI host — the trampoline, `export!`, loading, hot-swap — is
  [Components & lifecycle](../systems/components.md).
- Adding a native capability is a recipe ([Recipes](../recipes.md)); it's the same
  `#[actor]` shape against `NativeActor`.

## Where to read more

- What flows between actors — [The type system](type-system.md).
- The rules the model guarantees (ordering, fire-and-forget, capability =
  reachability, single-threaded) — [Invariants & guarantees](invariants.md).
- How mail routes and in what order — [Mail, kinds & scheduling](../systems/mail-and-kinds.md).
- How the scheduler keeps an actor single-threaded, and what to do instead of
  blocking — [Concurrency & blocking](../systems/concurrency.md).
- The wasm host in depth — [Components & lifecycle](../systems/components.md).
