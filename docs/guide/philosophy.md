# Why aether is shaped this way

Aether is not a conventional engine that happens to have an agent bolted on.
The agent-in-a-harness premise is upstream of almost every structural
decision. This page states the principles out loud, because once you hold
them the rest of the system stops looking arbitrary.

## 1. The agent is the operator

The motivating image is Claude sitting in a harness as assistant, engineer,
and designer — driving a running engine, observing it, and modifying it. That
is not a feature; it is the **load-bearing constraint**. A native **substrate**
owns the things only native code can own — I/O, the GPU, audio — and hosts a
WebAssembly runtime. Everything above the substrate is an **actor**, and the
agent reaches in from outside through a stable tool surface (MCP) to make
things happen.

Consequences that follow from taking this seriously:

- The control surface is **out-of-process and restartable** without dropping
  the agent's session (ADR-0089). The agent must be able to rebuild and
  relaunch the volatile backends mid-task and keep its connection.
- Observation is first-class. The agent can capture a frame, read an actor's
  logs, trace a mail chain, and inspect handles — because an operator that
  can't see can't act.

## 2. Everything is mail

Actors do not call each other. They **send mail** (ADR-0002). A piece of mail
is a typed payload (a *kind*) addressed to a *mailbox*. This is the single
most pervasive decision in the system, and it buys:

- **A uniform boundary.** The same mechanism carries a tick event, a draw
  command, an audio note, a file write, and a request to load a component.
  There is one thing to learn, not twelve.
- **Location independence.** A mailbox might be a native capability, a wasm
  component, or a peer on another process reached through the hub — the
  sender addresses it the same way. Mail bubbles up to the hub when it isn't
  local (ADR-0037).
- **Observability for free.** Because all interaction is mail, the tracing and
  settlement machinery (ADR-0080/0086) can watch *all* of it without each
  subsystem opting in.

Mail is **fire-and-forget by default.** A handler promises nothing about a
reply; if a reply matters, that's a separate, explicit contract — never an
implicit "every kind has a response."

## 3. The substrate is thin; the engine is actors

The native base layer is deliberately small. It owns I/O, GPU, audio, the
mail scheduler, and the wasm host — and little else. Game and tool behavior
lives **above** it, as actors (ADR-0034/0035/0073). Two kinds of actor, one
model (ADR-0074):

- **Native chassis capabilities** — render, audio, file I/O, input, the
  component loader, the handle store. Compiled into the substrate.
- **Wasm components** — your logic, loaded at runtime and hot-swappable.

They are the *same actor model*, addressed the same way. A component talks to
the renderer exactly as one native capability talks to another. The symmetry
is intentional and defended: prefer a design that treats wasm and native
uniformly over one that special-cases the target.

The chassis is **composed**, not monolithic — a builder assembles the
capabilities a given deployment needs (ADR-0070/0071), which is why there are
several chassis (desktop, headless, hub, test-bench) sharing one runtime.

## 4. Design for machine consumers

The surfaces are built to be legible to an LLM, which is not the same as
being legible to a human. Where human API design prizes terseness and DRY,
aether's surfaces prize being **regular, explicit, self-describing, and
repetition-tolerant**:

- Kinds carry their own schema (ADR-0031/0032) and ids are type-tagged on the
  wire (ADR-0064/0065), so a tool result can be handed back verbatim and a
  kind can describe itself.
- Prefer **explicit nulls over absent-field semantics** — every option
  addressed in a payload, because verbosity is nearly free for a machine
  caller and ambiguity is not.
- The vocabulary is introspectable live: an agent can ask the engine what
  kinds exist, what a component handles, what transforms are linked, what
  handles are stored.

## 5. ADRs are the memory; tutorials are the proof

Two habits keep the system honest as it grows:

- **Load-bearing decisions are recorded as ADRs** (ADR-0001) — numbered,
  reviewed like code, and cited from the code and from this guide. The ADR is
  the durable "why." When in doubt, the ADR wins over prose that has drifted.
- **Every callable surface ships a tutorial, and the tutorial is the sanity
  check.** If you cannot write a clean walkthrough that a fresh agent can
  build from, the surface is mis-shaped — fix the design, not the words. This
  guide is where those tutorials live, and writing them is how the API shapes
  get tested. (See the [recipes](recipes.md) section.)

---

These five hang together. Because the agent is the operator (1), interaction
has to be uniform, addressable, and observable — so everything is mail (2).
Because the agent both *uses* and *extends* the engine, the line between
native and wasm has to be thin and symmetric (3). Because the consumer is a
model, the surfaces are explicit and self-describing (4). And because all of
this drifts without discipline, decisions are recorded and tutorials prove the
surfaces stay sane (5).
