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

GitHub Actions is the full build engine and merge gate. `CI pass` aggregates
the jobs that must be green: `Format`, `Clippy`, `Rustdoc`, the sharded `Test`
matrix, `Duplicate code` (jscpd), `Unused dependencies` (cargo-machete), `Cargo
lock freshness`, and `New suppressions` on pull requests. Branch protection
requires `CI pass` and `Lint title`, which enforces a Conventional Commit pull
request title with a lowercase subject; see
[.github/workflows/README.md](.github/workflows/README.md) for the CI
conventions.

To reproduce a check locally before pushing, narrow to the crate or the test:

```
cargo test -p <crate>
cargo test <name>
```

`cargo xtask transform verify.check --out <dir>` runs the whole mechanical set
under the same invocations CI uses, writing its evidence into `<dir>`; the
individual ids are `verify.fmt`, `verify.clippy`, `verify.docs`, `verify.test`,
`verify.dup`, `verify.deps`, and `verify.suppress`.

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

## Reporting a vulnerability

Do not open a public issue for a security problem. [SECURITY.md](SECURITY.md)
has the private reporting route.
