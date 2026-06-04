# Mail, kinds & scheduling

This is the spine the rest of the engine hangs on. Actors don't call each
other — they send **mail**. A piece of mail is a typed payload (a *kind*)
addressed to a *mailbox*, and the scheduler runs the handlers. Understand this
page and every other subsystem reads as "an actor that receives some kinds."

> Governing ADRs: **ADR-0002** (mail-first actor model), **ADR-0005** (mail
> typing), **ADR-0019** (unified encoding), **ADR-0087** (blob dispatch + the
> ordering spine). The mail/kind model is **stable**; the *scheduler internals*
> (how work is batched and balanced across threads) are **settling** — this
> page documents the stable contract and defers the guts to ADR-0087.

## Why it exists

Aether could have let subsystems share memory and coordinate through a
scheduler. It deliberately doesn't (ADR-0002). One uniform message boundary
buys four things the project treats as non-negotiable:

- **Hot-swap.** A component can be unloaded and reloaded mid-run without the
  host contortions shared state would require — because nothing else holds a
  pointer into it; it only receives mail.
- **Sandboxing.** Agent-authored code can't be trusted with shared memory. Mail
  is the only channel, so a component's reach is exactly the mailboxes it can
  address.
- **Capability equals reachability.** There is no privileged side-channel — not
  even for Claude. What an actor can do is what it can mail. Gating a build is
  just choosing which mailboxes are registered.
- **Location independence.** A recipient can't tell whether mail came from a
  sibling actor in the same process or from Claude over the MCP wire. The
  "in-process vs out-of-process" question dissolves into a transport choice.

The one discipline this demands: **actors are subsystem-sized, not
entity-sized** (ADR-0002). A physics actor owns the whole physics world; inside
it, state is plain data and iteration is a tight loop. "One actor per entity"
would turn every interaction into per-frame mail and is explicitly *not* what
the model authorizes.

## What it does

**Mailbox vs kind — the distinction to internalize first.** The *mailbox* is
the address; the *kind* is the payload shape. They are independent even when
they share a name prefix: you send the kind `aether.audio.note_on` to the
mailbox `aether.audio`. Getting these two confused is the single most common
early mistake.

**A kind is a typed, self-describing payload.** It's a Rust type deriving
`Kind` + `Schema` with a `#[kind(name = "…")]`, carrying a stable hashed
`KindId` and a schema that describes its bytes (ADR-0005/0031). Because a kind
carries its own schema, the wire layer can encode it from JSON and a recipient
can decode it without a shared header — and an agent can ask a live engine what
kinds exist (`describe_kinds`).

**Encoding is schema-driven** (ADR-0019). On the way in, params are encoded to
wire bytes against the kind's schema; the recipient decodes those bytes back
into the kind per-kind. The bytes stay opaque until the addressed handler
decodes them — nothing in the middle needs to understand the payload.

**Mail is fire-and-forget by default.** A handler promises *nothing* about a
reply. If a reply matters, that's a separate, explicit contract between the two
kinds — never an implicit "every kind has a response." Don't write a caller
that blocks waiting for a reply a handler never agreed to send.

**The ordering spine** (ADR-0087, the contract you can rely on):

> Same recipient, same sender → handled in the order you sent them
> (per-recipient FIFO). Different recipients → **no** ordering guarantee; each
> send is async, like a server call.

That's the whole guarantee. Cross-recipient sequencing is *causal* — B happens
after A because A's completion triggered it or A mailed B — never inferred from
the order you happened to send things. Strict cross-recipient order is
deliberately not a contract: it would let one slow recipient stall every later
one in a fan-out.

**Scheduling, at the level you can depend on.** Each actor is single-threaded
*from its own perspective* — its handlers never run concurrently with each
other, so actor state is plain fields (no locks, no `RefCell`). The scheduler
runs handlers cooperatively across a thread pool; a long handler ties up only
its own worker and blocks nobody else's work. *How* mail is batched and
balanced across workers (the per-producer rings, the work-stealing pool, the
blob-as-unit-of-dispatch) is the part still settling — read ADR-0087 for the
current design, but don't build on its internals; build on the spine above.

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
