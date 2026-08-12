# AGENTS.md

Repository guidance for Codex working in Aether.

## Status

Aether is a pre-1.0 Rust 2024 workspace for a game engine whose native substrate hosts wasm actors and native chassis capabilities. Engine actors communicate by mail. Load-bearing design lives in `docs/adr/NNNN-title.md`; the contributor and agent reference is the mdbook source in `docs/guide/`.

Read relevant guide pages and ADRs before changing a subsystem. Prefer current code over prose when they disagree.

## Workflow

- Planned work lives in GitHub issues. `scope` records the managed Plan sections, declared surface, and exact size/model routing lines in the issue body. `approve` appends a trusted hidden record bound to that Plan digest and an exact `origin/main` commit; taxonomy labels do not carry workflow state or routing authority.
- Use the repo skills in `.agents/skills/` for sketch, scope, approve, implement, land, sweep, wish, review, dogfood, and related flows. Codex skills and their shared contracts are authoritative for Codex and execute through the active Codex tools. `CLAUDE.md` and `.claude/skills/` are the separate Claude Code surface; consult them only when intentionally adapting a workflow.
- Do not implement directly in the primary `main` checkout. Codex issue work uses one verified `.agents/worktrees/issue-<N>` worktree and issue branch cut from the approved base commit. Other agent surfaces may use `.claude/worktrees/`; Codex issue work stays under `.agents/worktrees/`.
- Branches use `type/short-slug` or the issue branch shape from the implement skill, for example `chore/issue-2742-make-repository-codex-friendly`.
- PR titles and commits use Conventional Commits.
- An implementation remains a draft PR while its current head accumulates green checks, direct-review acceptance, resolved threads, and required dogfood evidence. Repair findings in the implementation loop; a new head must prove those facts again.
- Landing is a separate explicitly authorized operation. Do not push to `main`, force-push reviewed branches, clear draft state, merge, or run destructive git commands without the workflow's required user approval.
- Keep PRs focused: one concept per PR.

## Commands

- Build: `cargo build`
- Release build: `cargo build --release`
- Run a crate: `cargo run -p <crate>`
- Chassis binaries: `cargo run -p aether-chassis-hub --bin aether-hub`, `-p aether-chassis-desktop --bin aether-desktop`, or `-p aether-chassis-headless --bin aether-headless`
- Test: `cargo test`
- Single test: `cargo test <name>`
- Clippy: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt`
- Format check: `cargo fmt -- --check`
- Check only: `cargo check`

For implementation PRs, this repo uses GitHub Actions as the full build engine. Before pushing an implement branch, locally run the cheap deterministic tier: `cargo fmt -- --check` and `cargo clippy --all-targets -- -D warnings`; fix any red locally before opening or updating a draft PR. Let CI run the expensive checks unless the issue explicitly asks for local verification. If a user explicitly asks for full local build, test, or dist verification, report the result and then note that `target/` and generated `dist/` artifacts can be large, so clean them up when they are no longer needed or ask before preserving them.

## Codex Hooks

Project-local Codex hooks live in `.codex/hooks.json` and `.codex/hooks/`.
Because Codex trust-records hook definitions, new or changed hooks may need
review through `/hooks` before they run.

The SessionStart hook can prepare a per-session worktree under
`.agents/worktrees/` when the harness exposes a stable session or thread id, but a
hook subprocess cannot change the agent's cwd. When a SessionStart reports
`prepared git worktree at: <path>`, the agent should treat `<path>` as its repo
root for the session and `cd` there for edits (other platforms auto-enter this way).
Planned issue work still follows the implement skill and edits
`.agents/worktrees/issue-<N>`.

Codex hooks are guardrails, not a new local preflight or CI surface. Do not add
hook tests or CI hook jobs unless a scoped issue explicitly asks for them.

## MCP Harness

The Aether MCP endpoint is configured for Codex in `.codex/config.toml` and
for MCP clients that read `.mcp.json` as `aether-hub` at
`http://127.0.0.1:8890/mcp`.

Start the local tunnel only when a task needs live engine tools:

```bash
scripts/ensure-tunnel.sh
```

Expected MCP tools are the `mcp__aether-hub__*` family, including engine
listing, substrate spawn/terminate, component upload/load/replace, mail
sending, kind/component description, frame capture, actor logs, and cost
inspection. If those tools are missing after starting the tunnel, reconnect MCP
in the active Codex surface with `/mcp`.

## Coding Rules

- Preserve user changes. Never revert edits you did not make unless the user explicitly asks.
- Prefer `rg`/`rg --files` for repository search.
- Use `apply_patch` for manual edits.
- Avoid section-divider banner comments in source.
- Chain calls when a value flows through them (no single-use `let` intermediates, no mut local driven one call at a time); separate logical units of code with blank lines; leave line width to `cargo fmt` (`max_width = 120`).
- In load-bearing code, prefer iterative algorithms over recursion unless depth is structurally bounded or capped.
- In a capability crate (`aether-<cap>`, one per capability mailbox), visibility is either `pub` or private; avoid scoped forms such as `pub(crate)`.
- Spell units out in identifiers (`millis`, `nanos`, `micros`, `bytes`) and do not encode Rust primitive types in names.

## Review Posture

When asked for a review, lead with findings ordered by severity and cite files/lines. Prioritize correctness, regressions, missing tests, and architecture/convention drift over style.
