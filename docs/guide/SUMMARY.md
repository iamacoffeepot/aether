# Summary

# Start here

- [Introduction](introduction.md)
  - [How to use this guide](orientation/using-the-guide.md)
  - [Repository map](orientation/repository-map.md)
  - [First live-engine session](orientation/first-engine-session.md)

# Design and architecture

- [Why Aether is shaped this way](philosophy.md)
- [Architecture overview](architecture.md)
  - [Process topology and chassis](architecture/process-topology.md)
  - [Guest, native, and wire boundaries](architecture/guest-native-boundary.md)

# Foundations

- [The type system](foundations/type-system.md)
- [The actor model](foundations/actor-model.md)
- [Invariants and guarantees](foundations/invariants.md)

# Operating a live engine

- [Operating an engine](operating/index.md)
  - [The MCP harness](mcp-harness.md)
    - [Harness lifecycle and fleet-wide mutations](operating/harness-lifecycle.md)
  - [Engine fleet and artifact stores](operating/engine-fleet.md)
  - [Host paths, artifact trust, and evidence files](operating/host-paths-and-artifacts.md)
    - [Artifact trust and provenance](operating/artifacts/trust-and-provenance.md)
    - [Host evidence files and capture destinations](operating/evidence/host-files.md)
  - [Component registry and replacement](operating/component-registry.md)
    - [Replacement failure states](operating/components/replacement-failure-states.md)
  - [Inspection and debugging](operating/inspect-and-debug.md)
  - [Recovery runbook](operating/recovery.md)

# Systems

- [Subsystem map](systems.md)
  - [Core runtime systems](systems/core-runtime.md)
    - [Mail, kinds, and scheduling](systems/mail-and-kinds.md)
    - [Concurrency and blocking](systems/concurrency.md)
    - [Scheduler internals](systems/scheduler.md)
    - [Frame lifecycle](systems/lifecycle.md)
    - [Tracing and settlement](systems/tracing-and-settlement.md)
    - [Logging](systems/logging.md)
    - [Configuration](systems/configuration.md)
  - [Hosted code and replacement](systems/hosted-code.md)
    - [Components and lifecycle](systems/components.md)
    - [Behaviors](systems/behaviors.md)
    - [Inventory and transforms](systems/inventory-and-transforms.md)
  - [Platform and network I/O](systems/platform-io.md)
    - [File I/O](systems/file-io.md)
    - [HTTP egress](systems/http.md)
    - [HTTP server and typed routes](systems/http-server.md)
    - [TCP listeners and sessions](systems/tcp.md)
    - [RPC wire and engine routing](systems/rpc.md)
    - [Clipboard](systems/clipboard.md)
    - [Content-generation capabilities](systems/content-generation.md)
  - [Media, interaction, and product tools](systems/media-and-tools.md)
    - [Rendering and camera](systems/rendering.md)
    - [Puppet controls](systems/puppet.md)
    - [Authored render programs](systems/render-programs.md)
    - [Text](systems/text.md)
    - [Mesh authoring and the DSL](systems/mesh-authoring.md)
    - [Audio](systems/audio.md)
    - [Input streams](systems/input.md)
    - [Window](systems/window.md)
    - [Widget set and focus model](systems/widgets.md)

# Building with Aether

- [Choose the owning extension point](building/extension-points.md)
- [Capability module anatomy](capability-anatomy.md)
- [Writing guest code](writing-guest-code.md)
- [Distribution and packaging](building/distribution.md)
- [Recipes](recipes.md)
  - [Adding a config knob](recipes/adding-a-config-knob.md)
  - [Adding a mail kind](recipes/adding-a-substrate-kind.md)
  - [Drawing your first text](recipes/drawing-text.md)
  - [Authoring a render program](recipes/authoring-a-render-program.md)
  - [Adding a chassis capability](recipes/adding-a-chassis-capability.md)
  - [Wiring an MCP tool](recipes/wiring-an-mcp-tool.md)
  - [Writing a component](recipes/writing-a-component.md)
  - [Writing a behavior](recipes/writing-a-behavior.md)
  - [Serving HTTP from a component](recipes/serving-http.md)
  - [Driving a bloom over the REST control API](recipes/bloomery-rest-api.md)
  - [Amending a member's declared surface](recipes/amending-a-declared-surface.md)
  - [Supervising the coordinator with systemd](recipes/supervising-the-coordinator.md)
  - [Supervising the hub with systemd](recipes/supervising-the-hub.md)
  - [Debugging a hung settlement](recipes/debugging-a-hung-settlement.md)

# Testing and verification

- [Tests that earn their place](testing.md)
  - [SubstrateHarness, FleetHarness, and LaneHarness](testing/substrateharness-and-fleetharness.md)
  - [Performance, load, and fuzzing](testing/performance-and-fuzzing.md)
  - [Offline quality eval](testing/quality-eval.md)

# Contributing

- [Agent and contributor workflow](contributing/agent-workflow.md)
  - [Worktrees, safety, and ownership](contributing/worktrees-and-safety.md)
  - [Local checks and CI](local-verification.md)
  - [Architecture decisions](contributing/architecture-decisions.md)
  - [Maintaining the guide](contributing/documentation.md)

# Reference

- [Glossary](reference/glossary.md)
- [Capability and service index](reference/capability-index.md)
- [ADR map by topic](reference/adr-map.md)
- [Sources and live reference](reference.md)
