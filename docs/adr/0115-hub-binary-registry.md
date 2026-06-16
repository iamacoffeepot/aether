# ADR-0115: Hub Binary Registry

- **Status:** Proposed
- **Date:** 2026-06-15

## Context

`spawn_substrate` and `load_component` take an absolute host filesystem path, and the hub does not resolve or locate binaries — it forks exactly the path it is handed. The caller therefore has to know where the build output lives (`target/release` vs `target/debug`), which binary name to use, and that the binary has actually been built. That couples every caller to the local build layout, and the friction is acute for any consumer that does not already know the host: it must discover which binaries are built, and where, before it can spawn anything at all.

This carries two distinct costs:

1. **Ergonomic** — the caller deals with paths and build-system specifics on every spawn.
2. **Reproducibility** — a path reference is only as good as whatever currently sits at that path. A stale `target/release` silently runs the wrong binary, and a test cannot pin a known-good substrate to run.

Prior decisions and facts in play:

- ADR-0049 gives a persistent, content-addressed handle store (typed `Ref<K>`, a disk budget, `describe_handles`). It is **per-engine** — `describe_handles` is scoped to an `engine_id` — so it cannot directly hold the binaries the hub forks *before* any engine exists.
- The hub already fork+execs whatever path it is handed via `spawn_substrate`, so executing caller-supplied bytes is not a new trust boundary **on a single-user local host**. Widening that scope is a separate concern, addressed under follow-on below.
- Large payloads already stage rather than ride inline through a tool call: DAG sources stage via a `payload_path`. A binary is large enough that inlining its bytes through the tool channel is never acceptable, so binary upload follows the staged-path model.

## Decision

The hub gains a **content-addressed binary store**. A binary is uploaded once and thereafter referenced by content hash or assigned name; the host filesystem path collapses to the single upload input and disappears from the spawn and test surfaces.

**Surface.**

- `upload_binary(staged_path, name?)` → `{ hash, name? }`. The argument is **always a path to the binary already present on the fleet host — never inline bytes**; the hub ingests the file from that path and stores it content-addressed, and identical uploads dedup to one entry. An optional name is a mutable pointer to the resulting hash.
- `spawn_substrate` (and `load_component`) take a **selector** and no longer accept a host path: `default | name | name@version | hash`, plus an **attribute query** over the binary's self-reported manifest (e.g. `chassis=headless, caps=[audio]`, `target=…`) that resolves to a hash. Exact selectors win first (`hash` > `name@version` > `name`), then an attribute query, then `default`.
- `list_binaries` enumerates the store with each entry's manifest and accepts the same attribute filters, so a consumer reads what the available binaries *are* rather than guessing names — the menu that keeps a selector from just moving "which path?" to "which name?".

**Identity.**

- The content hash is immutable and is the unit of reproducible pinning.
- A name is a mutable tag pointing at a hash; uploading the same name again repoints it at the new hash. `name@version` selects a specific historical entry of a name, where **"version" is the build id the binary self-reports** in its manifest (git sha and/or build timestamp) — not a tag the uploader assigns, and not a semantic version (the engine is not versioned per-binary). Reading version from self-description means two uploads of the same source resolve identically without anyone having to label them.
- `default` resolves to a configured selection — proposed: the binary whose chassis is `headless`, since it has no window, runs on any host, and makes a bare `spawn_substrate()` self-sufficient. This is the one genuinely open call; it is what a no-argument spawn lands on.

**Self-description.** A binary reports its own metadata rather than the uploader vouching for it — the native-binary analogue of the `aether.kinds` custom section wasm components already carry and the build-time inventory `describe_transforms` already surfaces. The hub captures a manifest at upload via a `--describe` invocation it runs once (reusing the inventories the chassis already assembles for `describe_kinds` / `describe_transforms`) and stores it next to the hash. The manifest carries:

- **chassis kind** (desktop / headless / hub / test-bench) — intrinsic, so the registry no longer depends on the uploader naming a binary correctly.
- **linked capabilities** — which chassis caps are built in (render, audio, fs, window, …). This is the primary query axis: "a headless one that still has audio," "one with real render."
- **build provenance** — git sha, build profile, and target triple. The triple is load-bearing once a binary is built for a host that differs from the hub's.
- optionally the **transform / kind inventory**, so a binary can be selected for a transform a DAG needs.

`--describe` requires the binary to run on the hub's host, which holds for the same-host first cut; a statically embedded manifest section is the cross-target-robust successor — it reads without executing — and is a fast-follow, as is the transform/kind inventory. Chassis kind, capabilities, and provenance are the committed v1 axes.

**Execution.** A stored binary is content bytes, which cannot be fork+exec'd directly. At spawn the hub materializes the selected entry to a temp file, marks it executable, and forks that — the caller never sees the realized path. This mirrors how DAG sources already stage large payloads to disk.

**Storage.**

- The store is **hub-level** — binaries are forked by the hub, above any engine — and persists on the hub's disk across the `restart-hub` re-fork, so uploads survive a hub restart rather than needing re-upload.
- It reuses the ADR-0049 content-addressing machinery (content hash, disk budget, describe-style reporting) as a **hub-scoped instance**, distinct from the existing per-engine handle store.
- The registry lives in the existing `aether.engine` cap that already owns spawn and the fleet, not a new actor — selector resolution and realize-to-exec are part of forking a substrate, and the store has no consumer independent of spawn. The store is kept **artifact-generic** (a content-addressed blob plus a type-tagged manifest), so when component wasm joins as a second artifact type (`load_component`, ADR-0116) it is a second set of handlers on the same cap — both spawn and component resolution have one owner, with no store to extract.
- Eviction: content-addressing dedups identical builds; named and explicitly-pinned hashes are eviction-protected; the remainder is reclaimed LRU under the disk budget. Pin-protection is load-bearing — a pinned test fixture must not be evicted out from under a test.

**Scope for the first cut:** substrate binaries only. The same selector is designed to extend to component wasm later (name = component namespace, hash = content), but that is not built now.

## Consequences

**Positive.**

- A bare `spawn_substrate()` with no arguments returns a working engine with zero host knowledge — the tool becomes self-sufficient, removing the largest first-contact stumbling block on the spawn surface.
- Tests and harnesses (FleetBench, TestBench, and any run that needs a known substrate) pin a binary by hash and get exactly that binary every run, regardless of local build state. The "stale `target/` ran the wrong thing" class of bug is eliminated.
- The producer of a binary deals with build configuration (debug/release, cross-compile) once, at upload; every consumer only selects.
- Selection is by what a binary *is* — its chassis and linked capabilities — not by a name someone remembered to assign, and the metadata is trustworthy because the binary reports it rather than an external label that can drift.

**Negative / cost.**

- The hub becomes stateful with a persistent, disk-budgeted binary store and the eviction policy that implies. Binaries are large, so the budget and pin-protection must be real rather than nominal.
- First use of a freshly-built binary pays a one-time upload before it is selectable.
- Two new tools (`upload_binary`, `list_binaries`) plus a selector on `spawn_substrate` / `load_component`; the raw path form is removed from those two entirely and survives only as the input to `upload_binary`.
- Upload runs the binary once (`--describe`) to capture its manifest, so a binary must be runnable on the hub's host until the embedded-section form lands.

**Neutral / follow-on.**

- The natural place to upload the chassis binaries is the build/preflight flow (e.g. `ensure-tunnel.sh` after a build), so the store is populated without a manual step.
- "Version" is resolved as the binary's self-reported build id; the only remaining detail is whether `name@version` keys on git sha, build timestamp, or both.
- **Signature verification is the path for when the trust scope widens** beyond a single-user local host (a shared box, multiple uploaders, or networked upload). Binaries would be signed over their content hash and verified at upload — and optionally again at realize-to-exec — against a public-key keyring the hub trusts. Deferred, but the content-addressed store is the right anchor for it: the signature covers the hash, so a verified upload stays verified for every later selection of that hash.

## Alternatives considered

- **Path-reference registry** (the hub indexes host paths and the caller selects by name): still couples to the local build layout — a resolved path must exist, can go stale, and gives no reproducible pin. Uploading into the store cuts the host-path cord entirely.
- **Reuse the per-engine ADR-0049 handle store directly:** it lives below an engine, but binaries are forked by the hub before any engine exists, so the store must be hub-level. The machinery is reused; the instance is not.
- **Keep a raw host-path escape hatch on `spawn_substrate` alongside the selector:** rejected outright — not deferred, not reconsiderable. A path that exists is a path agents reach for by default, and spawning is common enough that the escape hatch would quietly become the norm and re-create the coupling the registry removes. The registry is the sole way to spawn; the only path anywhere is the one-time upload input.
- **Inline the binary bytes in the upload call** (base64 in JSON): rejected outright — a binary is far too large to ride through a tool call, and inlining it would blow the very context budget the registry is meant to keep tidy. `upload_binary` is always a staged path; the bytes are read host-side and never put on the wire.
- **Uploader-supplied metadata** (the build step labels the binary at upload): rejected in favour of self-description — a binary cannot misreport what is linked into it the way an external label can drift out of sync.
- **A statically embedded manifest section instead of `--describe`:** the cross-target-robust form (it reads without running the binary), deferred because emitting and parsing a native section per target is more than the same-host first cut needs.
