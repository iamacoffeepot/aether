# ADR-0073: Substrate cluster consolidation and naming convention

- **Status:** Proposed
- **Date:** 2026-05-02

## Context

The substrate cluster grew incrementally across ADR-0035 (substrate-chassis split — `aether-substrate-core` runtime + per-chassis binaries `aether-substrate-{desktop,headless,hub}`), ADR-0070 (native capabilities + `aether-hub` library extracted from the substrate), and ADR-0067 (test-bench chassis as `aether-substrate-test-bench`). Each ADR was right at the time it landed, but the cumulative shape diverged from how the rest of the workspace organises crates.

By April 2026 the cluster looked like:

```
aether-substrate-core         runtime
aether-substrate              chassis bundle (lib + 3 bins)   ← created by PR 500
aether-substrate-desktop      ⟂ collapsed into ↑ by PR 500
aether-substrate-headless     ⟂ collapsed into ↑ by PR 500
aether-substrate-hub          ⟂ collapsed into ↑ by PR 500
aether-hub                    ⟂ collapsed into ↑ by PR 500 (now src/hub/)
aether-substrate-test-bench   chassis (4th chassis kind)
aether-scenario               YAML-driven test framework over TestBench
aether-scenario-cli           74-line bin wrapper over aether-scenario
aether-scenario-macros        proc-macro: scenario_dir!
```

Two problems:

1. **The naming inverts the workspace convention.** Every other crate cluster uses `X` (core) + `X-{suffix}` (derived): `aether-data` / `-derive`, `aether-mesh` / `-viewer`, `aether-scenario` / `-cli` / `-macros`. The substrate cluster put the runtime in `-core` and the chassis bundle at the bare name. Readers reasonably expected `aether-substrate` to be the runtime.
2. **Two crates were structurally redundant.** `aether-substrate-test-bench` is a chassis — it implements the same `Chassis` trait, has a hub-connecting bin like the other three, and ships a `TestBench` library API. PR 500 collapsed the other three chassis crates into `aether-substrate` (the bundle) but kept test-bench out only because of its lib API. Similarly `aether-scenario-cli` is a 74-line bin wrapper with a single dep on `aether-scenario` — same shape as the chassis bins folded into the bundle.

Issue 501 proposed flipping the naming and finishing the consolidation as three sequenced PRs.

## Decision

Consolidate the substrate cluster so the names match the workspace convention and the structurally-redundant crates fold into their natural homes:

- `aether-substrate-core` → `aether-substrate` (runtime).
- `aether-substrate` (chassis bundle) → `aether-substrate-bundle` (bundle).
- `aether-substrate-test-bench` folds into `aether-substrate-bundle` as a fourth chassis (`src/test_bench/` lib module + `src/bin/test-bench.rs`); the `TestBench` library API surfaces at `aether_substrate_bundle::test_bench::TestBench`.
- `aether-scenario-cli` folds into `aether-scenario` as a `[[bin]]` entry.

Keep:

- **`aether-scenario`** as a separate crate. It's a test framework that *consumes* the chassis (via `TestBench`), not a chassis itself. Folding it into `aether-substrate-bundle` would push YAML-parsing and test-helper deps (`serde_yml`, `thiserror`) into chassis-land and pin the `aether-scenario-macros` proc-macro emit paths to `aether_substrate_bundle::*`, coupling the macro crate to the bundle's namespace.
- **`aether-scenario-macros`** as a separate crate. Rust's `proc-macro = true` constraint requires it.

Bin output names are preserved across the rename and folds — `aether-substrate`, `aether-substrate-headless`, `aether-substrate-hub`, `aether-substrate-test-bench`, and `aether-scenario` are external contracts (baked into `spawn_substrate`, MCP tooling, deployed scripts), so output names don't change even though they no longer match their owning package's name in every case.

Tracing target strings (`target: "aether_substrate::scheduler"` etc.) keep their uniform `aether_substrate::*` namespace regardless of where the emitting code lives, so `AETHER_LOG_FILTER=aether_substrate=debug` filters keep working unchanged.

## Consequences

**Positive:**

- Workspace member count drops from 26 (pre-PR-500) → 20. The cluster fits the workspace's `X` / `X-{suffix}` mental model.
- `aether-substrate` is now what readers expect — the runtime. The chassis bundle is honestly named (`-bundle`) for what it is: a multi-bin package of the four chassis impls plus their shared hub library.
- The four chassis live together (`src/{desktop,headless,hub,test_bench}/`), so refactors that touch the `Chassis` trait or `DriverCapability` shape edit one crate instead of four.
- LLVM dead-code elimination keeps per-binary sizes lean: empirically validated during PR 500 (bundle-headless 63.5 MB vs standalone-headless 63.5 MB, ~80 KB diff).

**Negative:**

- ADRs that pre-date this consolidation (ADR-0024, 0035, 0036, 0060, 0065, 0068, 0069, 0070, 0071, 0072) reference retired crate names (`aether-substrate-core`, `aether-substrate-desktop`, `aether-substrate-headless`, `aether-substrate-hub`, `aether-hub`, `aether-substrate-test-bench`, `aether-scenario-cli`). These stay as historical record; this ADR is the forward-pointer for the current layout. New readers should treat any ADR's crate-layout descriptions as time-stamped to that ADR's date.
- One-time large rename (~83 files in PR 502) plus two structural folds. Mitigated by `git mv` rename-detection (95-100% similarity on the moved files) and by gating each phase behind a separate PR per `feedback_file_issue_for_multi_pr_work`.
- `aether-substrate-bundle` dev-deps `aether-test-fixture-probe` (was test-bench's dev-dep). Mostly invisible — bundle's test build pulls another wasm component crate. Acceptable.

**Neutral / forward:**

- `aether-substrate` (runtime) folding into `aether-substrate-bundle` was considered and explicitly parked. Reasons: feature gates (`render = ["dep:wgpu", ...]`, `audio = ["dep:cpal", ...]`) gate ~19K lines and the wgpu/cpal compile cost; the runtime/chassis layering boundary is load-bearing as documentation; folded crate would be ~24K lines as a single compile unit. May revisit when a forcing function appears (e.g. workspace gets too thin to justify the boundary).
- `aether-scenario` folding into `aether-substrate-bundle` was considered and rejected for the layering and proc-macro-coupling reasons documented above.

## Alternatives considered

- **Leave the names inverted and the crates separate.** Lowest churn but readers keep getting tripped up by `aether-substrate-core` being the runtime, and the two redundant crates keep adding workspace noise.
- **Fold scenario + scenario-cli + test-bench all into one mega-bundle crate.** Considered and rejected. Test framework concerns belong above chassis concerns; folding scenario down forces YAML-parsing into chassis-land and pins the proc-macro to bundle's name. The macros crate would have to know about bundle's namespace just to emit `aether_substrate_bundle::Runner` paths — a regression in dependency hygiene.
- **Keep `aether-substrate-test-bench` as a sibling library crate.** PR 500 left it separate for exactly this reason — its `TestBench` library API has external consumers. After the bundle was already a multi-bin lib crate, that argument weakens: the lib API can live one module deeper at `aether_substrate_bundle::test_bench::TestBench` with the same five callsites updated. The cost of the rename is small relative to the structural-symmetry win (all chassis live together).
- **Different suffix than `-bundle`.** Considered `-chassis` (purpose-oriented, matches doc terminology), `-host`, `-shell`, `-bins`. Picked `-bundle` for being honest about the crate's shape (a bundled collection of chassis bins) without forcing a singular role-name onto a multi-purpose crate.
