---
name: release-init
description: Bootstrap a new aether release — ensure the phase / bounce-to / size / model labels exist. Issue phase is carried by phase:* labels (no project board).
---

# /release-init — release bootstrap skill

Bootstraps the label vocabulary + local marker the other release skills depend on. Issue phase is carried entirely by `phase:*` labels — Backlog and Done are label-absence, each active phase has its own label, and `size:*` / `model:*` carry the routing metadata `/scope` stamps at Plan. There is no project board: every pipeline write rides REST, so the contended GraphQL pool stays free for the one op that needs it (the PR un-draft in `/land`).

## Invocation

```
/release-init <version>                  ensure labels for "aether <version>"
/release-init <version> --owner <owner>  override default owner (iamacoffeepot)
```

## Preconditions

1. `gh auth status` must include `repo` scope (the standard scope). If not, instruct the user to run `gh auth refresh` and abort.
2. The bootstrap script must exist at `scripts/release-project-init.sh` (committed in repo). If missing, abort with a pointer.

## Steps

### 1. Ensure the label vocabulary

```bash
bash scripts/release-project-init.sh <version> --owner <owner>
```

The script ensures every pipeline label exists and is idempotent — a re-run only fills gaps. It covers the phase labels (`phase:define` … `phase:stalled`; Backlog and Done carry none), the `bounce-to:*` resume targets `/bounce` stamps, the `size:*` weights (including `size:xl` = fat), and the `model:*` routing labels. `type:*` labels are stamped by `/sketch` from the title's conventional-commit prefix and `crate:*` labels are created on demand at filing, so this step doesn't touch them.

### 2. Print summary

```
✓ aether <version> bootstrapped
  Labels ensured: phase:* / bounce-to:* / size:* / model:*

Next:
  1. File an issue: /sketch (a new issue is Backlog by carrying no phase:* label)
  2. Scope an issue: /scope <issue-number>
```

## Failure modes

- **`gh` lacks `repo` scope**: abort with the `gh auth refresh` pointer.
- **Bootstrap script fails partway**: report which label create failed; the script is idempotent, so a re-run resumes from where it stopped.

## What `/release-init` does NOT do

- Create or configure a project board — phase is carried by `phase:*` labels, not a board.
- Stamp `type:*` / `size:*` / `model:*` labels on any issue — `/sketch` and `/scope` own those; this skill only ensures the labels exist for them to stamp.
- Import or migrate issues — a new issue enters the pipeline via `/sketch`.
- Delete or close old releases. The user does that manually when they're done with a release.
