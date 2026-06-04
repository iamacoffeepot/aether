# Introduction

Aether is an application engine — built for games, and **driven by an
agent**. The substrate underneath is general: it hosts whatever real-time,
interactive software runs on it (a game, a tool, a server), and games are the
motivating target rather than a baked-in assumption — so don't expect a fixed
game loop to be load-bearing. What *is* baked in everywhere is **who operates
it**: the one who spawns engines, loads components, sends mail, authors
content, and extends the codebase is Claude, sitting in a harness. Humans are
welcome, but the surfaces are shaped for a machine consumer first.

This guide is the narrative companion to that codebase. It is written for
the agent that has to *do something* with aether and wants to understand
the system well enough to extend or reuse it, not just call it.

## What this guide is (and isn't)

Aether already has two kinds of agent-facing documentation:

- **`CLAUDE.md`** — reference: the facts, conventions, commands, and the
  operational surface (what to mail where). Loaded into context every
  session. Terse by design.
- **ADRs** (`docs/adr/NNNN-*.md`) — the decision record: *why* each
  load-bearing choice was made, in the order it was made. Authoritative,
  but raw and chronological.

This guide is the missing middle: a **digested, navigable explanation** of
how the system works and how to build with it. It synthesizes the ADRs into
per-subsystem explainers, states the design philosophy out loud, and
collects the worked "how to do X here" recipes. Where a section makes a
claim about a subsystem, it cites the ADR that governs it — read the ADR
for the authoritative detail; read the guide to understand the shape.

## How it's organized

- **[Design & philosophy](philosophy.md)** — the load-bearing principles.
  Why aether is mail-first, why the substrate is thin, why every surface is
  built for a machine reader. Read this first; the rest of the system makes
  sense in its light.
- **[Architecture overview](architecture.md)** — the system map. The crates,
  the substrate/chassis/capabilities split, the actor model, how mail flows
  end to end, and how an agent reaches the engine through MCP.
- **[The systems](systems.md)** — per-subsystem explainers: what each one is
  for, what you mail it, and how to extend or reuse it.
- **[Building with aether](recipes.md)** — recipes: worked, copy-able
  walkthroughs for the recurring multi-file dances (adding a kind, a config
  knob, a capability, a component).
- **[Reference](reference.md)** — where to go for the authoritative detail.

## A note on staleness

This guide carries file paths and symbol names, so it can drift as the code
moves. Two defenses: every page cites the ADR it is digesting (the ADR is
the durable source of truth), and recipes point at a **real in-tree example**
rather than freezing a code snippet that rots. If a page names a file,
function, or kind that no longer exists, trust the code and the ADR over the
prose — and fix the page.

## Why a guide at all: the dogfood premise

There's a second reason this guide exists, beyond helping an agent navigate.
Aether's policy is that **every feature with a callable surface ships a short
tutorial, and the tutorial _is_ the API sanity check** — a dual pass: a human
reads it and the API looks sane, and an agent reads it and can build on the
API from the tutorial alone. If either fails, the API is mis-shaped — and the
fix is the design, not the prose.

So this guide is also a forcing function. An API you can't write a clean,
followable explanation for is telling you something. Writing these pages is
how we find out whether the surfaces we've built are actually coherent.
