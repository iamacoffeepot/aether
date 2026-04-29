# ADR-0067: Test-bench chassis and aether-smoke runner

- **Status:** Proposed
- **Date:** 2026-04-29

## Context

Visual regressions are caught late. Today the loop is:

- The user runs the MCP harness by hand (`spawn_substrate` → `load_component` → `send_mail` → `capture_frame`) to verify a render-touching change.
- `/delegate` agents have no MCP access. Their PRs ship with no visual verification — only structural cargo gates.
- `/sweep` runs smoke prose in PR bodies by hand when the user remembers.
- CI doesn't run smoke at all.

The PR 399 → issue 403 → PR 404 sequence on 2026-04-29 was the canonical failure of this loop: PR 399 deferred mailbox registration past `instantiate`, which silently broke every component's SDK auto-subscribe. Three component PRs landed on top before the regression surfaced through manual MCP smoke. Hours of debug, two follow-up PRs, no test that could have caught it because the existing unit tests exercised `handle_subscribe` against pre-registered mailboxes — never the load → init → subscribe chain end-to-end.

The structural prevention is a render-capable substrate that runs in CI, scriptable in the same vocabulary the agent already knows.

### Why a new chassis

ADR-0035 split the substrate into `desktop` (windowed + GPU + presents), `headless` (no GPU), and `hub` (coordination, no GPU). None render without a window. The desktop chassis renders to an offscreen color target and only blits to the swapchain for presentation, so the rendering path itself doesn't need a window — but the chassis insists on creating one through winit.

`xvfb` + desktop chassis on Linux CI works but is gross, Linux-only, and useless for local agent runs (Mac/Windows agents can't reproduce it). The clean answer is a chassis that mirrors desktop's offscreen path with no window and no winit.

### Why script + library, not script alone

Two consumers want this surface:

1. **Agents and `/sweep`.** Run smoke YAML scripts that mirror the MCP transcript shape they already author by hand. Scripts ship in PR bodies; CI parses and runs them.
2. **Rust integration tests.** Exercise loops, edge cases, error paths, timing — none of which fit a step-and-assert script. They want plain Rust against a library API.

A YAML-only design forces case 2 to either contort (50 YAML files for a parameterized test) or roll its own substrate-driving library (duplicates work). A library-only design forces agents to author Rust, which they can't (no MCP, no compile loop).

The shape that pays for both: a test-bench chassis exposed as both a binary (for `spawn_substrate`-style scripts) and a library (for direct Rust integration tests), with a separate smoke runner crate that adds the YAML/Script DSL on top.

## Decision

### New chassis: `aether-substrate-test-bench`

A fourth chassis sibling to `desktop` / `headless` / `hub`, satisfying the same `Chassis` trait per ADR-0035.

- **GPU init without a window.** wgpu device + queue, no `Surface`, no swapchain, no winit. Same offscreen color + `Depth32Float` textures the desktop chassis already uses for capture. The render pipeline code is shared with desktop where possible (offscreen path is identical; presentation step is omitted).
- **Capture-first.** `aether.control.capture_frame` enabled with the same wire kind as desktop. This is the chassis's primary observable.
- **Window operations reply `Err`.** `set_window_mode`, `set_window_title`, `platform_info` mirror the headless pattern: well-defined `Err { error: "unsupported on test-bench chassis" }` so callers fail fast.
- **Sink set: `aether.sink.{render, camera, io, log}`.** `aether.sink.audio` is omitted — smoke tests don't need cpal init, and skipping it removes a build dependency and a flaky-driver surface on CI runners. Tests that need audio assertions ship that as v2.
- **Tick driver: control-mail.** `aether.test_bench.advance { ticks: u32 }` advances the mail clock by `ticks`; replies `aether.test_bench.advance_result` once the requested ticks are dispatched. No std-timer free-running — that races with mail dispatch and produces flaky captures. Deterministic "send mail → step N → capture" cycles are the whole point.
- **Driver fallback policy** (boring but pinned so CI is one copy-paste away):
  - macOS: Metal.
  - Linux: Vulkan with `mesa-vulkan-drivers` (lavapipe) as the CI fallback. CI workflow runs `apt-get install -y mesa-vulkan-drivers libvulkan1` before the test step.
  - Windows: DX12.

### Test-bench library API

`aether-substrate-test-bench` is a workspace member with both `[lib]` and `[[bin]]` targets. The bin is a thin wrapper that satisfies `spawn_substrate`'s contract; the lib is the actual chassis driver and is what Rust integration tests link.

```rust
use aether_substrate_test_bench::TestBench;

let mut tb = TestBench::start()?;             // boot the chassis in-process
let mbx = tb.load_component(wasm_bytes, "viewer")?;
tb.send(mbx, MeshLoad { namespace: "assets".into(), path: "box.dsl".into() })?;
tb.advance(1)?;                                // dispatch one tick worth of mail
let frame = tb.capture()?;                     // PNG bytes + metadata
assert!(frame.non_bg_pixels() > 100);
```

The `TestBench` handle owns a `SubstrateCore` and a `TestBenchChassis`; dropping it tears down cleanly. `advance` and `capture` block on the corresponding control-plane reply. Methods return `Result` so library tests can `?` through them.

### `aether-smoke` runner

A separate crate providing the YAML grammar and script orchestration on top of `aether-substrate-test-bench` (and, for full-integration mode, on top of `aether-substrate-hub` as a library):

- `aether-smoke` (lib): `Script`, `Runner`, `RunReport`, YAML parser, in-process driver. No process boundary by default.
- `aether-smoke-cli` (bin): `cargo run -p aether-smoke-cli -- script.yaml`. Thin wrapper: parse argv, call `Runner::run_yaml`, format `RunReport` for terminal, exit code from result.

### Smoke YAML grammar

Vocabulary mirrors MCP tool names so an agent's smoke script reads like the transcript they would have driven by hand:

```yaml
- tool: spawn_test_bench         # optional; Rust tests skip it
  id: e1
- tool: load_component
  args: { engine_id: "{{ e1.engine_id }}", binary_path: "target/wasm32-unknown-unknown/release/aether_mesh_viewer_component.wasm" }
  id: viewer
- tool: send_mail
  args:
    - { engine_id: "{{ e1.engine_id }}", recipient_name: "{{ viewer.name }}", kind_name: "aether.mesh.load",
        params: { namespace: "assets", path: "box.dsl" } }
- tool: advance
  args: { engine_id: "{{ e1.engine_id }}", ticks: 1 }
- assert: capture_frame
  args: { engine_id: "{{ e1.engine_id }}" }
  match: { not_all_black: true, min_non_bg_pixels: 100 }
```

- **Step ids + `{{ id.field }}` substitution** for cross-step references (engine ids, mailbox ids, names).
- **Spawn step optional.** YAML scripts default to `spawn_test_bench` (full-integration; matches agent transcript). Rust tests using the runner skip it; the runner detects no-spawn-step and instantiates the chassis in-process.
- **Visual asserts: `not_all_black`, `min_non_bg_pixels`, `dominant_color_region`.** Coarse only. No pixel-exact diff in v1 — cross-driver flake is the canonical reason. Pixel-exact-per-platform-golden may return as v2.
- **Non-zero exit / panic on first failed assert.** No "continue past failures" mode in v1.

### `RunReport` and CI artifacts

The runner returns a `RunReport` with per-step success/failure, per-assert outcome, and on failure, persisted artifacts:

```
target/aether-smoke-artifacts/<test-name>/
├── step-3.png       # captured frame at point of failure
├── engine.log       # drained engine_logs at failure
└── report.json      # machine-readable per-step outcome
```

`report.assert_passed()` panics with the artifact directory in the message so `cargo test` failures point straight at the PNG. CI uploads `target/aether-smoke-artifacts/**` as a GitHub Actions artifact on failure (one workflow line); the PR author downloads it and sees the actual broken frame.

### Test discovery: per-component, proc-macro

```rust
// crates/aether-mesh-viewer-component/tests/smoke.rs
aether_smoke::smoke_dir!("tests/smoke");
```

The `smoke_dir!` proc-macro emits one `#[test] fn` per `.yaml` in the directory at compile time. Smoke files live next to the code they verify. `cargo test --workspace` picks them all up. A component's smoke breaks → that component's test fails. Ownership is unambiguous.

A component-agnostic corpus crate (`crates/aether-smoke-corpus`) hosts cross-cutting tests that span multiple components — boot the substrate, load camera + viewer + a player, verify the integrated frame.

### Smoke vs direct test-bench: the heuristic

Both layers ship and are first-class. The split:

- **Smoke (YAML or Script builder)**: transcript-shaped tests — boot, load, send mail, capture, assert. Same shape an agent would author. One source of truth across agent / `/sweep` / CI.
- **Direct `TestBench` API**: loops, parameterization, race conditions, mail ordering, error paths, anything where Rust expressiveness pays. Plain `cargo test` against the test-bench library, no smoke layer.

Authors choose based on test shape. If removing the YAML and rewriting in Rust would make the test clearer, skip smoke. If the YAML *is* the test (because that's what the agent would have written), use smoke.

### Skill updates

- **`/delegate`**: agent prompt grows a "leave a smoke block in the PR body" instruction. The smoke block is YAML in a fenced ` ```smoke ` block; agents author it from the change description.
- **`/sweep`**: parses smoke blocks of merged PRs since the last sweep, runs them via `aether-smoke-cli`, surfaces failures.

## Consequences

**Positive**

- Visual regressions caught in CI per-PR, not after-the-fact during a sweep cycle. The PR 399-shaped failure costs minutes (red CI), not hours (manual smoke discovery).
- `/delegate` agents can leave runnable smoke artifacts. Three of three delegated PRs in the most recent batch shipped with no visual verification; that floor lifts to "every PR ships with at least one smoke."
- One source of truth for transcript-shaped tests across agent / `/sweep` / CI. No diverging vocabularies between "what the agent wrote in chat" and "what runs in CI."
- Rust integration tests get a substrate-driving library they didn't have before. The test-bench `TestBench` API is reusable by any crate that needs to instantiate the chassis in-process.
- ADR-0035's chassis split absorbs the addition cleanly. The fourth chassis is a sibling, not a refactor.

**Negative**

- Linux CI gains a `mesa-vulkan-drivers` install step. One-time cost; documented in the ADR and CI workflow.
- Two new crates to maintain (`aether-substrate-test-bench`, `aether-smoke`) plus `aether-smoke-cli` as a workspace bin. Workspace handles this fine; review velocity costs are the new YAML grammar and the proc-macro test discovery.
- Smoke YAML grammar is a new authoring surface. Versioning is implicit until v2; breaking grammar changes will need a migration story.
- `aether-substrate-hub` linked as a library (for full-integration smoke runs that include `spawn_substrate`) means the hub crate must build clean as both bin and lib. Likely already does, but confirms a constraint.
- Non-zero ramp-up: the first few smokes will be authored by hand to seed the corpus before agents start producing them via `/delegate`.

**Neutral**

- Test-bench's offscreen render path duplicates desktop's at the chassis seam. ADR-0035's `Chassis` trait already isolates the difference; the duplication is at the crate-targets layer, not the render code itself, which is shared. A future refactor could route desktop through the test-bench codepath under the hood — out of scope for v1.
- `aether-substrate-test-bench` shares the binary-spawn code path of `aether-substrate-desktop` for `spawn_substrate` integration. The hub treats it like any other chassis binary.

**Follow-on work**

- New crate `aether-substrate-test-bench` (lib + bin); offscreen wgpu init, control-mail tick driver, sinks `{render, camera, io, log}`.
- New crate `aether-smoke` (lib); `Script`, `Runner`, `RunReport`, YAML parser, `smoke_dir!` proc-macro.
- New crate `aether-smoke-cli` (bin); thin argv wrapper.
- New optional crate `aether-smoke-corpus`; cross-component smokes that don't fit any single component's `tests/smoke/`.
- Add `aether.test_bench.{advance, advance_result}` kinds to `aether-kinds` (chassis primitives per ADR-0066).
- Per-component first-pass smokes: at minimum `aether-mesh-viewer-component` (load box.dsl, capture, assert non-black) and `aether-camera-component` (orbit one tick, assert frame stable).
- CI workflow update: install `mesa-vulkan-drivers` on Linux; run `cargo test --workspace` with the test-bench targeted; upload `target/aether-smoke-artifacts/**` on failure.
- `/delegate` skill: smoke-block-in-PR-body instruction.
- `/sweep` skill: parse + run smoke blocks of merged PRs.
- ADR amendment to ADR-0035 noting the four-way chassis split (or this ADR is treated as the amendment by reference).

## Alternatives considered

- **xvfb + desktop chassis on Linux CI.** Rejected — Linux-only, doesn't help local agent runs on Mac/Windows, and `xvfb` adds a chassis-external dependency that masks failures (e.g., a real wgpu init regression hides behind an xvfb env issue).
- **Free-running tick driver (std timer, like headless).** Rejected — races with mail dispatch in deterministic capture flows. The whole point of the chassis is reproducibility; a non-deterministic clock undermines it.
- **Pixel-exact frame diffs in v1.** Rejected — cross-driver flake (Metal vs Vulkan vs DX12 produce visibly identical but byte-distinct frames). Coarse asserts (`not_all_black`, `min_non_bg_pixels`) cover the regression class that motivates the ADR. Per-platform golden images may return as v2 if coarse asserts prove too loose.
- **TOML or s-expression smoke grammar.** Rejected — TOML hates nested arrays (every `send_mail` step is an array of mail items), s-expression is overkill and hostile to non-Rust authors. YAML is the human-authoring sweet spot and matches CI-config conventions agents already see.
- **MCP-client transport for v1 (smoke runner shells out to a real `aether-substrate-hub` over the MCP wire).** Rejected — slower, more failure modes (socket setup, auth, JSON-RPC framing), and offers no reuse benefit when the in-process hub library suffices. Deferred to v2 if a transport-validation story emerges.
- **Centralized smoke corpus instead of per-component test discovery.** Rejected — coupling cost is high, ownership is unclear when smoke fails (which crate owns this YAML?), and component crates lose the locality property that makes their `tests/` directory authoritative for their own behavior. Cross-cutting smokes get a separate corpus crate; component-specific smokes stay with their component.
- **Skip the smoke library entirely; integration tests roll their own substrate-driving boilerplate.** Rejected — duplicates the chassis-driving harness across N component crates. The lib pays for itself within two test files. Direct `TestBench` API is still available for tests that don't fit the smoke shape; the smoke runner is additive, not mandatory.
- **Use the desktop chassis as the test-bench (skip a new crate).** Rejected — desktop's winit dependency drags in display-server requirements (xcb/X11/Wayland on Linux, headed AppKit on Mac for the test runner) that CI runners don't reliably provide. Stripping winit out of desktop is a larger refactor than adding a sibling chassis.

## References

- Issue 400 — feature request and conversation chain that led to this ADR.
- ADR-0008 — observation path; smoke scripts assert via `receive_mail`.
- ADR-0035 — substrate-chassis split; `desktop` / `headless` / `hub`. This ADR adds `test-bench` as the fourth.
- ADR-0066 — per-component trunk rlibs; `aether.test_bench.*` kinds live in `aether-kinds` as chassis primitives.
- `mcp__aether-hub__capture_frame` — current visual capture surface; smoke YAML's `capture_frame` step calls into the same control-plane kind.
