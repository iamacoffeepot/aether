# Components & lifecycle

A **component** is an actor you load into a running engine. It's the unit an
agent writes and ships: a small Rust crate, compiled to wasm, dropped into a live
substrate with `load_component` and addressable by mail the moment it boots. This
page is how to author one — the trait you implement, the lifecycle stages it
moves through, how to send and subscribe from inside it, and how it gets loaded,
dropped, and hot-swapped.

> Governing ADRs: **ADR-0074** (the unified actor model — components and native
> capabilities are *one* primitive with two hosts), **ADR-0033** (the `#[actor]`
> macro and handler-driven inputs manifest), **ADR-0079** (the lifecycle stages),
> **ADR-0090** (typed boot config), **ADR-0022 / ADR-0038** (in-place hot-swap).
> The authoring surface here — the `#[actor]` block, `init`/`wire`/`unwire`,
> `export!` — is **stable**: it's the contract every reference component
> (`aether-camera`, `aether-mesh-viewer`) and example is built on. Where this
> page shows an exact signature it was read from the current SDK, not an ADR.

## What a component is (and isn't)

There's one actor model in the engine, not two (ADR-0074). A **capability** is a
native actor — Rust compiled into the substrate, hosting I/O and GPU and audio.
A **component** is a *loaded* actor — wasm today, instantiated at runtime behind
an FFI boundary. They share the same `Actor` super-trait, the same lifecycle
shape, the same mail surface, and address each other identically. The difference
is the host: a capability is linked at build time and trusted with raw I/O; a
component arrives as bytes, runs sandboxed in the wasm wall, and reaches the
outside world only by mailing capabilities. (See the [invariants
page](../foundations/invariants.md) on capability = reachability.)

So "writing a component" is the agent-facing path for extending the engine: you
add behavior without touching or rebuilding the substrate. The substrate stays a
thin host; the engine grows in wasm.

A component implements **`FfiActor`** (FFI = the loaded, foreign host). A native
capability implements `NativeActor`. Both sit on `Actor`, which owns only the
shared `NAMESPACE` const. You author the receive side with the `#[actor]` macro
on one `impl FfiActor for C` block, and emit the FFI shims with `export!`.

## The lifecycle: `init` → `wire` → handlers → `unwire`

A component moves through three authored stages (ADR-0079). Each gets a different
context, and the context is the contract — it's exactly what you're allowed to do
at that stage.

**`init` — the constructor. No mail.**

```rust
fn init<C>(ctx: &mut C) -> Result<Self, BootError>
where
    C: Resolver,
```

Runs once. Build and return the actor's initial state. The ctx is **`Resolver`
only**: you can resolve kind ids and mailbox addresses (`ctx.resolve::<K>()`,
`ctx.mailbox_id()`), but you *cannot send mail* — the actor's mailbox isn't
published yet, and peers may not exist. This is deliberate (ADR-0079): `init` is
a pure synchronous constructor. If startup can fail — a missing handle, an
unparseable config — return `Err(BootError::new("…"))` and the failure surfaces
to the loader as `LoadResult::Err { error }` instead of a half-built actor.

**`wire` — post-init, mail allowed.**

```rust
fn wire(&mut self, ctx: &mut FfiCtx<'_>) { … }
```

Runs after `init` returns `Ok` and the mailbox is published, before the first
inbound envelope is dispatched. Now the ctx is a full `FfiCtx`: you can send. This
is where mail-driven setup goes — **subscribe to input streams**, announce
yourself to peers, kick off a self-mail poll loop. The reference camera subscribes
here:

```rust
fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
    let me = MailboxId(ctx.mailbox_id());
    let input = ctx.actor::<InputCapability>();
    input.subscribe(Tick::ID, me);
    input.subscribe(WindowSize::ID, me);
}
```

Subscriptions belong in `wire`, not `init`, precisely because `init` can't mail.
Default is a no-op; override only if you have setup to do.

**handlers — steady state.** Between `wire` and shutdown the actor just receives
mail, one handler call per kind. Covered below.

**`unwire` — pre-shutdown, mail allowed.**

```rust
fn unwire(&mut self, ctx: &mut FfiCtx<'_>) { … }
```

Runs after the dispatcher drains the inbox, before the actor value drops. Full
`FfiCtx` again — final broadcasts, monitor signals, a flush all land. Mail to a
live peer is delivered; mail to a peer that's already gone warn-drops. (The older
`on_drop` hook was retired in favor of this single mail-allowed teardown stage.)
Default no-op.

## Writing the receive side: `#[actor]` and `#[handler]`

The `#[actor]` attribute goes on the one `impl FfiActor` block. Each `#[handler]`
method *is* a mail handler, and the macro infers the kind it handles **from the
method's third parameter** — no typelist, no `is::<K>()` dispatch:

```rust
#[actor]
impl FfiActor for Hello {
    const NAMESPACE: &'static str = "hello";

    fn init<C>(ctx: &mut C) -> Result<Self, BootError>
    where C: Resolver {
        Ok(Hello { pong: ctx.resolve::<Pong>() })
    }

    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
        ctx.actor::<InputCapability>().send(&SubscribeInput {
            kind: Tick::ID,
            mailbox: MailboxId(ctx.mailbox_id()),
        });
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

aether_actor::export!(Hello);
```

The third parameter `Ping` is the kind `on_ping` handles; `Tick` is the kind
`on_tick` handles. The macro reads those types and codegens the dispatch table
that matches an inbound envelope's kind id against each handler's `K::ID` (a
compile-time const — no runtime registration, no host round-trip to resolve the
address). It also emits the **`aether.kinds.inputs` custom section** in the wasm:
a manifest of every handler kind plus its doc, which is exactly what
`describe_component` reads back to tell a live engine what this component accepts.
Your doc comments — filtered through a `# Agent` section if you write one — ride
along into that manifest.

A handler takes the decoded mail **by value** (`ping: Ping`), not by reference;
the macro-generated trampoline owns the decoded payload. `&mut self` gives the
handler the actor's state, which is plain fields — no locks, because an actor is
single-threaded from its own view (see [concurrency](concurrency.md)).

**Strict by default; `#[fallback]` to catch the rest.** A component with no
fallback is a *strict receiver*: mail of a kind it doesn't handle is reported as
`DISPATCH_UNKNOWN_KIND` rather than silently swallowed. Add one catch-all if you
want to absorb everything else:

```rust
#[fallback]
fn on_other(&mut self, ctx: &mut FfiCtx<'_>, mail: Mail<'_>) { … }
```

The fallback takes the raw `Mail<'_>` (it doesn't know the kind statically) and
its presence is recorded in the manifest too, so introspection can tell a strict
receiver from a permissive one.

## `export!` — the FFI shims

```rust
aether_actor::export!(Hello);
```

This is **required** — without it the wasm has no FFI exports and the substrate
can't drive the actor. It emits the `#[no_mangle]` entry points the host calls
across the boundary (`init`, `wire`, `receive_p32`, `unwire`) plus the two custom
sections (`aether.kinds.inputs` and `aether.namespace`). You never write
`extern "C"` by hand.

The `_p32` suffix on the pointer-taking exports (`receive_p32`,
`on_rehydrate_p32`) is the dual-target FFI convention (ADR-0024): wasm32 addresses
are 32-bit, so those shims take `u32` pointers/lengths. The exports are
**wasm32-only** — a native (host) build of the same crate carries no FFI symbols,
which is why host-side tests of a component drive it through the in-process
transport rather than the FFI path. (See the [type system
page](../foundations/type-system.md) for how the kind vocabulary itself crosses
the boundary.)

## Addressing and sending from inside

You address another actor **by type**, and it resolves at compile time:

```rust
ctx.actor::<RenderCapability>().send(&Camera { view_proj });   // singleton, by type
```

`ctx.actor::<R>()` returns a typed mailbox handle for capability `R`, and
`.send::<K>(&payload)` compiles only if `R` actually handles `K` (`R:
HandlesKind<K>` — the macro emits one such impl per `#[handler]`). Both the
mailbox id and the kind id are compile-time consts, so there's no host round-trip
to resolve an address.

Other shapes:

- **Self-address:** `ctx.mailbox_id()` — your own mailbox, e.g. to hand to a
  subscription so the stream routes back to you.
- **Subscribe to an input stream:** `ctx.actor::<InputCapability>().subscribe(Tick::ID, me)`
  (the convenience on `InputMailboxExt`), or send `SubscribeInput { kind, mailbox }`
  directly. Subscriptions clear on drop and survive a hot-swap (the mailbox id is
  stable).
- **Reply to a request:** `ctx.reply_target()` returns the sender if the inbound
  mail had one; reply to it. A handler promises *nothing* about replies — replying
  is the handler's own business, not part of the kind contract (see the [mail
  page](mail-and-kinds.md)).
- **Name-keyed escape hatch:** when you only know a mailbox by string, resolve a
  `Mailbox<K>` token and send through it — used for peer addresses you learn at
  runtime (e.g. a `LoadResult.name`).

## Boot configuration

A component can take typed boot config (ADR-0090). Declare a `type Config` and
the chassis threads decoded bytes into `init`:

```rust
#[actor]
impl FfiActor for ProbeWithConfig {
    type Config = ProbeConfig;
    const NAMESPACE: &'static str = "probe_with_config";

    fn init<C>(config: ProbeConfig, ctx: &mut C) -> Result<Self, BootError>
    where C: Resolver { … }
}
```

Omit `type Config` and the macro synthesizes `type Config = ()` *and* injects the
unused `_config` argument, so a no-config component's `init` stays terse —
`fn init<C>(ctx: &mut C)`, exactly as `Hello` writes it above. The config rides
as raw bytes on `LoadComponent.config`: the hub / MCP edge encodes the config
struct to bytes (SDK-typed, not wire-typed), so the substrate stays
byte-transparent. A declared config kind shows up in the component's advertised
capabilities, so `describe_kinds` can resolve its schema.

## Loading, dropping, and the trampoline address

Lifecycle control is mail to the `aether.component` mailbox:

| kind | does | reply |
|---|---|---|
| `aether.component.load` | compile + instantiate the wasm, register its kinds, publish a mailbox | `LoadResult` |
| `aether.component.drop` | tear down a component, invalidate its mailbox id | `DropResult` |
| `aether.component.replace` | hot-swap the wasm behind a stable mailbox id | `ReplaceResult` |

`LoadResult::Ok` carries the assigned `mailbox_id`, the **resolved name** (so a
caller that omitted `name` learns the substrate-defaulted one), and the parsed
`ComponentCapabilities` (handlers, fallback, doc, config) read from the manifest.
A loaded component registers at **`aether.component.trampoline:NAME`** — that full
string is the address you send subsequent mail to. Bare names (`"player"`) are
*not* registered and warn-drop; always use the name from `LoadResult`.

In practice you drive this through the MCP harness — `load_component(engine_id,
binary_path, name?)`, `replace_component(...)`, `terminate_substrate(...)` — which
takes a *path* and reads the bytes for you (tool JSON never carries the wasm
buffer; the wire kind does). The component's kind vocabulary travels inside the
wasm's `aether.kinds` custom section (ADR-0028), so the loader declares nothing —
the substrate reads the types directly off the binary.

## Hot reload

Replacing a component's wasm *without* dropping its mailbox is opt-in. By default
a component isn't replaceable; to participate, impl the `Replaceable` subtrait and
emit with the `replaceable` flag:

```rust
impl Replaceable for MyComponent {
    fn on_replace<C>(&mut self, ctx: &mut C)
    where C: MailSender + Persistence {
        ctx.save_state_kind(1, &self.snapshot());   // hand state to the successor
    }

    fn on_rehydrate<C>(&mut self, ctx: &mut C, prior: PriorState<'_>)
    where C: OutboundReply {
        if let Some(snap) = prior.as_kind::<Snapshot>(Snapshot::ID) { … }
    }
}

aether_actor::export!(MyComponent, replaceable);
```

`aether.component.replace` (ADR-0022) freezes the target mailbox, **drains
in-flight mail through the old instance**, calls `on_replace` on it (its chance to
serialize state via the `Persistence` ctx), instantiates the new wasm module
**behind the same binding**, and calls `on_rehydrate` on the new instance with the
prior state bundle if one was saved. If the drain exceeds its timeout the replace
fails and the **old instance stays bound** — a failed swap is a no-op, not a
half-swapped actor. `ReplaceResult::Ok` carries the new component's capabilities
so the hub's cached view reflects the swapped binary.

The load-bearing property is **binding stability** (ADR-0038): the swap replaces
the wasm Module *in place* behind a stable mailbox handle, so the mailbox id, any
route cache, and existing input subscriptions all stay valid across the swap.
Peers mailing the component never learn it changed. Prefer
`save_state_kind` (which carries schema identity through the kind system) over the
raw `save_state` byte bundle unless you're persisting a non-kind blob or driving
an explicit migration.

## Where this fits

Writing a component is the standard way to extend the engine in wasm — new
behavior, no substrate rebuild. When the thing you need is genuinely native (it
must own a socket, a GPU resource, an audio device), that's a *capability*
instead, authored against `NativeActor` with the same `#[actor]` shape; the
[recipes](../recipes.md) cover adding one. The reference components to read are
`aether-camera` (multi-instance state, input subscriptions, a rich mail family)
and `aether-mesh-viewer` (fetch-and-cache, replay-every-tick).

## Where to read more

- The lifecycle and single-threaded-actor contract this builds on — [Invariants & guarantees](../foundations/invariants.md).
- How mail routes and what a kind is — [Mail, kinds & scheduling](mail-and-kinds.md).
- Why a handler must never block, and how to wait instead — [Concurrency & blocking](concurrency.md).
- Kinds, ids, and how the vocabulary crosses the wasm boundary — [The type system](../foundations/type-system.md).
- Loading and inspecting a live component — the `load_component` / `describe_component` MCP tools (`CLAUDE.md`).
