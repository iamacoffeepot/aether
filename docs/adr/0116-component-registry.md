# ADR-0116: Component Registry

- **Status:** Accepted
- **Date:** 2026-06-15

## Context

`load_component` (and the `spawn_substrate` boot manifest's component entries) take a host path to a `.wasm`, with the same path-and-build coupling ADR-0115 removed for substrate binaries: the caller has to know where the component was built and that it exists. Components feel this harder than chassis binaries do. There are four chassis; components are open-ended — they are the unit you actually author, load, hot-reload, and compose, and you do it constantly. The path friction and the absence of reproducible pinning bite on the daily path, not the occasional one.

This ADR builds directly on ADR-0115, which gives the hub a content-addressed store kept deliberately **artifact-generic** in the `aether.engine` cap. Component wasm is the second artifact type that store holds.

Two facts make components a stronger fit for a registry than binaries:

- **Components are born self-describing.** The manifest a native binary has to surface via a `--describe` invocation is already embedded in every component: the kind vocabulary rides in the `aether.kinds` custom section (ADR-0028/0032), `describe_component` already surfaces handler kinds, per-handler docs, and `#[fallback]` presence (ADR-0033), and a multi-actor module exports several actors by `NAMESPACE` (ADR-0096). The registry reads what is already there, with no execution step.
- **Components are portable.** A `.wasm` is target-independent, so the target-triple axis that matters for native binaries does not apply — a component selected on one host runs on any other.

Prior art: components are already discovered structurally at build time (a `cargo metadata` package with `crate-type = cdylib` and a dependency on `aether-actor`). The registry is the runtime-side catalog of what has been built and uploaded, complementing that build-side discovery.

## Decision

Extend the ADR-0115 content-addressed store to hold component wasm, and give `load_component` and the `spawn_substrate` boot-manifest entries the same selector the substrate spawn surface gets.

**Upload.** Same rule as ADR-0115: upload takes a **staged path, never inline bytes** — a `.wasm` is read host-side and stored content-addressed, identical uploads dedup, and an optional name is a mutable pointer to the resulting hash.

**Selection.** `load_component` and boot-manifest component entries take a selector — `name | name@version | hash`, plus an **attribute query** over the component's self-reported manifest, resolving to a hash. **The host wasm path is retired from `load_component` entirely, not kept as an escape hatch** — an available path is one an agent reaches for by default, and on a procedure this common that quietly becomes the norm and re-creates the exact coupling the registry removes. A component is loaded only from the registry; the sole path anywhere is the upload input. The query axes follow what a component *is*:

- by **namespace** / **exported actor** — `module@actor` selects a specific actor from a multi-actor module, mirroring the ADR-0096 export selector.
- by **handled kind** — "a component that handles `Tick`," "one that sends to `aether.render`," read from the `aether.kinds` inputs section.

**Self-description.** Read directly from the wasm — the `aether.kinds` custom section plus the export table — with no `--describe` step. The manifest carries the namespace(s) and exported actors, the handled kind ids (and `#[fallback]` presence), and build provenance.

**Identity.** As in ADR-0115, a name is a mutable tag pointing at an immutable content hash. Pinning by hash drives the hot-reload workflow: `replace_component` to a specific hash pins or rolls a component to an exact build, and a boot manifest written in selectors makes a demo's or test's whole component set reproducible.

**Placement.** The store stays in the `aether.engine` cap — the cap that already manages engines also owns the registry. Both consumers are handlers on that one cap: spawn resolves a binary selector in `on_spawn`, and `load_component` resolution is a hub-local `ResolveComponent` handler aether-mcp calls to get the wasm bytes before forwarding `LoadComponent` to the target substrate. There is a single owner and nothing to extract. A separate store actor would force a shared `Arc` across two actors, because `on_spawn` resolves-then-forks synchronously while native mail is fire-and-forget; the store stays the cap's own single-threaded field.

**Open, deferred to scope/implementation:**

- Naming for the open-ended component space. A component's `NAMESPACE` is the natural name, but collisions across unrelated components and the exact name→hash tag semantics need pinning.
- How much of the handler/export index to materialize at upload versus read lazily through `describe_component`.
- Signature verification (ADR-0115's deferred keyring) lands here first: loading a component from an untrusted source is the realer threat, so the keyring gates component loads when components are shared across hosts or users.

## Consequences

**Positive.**

- `load_component` and the boot manifest reference a component by what it *is* — namespace, exported actor, handled kinds — and pin by hash. The wasm path papercut goes away, and a component set becomes reproducible.
- Self-description is free: the manifest is read straight from the wasm, so the catalog and query surface are richer and cheaper than the binary side, which needs `--describe`.
- The one cap that manages engines manages the whole registry: a second artifact type adds handlers, not a second owner, so no store extraction or cross-actor sharing is needed.

**Negative / cost.**

- Far more entries and far more churn than binaries — components are iterated constantly — so the disk budget, eviction, and pin-protection are load-bearing rather than nominal.
- A second selector surface (`load_component`, boot-manifest entries) plus the query index built from each component's manifest.
- Component upload / resolve / list add a second artifact type and its handlers to the `aether.engine` cap; keeping the store artifact-generic from ADR-0115 means the type tag and manifest variant are the main additions, not a new store.

**Neutral / follow-on.**

- Signature verification becomes concrete here the moment components are shared across hosts or users.
- Because wasm is portable, component selection needs no target axis — a simplification over the binary registry, not a new dimension.

## Alternatives considered

- **Keep `load_component` accepting a host path** (whether path-only, or a path alongside the selector as an escape hatch): rejected — the same coupling and reproducibility gap as the substrate binary path, and a path that exists is one agents default to, re-creating the friction on one of the most common procedures there is. The registry is the only way to load a component; the path survives solely as the upload input.
- **A separate component-only store, distinct from the binary store:** rejected — ADR-0115's store is artifact-generic precisely so one store with type-tagged manifests serves both. Two stores duplicate addressing, the disk budget, and eviction.
- **Require a `--describe` step for components:** unnecessary — a component already embeds its manifest in the `aether.kinds` section and export table, so the registry reads it directly rather than running the component to ask.
