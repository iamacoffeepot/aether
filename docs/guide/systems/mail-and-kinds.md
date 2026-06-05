# Mail, kinds & scheduling

> **Governing ADRs:** [ADR-0002](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0002-mail-first-architecture.md) (mail-first actor model), [ADR-0005](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0005-mail-typing-system.md) (mail
> typing), [ADR-0019](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0019-unified-mail-encoding.md) (unified encoding), [ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md) (blob dispatch + the
> ordering spine). The mail/kind model is **stable**; the *scheduler internals*
> (how work is batched and balanced across threads) are **settling** — this
> page documents the stable contract and defers the guts to [ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md).

This is the spine the rest of the engine hangs on. Actors don't call each
other — they send **mail**. A piece of mail is a typed payload (a *kind*)
addressed to a *mailbox*, and the scheduler runs the handlers. Understand this
page and every other subsystem reads as "an actor that receives some kinds."

**The cast, in one breath.** Everything in the engine is an **actor** — a unit
that owns some state and talks only by mail. Two kinds: a **capability** is a
*native* actor compiled into the substrate (render, audio, file I/O, and the
rest), and a **component** is an actor *loaded at runtime* across the engine's
FFI boundary (the `FfiActor` ABI) — your logic, and the thing that gets
hot-swapped. The split is built-in-native vs loaded-guest, **not a specific
language**: the FFI boundary is target-agnostic by design, and WASM is simply
the only guest target wired up today. So a component is WASM *right now*, but a
C / C++ / other guest that speaks the ABI is the same category. All of them are
one actor model and address each other identically. When this page says
"component" it means a loaded FFI actor; "actor" means either kind. Deep dive:
[Components & lifecycle]().

## Why it exists

Aether could have let subsystems share memory and coordinate through a
scheduler. It deliberately doesn't ([ADR-0002](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0002-mail-first-architecture.md)). One uniform message boundary
buys four things the project treats as non-negotiable:

- **Hot-swap.** A component can be unloaded and reloaded mid-run without the
  host contortions shared state would require — because nothing else holds a
  pointer into it; it only receives mail.
- **Sandboxing.** Agent-authored code can't be trusted with shared memory, so it
  runs as a loaded **component** whose only channel is mail — its reach is
  exactly the mailboxes it can address, nothing more. The current guest target,
  WASM, backs that with a real memory sandbox (its own linear memory, traps
  contained), which is a large part of why WASM is the target wired up first.
- **Capability equals reachability — and so does observability.** There is no
  privileged side-channel, not even for Claude: what an actor can do is exactly
  what it can mail, so gating a build is just choosing which mailboxes are
  registered. The payoff on the flip side is the point, not a side effect —
  because *every* function in the engine is reachable as mail, you can interpose
  anywhere in the stack: address any mailbox, watch any exchange, inject a
  message mid-flow to reproduce or debug a state. Universal reachability is what
  makes the running engine inspectable by the agent driving it, and it is a
  primary reason the model is shaped this way.
- **Location independence.** Transport is a choice the recipient doesn't have
  to reason about: mail from a sibling actor in the same process and mail from
  Claude over the MCP wire run the same handler path, so the "in-process SDK vs
  out-of-process server" question dissolves ([ADR-0002](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0002-mail-first-architecture.md)/[ADR-0006](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0006-external-mail-transport.md)). A handler isn't
  fully blind to provenance — it can see a reply target and sender lineage
  ([ADR-0083](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0083-mail-sender-lineage.md)) — but the guarantee is that correctness and capability never
  *depend* on where mail came from. (Pinning down the exact origin, e.g. for a
  security policy, isn't first-class today; it's a plausible future.)

**A note on granularity.** Actors span a wide range of sizes. A coarse actor
can own a whole subsystem — the entire physics world, state as plain data and a
tight inner loop. A fine one can be a single instance: **instanced** actors are
a first-class category (cardinality — `Singleton` vs `Instanced` — is its own
axis, [ADR-0079](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0079-instanced-actors-as-a-first-class-category.md)), and the blob dispatcher ([ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md)) is built to rip through
large sets of mail, so fan-out across many small actors is cheap, not the
performance trap an earlier design assumed. [ADR-0002](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0002-mail-first-architecture.md)'s "subsystem-sized, never
per-entity" rule predates both and is superseded on this point.

When do you split into instances versus manage multiplicity inside one actor?
Reach for an instanced actor when each instance needs its own lifecycle and
isolation — one actor per TCP session is the canonical case (a `NetCapability`
listener spawns a `SessionActor` per connection, each able to drop
independently); per-session game logic and per-monster AI are the same shape.
Keep it a single actor when the multiplicity is just internal state — the
camera component drives many cameras from one actor, no per-camera mailbox
needed.

Fine granularity is cheap because actors **don't each own a thread** — they're
multiplexed onto a shared work-stealing scheduler ([ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md)), and the run-token
that lets only one worker run a given actor at a time is what gives you the
single-threaded-per-actor property. So thousands of instanced actors cost mail
and state, not threads. The rare exception is blocking I/O: an actor that must
make a blocking call may spawn a dedicated thread for *that* (a listener's
accept loop, a blocking provider call), but that's specific to blocking I/O,
not how actors run in general — see the no-blocking note under *Scheduling*.

## What it does

**Mailbox vs kind — the distinction to internalize first.** The *mailbox* is
the address (where mail goes); the *kind* is the payload shape (what the mail
is). They're independent — but they share a naming convention, and that's the
source of the confusion: a kind name is usually its mailbox's prefix plus a
verb (`aether.audio` + `.note_on` → `aether.audio.note_on`). So the kind reads
like a *more specific mailbox*, and the reflex is to use it as the address. The
trap, concretely: sending kind `aether.audio.note_on` to recipient
`aether.audio.note_on` instead of to `aether.audio`. No mailbox has that name,
so it silently warn-drops (see the addressing rules below). The mail routes by
mailbox; the kind only describes the bytes. When something you sent seems to
vanish, this is the first thing to check.

**A kind is a typed, self-describing payload.** It's a Rust type deriving
`Kind` + `Schema` with a `#[kind(name = "…")]`, carrying a stable hashed
`KindId` and a schema that describes its bytes ([ADR-0005](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0005-mail-typing-system.md)/[ADR-0031](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0031-const-constructible-schema-representation.md)). Because a kind
carries its own schema, the wire layer can encode it from JSON and a recipient
can decode it without a shared header — and an agent can ask a live engine what
kinds exist (`describe_kinds`).

**Encoding is schema-driven** ([ADR-0019](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0019-unified-mail-encoding.md)). On the way in, params are encoded to
wire bytes against the kind's schema; the recipient decodes those bytes back
into the kind per-kind. The bytes stay opaque until the addressed handler
decodes them — nothing in the middle needs to understand the payload.

**Mail is fire-and-forget by default.** A handler promises *nothing* about a
reply. If a reply matters, that's a separate, explicit contract between the two
kinds — never an implicit "every kind has a response." Don't write a caller
that blocks waiting for a reply a handler never agreed to send.

**The ordering spine** ([ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md)) — **the single contract to hold in your head
when writing handlers.** Get it wrong and you write code that passes in dev and
breaks under load:

> **Same sender, same recipient → arrives in send order** (per-recipient FIFO).
> **Different recipients → no ordering guarantee** — each send is an independent
> async call, like hitting a server.

In practice: if you `send(A)` then `send(B)` to the *same* actor, B is handled
after A — rely on it. If you send A to actor X and B to actor Y, you know
*nothing* about their relative order; Y might finish before X starts. So
**never encode a cross-actor sequence by send order.** When B must happen after
A across actors, make it **causal**: have A's handler trigger B, or have the
actor that needs the order receive both and sequence them itself. Strict
cross-recipient ordering is deliberately not offered — it would let one slow
recipient stall everything queued behind it in a fan-out.

**Scheduling, at the level you can depend on.** Each actor is single-threaded
*from its own perspective* — its handlers never run concurrently with each
other, so actor state is plain fields (no locks, no `RefCell`). The scheduler
runs handlers cooperatively across a thread pool.

**A handler must never block its thread.** No sleeping, no blocking I/O, no
waiting on a blocking lock or channel, no busy-spin. Because scheduling is
cooperative, a blocked handler doesn't just stall itself — it pins a worker
doing nothing and can deadlock a reply chain (the actor you're waiting on may
have *its* next mail queued behind you). A long *compute* handler is tolerated
(it ties up only its own worker), but anything that *waits on the outside
world* is not: await a reply through the framework so the scheduler can run
other work meanwhile, or hand heavy/async work to a computation DAG, off the
actor thread entirely. The full treatment — the await/hold primitives and how
to reason about concurrency here — is its own topic (forthcoming). The rule to
carry now: **don't block in a handler.**

*How* mail is batched and balanced across workers (the per-producer rings, the
work-stealing pool, the blob-as-unit-of-dispatch) is still settling — read
[ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md) for the current design, but build on the contracts above, not its
internals.

## How to use it

**From an agent (over MCP).** `send_mail` takes
`{engine_id, recipient_name, kind_name, params}` — `recipient_name` is the
mailbox, `kind_name` is the payload shape, `params` is JSON encoded against the
kind's schema. For a batch that must settle as one traced unit, use
`send_mail_traced`. The authoritative tool surface is in `CLAUDE.md`.

**From inside a component.** Address another actor *by type* —
`ctx.actor::<RenderCapability>().send(&kind)` — or hold a `Mailbox<K>` token.
`Kind::ID` and `mailbox_id_from_name` are compile-time constants, so there's no
host round-trip to resolve an address. You receive mail with a
`#[handler] fn on_x(&mut self, ctx, mail: K)` — the kind is inferred from the
third parameter (see [Components & lifecycle]() and the *Writing a component*
recipe).

**Addressing rules that bite if ignored:**

- Chassis mailboxes live under `aether.<name>` (`aether.render`, `aether.fs`,
  `aether.audio`, `aether.input`, `aether.window`, `aether.component`,
  `aether.handle`).
- A loaded component registers at `aether.component.trampoline:NAME` — use the
  full address `LoadResult.name` hands back.
- **Bare names** (`"camera"`, `"player"`) are not registered and warn-drop
  silently. If mail seems to vanish, check the address first.

## How to extend or reuse it

The mail spine is the thing you extend *through*, so most extension is "teach
the system a new kind" or "stand up a new mailbox":

- **A new message shape →** add a kind. See the *Adding a substrate kind*
  recipe: define the type in the right kind crate, derive `Kind`/`Schema`,
  register it in the inventory, surface it on the MCP wire.
- **A new mailbox →** stand up an actor to own it. A native one is a chassis
  capability (the *Adding a chassis capability* recipe); a wasm one is a
  component (the *Writing a component* recipe). Either way it's the same actor
  model and the same addressing.
- **Reuse the streams** rather than polling. Tick / key / mouse / window-size
  are publish-subscribe — subscribe from a component's `wire` hook and you get
  the events as mail. See [Input, file I/O & audio]().

Because everything is mail, these few moves compose: a new capability that
handles a new kind and publishes another is the whole vocabulary of building on
aether.
