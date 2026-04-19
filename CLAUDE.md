# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

Early-stage Rust project (edition 2024). Vision: a game engine where Claude sits in a harness as assistant/engineer/designer. Architectural direction (see `docs/adr/`): a thin native **substrate** owns I/O, GPU, audio, and hosts a WASM runtime; engine **components** run as WASM modules and communicate via a **mail** system. (The whole system — substrate + components + tooling — is "Aether" or "the engine"; the substrate is just the native base layer.)

## Workflow

- **Exploration and design discussion** happens in chat with the user. No artifact required.
- **Planned work** (spikes, features, open investigations) lives in GitHub Issues. Referenced by the PR that closes them.
- **Load-bearing architectural decisions** are recorded as ADRs in `docs/adr/NNNN-title.md`. Use `docs/adr/TEMPLATE.md` when starting a new one. Number sequentially. An ADR is reviewed via a PR like any other change.
- **Branches**: `type/short-slug` (e.g. `chore/ci-bootstrap`, `feat/mail-runtime`, `docs/adr-workflow`).
- **Commits and PR titles** follow Conventional Commits (`type(scope): subject`). Enforced in CI against PR titles. Main uses squash-merge with PR title as the commit subject, so PR title quality matters.
- **Merging**: `main` is protected (PR required, all CI checks required, linear history, no force-push). Claude does not push to `main`, does not force-push reviewed branches, does not self-merge, and asks before destructive operations.
- **PRs** should be small and focused — one concept per PR.

## MCP harness

Claude drives a running engine through MCP — the concrete form of the "Claude-in-harness" vision. Starting `cargo run -p aether-hub` (and either letting the hub spawn substrates via `spawn_substrate`, or running `AETHER_HUB_URL=127.0.0.1:8889 cargo run -p aether-substrate` by hand) exposes seven tools to a Claude Code session pointed at the project-scoped `.mcp.json`:

- `mcp__aether-hub__list_engines` — connected engines (UUID + name/pid/version + `spawned` flag: `true` if the hub launched the process, `false` if it connected externally).
- `mcp__aether-hub__describe_kinds(engine_id)` — the kind vocabulary the engine declared at handshake, with enough structural detail to build params.
- `mcp__aether-hub__send_mail(mails)` — batched, best-effort. Each item takes either `params` (hub encodes via the kind's descriptor) or `payload_bytes` (raw escape hatch for `Opaque` kinds). Response is a per-item status array; one failure doesn't abort siblings.
- `mcp__aether-hub__receive_mail(max?)` — non-blocking drain of observation mail the engine pushed to this session. Each item carries `engine_id`, `kind_name`, structured `params` (decoded against the engine's kind descriptor — symmetric to `send_mail`, ADR-0020), `payload_bytes` (always populated; primarily a fallback when `params` is null), an optional `decode_error` populated when decode failed, and a `broadcast` flag (`true` means fan-out to every attached session, `false` means targeted reply-to-sender). Read `params` first; fall back to `payload_bytes` only if it's null.
- `mcp__aether-hub__spawn_substrate(binary_path, args?, env?, timeout_ms?)` — launches a substrate binary as a child of the hub with `AETHER_HUB_URL` injected. Blocks until `Hello` handshake; returns `engine_id` + `pid`. The hub owns the child for its lifetime.
- `mcp__aether-hub__terminate_substrate(engine_id, grace_ms?)` — SIGTERM → grace (default 2s) → SIGKILL. Errors on externally connected engines (the hub only terminates children it owns).
- `mcp__aether-hub__engine_logs(engine_id, max?, level?, since?)` — drain captured substrate `tracing` events for an engine (ADR-0023). Cursor-based polling: pass back the previous response's `next_since` to receive only new entries. `level` filters server-side (`"trace"|"debug"|"info"|"warn"|"error"`, default `"trace"`); `max` defaults to 100 / clamped to 1000. `truncated_before` flags hub-side ring eviction so a slow poller knows it missed a window. The buffer survives engine exit, so post-mortem polls work after a substrate crash. Substrate-side filter: `AETHER_LOG_FILTER` (standard `EnvFilter` syntax, e.g. `AETHER_LOG_FILTER=aether_substrate=debug,wgpu=warn`) overrides the INFO+ default.

Prefer `params` over `payload_bytes` when the kind is describable — the hub does the `#[repr(C)]` byte packing so agents don't. When verifying substrate behavior end-to-end, reach for this before running a new test binary.

The observation path (ADR-0008) goes the other way: engines emit to the well-known sink `"hub.claude.broadcast"` and the hub fans out to every attached session. The live substrate binary pushes `aether.observation.frame_stats` there every 120 frames — a good smoke test for `receive_mail`. Reply-to-sender from a WASM component is plumbed at the wire level but not yet exposed as a host fn.

Input streams (tick, key, mouse_move, mouse_button) are publish/subscribe (ADR-0021): the substrate boots with empty subscriber sets and drops every input event until something subscribes. To wire a freshly-loaded component into the platform's event stream, mail `aether.control.subscribe_input` with `{ "stream": "Tick" | "Key" | "MouseMove" | "MouseButton", "mailbox": <id from load_result> }`. Multiple components may subscribe to the same stream; the substrate fans out to every subscriber. `aether.control.unsubscribe_input` removes one. Both reply via `aether.control.subscribe_input_result`. Subscriptions are auto-cleared when a component is dropped and preserved across `replace_component` (the mailbox id is stable).

`aether.control.replace_component` is freeze-drain-swap (ADR-0022): the substrate freezes the target mailbox, waits for in-flight `deliver` calls on the old instance to complete, then swaps. If the drain exceeds `drain_timeout_ms` (default 5000, per-replace overridable) the reply is `Err { error: "drain timeout ..." }` and the old instance stays bound — a loud failure rather than silent dropped mail. Mail that arrives during the freeze is parked and flushed through whichever instance ends up bound (new on success, old on timeout).

Design detail lives in ADR-0006 (wire + topology), ADR-0007 (schema-driven encoding), ADR-0008 (observation path), ADR-0009 (hub-supervised substrate spawn), ADR-0020 (symmetric receive_mail decode), ADR-0021 (input stream subscriptions), ADR-0022 (drain-on-swap for replace_component), ADR-0023 (substrate log capture + engine_logs), and ADR-0024 (`_p32`-suffixed FFI in anticipation of wasm64).

The component FFI surface uses a `_p32` suffix on every pointer-typed import (`aether::send_mail_p32`, `reply_mail_p32`, `resolve_kind_p32`, `resolve_mailbox_p32`, `save_state_p32`) and on the `receive_p32` / `on_rehydrate_p32` exports. Non-pointer exports (`init`, `on_replace`, `on_drop`) are unsuffixed. The suffix locks the wasm32/wasm64 naming convention without committing to dual registration today — see ADR-0024 for the deferred Phase 2.

## Commands

- Build: `cargo build` (release: `cargo build --release`)
- Run: `cargo run`
- Test: `cargo test` (single test: `cargo test <name>`; single-threaded with output: `cargo test -- --nocapture --test-threads=1`)
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt` (check-only: `cargo fmt -- --check`)
- Type/borrow check only: `cargo check`
