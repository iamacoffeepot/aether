# The actor model

The engine is built from one primitive: the **actor**. The renderer is an actor.
The audio mixer is an actor. A component you load is an actor. There is no second
kind of thing — no privileged "system objects" that actors orbit. Everything in
the substrate is an actor, and actors interact in exactly one way: by **mail**.

Understand the actor and most of the engine follows, because every subsystem in
this guide is "an actor (or a few) that does X." This page is the primitive
itself: what an actor is, the lifecycle every actor moves through, how you author
one, and the two *hosts* the same primitive runs under — native **capabilities**
and wasm **components**.

> Governing ADR: **ADR-0074** (the unified actor model — capabilities and
> components are one primitive, not two) with **ADR-0079** (the lifecycle stages)
> and **ADR-0033** (the `#[actor]` macro). This model is **stable**; it's the
> spine everything else hangs off. Signatures here were read from the current SDK
> (`aether-actor`) and runtime (`aether-substrate`).

## What an actor is

An actor is a bundle of **private state** and a set of **typed handlers**. It does
nothing until mail arrives; when an envelope lands, the handler registered for
that kind runs, with exclusive `&mut` access to the state. That's the whole shape:
state in, mail drives handlers, handlers mutate state and send more mail.

Two properties make it tractable to reason about:

- **Actors communicate *only* by mail.** No actor holds a reference into another
  actor's memory, calls another's methods, or shares a lock with it. The only way
  to affect another actor is to send it a kind it handles. This is what lets the
  same model span a trusted in-process capability and a sandboxed wasm component
  without either knowing which it's talking to — mail is the only coupling, so the
  *host* is an implementation detail. (See [capability = reachability](invariants.md)
  for the security consequence.)
- **An actor is single-threaded from its own perspective.** The scheduler
  guarantees that no two threads ever run one actor's handlers at once, so actor
  state is **plain fields** — no `Mutex`, no `RefCell`, no atomics. You write
  ordinary sequential Rust and the runtime makes it safe. (How the scheduler
  enforces this — the run-token — is the [concurrency](../systems/concurrency.md)
  page; here, just take it as the contract.)

What flows between actors — the kinds, their ids, the wire encoding — is the
[type system](type-system.md). How it routes and in what order — mailboxes, FIFO,
fire-and-forget — is [mail & scheduling](../systems/mail-and-kinds.md). This page
is the actor on the receiving end of all that.

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

`init` is a **pure synchronous constructor**: its ctx can resolve kind ids and
mailbox addresses but *cannot send mail*, because the actor's own mailbox isn't
published yet and peers may not exist. Anything mail-driven — above all,
subscribing to the tick or input streams — belongs in `wire`, which runs once
`init` returns `Ok` and the mailbox is live. If startup can fail (a missing
handle, an unparseable config), `init` returns `Err(BootError)` and the failure
surfaces cleanly rather than producing a half-built actor.

`wire` and `unwire` default to no-ops; override them only when you have setup or
teardown to do. (Both are mail-allowed; the older single `on_drop` hook was
folded into `unwire`.)

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

## One primitive, two hosts

Here's the part that ties the engine together. There aren't two actor systems —
there's **one primitive with two hosts**, differing only in where the actor's code
lives and what it's trusted with (ADR-0074):

- A **native capability** is an actor compiled *into* the substrate. It implements
  `NativeActor`, it's linked at build time, and it's trusted with raw I/O — the
  renderer, the audio mixer, the filesystem, the input streams, the
  component-loader itself are all capabilities. This is the chassis.
- A **component** is an actor *loaded at runtime* — wasm today — that runs
  sandboxed behind the wasm wall and reaches the outside world only by mailing
  capabilities. It implements `FfiActor`, and the substrate drives it through an
  FFI **trampoline**. This is the agent-facing extension path: new behavior with
  no substrate rebuild.

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
