---
name: sketch
description: "Capture a rough idea as a lint-clean Aether Backlog issue while preserving the user's words. Use to file new work or a persisted wish leaf; do not scope, design, plan, or implement it."
---

# Sketch

Read [Codex harness](../_shared/codex-harness.md) and [GitHub workflow](../_shared/github-workflow.md). Filing the requested issue and, when required, creating its one prerequisite real-crate label are the only external mutations.

Support an idea, optional `--type`, `--scope` (or compatible `--crate`) and extra `--label` values, or `--from-wish <leaf/wish.md>`. Ask for the idea only when it is absent and was not just supplied in the user's current message.

## Title and labels

Read the authoritative `TYPES` and `META_SCOPES` arrays from `.github/workflows/issue-labels.yml` before filing. Build a lowercase Conventional Commit title:

```text
<type>(<scope>): <subject>
```

Infer type conservatively:

- bug, regression, broken, panic → `fix`;
- new capability, add, support → `feat`;
- documentation or guide gap → `docs`;
- performance/latency/throughput → `perf`;
- behavior-preserving restructure → `refactor`;
- intermittent/flaky/contention → `flake`;
- tooling, workflow, CI, cleanup, tests, or genuine ambiguity → `chore`.

Infer scope from an explicit crate/path or established meta-scope. If it is ambiguous, end the turn with one concise scope question rather than guessing. A separable multi-idea sketch needs a user-approved split before filing multiple issues.

Apply `type:<type>` inline. Apply `crate:<scope>` only when scope is a real crate; meta-scopes such as `workflow`, `repo`, `ci`, `docs`, and `guide` deliberately have no crate label. If a real checked-in crate lacks its label, create that one crate label first. Validate extra labels instead of inventing them. Never add `phase:*`, `size:*`, or `model:*`.

## Body

Stage markdown in `/tmp` with `apply_patch`:

```markdown
## Description

> <the user's words, verbatim; prefix every source line with `>`>

<up to three short grounding sentences from context already in hand: the touched area,
known file pointers, and user-stated constraints. No speculative design.>
```

Do not create any scope-managed H2 section. Do not read implementation internals merely to enrich a sketch.

For `--from-wish`, resolve the persisted leaf from the main repository root. Require a `wish.md` whose frontmatter has a non-empty `wish:` and no existing valid `filed: "#N"`. Use the `wish:` value as the verbatim description and append its prose body under `## From wish`. If already filed, report the existing number and stop. After a successful create, update only that leaf's frontmatter with quoted `filed: "#N"`.

## File and verify

Create over REST with title, file-backed body, and labels in the same request. On an uncertain response, search for the exact title and author before retrying so a timeout cannot create a duplicate.

Verify the returned object is an issue, its title/labels match, and it has no phase label. Report:

```text
Filed #N: <title>
Labels: <labels>
Next: $scope N when it is ready.
```

Do not post a progress comment, edit another issue, open a PR, or continue into scoping.
