---
name: release-init
description: "Ensure Aether's phase, bounce, size, and model-routing GitHub labels exist for a release. Use to bootstrap or reconcile the issue lifecycle vocabulary; do not mutate issues or branches."
---

# Release Init

Read [Codex harness](../_shared/codex-harness.md) and [GitHub workflow](../_shared/github-workflow.md).

Require a release version; accept an explicit owner override and otherwise use `iamacoffeepot`. Verify `gh` repository access and that `scripts/release-project-init.sh` exists in the current repository state.

Run:

```text
scripts/release-project-init.sh <version> --owner <owner>
```

The repository-owned script is idempotent and owns the exact label names, colors, and descriptions. Do not reproduce or extend its list in the skill. On partial failure, report the failed label; a later rerun may safely reconcile the remainder.

Verify that every label declared by the script now exists. Report the release version and the `phase:*`, `bounce-to:*`, `size:*`, and `model:*` families. Point to `$sketch` as the next entry point.

Do not create a project board, issue, comment, branch, marker file, PR, or release.
