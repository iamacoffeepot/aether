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
  - [The scheduler](systems/scheduler.md)
  - [Components & lifecycle](systems/components.md)
  - [Rendering & camera](systems/rendering.md)
  - [Mesh authoring & the DSL]()
  - [Input streams](systems/input.md)
  - [The frame lifecycle](systems/lifecycle.md)
  - [File I/O](systems/file-io.md)
  - [HTTP egress](systems/http.md)
  - [Audio]()
  - [Window](systems/window.md)
  - [Tracing & settlement](systems/tracing-and-settlement.md)
  - [Logging](systems/logging.md)
  - [Configuration](systems/configuration.md)

# Building with aether

- [Recipes](recipes.md)
  - [Adding a config knob](recipes/adding-a-config-knob.md)
  - [Adding a substrate kind](recipes/adding-a-substrate-kind.md)
  - [Drawing your first text](recipes/drawing-text.md)
  - [Drawing your first UI](recipes/drawing-ui.md)
  - [Adding a chassis capability](recipes/adding-a-chassis-capability.md)
  - [Wiring an MCP tool](recipes/wiring-an-mcp-tool.md)
  - [Writing a component](recipes/writing-a-component.md)
  - [Debugging a hung settlement](recipes/debugging-a-hung-settlement.md)

# Testing

- [Writing tests that earn their place](testing.md)

# Contributing

- [Local verification](local-verification.md)

# Reference

- [Pointers & where to read more](reference.md)
