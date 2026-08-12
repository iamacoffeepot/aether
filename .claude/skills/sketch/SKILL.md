---
name: sketch
description: "Capture a rough idea as a lint-clean unscoped Aether issue while preserving the user's words. Do not scope, design, plan, or implement it."
---

# /sketch — file an unscoped issue

Support an idea plus optional `--type`, `--scope` or `--crate`, extra `--label` values, or `--from-wish <leaf/wish.md>`. Ask only when required input is absent.

## Title and labels

Read `TYPES` and `META_SCOPES` from `.github/workflows/issue-labels.yml`. Build a lowercase Conventional Commit title `<type>(<scope>): <subject>`.

Infer type conservatively: bug/regression → `fix`; new capability → `feat`; documentation → `docs`; performance → `perf`; behavior-preserving structure → `refactor`; intermittent behavior → `flake`; tooling, workflow, CI, cleanup, tests, or ambiguity → `chore`.

Infer scope from an explicit crate/path or established meta-scope. Ask when ambiguous and obtain approval before splitting multiple ideas. Apply `type:<type>`. Apply `crate:<scope>` only for a real crate; meta-scopes deliberately have no crate label. Create one missing real-crate label only when required. Validate extra labels. Never add managed sections, body routing, or routing/lifecycle labels.

## Body

Stage file-backed markdown:

```markdown
## Description

> <the user's words verbatim, one quoted source line per input line>

<up to three grounded sentences from context already in hand; no speculative design>
```

Do not create any scope-managed H2 section or read implementation internals merely to enrich a sketch.

For `--from-wish`, require a persisted leaf with non-empty `wish:` and no existing valid `filed: "#N"`. Use the wish value verbatim, append leaf prose under `## From wish`, and after successful filing update only `filed:`.

## Create and verify

Create over REST with title, file-backed body, and labels in one request. After an uncertain response, search exact title and author before retrying. Verify it is an issue, taxonomy is correct, and no managed section exists.

Report number, title, labels, and `Next: /scope <issue>`. Do not comment, edit another issue, open a pull request, or continue into scoping.
