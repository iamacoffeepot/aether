# Recipes

A recipe is a worked, copy-able walkthrough for one of the recurring
multi-file dances in aether — "how do I add X here, the blessed way, with the
gotchas inline." Recipes are the middle ground between reference (facts) and a
skill (a deterministic workflow an agent runs): they carry **judgment** plus a
walkthrough you can follow end to end.

This section is the home for the agent-recipe corpus
([iamacoffeepot/aether#1344](https://github.com/iamacoffeepot/aether/issues/1344)).

## Why recipes earn their own section

The motivating failure: an agent reached for a raw `env::var(...).parse()` to
add a tuning knob, when the blessed path was the layered config API
([ADR-0090](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0090-application-configuration.md)). The "why" was in an ADR and the mechanism existed in code,
but **nobody showed the steps**. A recipe titled *Adding a config knob* — the
`derive(Config)` → emitted overlay → `from_argv_then_env` → wire into the
chassis CLI → add to the `--config` dump — would have made the right path the
obvious one. Recipes exist to make the blessed multi-file dance the path of
least resistance.

## The dual-pass test (what makes a recipe done)

Every recipe is also an **API sanity check**, per aether's tutorial policy. A
recipe is only finished when it passes both directions:

- **A human reads it** → the steps make sense and the API looks sane.
- **An agent reads it** → can build the feature *on* that API from the recipe
  alone, without spelunking the source.

If either side fails, the recipe isn't the problem — **the API is mis-shaped.**
Fix the design, then the recipe follows. This is the whole reason to write
recipes during design rather than after: the writing is the test.

## The one structural seam: does it recompile?

Recipes split on a single practical axis — not "using vs extending" (that line
is blurry; writing a component is both at once), but **whether the task touches
aether's Rust and rebuilds**:

- **Drive-only** — MCP tool calls against a running engine (author a mesh, move
  the camera, capture a frame). Prereq: the harness is up. No build.
- **Recompile** — editing aether's Rust (a kind, a config knob, a capability).
  Prereq: `cargo` + the pre-flight loop.
- **The middle** — writing a wasm component with the actor SDK. You compile
  *your* crate to extend the running engine without touching aether's
  internals. Both at once.

Each recipe states its prereqs up front so you know which loop you're in.

## Recipe map

These recipes are part of the current book. Choose by the boundary you need to
cross, then verify volatile tool/type details against the linked system page and
current source.

- **Adding a config knob** (recompile) — the [ADR-0090](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0090-application-configuration.md) layered-config dance.
- **Authoring terrain** (drive-only) — create a revisioned mark, stage a bounded edit, inspect its rendered preview, and explicitly commit or discard it.
- **Adding a substrate kind** (recompile) — choose the correct kind owner,
  register/describe it, and prove the mail boundary end to end.
- **Adding a chassis capability** (recompile) — a native actor, its mailbox,
  its handlers, and the builder wiring.
- **Wiring an MCP tool** (recompile) — args → tool → wire-kind round-trip.
- **Writing a component** (the middle) — `#[actor]`, handlers, `export!`,
  loading it, and talking to it.
- **Writing a behavior** (the middle) — compile a narrow filter, host it at a
  tree slot, and swap it without treating it as a security gate.
- **Drawing your first text** (drive/build) — load a font, draw with explicit
  origin/clip, and gather frame evidence.
- **Serving HTTP from a component** (recompile) — the `aether.http.server`
  capability, the request/response handler, and claiming a route.
- **Debugging a hung settlement** (drive-only) — reading a stuck mail chain
  with the trace tools.

## The staleness rule for recipes

Recipes carry file paths and symbol names, so they rot faster than the
explainers. Two rules keep them honest:

1. **Point at a real in-tree example**, don't freeze a snippet. Reference the
   actual file/PR that did the thing; a copy drifts, a pointer doesn't.
2. **Validate named symbols and tool arguments.** Before following a recipe,
   confirm the exemplar, live schema, and source still agree. If they do not,
   repair the recipe as part of the work.
