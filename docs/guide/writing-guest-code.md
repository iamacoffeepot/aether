# Writing guest code

There are two ways to run your own code on a running engine, and picking the
right one is the first decision you make: write a **component** when your code
needs its own vocabulary, its own mailbox, and its own subscriptions, and write
a **behavior** when it only needs to transform mail that already flows past a
spot in a running cluster. A component is a full actor you compile and load; a
behavior is a small script the engine interprets in place, at a position you
choose in an existing actor tree. This page draws the line between them so you
reach for the right surface before you start writing.

## The two axes

Guest code differs along two axes: **when it arrives** — compiled into a module
ahead of time, or injected while the engine runs — and **where it runs** — in
its own instance with its own mailbox lineage, or inside an existing cluster
sharing that cluster's mail path. Three mechanisms sit on those axes:

| Mechanism | When it arrives | Where it runs |
|---|---|---|
| `#[actor]` + `export!` inline children | compile time | inside the cluster |
| `load_component` | runtime | its own instance & lineage |
| a behavior script | runtime | inside the cluster |

A component authored with `#[actor]` gives you an actor either way you deploy it:
compiled inline as a child of another actor, it settles mail cascades inside the
cluster; loaded on its own with `load_component`, it becomes an independent
instance with its own lineage. Both are the same authoring surface — the actor
you write, [compiled to wasm](recipes/writing-a-component.md).

A behavior fills the remaining corner: code that arrives at runtime *and* runs
inside an existing cluster. That corner has no component form — a wasm instance's
functions are fixed when it is instantiated, so new code cannot be grown into a
live instance. A behavior instead interposes *around* the actors already there,
occupying a slot in the tree and transforming the mail that passes through it.
The forcing case is widget glue: the logic that makes one panel *this* panel
("when the slider passes 0.8, clamp it and flash the label") is small, changes
constantly, and belongs at the widget it modifies — too light for a component
build-and-load cycle every time it changes.

## Component or behavior

The decision follows from what your code needs to touch.

Reach for a **component** when it:

- declares new kinds — a component carries its own `aether.kinds` vocabulary and
  registers it, so `describe_kinds` and `describe_component` can see it;
- owns a mailbox others address by name, or subscribes to input streams;
- runs on its own, addressable across the wire, with its own mail lineage.

Reach for a **behavior** when it:

- transforms kinds that already flow past its position — it declares no new
  vocabulary and registers nothing;
- reads and mutates that traffic in place, or emits effects back into the
  cluster through the widgets around it;
- is small enough to author, attach, and swap live, without a build-and-load
  round trip.

A behavior's whole mail surface is the set of kinds already flowing at its slot.
It reads them through a per-kind mirror (`last::<K>()`), intercepts them by taking
the mail `&mut` (the mutated value forwards) or observes them by taking it `&K`,
and emits effects by projecting mail onto the widgets in its subtree. It never
appears in the mailbox topology — the chain sees the host actor it runs inside,
and the script's cost lands in that host's handler cost.

## Graduating a behavior to a component

The line is not a wall. A behavior that grows to want its own kind vocabulary,
its own mailbox, or its own subscriptions has outgrown the shared cell — that is
exactly the set of things a component provides and a behavior does not. The
graduation path is `load_component`: lift the logic into an actor, give it the
vocabulary it now needs, and load it as its own instance. Because the two
authoring surfaces mirror each other — `#[behavior]` handlers infer their kind
from a parameter the same way `#[actor]` handlers do, and both carry the same
lifecycle shape — the concepts you learned writing the behavior carry over, and
the move is mostly a change of where the code lives, not how it is written.

## A behavior is not a gate

One property is load-bearing enough to state up front: a behavior **fails open**.
Every script runs under a fuel budget, and on any fault — fuel exhaustion, a
memory error, a bad decode — the in-flight mail forwards untransformed and the
failure is logged; after repeated faults the script is disabled into a passthrough
until it is replaced. A degraded-but-alive widget is the correct residue of a
broken script, never a dead subtree.

That default has a direct consequence for what you may express as a behavior:
anything that must *block* traffic to be correct or safe cannot be a behavior,
because a fault would let that traffic straight through. Gate-shaped logic belongs
in a real actor, where containment is structural. A behavior is a convenience
layer over traffic that flowed fine before the script existed — treat it as one.

## Where to read more

- The full end-to-end loop for a component — crate setup, the `#[actor]` block,
  `export!`, the wasm build, and loading it over MCP —
  [Writing a component](recipes/writing-a-component.md).
- How you write an actor at all — its lifecycle, handlers, and addressing by type
  — [The actor model](foundations/actor-model.md).
- The mechanism behind behaviors — interposition as tree position, the fuel-metered
  fail-open firewall, the mirror-and-effects model, and the vocabulary boundary —
  [Behaviors](systems/behaviors.md) and
  [ADR-0137](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0137-in-cluster-behavior-script-host.md).
- The current end-to-end authoring loop — [Writing a behavior](recipes/writing-a-behavior.md).
