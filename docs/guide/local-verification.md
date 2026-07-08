# Local checks and CI

GitHub Actions is the build engine. CI runs the full check set — format, clippy, doc build, the marker-only host build, the workspace tests, and qodana — on every push, and it is the gate a PR merges through. There is no local pre-flight script and no pre-push hook: the heavy checks run on the runner, which builds many branches in parallel and never flakes on a local toolchain.

Before you push, run one command:

```sh
cargo fmt
```

A formatting slip is the one CI red worth catching locally — instant to fix and the cheapest failure to avoid. Everything heavier is CI's job.

Codex sessions may also load project hooks from `.codex/hooks.json` after you
trust them with `/hooks`. Those hooks are interactive guardrails for the local
agent loop, including the best-effort `.agents/worktrees/` helper and
source/text checks; they are not a required local preflight and they do not add
a separate CI hook-test surface.

## Watching CI

Open your PR as a draft, then watch the checks and fix a red as soon as it surfaces rather than waiting for the whole run to finish:

```sh
scripts/wave-status.sh --wait <pr>
```

`--wait` polls CI over REST and exits 0 when the `CI pass` aggregator goes green, 1 when it fails. It fast-fails the moment a deterministic check — Format, Clippy, Docs, the marker-only host build, or the guardrail hook tests — concludes failure, so a cheap red surfaces without waiting out the slow test and qodana jobs. A fix pushed to the same branch supersedes the in-flight run, so an early fix costs nothing.

`/implement` drives this loop for a scoped issue; see the [reference](reference.md) and `CLAUDE.md` for the full agent workflow.

## Cross-worktree checks

If you run `cargo doc` or `cargo clippy` by hand across several worktrees, keep each worktree on its own `target/` — never export a shared `CARGO_TARGET_DIR` across worktrees whose sources diverge. Cargo's incremental cache can otherwise surface a dependency last compiled from another worktree's source, producing phantom errors that look like regressions but are tooling artifacts.
