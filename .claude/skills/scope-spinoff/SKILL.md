---
name: scope-spinoff
description: "Turn selected Aether Side findings into linked unscoped issues, then remove only the filed findings from the parent."
---

# /scope-spinoff — file selected Side findings

Read [sketch](../sketch/SKILL.md). Support a parent issue plus comma-separated 1-based indices, `--all`, or `--dry-run`. A closed parent is allowed.

1. Read the parent body over REST and extract bullets only from its exact `## Side findings` section.
2. If absent or empty, report none. If no selection was supplied, print a numbered list and ask for indices, all, or cancel.
3. Validate the complete selection before filing. Resolve every selected title/type/scope using `/sketch`; ask before any mutation when a scope is ambiguous.
4. Search existing issue bodies for the parent link plus exact finding text and surface probable duplicates.
5. For dry-run, print every proposed title, labels, body, and parent removal, then stop.

File confirmed selections sequentially through `/sketch`. Preserve the finding line verbatim under Description and append:

```markdown
## Found during

Spun off from #<parent> Side findings via `/scope-spinoff` on <YYYY-MM-DD>.
```

Children start unscoped with no managed artifact or body routing. Do not create dependencies or parent comments; the body reference supplies the timeline link.

After each successful child create, re-read the parent and remove exactly that finding from Side findings while preserving every other byte. Delete the H2 only when none remain. Abort on concurrent section edits. If child creation succeeds but parent patch fails, report the child and repair instruction and never duplicate it on retry.

Report each child and `Next: /scope <child>`. Never scope the child or modify any other parent section.
