# Contributing to Aether

Thanks for your interest in contributing. A couple of things to know before you open a PR.

## Licensing of contributions

Aether is dual-licensed under [MIT](LICENSE-MIT) or [Apache License 2.0](LICENSE-APACHE), at the recipient's option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

## Before you push

Run `cargo fmt`, then push. GitHub Actions is the build engine: it runs the
full check set — fmt, clippy, doc, tests, the wasm32 component cross-build, and
qodana — on every push and is the merge gate, so the heavier checks are
offloaded to CI rather than run locally. Push early as a draft and fix any red
as it surfaces.

If you want to reproduce a specific CI check locally before pushing, the
workspace commands are:

```
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

See `CLAUDE.md` § "Local checks and CI" and
[docs/guide/local-verification.md](docs/guide/local-verification.md) for the
full workflow.
