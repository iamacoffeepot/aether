---
name: scope-spinoff
description: Triage §Side findings into child GitHub issues. Reads the parent issue's §Side findings section, lets the user pick which entries to spin off, files each via /sketch's mechanics as a new Backlog-Phase issue with a link back to the parent, then removes the spun-off entries from the parent body. The children's timeline cross-references on the parent record what was spun.
---

# /scope-spinoff — side findings → child issues

`/scope` populates `## Side findings` on an issue with unrelated stuff it notices while reading the code (dead code, undocumented invariants, latent bugs, drift). At review time the user decides which of those deserve their own issues. `/scope-spinoff` handles the filing mechanics.

## Invocation

```
/scope-spinoff <issue>                  interactive — list findings, prompt for picks
/scope-spinoff <issue> <indices>        non-interactive — e.g. "1,3,5"
/scope-spinoff <issue> --all            spin off every finding
/scope-spinoff <issue> --dry-run        show what would be filed, change nothing
```

`<indices>` are 1-based, matching the order findings appear in the §Side findings section.

## Preconditions

1. `.claude/release-state.json` exists.
2. Parent issue is in the active project (no Phase restriction — even Done parents can spin off informational findings).
3. Parent's body has a `## Side findings` section with at least one bullet.

## Interactive mode

Read `## Side findings` from the parent's body. Print a numbered list:

```
Found 4 side findings on #<parent>:
  1. <text>
  2. <text>
  3. <text>
  4. <text>

Pick which to spin off (comma-separated, "all", or empty to cancel):
```

Wait for the user's response. Parse "1,3,4" into a set of indices.

## Per-finding actions

For each selected finding:

1. **File the child via `/sketch`'s mechanics** (read `.claude/skills/sketch/SKILL.md` — it is the single definition of issue filing). The finding text is the sketch input: `/sketch` owns title inference (type prefix from its inference table, crate scope from the finding's file pointer — e.g. `aether-substrate/dispatch.rs:142` → `substrate`; ask the user inline if the pointer is missing or ambiguous), label selection, board placement at `Phase=Backlog`, and the item-ID cache write.

2. **Append the spinoff context** to the body `/sketch` produces — the lead comment plus a `## Found during` section after `## Description`:

   ```markdown
   <!-- pr-body-ok: e — auto-filed from scope-spinoff, scope is parent-issue context -->

   ## Found during

   Spun off from #<parent> §Side findings during `/scope-spinoff` on <date>.
   ```

   For a spun-off finding the verbatim blockquote in `## Description` is the finding line as it appeared in the parent body; the expansion is any context the agent already has from reading the code. Don't set Type/Size/AgentReady — `/scope` handles those when the child gets scoped.

No comment is posted on the parent: the child's `Spun off from #<parent>` line creates a cross-reference event in the parent's timeline automatically, one per filing, which is the per-finding record.

After all selected findings are filed, **rewrite the parent's body** to remove the spun-off entries from §Side findings. Remaining findings keep their original numbering shifted to fill gaps (1, 2, 3 after removing original 2 means new 1, 2 — the user re-runs `/scope-spinoff` with fresh indices).

If §Side findings becomes empty after removal, delete the section header too.

## Dry-run

`--dry-run` prints the planned actions without executing:

```
Would file:
  - "chore(substrate): drop unreferenced drop_handler" (from finding 1)
  - "test(actor): add coverage for ReplaceResult error path" (from finding 3)

Would remove findings 1, 3 from #<parent> §Side findings.

(no changes made — re-run without --dry-run to file)
```

## Failure modes

- **No §Side findings on parent**: refuse with *"No side findings on #N to spin off."*
- **Index out of range**: refuse with valid range, e.g. *"Index 5 is out of range — valid: 1-4."*
- **Filing partway through fails** (e.g. GitHub rate limit between findings 2 and 3): commit completed work — already-filed issues stay filed, already-removed entries stay removed. Report which indices succeeded and which failed. The user re-runs with the failed indices once the cause is resolved.
- **Conventional-commit scope unknown** (file pointer ambiguous): ask the user inline:

  ```
  Finding 3 has no clear crate scope: "<text>"
  Use which scope? (e.g. substrate, actor, mesh, or "skip" to omit this finding)
  ```

- **Parent issue closed**: still allowed — informational findings can be filed against a closed parent. Note it in the run's output.

## What `/scope-spinoff` does NOT do

- Scope the child issues. They're filed at `Phase=Backlog` with a title + description; running `/scope <child>` is a separate operation.
- Auto-link as dependencies. The §Found during line in the body (and the timeline cross-reference it creates on the parent) is the connection. GitHub's native `--add-dependency` feature could be added in v2 if the dependency graph view becomes load-bearing.
- Modify §Problem statement, §Design notes, or §Implementation plan on the parent. Only §Side findings is touched.
- Reorder remaining findings. Index reuse means re-run = different number for the same item; user is expected to re-read after a partial spin-off.

## Why removal, not strikethrough

Two alternatives considered for handling spun-off entries:

- **Strikethrough**: visually clear what was spun, preserves history in-body. But clutters the section over time and competes with future spin-off runs.
- **Move to a §Spun off subsection**: keeps the body as the canonical record. But duplicates the timeline's cross-reference trail and bloats the body.

**Removal** is cleanest: the §Side findings section stays a live to-triage list; the timeline cross-references are the historical record; the child issue itself carries the long-form context. The body shouldn't be a log.
