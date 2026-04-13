# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

This is a freshly-initialized Rust project (Rust edition 2024). It currently contains only the default `cargo new` scaffolding: a single `src/main.rs` printing "Hello, world!" and a `Cargo.toml` with no dependencies. There is no architecture to describe yet — treat early work here as greenfield.

## Commands

- Build: `cargo build` (release: `cargo build --release`)
- Run: `cargo run`
- Test: `cargo test` (single test: `cargo test <name>`; single-threaded with output: `cargo test -- --nocapture --test-threads=1`)
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt` (check-only: `cargo fmt -- --check`)
- Type/borrow check only: `cargo check`
