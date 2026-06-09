# CLAUDE.md

Guidance for Claude Code (claude.ai/code) working in this repository.

## Status

Pre-1.0 Rust project (edition 2024). Vision: a game engine where Claude sits in a harness as assistant/engineer/designer. A thin native **substrate** owns I/O, GPU, and audio and hosts a WASM runtime; engine **actors** — wasm components and native chassis capabilities — run on it and communicate only by **mail**. "Aether" / "the engine" is the whole system; the substrate is the native base layer. Load-bearing design is recorded as ADRs in `docs/adr/NNNN-title.md` (use `docs/adr/TEMPLATE.md` to start one) — read the cited ADR before changing a subsystem.

## Architecture & crates

Infrastructure (non-actor) crates:

- **`aether-data`** — universal data layer (`no_std` + `alloc`). Typed-id newtypes (`MailboxId`, `KindId`, `HandleId`), wire identity (`EngineId`, `SessionToken`, `Uuid`), schema vocabulary (`SchemaType`, `KindShape`, `KindLabels`), the `Kind` / `Schema` traits, `Ref<K>`, encode/decode helpers, and the native descriptor + transform inventories. Everything that describes typed bytes depends on it. Its proc macros (`#[derive(Kind, Schema)]`, `#[transform]`) live in `aether-data-derive`.
- **`aether-codec`** — schema-driven JSON ↔ wire bytes over `SchemaType` (`encode_schema` / `decode_schema`) plus length-prefix postcard stream framing (`frame::*`, ADR-0072).
- **`aether-kinds`** — the substrate kind vocabulary: `Tick`, `Key`, `WindowSize`, `DrawTriangle`, and the `aether.{audio,fs,render,window,input,component,camera,log,handle,dag}.*` families.
- **`aether-math`** — `Vec2/3/4`, `Mat4`, `Quat`, `Aabb` (column-major, YXZ Euler, right-handed Y-up, `f32`, `no_std`). Reach for it before hand-rolling `cross` / `dot` / `normalize` / aabb checks; add missing domain-agnostic primitives here, not locally. `[f32; 3]` survives at wire boundaries via `Vec3::from_array` / `to_array`.

Runtime + chassis (ADR-0073): the shared runtime is **`aether-substrate`**; native capabilities live in **`aether-capabilities`**. All four chassis live in **`aether-substrate-bundle`** as `src/{desktop,headless,hub,test_bench}/` submodules with `src/bin/{desktop,headless,hub,test-bench}.rs` entry points; the hub library (substrate-side client, wire types, MCP coordinator) is `src/hub/`, and the hub channel wire vocabulary (`EngineToHub`, `HubToEngine`, `Hello`, `MailFrame`) is `aether-substrate-bundle::hub::wire`.

The guest/actor SDK is **`aether-actor`** — the `Actor` / `FfiActor` traits, `Mailbox<K>`, `FfiCtx`, the `#[actor]` macro, and `export!` (proc macros in `aether-actor-derive`). See [Writing components](#writing-components).

## Workflow

- **Exploration and design discussion** happens in chat with the user. No artifact required.
- **Planned work** (spikes, features, open investigations) lives in GitHub Issues. Referenced by the PR that closes them.
- **Load-bearing architectural decisions** are recorded as ADRs in `docs/adr/NNNN-title.md`. Use `docs/adr/TEMPLATE.md` when starting a new one. Number sequentially. An ADR is reviewed via a PR like any other change.
- **Branches**: `type/short-slug` (e.g. `chore/ci-bootstrap`, `feat/mail-runtime`, `docs/adr-workflow`).
- **Worktrees** live under `./.claude/worktrees/` (gitignored), never as siblings of the repo. `git worktree add .claude/worktrees/<slug>` — the path is already excluded so the worktree never shows up in `git status`.
- **Commits and PR titles** follow Conventional Commits (`type(scope): subject`). Enforced in CI against PR titles. Main uses squash-merge with the PR title as the commit subject, so PR title quality matters.
- **Merging**: `main` is protected (PR required, all CI checks required, linear history, no force-push). Claude does not push to `main`, does not force-push reviewed branches, does not self-merge, and asks before destructive operations.
- **PRs** should be small and focused — one concept per PR.
- **Recursion in load-bearing code**: prefer iterative implementations (explicit work-stack/queue, arena-with-indices for tree data) over recursive ones in any algorithm whose depth could exceed a few hundred frames in practice. Recursion is OK for parse/AST walks where depth is structurally bounded by a small input file. Either way, recursive code on user-controlled or geometrically-derived data must enforce a depth/budget cap that returns an error rather than overflowing the stack.
- **Naming — units and types**: spell units out in identifiers (`millis`, `nanos`, `micros`, `secs`, `bytes`), never the two-letter abbreviation (`ms`, `ns`, `us`, `kb`) — two letters is the ambiguous zone (`ms` reads as milliseconds *or* movement-speed). Longer well-known abbreviations are fine. And don't encode a value's Rust type in its name (`u32` / `u64` / `usize`) — the signature already states it. E.g. `parse_u32_ms_strict` → `parse_millis_strict`.

## Commands

- Build: `cargo build` (release: `cargo build --release`)
- Run: `cargo run -p <crate>` — workspace root has no default binary. The chassis binaries all live in `aether-substrate-bundle`; pick one with `--bin`: `cargo run -p aether-substrate-bundle --bin aether-substrate-hub`, `--bin aether-substrate` (desktop), or `--bin aether-substrate-headless`.
- Test: `cargo test` (single test: `cargo test <name>`; single-threaded with output: `cargo test -- --nocapture --test-threads=1`)
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt` (check-only: `cargo fmt -- --check`)
- Type/borrow check only: `cargo check`

## MCP harness

Claude drives a running engine through MCP — the concrete form of the "Claude-in-harness" vision. The harness is the out-of-process **`aether-mcp`** crate: an RPC client that dials the hub's `RpcServerCapability` and relays each tool call as a wire `Call`. The whole stack is fronted by a long-lived **tunnel** (`aether-tunnel`, ADR-0089) so Claude can restart the volatile backends without losing its MCP connection. Three processes, one nested under the other:

```
:8890  aether-tunnel  — stable MCP front; reverse-proxies /mcp, supervises the two below
:8891  aether-mcp     — forked by the tunnel; dials + re-dials the hub
:8901  aether-substrate-hub — forked by the tunnel; the RPC server the fleet talks to
```

The tunnel binds `:8890` (the port `.mcp.json` targets) and forks `aether-mcp` (`AETHER_MCP_PORT=8891`) and the hub (`AETHER_RPC_PORT=8901`, reached via `AETHER_HUB_RPC_ADDR`) itself. Bring the stack up by running `scripts/ensure-tunnel.sh` yourself when you need the MCP harness — it is idempotent (a no-op if `:8890` is already bound, otherwise launches the tunnel detached). It is *not* auto-started on session start: a cold `cargo` build of the tunnel can take long enough to look like a frozen session, so the launch is left to the point of use. Env overrides: `AETHER_TUNNEL_PORT` (default 8890), `AETHER_MCP_PORT` (default 8891), `AETHER_RPC_PORT` / `AETHER_HUB_RPC_PORT` (hub, default 8901). Get a substrate with `spawn_substrate`; the hub forks it, assigns its RPC port, and tracks the fleet via its `aether.engine` cap.

To restart the hub after a rebuild **without dropping the MCP session**, hit the tunnel's out-of-band admin endpoint from a shell — `curl -fsS -X POST http://127.0.0.1:8890/admin/restart-hub`. The tunnel SIGTERMs + re-forks the hub child; `aether-mcp` (and Claude's MCP session) stay up and re-dial the fresh hub on the next tool call. `GET http://127.0.0.1:8890/admin/status` reports child liveness + the resolved ports. Restarting `aether-mcp` itself is rare and *does* invalidate the MCP session (Claude re-initialises) — prefer `restart-hub`.

Tools (`mcp__aether-hub__*`):

- `list_engines` — every engine the hub supervises: `{engine_id, rpc_port, last_heartbeat_age_millis}`. The hub heartbeats each engine (`AETHER_HUB_HEARTBEAT_INTERVAL_SECS` / `_MISS_LIMIT`, default 5 s × 3) and evicts a dead/wedged one from this list once it crosses the miss limit, so a listed engine is live (ADR-0090 / issue 1339); `last_heartbeat_age_millis` shows staleness short of eviction.
- `spawn_substrate(binary_path, args?)` — fork+exec a substrate (RPC port injected as `AETHER_RPC_PORT`). Returns `{engine_id, rpc_port, last_heartbeat_age_millis}` (`0` — just spawned).
- `terminate_substrate(engine_id)` — the engine's proxy SIGKILLs the child and self-shuts-down.
- `send_mail(mails)` — best-effort batch; each item `{engine_id, recipient_name, kind_name, params}` is schema-encoded against the substrate vocabulary and routed as a wire `Call`. Per-item status.
- `send_mail_traced(engine_id, mails, settlement_timeout_ms?)` — atomic batch under one shared trace root (the `aether.trace` mailbox); returns the full trace subtree once the chain settles, no window guessing (ADR-0080/0086). A bad spec aborts the whole batch.
- `describe_kinds` — the static substrate kind vocabulary with full schemas (enough to build `send_mail` params; component-defined kinds aren't included — use `describe_component` for those).
- `describe_component(engine_id, mailbox_id)` — a loaded component's handler kinds + per-handler docs + `#[fallback]` presence (ADR-0033). Reads aether-mcp's cache, populated by `load_component` / `replace_component`.
- `describe_transforms` — native `#[transform]` functions linked at build time: `transform_id`, name, input/output kind ids (ADR-0048).
- `describe_handles(engine_id, max?)` — the persistent handle store's entry counts, bytes vs disk budget, and top-N handles by size / recency (ADR-0049).
- `load_component(engine_id, binary_path, name?)` — forwards `aether.component.load` to the engine's `aether.component` mailbox, awaits `LoadResult`. The component's kind vocabulary rides in the wasm's `aether.kinds` custom section (ADR-0028/0032).
- `replace_component(engine_id, mailbox_id, binary_path, drain_timeout_ms?)` — ADR-0022 in-place wasm swap; awaits `ReplaceResult` (`drain_timeout_ms` accepted for wire compatibility but ignored).
- `capture_frame(engine_id, mails?, after_mails?)` — synchronous PNG readback, returned inline as image content. `mails` dispatch before readback (state that should appear), `after_mails` after (cleanup); a bad bundle entry aborts the capture.
- `actor_logs(engine_id, mailbox_name, max?, level?, since?)` — recent entries from one actor's per-actor log ring (ADR-0081). Any mailbox is queryable (`"aether.audio"`, `"aether.component/aether.embedded:camera"`). `max` defaults 100 / clamps 1000; `level` filters server-side; `since` paginates via the prior call's `next_since`. Only in-actor `tracing::*` events flow into the rings (host events go to stderr). Filter with `AETHER_LOG_FILTER` (`EnvFilter` syntax; default `info`).
- `submit_dag(engine_id, descriptor, timeout_ms?)` — submit a computation DAG (ADR-0047). Validation runs synchronously (returns `{dag_id, output_handles}` or an immediate `DagError`); sources execute async after the ack. Large source payloads stage via a `payload_path` instead of inline bytes.
- `dag_status(engine_id, dag_id)` — poll a DAG: `Pending` / `Running` / `Complete` / `Failed`.
- `dag_cancel(engine_id, dag_id)` — cancel an in-flight DAG.

When verifying substrate behavior end-to-end, reach for the MCP harness before writing a new test binary.

## Test bench (ADR-0067)

For Rust integration tests / CI gating without a live MCP session, **`aether_substrate_bundle::test_bench`** drives the same in-process substrate from a Rust thread. `TestBench::start()` (or `TestBench::builder().size(w, h).build()`) boots a full chassis (scheduler, mail queue, wgpu offscreen render target) with a loopback channel so substrate replies route back without a hub. Drive it with a sequence of `Step`s — `Step::advance(ticks)`, `capture()`, `capture_with_mails(pre, after)`, `send_mail::<K>(recipient, &mail)`, `send_and_await::<K>(recipient, &mail)` — and inspect the returned report (`captured(label)`, `reply::<R>(label)`, `count_observed(kind)`). `send_mail` encodes params the same path as the MCP tool, so any substrate kind is sendable.

Scenarios need a wgpu adapter; CI installs `mesa-vulkan-drivers` on Linux runners and pre-builds component wasm before `cargo test`. Driverless dev boxes skip cleanly. Reach for the test bench for repeatable `cargo test` verification; the MCP harness for exploratory / live observation.

Wasm components are discovered structurally (issue 439): a `cargo metadata` package with `crate-type = cdylib` AND a dependency on **`aether-actor`**. Both signals are structural — no filename convention.

## Runtime & subsystems

Each subsystem's design lives in its ADR; below is the operational surface — what to mail where. Mail is fire-and-forget unless a reply kind is noted.

- **Input streams** (ADR-0021/0068) — tick / key / mouse / window-size are publish/subscribe, keyed by `KindId`; the substrate drops events until something subscribes. A component subscribes from its `wire` hook (see [Writing components](#writing-components)); the reference `aether-camera` subscribes `Tick` and `WindowSize`. Subscriptions clear on drop and survive `replace_component` (the mailbox id is stable).
- **Component lifecycle** — `aether.component.{load,drop,replace}` on the `aether.component` mailbox. `replace` swaps the wasm Module in place behind a stable mailbox handle (ADR-0022 + ADR-0038), so the mailbox id and any route cache stay valid.
- **Window** (desktop only, ADR-0035) — mail `aether.window.set_mode` (`Windowed { width?, height? }` / `FullscreenBorderless` / `FullscreenExclusive { width, height, refresh_mhz }`), `aether.window.set_title`, or `aether.window.focus` (no fields — un-minimize + show + raise the window to the front, e.g. so a backgrounded window can be `capture_frame`d) to the `aether.window` mailbox; each replies with the value actually applied. Boot overrides: `AETHER_WINDOW_MODE` (`windowed` | `windowed:WxH` | `fullscreen-borderless` | `exclusive:WxH@HZ`), `AETHER_WINDOW_TITLE`. Window ops and `capture_frame` are desktop-only; the headless chassis replies `Err` (fail-fast) rather than hanging.
- **Headless chassis** (ADR-0035) — ticks from a std timer (`AETHER_TICK_HZ`, default 60). Same hub client / mail scheduler / component surface as desktop; a nop `aether.render` mailbox absorbs `DrawTriangle` + `aether.camera` so desktop-built components don't warn-storm.
- **Rendering & camera** (ADR-0066, ADR-0074 §7) — vertex geometry is world-space; the substrate applies a 4×4 `view_proj` uniform (column-major, defaults to identity). A camera publishes `aether.camera { view_proj: [f32; 16] }` to the `aether.render` mailbox (latest wins). Depth test is on (`Depth32Float`, `LessEqual`): larger world-z draws on top, so floors / backdrops sit at `z = 0` and movers at `z ≥ 0.1`. Reference: `aether-camera` (multi-camera / multi-mode; driver kinds `aether.camera.{create,destroy,set_active,set_mode,orbit.set,topdown.set}`).
- **Mesh authoring** — load the `aether-mesh-viewer` component, then mail `aether.mesh.load { namespace, path }` to it: it fetches the file via `aether.fs`, dispatches on extension (`.dsl` → `aether-mesh`'s parser + mesher with polygon wireframes; `.obj` → built-in fan-triangulation), atomically swaps the cached triangle list (a parse / mesh failure keeps the previous mesh and logs the error), and replays it to `aether.render` every tick. Agent loop: `aether.fs.write` the DSL text, then `aether.mesh.load` the same path. DSL vocabulary + palette: ADR-0026 / ADR-0051; examples in `crates/aether-mesh/examples/` (box, lamp_post, teapot).
- **File I/O** (ADR-0041) — mail `aether.fs.{read,write,delete,list}` to the `aether.fs` mailbox. Three namespaces, addressed by short prefix (`"save"`, not `"save://"`): `save` (writable, per-user persistent), `assets` (read-only), `config` (writable). Replies echo `namespace` + `path` (or `prefix`) for correlation; `FsError` is `NotFound` / `Forbidden` / `UnknownNamespace` / `AdapterError`. Env: `AETHER_SAVE_DIR`, `AETHER_ASSETS_DIR`, `AETHER_CONFIG_DIR`. Desktop + headless only.
- **Audio** (desktop only, ADR-0039) — fire-and-forget `aether.audio.{note_on,note_off,set_master_gain}` to the `aether.audio` mailbox. Five built-in instruments; mixing / effects are user-space. Env: `AETHER_AUDIO_DISABLE`, `AETHER_AUDIO_SAMPLE_RATE`.
- **Computation DAG & handles** (ADR-0045/0047/0049) — large or async work runs as a DAG of `Kind → Kind` transforms producing typed `Ref<K>` handles in a persistent store. Drive it through the `submit_dag` / `dag_status` / `dag_cancel` MCP tools; inspect the store with `describe_handles`.

**Recipient-name convention.** `recipient_name` names the *mailbox*; `kind_name` names the *payload shape*. They often share a leading prefix but route independently — send `aether.audio.note_on` (kind) to `aether.audio` (mailbox), `aether.draw_triangle` to `aether.render`, or `aether.camera.destroy` to `aether.component/aether.embedded:cam`. Chassis-owned mailboxes live under `aether.<name>`: `aether.render`, `aether.audio`, `aether.fs`, `aether.http`, `aether.handle`, `aether.input`, `aether.window`, `aether.component`. A loaded wasm component registers at `aether.component/aether.embedded:NAME` (the `/`-rendered lineage, ADR-0099) — `LoadResult.name` returns the full address; send subsequent peer/runtime mail to that string. Bare names (`"camera"`, `"player"`) are not registered and warn-drop.

## Writing components

A component is an `Actor` whose receive side is declared with the **`#[actor]`** attribute macro on one `impl FfiActor for C` block (ADR-0033 / ADR-0074):

```rust
#[actor]
impl FfiActor for CameraComponent {
    const NAMESPACE: &'static str = "camera";              // default load name

    fn init<C: Resolver>(ctx: &mut C) -> Result<Self, BootError> { /* build state */ }

    fn wire(&mut self, ctx: &mut FfiCtx<'_>) {              // post-init, mail allowed
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>(); // subscribe the calling actor
    }

    #[handler]
    fn on_tick(&mut self, ctx: &mut FfiCtx<'_>, _t: Tick) {
        ctx.actor::<RenderCapability>().send(&Camera { view_proj });
    }
}
aether_actor::export!(CameraComponent);                    // required; emits wasm32-only FFI shims
```

- **Handlers**: each `#[handler] fn on_x(&mut self, ctx: &mut FfiCtx<'_>, mail: K)` infers the kind from its third parameter — no typelist, no `is::<K>()`. An optional `#[fallback] fn(&mut self, ctx, mail: Mail<'_>)` catches everything else; omit it for a strict receiver. `#[actor]` codegens the dispatch table and emits the `aether.kinds.inputs` custom section that `describe_component` surfaces.
- **Lifecycle**: `init` (its ctx is `Resolver`-only — no mail yet), `wire` (mail allowed; subscribe to input here), `unwire` (teardown; the old `on_drop` is retired). Hot reload is opt-in via the `Replaceable` subtrait (`on_replace` / `on_rehydrate`) plus `export!(C, replaceable)`; state persistence is a host fn on the `Persistence` ctx trait.
- **Sends & addressing**: address another actor by type — `ctx.actor::<RenderCapability>().send(&kind)` — or hold a `Mailbox<K>` token. `Kind::ID` and `mailbox_id_from_name` are compile-time consts, so there is no host round-trip for address resolution. The FFI keeps a `_p32` suffix on pointer-typed exports (`receive_p32`, `on_rehydrate_p32`) for the wasm32 / wasm64 path (ADR-0024).
- **Kind types**: `#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]` with `#[kind(name = "…")]`. A component and the peers that talk to it share the kind crate (ADR-0066); under the `runtime` feature the same crate emits the cdylib via `export!`.

## Pre-push pre-flight

`scripts/preflight.sh` runs the CI-equivalent local checks (fmt + clippy + doc + nextest + wasm32 component cross-build) over the workspace and, on success, stamps `.git/aether-preflight-passed` with the HEAD sha so a re-push of the same commit short-circuits. The pre-push git hook (`.githooks/pre-push`) invokes it automatically against the changed-file set. Enable once per clone: `scripts/setup-githooks.sh` (sets `core.hooksPath -> .githooks`).

Exception classes that skip the Rust pre-flight (only when *every* changed path matches the class):

- **Docs-only**: `docs/**` or `*.md` at the root.
- **CI / repo-config-only**: `.github/**`, `.claude/**`, `.githooks/**`, `scripts/**`, `qodana.{yaml,sarif.json}`, `.mcp.json`, `.gitignore`, `.gitattributes`, or `{rust-toolchain,rustfmt,clippy}.toml`.

A Claude-side hook (`.claude/hooks/check-pre-push.sh`) checks the stamp ahead of `git push` / `gh pr create`. If HEAD has no matching stamp it blocks and prompts Claude to run `scripts/preflight.sh` plus `mcp__rustrover__get_file_problems` over each changed `.rs` file (the qodana-equivalent the shell-only git hook can't reach). Bypass either layer with `git push --no-verify`.

## Qodana pre-flight (local)

Qodana gates merges via the `ci-pass` aggregator. Run the **same scan CI submits**, locally, with `scripts/qodana-local.sh` (issue 1099). The naive `qodana scan` times out because it bind-mounts the repo over colima's virtiofs and Qodana's `cargo metadata` pass does thousands of small reads there; the script sidesteps that the way CI does — it syncs the working tree into a Docker named volume (the VM's native fs) and keeps a persistent cache volume for the bootstrapped toolchain + analysis caches. Same linter image / `qodana.recommended` profile / `--fail-threshold 0` as the CI job, reading the same `qodana.yaml`.

- **Run it**: `colima start` first (the script won't auto-boot a cold VM), then `scripts/qodana-local.sh`. ~3.3min warm; SARIF + HTML report land in `./.qodana-local/` (gitignored). Exit code is Qodana's gate (non-zero = findings). `--rebuild-cache` drops the cache volume.
- **Fidelity**: validated against a main CI run — local reproduced 19/22 findings exactly (`RsUnnecessaryQualifications`, `DuplicatedCode`, `RsUnusedImport`, `CargoUnusedDependency` all matched). The one offline gap is `NewCrateVersionAvailable` (queries crates.io; needs `QODANA_TOKEN`) — export the token to close it, else it stays CI-only. Note `CargoUnusedDependency` **does** run locally.

For a quick single-file check without spinning up the container, RustRover's inspector rehosts most of the same checks:

- **Agent-side**: `mcp__rustrover__get_file_problems` (`errorsOnly: false`) returns the problem set for one file. Iterate it over `git diff --name-only` for a fast pre-commit pass on Qodana-touched files. RustRover MCP has no project-wide runner — use `scripts/qodana-local.sh` (or the IDE's `Code → Inspect Code…`) for whole-project.
- **Per-file in-IDE**: the gutter + Problems tool window surface the same inspection ids (`RsUnnecessaryParentheses`, `RsApproxConstant`, `DuplicatedCode`, …).

Re-baselining (`qodana.sarif.json`) is done by downloading the `qodana-report` workflow artifact from a CI run.

## Heavy (contention-sensitive) tests

Concurrent / scheduler / mail-dispatch tests are timing-flaky — they pass on a lucky run and fail intermittently, and under a saturated `--workspace` run they oversubscribe cores so a settlement that needs timely cross-thread progress can miss its ~30s deadline (passing in isolation but wedging the full suite). A single green CI pass does not clear one.

Mark such a test by declaring it inside a **`mod heavy`** submodule of its test module — e.g. `mod heavy { #[test] fn lost_wakeup_stress() { … } }` (shared helpers stay in the parent `mod tests`, reached via `use super::*`; or keep a body fn at module scope and delegate with `super::body()`). `nextest` selects by name/path, so the `::heavy::` path segment is the marker — no macro, no `flaky_` duplicate (iamacoffeepot/aether#1341 retired the old duplicate-wrapper convention). The marker serializes the set:

- **Serialize in every run.** `.config/nextest.toml` puts `test(/::heavy::/)` in the `serial-heavy` test-group (`max-threads = 1`), so the heavy set runs single-file — never contending with one another — while the thousands of lightweight tests keep saturating cores. The override is repeated on the `default` and `ci` profiles (profiles don't inherit). Verify membership with `cargo nextest show-config test-groups --profile ci`.

Serialization is the whole mechanism: it removes the cross-test contention that wedges these under a saturated `--workspace` run. A single green run still doesn't fully clear a contention test, but the guard for that is the serialization plus the full-suite run, not a repeat-soak. To chase a *suspected* intrinsic race (one that flakes even when run alone), repeat it in isolation ad-hoc with `cargo nextest run --stress-count <N> -E 'test(<name>)'`.
