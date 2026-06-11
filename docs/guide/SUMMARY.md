# Summary

[Introduction](introduction.md)

# Design & philosophy

- [Why aether is shaped this way](philosophy.md)
- [Architecture overview](architecture.md)

# Foundations

- [The type system](foundations/type-system.md)
- [The actor model](foundations/actor-model.md)
- [Invariants & guarantees](foundations/invariants.md)

# Driving the engine

- [The MCP harness](mcp-harness.md)

# The systems

- [Subsystem map](systems.md)
  - [Mail, kinds & scheduling](systems/mail-and-kinds.md)
  - [Concurrency & blocking](systems/concurrency.md)
  - [Components & lifecycle](systems/components.md)
  - [Rendering & camera]()
  - [Mesh authoring & the DSL]()
  - [Input streams](systems/input.md)
  - [File I/O](systems/file-io.md)
  - [Audio]()
  - [Tracing & settlement](systems/tracing-and-settlement.md)
  - [Configuration](systems/configuration.md)
  - [Handles](systems/handles.md)
  - [The computation DAG]()

# Building with aether

- [Recipes](recipes.md)
  - [Adding a config knob]()
  - [Adding a substrate kind](recipes/adding-a-substrate-kind.md)
  - [Adding a chassis capability]()
  - [Wiring an MCP tool](recipes/wiring-an-mcp-tool.md)
  - [Writing a component](recipes/writing-a-component.md)
  - [Debugging a hung settlement](recipes/debugging-a-hung-settlement.md)

# Reference

- [Pointers & where to read more](reference.md)
