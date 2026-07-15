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
and unused-dependency checks, and other applicable contract jobs. Keep the PR draft while those facts and, for a
same-repository branch, the automated review/dogfood results accumulate. Fork
PRs do not receive repository secrets, so a maintainer must provide the
corresponding review or deliberately dispatch the trusted workflow.

If you want to reproduce a specific CI check locally before pushing, the
workspace commands are:

```
cargo test -p <crate>
cargo test <name>
```

See [Local checks and CI](docs/guide/local-verification.md) for verification and
[Agent and contributor workflow](docs/guide/contributing/agent-workflow.md) for
the issue/PR lifecycle. Codex uses `AGENTS.md` and `.agents/skills/`; other agent
surfaces follow their own checked-in contracts.
