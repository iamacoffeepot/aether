# ADR-0155: Staged Chassis Boot and Claim-Derived Describe

- **Status:** Proposed
- **Date:** 2026-07-20

## Context

ADR-0115 gave every chassis binary a `--describe` mode: the hub's binary store forks `<binary> --describe` once at upload time and stores the printed `BinaryManifest` — chassis profile, capability namespaces, build provenance. The capability list is the store's selection metadata: it is how an observer decides which stored binary serves which mailboxes before ever spawning one.

Today that list is hand-maintained. Each chassis assembles it from `common_cap_namespaces()` plus a per-chassis extras array, and a doc comment pleads that the list's membership "must be kept in lockstep" with the `with_actor` composition chain. The plea has already failed, in both directions a hand list can fail:

- The desktop chassis serves `aether.window` through its driver-as-actor (ADR-0071 phase 3), and the headless chassis answers `aether.audio` through an inline fail-fast sink (`register_inline`). Neither appears in any manifest, because neither registers through the `with_actor` chain the lists mirror.
- The conditional `aether.http.server` capability is linked into every full-stack binary but listed in no manifest, while the equally conditional `aether.rpc.server` is listed — membership decided by ad-hoc judgment per chassis rather than by any rule.

The deeper problem is that registration has three contributors — the `with_actor` chain, driver claims, and inline sinks — and only the substrate registry ever sees all three. Any describe design that reads a declaration (a hand list, a const capability array, a composed-but-unbooted chain) reports at most one contributor and re-introduces a second source of truth that drifts from what actually loads.

Reading the registry, however, requires the application to exist — and today existence is all-or-nothing. `Builder::build` fuses the whole lifecycle in one call: the issue-697 multi-pass (claim → init → wire → spawn) plus driver boot. There is no way to stop after "the application is made" and before "the application runs". And the stages genuinely differ in what they touch: the claim pass is pure in-memory registry reservation, while init already has side effects (`FsCapability::init` creates directories, `AudioCapability::init` opens the audio device), and spawn starts dispatcher threads, binds sockets, and — on desktop — needs a winit event loop that does not exist on the headless host where the hub captures manifests.

## Decision

Name the chassis lifecycle stages, expose the seam between making and starting, and derive the `--describe` capability roster from the claim stage of the real registration path.

**1. The stage vocabulary.** Chassis boot is five stages, and the code names them:

- **Compose** — declare the capability chain (`with_actor`) and inline sinks. Pure; nothing executes.
- **Claim** — every namespace/mailbox reservation lands in the registry, including driver claims. In-memory only; no OS resources.
- **Init** — actor construction from configs. May touch OS resources (filesystem roots, audio device).
- **Wire** — post-init lifecycle hooks; mail allowed.
- **Start** — dispatcher threads spawn, sockets bind, the driver runs.

The first four already exist inside `boot_passives` (claim / init / wire / spawn); the decision is to make the Compose/Claim boundary reachable from outside rather than fused into `build()`.

**2. Describe = Compose + Claim.** The `Builder` gains a claim-only terminal: run the claim pass over every composed passive plus the driver's claim, return the set of claimed namespaces, and tear down by plain drop. `--describe` makes the application exactly as a real boot would, stops before Init, reads the claimed namespaces off the registry, prints the manifest, and exits. The roster is derived from the same code path a real boot runs — no declaration of expected capabilities exists anywhere, so there is nothing to drift.

`DriverCapability` grows a claim hook that must not require runtime handles: the desktop driver claims `aether.window` without constructing an event loop. Inline sinks register during Compose and are claims like any other, so `aether.window` on desktop and `aether.audio` on headless finally appear in their manifests.

**3. Conditional capabilities always claim and conditionally start.** The `maybe_with_rpc_server` / `maybe_with_http_server` composition inverts: the RPC and HTTP server capabilities are always composed and always claim (they are linked surface, so they belong in every manifest), and the resolved enabled/configured state gates only what Start does (whether a socket binds). A claimed-but-not-started capability answers mail with the existing fail-fast `Err`-reply convention rather than today's unknown-mailbox warn-drop. This removes the manifest's dependence on the invoking environment: the same binary claims the same namespaces no matter where `--describe` runs.

**4. Configs are data; runtime handles are Start-stage inputs.** Describe resolves capability configs through the same argv/env/file path a real boot uses — the values ride the builder but are consumed only at Init, which describe never reaches. Whatever cannot be resolved anywhere (the winit event loop, capture-queue wiring) is by definition not config: it is a Start-stage input, constructed only on the boot path. The desktop chassis restructures accordingly — its capture backend moves out of `RenderConfig` into a Start-stage handoff (the `RenderHandles` exported-handle mechanism is the precedent) — so `--describe` runs on a headless host, preserving ADR-0115's capture guarantee.

**5. Manifest shape unchanged, meaning amended.** `BinaryManifest` keeps its fields (`chassis`, `caps`, `git_sha`, `profile`, `target`). ADR-0115's `caps` semantic is amended to: the namespaces this binary claims when made. This ADR amends ADR-0115 (manifest capture) and extends ADR-0071 (chassis builder) without superseding either.

## Consequences

- The hand lists, `common_cap_namespaces()`, and their lockstep comments are deleted. Drift becomes structurally impossible rather than policed by review.
- Manifests gain entries they were silently missing: driver claims (`aether.window` on desktop), inline sinks (`aether.audio` on headless), and both conditional servers (`aether.rpc.server`, `aether.http.server`) uniformly across chassis. Stored binaries re-capture on next upload; no wire-format change.
- The `Builder` API grows the claim-only terminal, and `DriverCapability` grows a value-free claim hook — API surface every chassis and the substrate-harness inherit.
- The desktop Env splits into config data and runtime handles. This is real surgery around the render capture backend, and it is the largest single work item this decision creates.
- A claimed-but-disabled capability answers mail with a fail-fast `Err` instead of an unknown-mailbox drop — an observable routing change that makes "linked but not enabled" a first-class, diagnosable state.
- The stage vocabulary gives the later capability-config ownership arc (dissolving the per-chassis Env bags into per-capability resolution) a foundation: configs already flow per-stage rather than per-chassis-bag.

## Alternatives considered

- **Hand-kept namespace lists (status quo)** — already drifted in both directions; rejected.
- **Compose the real chain with placeholder configs, read namespaces off the builder** — single-source for the chain, but demands config values that are pure theater at describe time, and stays blind to driver claims and inline sinks; rejected.
- **A const `CAPABILITIES` entry array the boot path interprets** — describe becomes a pure data read, but the array is a second representation of the chain (fn-pointer/macro binding machinery), and it is equally blind to driver and inline claims; rejected.
- **Boot fully and read the registry** — complete truth, but Start binds sockets, spawns threads, and needs winit on desktop, which the hub's headless capture host cannot provide; rejected.
- **First-boot attestation** — report the registry roster over the existing RPC hello and store it per content hash; zero declaration and full truth, but the roster is unknown until a binary first spawns and varies with the booted environment; rejected in favor of claim-stage capture, which yields the same registry-derived truth at upload time, environment-independently.
