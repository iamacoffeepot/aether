# CLAUDE.md

Guidance for Claude Code (claude.ai/code) working in this repository. This file holds the rules and a map; the reference material lives in the **`docs/guide/`** mdbook (`docs/guide/SUMMARY.md` is the entry point), and the code wins where the two disagree.

## Status

Pre-1.0 Rust project (edition 2024). Vision: a game engine where Claude sits in a harness as assistant/engineer/designer. A thin native **substrate** owns I/O, GPU, and audio and hosts a WASM runtime; engine **actors** — wasm components and native chassis capabilities — run on it and communicate only by **mail**. "Aether" / "the engine" is the whole system; the substrate is the native base layer. Load-bearing design is recorded as ADRs in `docs/adr/NNNN-title.md` (use `docs/adr/TEMPLATE.md` to start one) — read the cited ADR before changing a subsystem. The contributor and agent reference for using and extending aether is the `docs/guide/` mdbook; CI builds and deploys it to GitHub Pages.

## Architecture & crates

Infrastructure (non-actor) crates:

- **`aether-data`** — universal data layer (`no_std` + `alloc`): typed-id newtypes (`MailboxId`, `KindId`), wire identity, schema vocabulary (`SchemaType`), the `Kind` / `Schema` traits, encode/decode helpers, and the native descriptor + transform inventories. Its proc macros (`#[kind(name = …)]`, `#[derive(Kind, Schema)]`, `#[transform]`) live in `aether-data-derive`.
- **`aether-codec`** — schema-driven JSON ↔ wire bytes (`encode_schema` / `decode_schema`) plus length-prefix stream framing (ADR-0072) over the workspace's own `aether_data::wire` format (ADR-0118).
- **`aether-kinds`** — the substrate's own mail vocabulary: input events (`Key`, `KeyRelease`, mouse, `TextInput`, `ImePreedit`, `WindowSize`), lifecycle stages and subscription (`Tick`, `Render`, `aether.lifecycle.*`), component lifecycle (`aether.component.*`), fleet (`aether.fleet.*`), the log / trace / cost tail queries, diagnostics (`aether.mail.unresolved`, `aether.actor.monitor_notice`), `Ping` / `Pong`, and the frame-capture request. A kind owned by one capability lives in that capability's crate — `aether.draw_triangle` and the `aether.render.*` drawing family are in `aether-render`.
- **`aether-math`** — `Vec2/3/4`, `Mat4`, `Quat`, `Aabb`, `Rect2` (column-major, YXZ Euler, right-handed Y-up, `f32`, `no_std`). Reach for it before hand-rolling `cross` / `dot` / `normalize` / aabb checks; add missing domain-agnostic primitives here, not locally.

Runtime + chassis (ADR-0073): the shared runtime is **`aether-substrate`**; each native capability owns its own thin `aether-<cap>` crate (`aether-render`, `aether-audio`, `aether-fs`, `aether-http`, `aether-component`, `aether-lifecycle`, `aether-rpc`, `aether-fleet`, …). A cap's directory/identity/runtime/test shape is normative in `docs/guide/capability-anatomy.md` (ADR-0121/ADR-0122); a downstream crate depends only on the caps it uses, never on an aggregate. Each chassis lives in its own crate over the shared **`aether-chassis`** composition layer: **`aether-chassis-desktop`**, **`aether-chassis-headless`**, **`aether-chassis-hub`**, **`aether-chassis-harness`**, and **`aether-chassis-bloomery`** (the journal-driven `aether-bloomery` engine over the `aether-bloomery-*` crates), plus **`aether-harness-perf`** for perf tooling. `cargo xtask package` produces the shippable package depot.

The guest/actor SDK is **`aether-actor`** — the `Actor` / `WasmActor` traits, `WasmCtx`, the `#[actor]` macro, and `export!` (proc macros in `aether-actor-derive`). See [Writing components](#writing-components).

## Workflow

- **Exploration and design discussion** happens in chat with the user. No artifact required.
- **Planned work** lives in GitHub Issues. The skills in `.claude/skills/` are the contributor workflow: `/scope` writes the managed Plan sections, declared surface, and size/model routing lines into the issue body; `/approve` records a hidden approval bound to the Plan digest and exact base commit; `/implement`, `/resolve`, and `/land` carry it through a PR. Labels classify issues; they do not carry workflow state or model routing.
- **Load-bearing architectural decisions** are recorded as ADRs in `docs/adr/NNNN-title.md`, numbered sequentially from `docs/adr/TEMPLATE.md`, and reviewed via a PR like any other change.
- **Branches**: `type/short-slug` (e.g. `chore/ci-bootstrap`, `feat/mail-runtime`, `docs/adr-workflow`).
- **Worktrees** live under `.agents/worktrees/` in the primary checkout (gitignored), never as siblings of the repo: the SessionStart hook `.hooks/bind-session-worktree.sh` prepares a per-session one there, and issue work uses `.agents/worktrees/issue-<N>`, which `/implement` creates or verifies from the approved base before editing. `.claude/worktrees/` holds only legacy symlinks.
- **Spikes** run on their own `spike/<name>` branch cut from `main`, with the spike crate in its own `spikes/<name>/` subdir on that branch. `main` never carries a `spikes/` tree; list spike branches with `git branch -r | grep '^[[:space:]]*origin/spike/'`.
- **Commits and PR titles** follow Conventional Commits (`type(scope): subject`), checked on PR titles by the `Lint title` workflow. Main squash-merges with the PR title as the commit subject, so PR title quality matters.
- **Draft PR proof** is current-head specific: green checks, direct inspection of the complete diff, no active change request, resolved review threads, and surface overflow priced against the rules frozen at the start of work (auto lands, judge is assessed by the landing agent, human needs the owner's word). `/implement` reviews and repairs its own draft and hands off human-readable evidence, never a visible JSON/HTML machine marker. Each push invalidates earlier head-bound proof.
- **Conflicts** on a draft use `/resolve <PR>`: merge current `main` into the owned branch, preserve both intents, and prove the new head again — no rebasing or force-pushing.
- **Landing** is separate from implementation: `/land <PR>` independently revalidates approval, ancestry, surface overflow, checks, review, threads, and merge state before squash-merging. Claude does not push to `main`, force-push reviewed branches, or merge without explicit landing authorization.
- **PRs** should be small and focused — one concept per PR.
- **Recursion in load-bearing code**: prefer iterative implementations (explicit work-stack/queue, arena-with-indices for tree data) over recursive ones in any algorithm whose depth could exceed a few hundred frames in practice. Recursion is OK for parse/AST walks where depth is structurally bounded by a small input file. Either way, recursive code on user-controlled or geometrically-derived data must enforce a depth/budget cap that returns an error rather than overflowing the stack.
- **No section-divider banner comments**: comments that are a run of dashes/equals (`// ----------`, `// ---- label ----`) are banned in source — use a plain comment, or split into modules if visual structure is needed. ASCII diagrams that carry real content (state machines, coordinate sketches) are fine. Enforced at edit time by the `.hooks/check-no-divider-comments.sh` hook wired in `.claude/settings.json`.
- **Naming — units and types**: spell units out in identifiers (`millis`, `nanos`, `micros`, `secs`, `bytes`), never the two-letter abbreviation (`ms`, `ns`, `us`, `kb`) — two letters is the ambiguous zone (`ms` reads as milliseconds *or* movement-speed). Longer well-known abbreviations are fine. And don't encode a value's Rust type in its name (`u32` / `u64` / `usize`) — the signature already states it. E.g. `parse_u32_ms_strict` → `parse_millis_strict`.
- **Naming — dash-sibling namespaces**: a dash names a genuine adjacent sibling of an existing bare actor namespace, never a generic multi-word segment. `aether.kit.camera-controller` is the controller sibling of `aether.kit.camera`; the dash has no addressing semantics, so neither is nested under the other and the full `NAMESPACE` still determines identity before lineage determines its mailbox. `aether.kit.mesh` is the counter-case: `MeshViewer` is the bare mesh actor's implementation type, not a reason to call its namespace `aether.kit.mesh-viewer`.
- **Code layout — chains, paragraphs, width**: when a value flows through successive calls, write the chain — `send(input.load()?.parse()?)`, never `let loaded = input.load()?; let parsed = loaded.parse()?; send(parsed)` — and never unroll into a mut local driven one call at a time when the API chains; a single-use intermediate earns its `let` only when the name documents a non-obvious value. Write code in paragraphs: one blank line between logical units, none inside a unit — the bindings that assemble a request are one paragraph, the send-and-assert that follows is the next; an unbroken blob makes the reader re-segment it on every read. Line width belongs to rustfmt (`rustfmt.toml`, `max_width = 120`): never hand-wrap or hand-count columns — write the code, run `cargo fmt`.
- **Module structure — the tree is a review surface**: the same logical-unit principle, one level up. Break a crate into submodules by concept — a directory per command / capability / lane, a file per phase or policy inside it, `mod.rs`-style directories, tests co-located with the code they exercise — so the file listing reads as a structured word cloud of the design: the names, counts, and nesting answer "what does this do and where do I look" before any file is opened, and a wrong name or a lopsided submodule stands out on sight. Packing a crate into one large file destroys that surface — the reader must reconstruct the structure by scrolling and hopping, and a file past a reviewer's working context breeds misreadings. Sibling directories are the template: a new command / capability / lane mirrors the shape of its siblings rather than inventing a new one, so growth stays consistent and one learned layout amortizes across the codebase. The template is a default, not a mandate — when something genuinely doesn't fit the sibling shape, structure it honestly rather than forcing the fit.

## Harness self-modification guardrail

Edits under `.claude/` — skill text in `.claude/skills/` especially — and force-pushes of a skill branch trip Claude Code's built-in self-modification prompt, which asks for explicit sign-off rather than silently refusing. When the user has directed the skill-text change, that prompt is expected: confirm and proceed. The GitHub phase-label pipeline that once carried this authorization was retired on 2026-07-20; the skills remain as the contributor workflow. The classifier is harness-level and is not reconfigured here, and any change to guardrail configuration (the `.hooks/*` scripts, their wiring in `.claude/settings.json`, the classifier) needs the owner's explicit sign-off.

## Commands

- Build: `cargo build` (release: `cargo build --release`). The root manifest's `default-members` leaves out the build pipeline (`xtask`), the `aether-demo` release-demo component, and the `aether-test-fixtures-*` wasm crates; add `--workspace` to select every member.
- Run: `cargo run -p <crate>` — the workspace root has no default binary. Chassis binaries: `cargo run -p aether-chassis-hub --bin aether-hub`, `-p aether-chassis-desktop --bin aether-desktop`, or `-p aether-chassis-headless --bin aether-headless`.
- Test: `cargo test` (single test: `cargo test <name>`; single-threaded with output: `cargo test -- --nocapture --test-threads=1`)
- Lint: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- Format: `cargo fmt` (check-only: `cargo fmt -- --check`)
- Type/borrow check only: `cargo check`

## MCP harness

Claude drives a running engine through MCP. The harness is the out-of-process **`aether-mcp`** crate: an RPC client that dials the hub's `RpcServerCapability` and relays each tool call as a wire `Call`. The stack is fronted by a long-lived **tunnel** (the `aether-tunnel` binary in `aether-mcp`, ADR-0089) so the volatile backends can restart without dropping the MCP connection:

```
:8890  aether-tunnel  — stable MCP front; reverse-proxies /mcp, supervises the two below
:8891  aether-mcp     — forked by the tunnel; dials + re-dials the hub
:8901  aether-hub     — forked by the tunnel; the RPC server the fleet talks to
```

The tunnel binds `:8890` (the port `.mcp.json` targets) and forks `aether-mcp` (`AETHER_MCP_PORT=8891`, `AETHER_HUB_RPC_ADDR`) and the hub (`--rpc-port 8901`). Bring the stack up by running `scripts/ensure-tunnel.sh` yourself when you need it — it is idempotent (a no-op only when a **healthy** tunnel, both children alive per `/admin/status`, is already bound; a degraded one is replaced). It is *not* auto-started on session start: a cold `cargo` build can take long enough to look like a frozen session, so the launch is left to the point of use. A cold run announces which binaries it must build first; `AETHER_TUNNEL_SKIP_DESKTOP=1` drops the desktop chassis (the wgpu / winit build that dominates that time) and leaves `spawn_substrate` resolving `aether-headless` only. Port overrides: `AETHER_TUNNEL_PORT`, `AETHER_MCP_PORT`, `AETHER_HUB_RPC_PORT`. If the `mcp__aether-hub__*` tools are missing after a mid-session start, run `/mcp` to reconnect them.

To restart the hub after a rebuild **without dropping the MCP session**, `curl -fsS -X POST http://127.0.0.1:8890/admin/restart-hub`: the tunnel re-forks the hub and `aether-mcp` re-dials it on the next tool call. This restarts the whole fleet — see `docs/guide/operating/harness-lifecycle.md` before cycling it. `GET /admin/status` reports child liveness and ports. Restarting `aether-mcp` itself invalidates the MCP session.

Every engine-taking tool resolves its engine the same way: pass `engine_id`, or omit it to target the sole supervised engine — with zero or several engines an omitted id is an error naming the situation, never a guess — and the reply echoes the resolved `engine_id`. Each tool's description in the live MCP schema is authoritative for its arguments and reply shape; `docs/guide/mcp-harness.md` is the map and mental model, and `docs/guide/operating/` covers fleet ownership, the artifact registry, inspection, evidence, and recovery.

Tools (`mcp__aether-hub__*`):

- `list_engines(show?)` — supervised engines plus a bounded `recently_died` sidecar recording why each one left; `show` is `"alive"`, `"dead"`, or `"all"`.
- `spawn_substrate(selector?, chassis?, caps?, target?, args?, components?, mails?)` — fork a substrate from the hub's binary store (bare call = the headless `default`), optionally preloading `components` and dispatching init `mails` in the same call.
- `terminate_substrate(engine_id?)` — SIGKILL one engine.
- `upload_binary(staged_path, name?, pin?)` / `upload_component(staged_path, name?, pin?)` — the hub reads a fleet-host path into its content-addressed store and returns the `{hash, name}` selector.
- `pin_artifact(hash)` / `unpin_artifact(hash)` — set or clear durable eviction protection for one stored hash.
- `list_binaries(chassis?, caps?, target?, limit?, include_history?)` / `list_components(namespace?, handled_kind?, limit?, include_history?)` — browse the stored registries newest first; rows are stored artifacts, not live lineage addresses.
- `send_mail(mails, fire_and_forget?, replies?, format?)` — schema-encode and deliver each `{engine_id?, address, kind_name, params}`, blocking until its chain settles; a `[u8; N]` / `Bytes` / `Blob` / integer `params` field also takes `{"$hex": s}` (lowercase, no prefix; integers fixed width, most significant digit first, signed as two's complement). `format` is a per-kind reply mask — `"$hex"` at a `[u8; N]` / `Bytes` / `Blob` / integer leaf, or `{"*": "$hex"}` for every byte-array leaf — validated against the kinds' schemas before any mail is sent, applied after decode and before both spills, and never renaming keys.
- `send_mail_traced(engine_id?, mails, settlement_timeout_millis?, fire_and_forget?, trace?, format?)` — atomic batch under one trace root, returning the settled trace tree; `trace: "tree" | "nodes"` picks the compact tree (default) or the full node vector, and `format` is the same reply mask as `send_mail`'s.
- `describe_kinds(engine_id?, families?, names?, prefix?, detail?)` — the kind shapes to build `params` from; start with `families: true`, and ask `detail: "schema"` only with a selector.
- `describe_component(engine_id?, address, full?)` — a loaded component's handlers, docs, and fallback, addressed by its lineage name.
- `describe_handlers(engine_id?)` — the native caps' handler `In -> Out` reply contracts.
- `compare_component_contracts(baseline, candidate)` — diff two loaded revisions' typed-mail contracts and report `compatible`.
- `describe_transforms` — the native `#[transform]` set linked into `aether-mcp`.
- `load_component(engine_id?, selector, name?, config?, config_path?, export?, replicas?, full?)` — load a registry selector; `config` / `config_path` JSON is schema-encoded to the component's `Config` kind.
- `replace_component(engine_id?, address, selector, config?, config_path?, export?, full?)` — in-place wasm swap behind the same mailbox (ADR-0022).
- `capture_frame(engine_id?, window_id, mails?, after_mails?, checks?, similarity?, scale?, max_dimension?, include_image?, save_path?)` — desktop PNG readback of one window; `window_id` is required (the tagged `mbx-…` string `aether.window.list` reports).
- `collect_failure_evidence(engine_id?, primary_error, operation?, actor_addresses?, component_addresses?, kinds?, frame?)` — a bounded, non-mutating evidence bundle around a failure you already have.
- `actor_logs(engine_id?, address, max?, level?, since?, contains?)` — tail one actor's log ring; page by passing the prior `next_since` as `since`.
- `actor_cost(engine_id?, address, kind_id?)` — one actor's per-handler execution-cost EWMA table.

When verifying substrate behavior end-to-end, reach for the MCP harness before writing a new test binary.

## Test harnesses (ADR-0067)

**Tests must earn their place** (`docs/guide/testing.md`). Before writing a test, name the plausible bug it catches; if the only honest answer is "edits the test," do not write it. The decisive question: **what logic owned by *this crate* does the test exercise?** Do not test code you do not own — std, the compiler, serde, third-party crates, or anything the `Kind` / `Schema` / `Config` derives emit. The recurring junk shapes are a derived-constant mirror (`assert_eq!(NoteOn::NAME, "aether.audio.note_on")`), a derive-only roundtrip (`decode(encode(x)) == x` over plain derives), a derive-emitted registration check, and a schema-shape assertion that restates a keyword. A flat assertion against a fixed value is a tripwire (keep it) only when the pinned value is **computed** — a hash, a serialized byte layout, a derived `KindId` — and it carries a `// Tripwire:` comment naming the invariant; the comment is necessary but not sufficient.

Two in-repo harnesses cover non-overlapping surfaces for `cargo test` / CI (`docs/guide/testing/substrateharness-and-fleetharness.md`; the worked example is the running module doctest in `crates/aether-harness-substrate/src/lib.rs`):

- **SubstrateHarness** (`aether-harness-substrate`) drives the substrate in-process over a loopback channel — no hub, no wire — and plugs in rendering and pixel readback through `aether-harness-substrate-capture`. Its sends take proven references, never addresses: `actor_ref::<R>()` for a composed capability, `load::<R>` / `load_any` for a loaded component, and `child::<P, C>(&parent, key)` for a child spawned beneath a reference already held. Engine-internal correctness and anything visual go here.
- **FleetHarness** (`aether-harness-fleet`) drives the real hub → RPC → forked-headless-substrate stack over raw `WireFrame::Call` frames. The RPC/hub boundary, `Call` recipient-name resolution, and fleet lifecycle go here. It is headless, so rendered-output assertions must use SubstrateHarness; externally-addressable-over-the-wire assertions must use FleetHarness.

Pre-build component wasm with `cargo xtask build-wasm` before a scenario suite (without it `require_wasm` fails the scenario; `AETHER_ALLOW_WASM_SKIP=1` skips deliberately), and run `cargo xtask dist` before FleetHarness scenarios. Wasm components are discovered structurally: a `cdylib` package that depends on `aether-actor`.

## Runtime & subsystems

`docs/guide/systems.md` maps every subsystem to its guide page — mail and kinds, lifecycle, input, window, rendering, text, audio, file I/O, HTTP, components, logging, tracing and settlement, configuration, and the rest — and each page names its governing ADR. Read the page before mailing a subsystem you have not used.

**Addressing convention.** In a mail bundle, `address` names the *mailbox* and `kind_name` the *payload shape*; they route independently even when they share a prefix — send `aether.audio.note_on` to `aether.audio`, `aether.draw_triangle` to `aether.render`. Chassis-owned mailboxes live under `aether.<name>`. There is no `aether.input` mailbox: key / mouse / window-size subscription is on `aether.window` and tick subscription on `aether.lifecycle`. A loaded wasm component registers at `aether.component/aether.embedded:NAME` (ADR-0099), the full address `LoadResult.path` returns; `aether.component/:NAME` is its short path, where `:NAME` names the one instanced child (ADR-0166); bare names (`"camera"`) are not registered and warn-drop.

## Writing components

A component is an actor whose receive side is declared with **`#[actor]`** on one `impl WasmActor for C` block (ADR-0033 / ADR-0074). Guide: `docs/guide/writing-guest-code.md`, `docs/guide/systems/components.md`, `docs/guide/foundations/actor-model.md`, and `docs/guide/architecture/guest-native-boundary.md`.

```rust
#[actor(depends(LifecycleCapability, RenderCapability))]
impl WasmActor for CameraComponent {
    const NAMESPACE: &'static str = "aether.kit.camera";   // default load name

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> { /* build state; no mail yet */ }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {          // post-init, mail allowed: subscribe here
        ctx.subscribe::<LifecycleCapability, Tick>();
    }

    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _t: Tick) {
        ctx.send::<RenderCapability>(&ViewProjection { view_proj });
    }
}
aether_actor::export!(public = [CameraComponent]);         // required; emits wasm32-only FFI shims
```

- **Exports**: `export!` takes keyed entries only, in any order, each key once: `public = [..]`, `default = A`, `boot = B`, `private = [..]`, `generators = [..]`. `export!(public = [A, B, C])` (ADR-0096) designates no default, so a load without an `export` selector is refused and names the exports (ADR-0138); a single `public` actor loads selectorless, and `export!(default = A, public = [B, C])` names one. `boot = B` instantiates `B` once per engine and module on every load and is never selectable (ADR-0147). `private = [..]` lists inline children the module rebuilds but does not export (ADR-0114); a spawner declares the children it spawns through the typed verbs in `#[actor(spawns(..))]`, and every `export!` must list each declared child.
- **Handlers**: `#[handler::single | manual]` (ADR-0134) infers the kind from the third parameter; an optional `#[fallback]` taking `Mail<'_>` catches the rest — omit it for a strict receiver. A ctx that omits its actor is typed by it: `WasmCtx<'_>` reads as `WasmCtx<'_, Self>` (reply mode second: `WasmCtx<'_, Self, Manual>`), so it reaches only declared dependencies; spell `Erased` (`WasmCtx<'_, Erased>`) for the untyped view.
- **Lifecycle**: `init`, `wire`, `unwire`. `on_dehydrate` / `on_rehydrate` are default-no-op `WasmActor` methods; override them to carry state across `replace_component` (ADR-0101). The `wire` / `unwire` / `on_rehydrate` ctxs are typed by the actor the same way.
- **Dependencies and addressing** (ADR-0230, `docs/adr/0230-proven-actor-references.md`): `#[actor(depends(R))]` declares that `R` — a root singleton or a co-hosted `Embedded` peer, never an `Instanced` actor — must be live before this actor is created; several dependencies go in one list, `depends(A, B)`, and a repeated `depends(...)` is a compile error; a load or replacement whose dependency is not live is refused before `init`. `ctx.send::<R>(&kind)` (ADR-0232) mails a declared dependency and inherits the handler's causal chain; its siblings are `send_detached` (a fresh chain), `send_many` (one cast batch), `send_tracked` (returns the request id), and `send_with_context` (stores a typed context for the reply handler). They and `ctx.subscribe::<P, K>()` compile only on a ctx typed by an actor that declares the recipient; the erased ctx has no typed send. State that outlives a handler holds a proven reference, never a `MailboxId`: `ctx.actor_ref::<R>()` mints an `ActorRef<R>` for a declared dependency and `ctx.sender()` yields an `Option<ErasedActorRef>`; send through either with `ctx.send_to(reference, &kind)`, which checks the kind against an `ActorRef<R>` and leaves an `ErasedActorRef` unchecked. Hand-hashing a name into a `MailboxId` is closed: the name hashes are private to `aether-data`, a written path resolves through the host registry, which folds it node by node, and `MailboxId::from_name`, the one hash spelling left, is `disallowed-methods` in `clippy.toml`. CI's raw-mailbox ratchet (`scripts/check-raw-mailbox-ratchet.py`) lets the count of old-door call sites only fall.
- **Config**: a capability configures through the ADR-0090 `#[derive(aether_substrate::Config)]` path (argv > env > default, handed into `init`), never a naked `std::env::var` / `var_os` read. Both are `disallowed-methods` in `clippy.toml`; a legitimately external read (the config machinery, a process-level tuning knob, a `HOME` / `XDG` lookup, a build script, test code) carries `#[allow(clippy::disallowed_methods)]` plus a one-line reason. A secret is never a knob value, argv, env, or kind field: name it in a `#[config(secrets)]` knob and supply it as a file under `--secrets-dir` (ADR-0235). See `docs/guide/systems/configuration.md`.
- **Kind types**: `#[aether_data::kind(name = "…")]` declares the kind and emits the standard stack (`Kind`, `Schema`, `Debug`, `Clone`, `Serialize`, `Deserialize`), with `copy` / `default` / `partial_eq` / `eq` / `pod` / `no_serde` / `derive(…)` naming the departures and `engine_only` marking engine-only mail no actor may send (ADR-0233). A component and its peers share the kind crate (ADR-0066); under the `runtime` feature the same crate emits the cdylib via `export!`.

## Local checks and CI

GitHub Actions is the full build engine. Before opening or updating an implementation PR, run `cargo fmt -- --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings`; the expensive build/test/package matrix belongs to CI unless the issue asks for local proof. `main` is protected by the `Protect main` ruleset: changes land only through a pull request, force pushes and deletion are refused, no one bypasses it, and no review is required. `CI pass` (the `ci.yml` aggregate) is the verdict to wait on and `Lint title` checks the PR title, but the ruleset does not enforce either. Those checks prove the tree and title, not direct review, thread resolution, or landing authority.

The working loop opens a PR, watches the current head, and repairs deterministic failures. `scripts/wave-status.sh --wait <PR>` polls until `CI pass` concludes; a fix pushed to the same branch supersedes the old run.

Manual cross-worktree `cargo doc` / `clippy` re-checks must keep each worktree on its own `target/` — never export a shared `CARGO_TARGET_DIR` across worktrees on divergent source, because cargo's incremental cache can surface a dependency last compiled from the other worktree's source, producing phantom errors that look like regressions but are tooling artifacts.

## Duplicate-code and dependency gates

`jscpd` (the `Duplicate code` job, `cargo xtask transform verify.dup`) runs token-based clone detection over `crates/` and fails when the duplicated-line percentage exceeds the threshold. Its format, min-tokens, and threshold are argv in `xtask/src/transform/verify/mod.rs`, and the repository deliberately has no `.jscpd.json`: jscpd answers a malformed config with one stderr warning and a silent fall back to built-in defaults — threshold included — whereas a bad flag it refuses outright. `cargo-machete` (the `Unused dependencies` job, `verify.deps`) fails on an unused dependency; a false positive — a dependency used only through a macro-emitted path or an optional feature — is silenced per-crate via `[package.metadata.cargo-machete] ignored`, never by dropping the dependency. It runs `--no-ignore --skip-target-dir`, because its walk otherwise skips gitignored files and a `.gitignore` pattern matching a crate path would drop that crate from the scan. Both feed `CI pass`.

RustRover's IDE inspector is **not** a merge gate: it analyzes the IDE's open project and rehosts only a subset of the checks. Reach for RustRover MCP for rename / symbol / refactor work.
