# Invariants & guarantees

An invariant is a property the engine holds true at all times — something you
can build on without re-checking, and something that, if violated, breaks code
that depended on it. This page is the scannable contract list: the load-bearing
guarantees, in one place, each grounded in the ADR that established it.

It splits in two:

- **What the engine guarantees you** — properties you can rely on. Build on
  them; don't defensively re-implement them.
- **What you must uphold** — obligations you owe in return. Violate one and you
  don't get a compile error — you get a *silent* failure: vanished mail, a hung
  reply chain, a garbage decode. Each entry names the tell so you can recognise
  it.

> Concurrency *enforces* several of these (single-threaded actors, ordering,
> no-blocking) but is not the same topic — the contracts live here; the
> machinery that makes them hold, and how to work with it, is
> [Concurrency & blocking](). Invariants are the *what*; concurrency is one of
> the *hows*.
>
> Maturity: the addressing, typing, and identity invariants are **stable** —
> they've held since the model's early ADRs and the wire format depends on them.
> Settlement is **stable as a contract** while its transport is still settling
> ([ADR-0086](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0086-decouple-settlement-from-trace.md)). Scheduler internals are **settling** ([ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md)) — but the
> concurrency *contracts* below are stable regardless of how dispatch is tuned.

## What the engine guarantees you

**Mail is the only channel between actors.** No actor holds a reference into
another actor's memory — no shared mutable state crosses the boundary, so a
peer's data can't be read or mutated directly ([ADR-0002](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0002-mail-first-architecture.md)). An actor's entire
interaction with the rest of the engine is the mail it sends and the mail it
receives. The *enforcement* differs by actor kind: a loaded component can't even
form such a reference (the sandbox gives it its own linear memory), while a
native capability — Rust in the same process — upholds it as a discipline (peer
references are bootstrap-only; handlers talk by mail, never by reaching into
sibling state). Same contract, a wall in one case and a rule in the other. This
is what makes hot-swap, sandboxing, and universal observability possible at all
— see [Mail, kinds & scheduling](../systems/mail-and-kinds.md).

**A mailbox is an address; a kind is a payload shape; they route
independently.** The mailbox decides *where* mail goes; the kind only describes
*what the bytes are*. They share a naming convention (`aether.audio` mailbox +
`.note_on` verb → `aether.audio.note_on` kind), but routing never consults the
kind. Hold these apart and most "my mail vanished" confusion disappears.

**Identity is name-derived and stable.** An actor carries two ids, each
deterministic ([ADR-0099](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0099-actor-identity-and-addressing.md)): its `ActorId` — *which actor* — is the FNV-1a 64
hash of its `NAMESPACE` (`hash(NAMESPACE:subname)` for an instanced actor), and
its `MailboxId` — *where it sits* — is a hash chain over its **lineage**, the
ordered ActorIds from the substrate root down to the actor. A root actor's
lineage is one node, so its `MailboxId` equals its `ActorId` — the name hash of
[ADR-0029](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0029-name-derived-mailbox-ids.md) is the depth-1 case of the fold. A `KindId` is a hash of the kind
name *plus its schema* ([ADR-0030](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0030-hashed-kind-ids.md)). Two consequences you can lean on:

- **Stable across processes.** Every id is computed from names and lineage,
  never assigned — `Kind::ID` and `mailbox_id_from_name` are compile-time
  constants, and the lineage fold (`fold_lineage`) is the same pure function on
  every substrate and guest — so two processes that hold the same names and the
  same lineage produce the same ids, and addressing works across a fleet
  without a resolution round-trip.
- **Stable across hot-swap.** Replacing a component in place changes neither
  its name nor its position under its host, so its lineage — and the
  `MailboxId` folded from it — is unchanged: senders and route caches survive
  the swap ([ADR-0029](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0029-name-derived-mailbox-ids.md); the `replace_component_preserves_mailbox_identity`
  scenario guards it).

One sharp edge follows from the lineage fold: a `/`-rendered address
(`aether.component/aether.embedded:camera`) resolves by parsing it into
segments and folding their ActorIds (`mailbox_id_from_path`). Hashing the
joined string as a flat name yields an id the registry never registered, and
mail to it warn-drops — the string is a rendering of the lineage, never the
hash input.

**A kind id encodes its shape — drift fails loud, not silently.** The `KindId`
hashes `name + schema`, where `name` is the kind's *declared* name
(`#[kind(name = "…")]`) and `schema` is its *structural shape* — field types and
positions only. The Rust type name and the field names are deliberately erased
from the hash ([ADR-0031](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0031-const-constructible-schema-representation.md)/[ADR-0032](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0032-canonical-schema-bytes-and-labels-sidecar.md)), so they're not what's being compared. What that
buys: a producer emitting `Thing { a }` and a consumer expecting
`Thing { a, b }` compute *different* ids, so the stale mail lands on "kind not
found" and can never silently garbage-decode into the wrong shape ([ADR-0030](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0030-hashed-kind-ids.md)).
The fix for a mismatch is always "recompile both sides," and the failure points
straight at it. (Exactly which edits move the id — and which, like a field
rename, leave it untouched — is the type system's story: [The type system]().)

**A kind is self-describing.** Every kind carries a schema describing its bytes
([ADR-0005](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0005-mail-typing-system.md)/[ADR-0031](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0031-const-constructible-schema-representation.md)), so the wire layer can encode it from JSON and a recipient can
decode it without a shared header — and a live engine can be asked what kinds
exist (`describe_kinds`). Payload bytes stay opaque in transit: nothing between
sender and the addressed handler needs to understand them ([ADR-0019](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0019-unified-mail-encoding.md)).

**An actor is single-threaded from its own perspective.** An actor's handlers
never run concurrently with each other, so its state is plain fields — no
`Mutex`, no `RefCell`, no atomics for actor-local state. This holds even though
actors do *not* each own a thread: they're multiplexed onto a shared
work-stealing scheduler, and a run-token guarantees only one worker runs a given
actor at a time ([ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md)). The property is achieved by scheduling, not by a
dedicated thread.

**Per-recipient FIFO; cross-recipient unordered.** Mail from the same sender to
the same recipient arrives in send order. Mail to *different* recipients carries
no ordering guarantee — each send is an independent async call ([ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md)). This
is the single contract most likely to bite under load; the *what you must
uphold* section restates it as a rule.

**Settlement is exact.** A traced chain of mail is reported `Settled` exactly
when `in_flight == 0 && held_open == 0` — the counter does not transiently reach
zero with work still coming, so `Settled` fires once and is a fact, not a hint
([ADR-0080](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0080-substrate-mail-tracing-and-settlement.md) §6, as amended; [ADR-0086](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0086-decouple-settlement-from-trace.md)). This is what lets `send_mail_traced` know a
request is *fully* done rather than guessing with a timeout. It depends on the
hold contract below being honoured.

**Capability equals reachability — with no privileged side-channel.** What an
actor can *do* is exactly what it can *mail*; there is no back door, not even for
the agent driving the engine. Gating a build is just choosing which mailboxes
are registered. The payoff is the point: because every function is reachable as
mail, you can interpose anywhere — address any mailbox, watch any exchange,
inject a message mid-flow to reproduce a state ([ADR-0002](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0002-mail-first-architecture.md)). Universal
reachability is what makes the running engine inspectable.

## What you must uphold

These are the obligations. Each one, violated, produces a silent failure — so
each lists its **tell**.

**Never block in a handler.** No sleeping, blocking I/O, blocking lock/channel
wait, or busy-spin. Scheduling is cooperative: a blocked handler pins a worker
doing nothing and can deadlock a reply chain (the actor you're waiting on may
have *its* next mail queued behind you). Await a reply through the framework, or
hand blocking/async work off the actor thread (a computation DAG, or the
sanctioned spawn primitives). *Tell:* a hang with no progress and, often, no
actor named — the worst diagnostic shape the runtime offers. Full treatment in
[Concurrency & blocking]().

**Address exactly — bare and unknown names warn-drop.** A bare name (`"camera"`,
`"player"`) or a kind name used as a recipient (`aether.audio.note_on` as an
*address*) matches no registered mailbox and is dropped with a warning, not an
error. Use the full address: `aether.<name>` for chassis mailboxes, the
`LoadResult.name` (`aether.component/aether.embedded:NAME`) a loaded component hands
back. *Tell:* mail seems to vanish; nothing handles it. Check the address first.

**Don't encode cross-actor sequence by send order.** Per-recipient FIFO is the
*only* ordering you get. If B must happen after A across different actors, make
it **causal** — have A's handler trigger B, or have one actor receive both and
sequence them itself. *Tell:* code that passes in dev and reorders under load,
because a second recipient happened to run first.

**Don't assume a reply.** Mail is fire-and-forget; a handler promises *nothing*
about a reply (there is no `Kind::REPLY`). If a reply matters it's a separate,
explicit contract between two kinds, not an implicit "every kind has a
response." *Tell:* a caller that blocks or waits forever on a reply the handler
never agreed to send.

**Hold the chain open across a deferred reply.** If a handler kicks off work
that will reply in a *later* turn (a slow provider call whose result lands after
the handler returns), it must keep a settlement hold on the root until that last
send — otherwise the chain settles early and any waiter is told "done" before
the real reply arrives ([ADR-0080](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0080-substrate-mail-tracing-and-settlement.md) §12; [ADR-0093](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0093-hold-until-resolve-dispatch-primitive.md)). Synchronous in-handler replies
need nothing (their `Sent` precedes their `Finished`); the sanctioned offload
primitives hold automatically. A hand-rolled drain that consumes an owned
dispatch must record `Finished` for the inbound mail id, or settlement never
fires ([ADR-0094](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0094-settlement-obligation-guard.md)). *Tell:* a multi-second hang to the settlement/MCP timeout with
no actor named.

**On a kind's schema change, recompile both sides.** A schema edit changes the
`KindId`. Until the peer is rebuilt, its `Kind::ID` no longer matches what you
registered and its mail lands on "kind not found" ([ADR-0030](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0030-hashed-kind-ids.md)) — and prebuilt
component wasm carries the *old* id, so rebuild it after any chassis-side kind or
mailbox-name change. *Tell:* "observed kinds: []", or mail that used to route
now missing after one side changed.

**Cap recursion on unbounded data.** Load-bearing code that recurses on
user-controlled or geometrically-derived data must enforce a depth/budget cap
that returns an error rather than overflowing the stack; prefer an explicit
work-stack for anything whose depth could exceed a few hundred frames
(`CLAUDE.md`). Recursion bounded by a small structural input (a parse/AST walk)
is fine. *Tell:* a stack-overflow abort on adversarial or pathological input.

## Where to read more

- The mechanics behind the concurrency invariants —
  [Concurrency & blocking]().
- How kinds, mailboxes, handles, and ids fit together —
  [The type system]().
- The mail spine these all hang on —
  [Mail, kinds & scheduling](../systems/mail-and-kinds.md).
- Settlement and tracing in depth — [Tracing & settlement](../systems/tracing-and-settlement.md).
