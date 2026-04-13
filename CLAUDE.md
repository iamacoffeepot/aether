# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

Early-stage Rust project (edition 2024). Vision: a game engine where Claude sits in a harness as assistant/engineer/designer. Architectural direction (see `docs/adr/`): a thin native kernel owns I/O, GPU, audio, and hosts a WASM runtime; engine components run as WASM modules and communicate via a mail system.

## Workflow

- **Exploration and design discussion** happens in chat with the user. No artifact required.
- **Planned work** (spikes, features, open investigations) lives in GitHub Issues. Referenced by the PR that closes them.
- **Load-bearing architectural decisions** are recorded as ADRs in `docs/adr/NNNN-title.md`. Use `docs/adr/TEMPLATE.md` when starting a new one. Number sequentially. An ADR is reviewed via a PR like any other change.
- **Branches**: `type/short-slug` (e.g. `chore/ci-bootstrap`, `feat/mail-runtime`, `docs/adr-workflow`).
- **Commits and PR titles** follow Conventional Commits (`type(scope): subject`). Enforced in CI against PR titles. Main uses squash-merge with PR title as the commit subject, so PR title quality matters.
- **Merging**: `main` is protected (PR required, all CI checks required, linear history, no force-push). Claude does not push to `main`, does not force-push reviewed branches, does not self-merge, and asks before destructive operations.
- **PRs** should be small and focused — one concept per PR.

## Commands

- Build: `cargo build` (release: `cargo build --release`)
- Run: `cargo run`
- Test: `cargo test` (single test: `cargo test <name>`; single-threaded with output: `cargo test -- --nocapture --test-threads=1`)
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt` (check-only: `cargo fmt -- --check`)
- Type/borrow check only: `cargo check`
