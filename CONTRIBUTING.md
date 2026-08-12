# Contributing to Aether

Thanks for your interest in contributing. A couple of things to know before you open a PR.

## Licensing of contributions

Aether is dual-licensed under [MIT](LICENSE-MIT) or [Apache License 2.0](LICENSE-APACHE), at the recipient's option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

## Before you push

Run the cheap deterministic tier before opening or updating an implementation
PR:

```
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

GitHub Actions is the full build engine and merge gate. It owns the expensive
workspace tests, docs, marker/feature boundaries, wasm packaging, duplicate-code
and unused-dependency checks, and other applicable contract jobs. The required
checks are `Lint title` and `CI pass`; see
[.github/workflows/README.md](.github/workflows/README.md) for the CI
conventions.

If you want to reproduce a specific CI check locally before pushing, the
workspace commands are:

```
cargo test -p <crate>
cargo test <name>
```

See [Local checks and CI](docs/guide/local-verification.md) for verification and
[Agent and contributor workflow](docs/guide/contributing/agent-workflow.md) for
the issue/PR lifecycle. Planned work records its scoped Plan, declared surface,
and size/model route in the issue body; approval is a hidden trusted record
bound to that Plan digest and base commit. Implementation stays in an owned
issue worktree and draft PR until current-head checks, direct review, threads,
and required dogfood are clear. Landing is separately authorized.

Codex uses `AGENTS.md` and `.agents/skills/`; Claude Code uses `CLAUDE.md` and
`.claude/skills/`. The checked-in skill for the active surface owns exact
mutations and pause boundaries.
