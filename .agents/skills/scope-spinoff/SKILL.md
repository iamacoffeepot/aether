---
name: scope-spinoff
description: "Turn selected Aether scope Side findings into linked Backlog issues, then remove only the filed findings from the parent. Use after scope when the user chooses indices or requests all/dry-run."
---

# Scope Spinoff

Read [Codex harness](../_shared/codex-harness.md), [GitHub workflow](../_shared/github-workflow.md), and [$sketch](../sketch/SKILL.md).

Support a parent issue plus comma-separated 1-based indices, `--all`, or `--dry-run`. A closed parent is allowed.

1. Read the parent body over REST and extract bullets only from its exact `## Side findings` section.
2. If the section is absent/empty, report no findings and stop. If no selection was supplied, print a numbered list and end the turn asking for indices, `all`, or cancel.
3. Validate the complete selection before filing. Reject out-of-range indices. Resolve every selected finding's title/type/scope using `$sketch` mechanics; if any scope is ambiguous, ask before filing anything.
4. Search existing issue bodies for the parent link plus the exact finding text. Surface a probable prior spinoff instead of filing a duplicate.
5. For dry-run, print each proposed title/labels/body and the parent entries that would be removed, then stop without mutation.

For each confirmed selected finding, file sequentially with `$sketch` mechanics. Preserve the finding line verbatim in `## Description` and append:

```markdown
## Found during

Spun off from #<parent> Side findings via `$scope-spinoff` on <YYYY-MM-DD>.
```

Children start at Backlog with no phase/size/model labels. Do not create dependency relationships or parent comments; the body reference creates the timeline cross-reference.

After each successful child create, re-read the parent and remove that exact finding from `## Side findings`, preserving every other byte of user and managed content. Delete the H2 only when no findings remain. Abort on a concurrent edit to the section rather than overwriting it.

If filing succeeds but the parent patch fails, report the child number and leave a clear repair instruction; never file that finding again on retry. If a later filing fails, retain completed children and already-applied removals, stop, and report successful/remaining original indices.

Report each child and `Next: $scope <child>` without scoping it.
