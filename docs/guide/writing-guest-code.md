# Writing guest code

There is one way to run your own code on a running engine: write a
**component**. A component is a full actor — its own vocabulary, its own
mailbox, its own subscriptions — that you compile to wasm and load, either as
its own instance with its own mail lineage (`load_component`) or compiled
inline as a child of another actor, settling mail cascades inside that
actor's cluster (`#[actor]` + `export!` inline children, ADR-0114). Both are
the same authoring surface — the actor you write,
[compiled to wasm](recipes/writing-a-component.md) — differing only in how it
is deployed.

## The two deployment shapes

| Mechanism | When it arrives | Where it runs |
|---|---|---|
| `#[actor]` + `export!` inline children | compile time | inside the cluster |
| `load_component` | runtime | its own instance & lineage |

A component authored with `#[actor]` gives you an actor either way you deploy it:
compiled inline as a child of another actor, it settles mail cascades inside the
cluster; loaded on its own with `load_component`, it becomes an independent
instance with its own lineage. Both are the same authoring surface — the actor
you write, [compiled to wasm](recipes/writing-a-component.md).

## Where to read more

- The full end-to-end loop for a component — crate setup, the `#[actor]` block,
  `export!`, the wasm build, and loading it over MCP —
  [Writing a component](recipes/writing-a-component.md).
- How you write an actor at all — its lifecycle, handlers, and addressing by type
  — [The actor model](foundations/actor-model.md).
