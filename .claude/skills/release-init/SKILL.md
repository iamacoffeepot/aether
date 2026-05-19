---
name: release-init
description: Bootstrap a new aether release. Creates a GitHub Project with the canonical schema (Phase column + Type/Size/AgentReady/BounceTo/ADR/AuthBudget custom fields), populates .claude/release-state.json with the project ID + field/option ID cache, optionally seeds the project with starter issues. Required before /scope works.
---

# /release-init — release bootstrap skill

Bootstraps the GitHub Project + local config that the other release skills depend on. Idempotent only when `--reuse` is passed; otherwise creates a fresh project each call.

## Invocation

```
/release-init <version>                            create new project "aether <version>"
/release-init <version> --reuse <project-number>   adopt an existing project (no creation)
/release-init <version> --import #N,#M,#...        add listed issues to project, set Phase=Backlog
/release-init <version> --owner <owner>            override default owner (iamacoffeepot)
```

Combinable: `/release-init 0.4 --reuse 2 --import #297,#694`.

## Preconditions

1. `gh auth status` must include `project` scope. If not, instruct the user to run `gh auth refresh -s project` and abort.
2. The bootstrap script must exist at `scripts/release-project-init.sh` (committed in repo). If missing, abort with a pointer.
3. `.claude/release-state.json` should not already exist (the file is the active-release marker; only one at a time). If it exists, ask the user to confirm overwrite before proceeding.

## Steps

### 1. Create or adopt the project

If `--reuse <num>` was not passed:

```bash
bash scripts/release-project-init.sh <version> --owner <owner>
```

Capture the project number from the script's output (`Project N created.`).

If `--reuse <num>` was passed, skip creation and use `<num>` directly. Verify the project exists and has the expected fields by running the next step's `field-list` and checking that Phase, Type, Size, AgentReady, BounceTo, ADR, AuthBudget are present. If any are missing, abort with a message naming the missing fields.

### 2. Query the field cache

```bash
gh project field-list <project-number> --owner <owner> --format json
gh project view <project-number> --owner <owner> --format json
```

Extract:
- The project's GraphQL node ID (from `view`, `.id`).
- For each of Phase, Type, Size, AgentReady, BounceTo: the field ID and, for single-select fields, every option's ID.
- For ADR and AuthBudget: just the field ID (text fields, no options).

### 3. Write `.claude/release-state.json`

Schema (formatted, no trailing comma):

```json
{
  "active_project": <number>,
  "project_node_id": "PVT_...",
  "release_version": "<version>",
  "owner": "<owner>",
  "field_cache": {
    "Phase": {
      "id": "PVTSSF_...",
      "options": {
        "Backlog": "<id>",
        "Define": "<id>",
        "Design": "<id>",
        "Plan": "<id>",
        "Ready": "<id>",
        "Executing": "<id>",
        "Refine": "<id>",
        "Done": "<id>",
        "Bounced": "<id>",
        "Stalled": "<id>"
      }
    },
    "Type":       { "id": "...", "options": { "feat": "...", "fix": "...", ... } },
    "Size":       { "id": "...", "options": { "S": "...", "M": "...", "L": "..." } },
    "AgentReady": { "id": "...", "options": { "No": "...", "Yes": "..." } },
    "BounceTo":   { "id": "...", "options": { "Plan": "...", "Design": "...", "Define": "..." } },
    "ADR":        { "id": "...", "options": null },
    "AuthBudget": { "id": "...", "options": null }
  }
}
```

Write atomically: temp file then rename. Set permissions to user-only (`chmod 600`) since this is operational state, not source.

### 4. Ensure `.gitignore` covers it

Append `.claude/release-state.json` to `.gitignore` if not already present. This file is per-machine operational state, not project source. Do not commit.

### 5. Import issues if `--import` was given

For each issue in the comma-separated list:

```bash
gh project item-add <project-number> --owner <owner> --url https://github.com/<owner>/aether/issues/<number>
```

Then set `Phase=Backlog` on each (using the now-cached field/option IDs). Don't set Type/Size/AgentReady yet — those are `/scope`'s responsibility per-issue.

### 6. Print summary

```
✓ aether <version> bootstrapped
  Project: https://github.com/users/<owner>/projects/<number>
  Local state: .claude/release-state.json
  Imported: <N> issue(s)

Next:
  1. Open the project URL above, switch board view → group by Phase.
  2. Add more issues: gh project item-add <project-number> --owner <owner> --url <issue-url>
  3. Scope an issue: /scope <issue-number>
```

## Failure modes

- **`gh` lacks `project` scope**: abort with the refresh command.
- **Bootstrap script fails partway**: leave whatever was created on the GH side, report the error, do not write `release-state.json`. The user can `gh project delete` and retry.
- **`field-list` returns fewer fields than expected** (e.g. someone hand-deleted a field): abort and report the missing fields by name.
- **`.claude/release-state.json` already exists and `--force` not passed**: ask the user to confirm overwrite before proceeding.
- **`--import` issue doesn't exist or is in a different repo**: skip with a warning comment; continue with the rest.

## What `/release-init` does NOT do

- Configure the board view layout (group-by, sort, sub-grouping). That's a UI-only step today. The CLI prints instructions; the user does it once.
- Add Type/Size/AgentReady values to imported issues — `/scope` handles that.
- Migrate items from an older release project. If you want to move issues from `aether 0.3` to `aether 0.4`, use `gh project item-archive` on the old + `--import` on the new.
- Mark the project as a template. Possible future refinement: `/release-init --as-template` followed by `gh project copy` per release. Not yet.
- Delete or close old release projects. The user does that manually when they're done with a release.

## Notes on `release-state.json`

- The file is the **active-release marker**. Only one exists at a time per repo. Switching releases means re-running `/release-init <newversion>` (after archiving the old).
- The `field_cache` is invalidated if anyone hand-edits fields/options in the UI. If `/scope` or another skill ever fails with a "field ID not found" error, run `/release-init <version> --reuse <num>` to rebuild the cache against the same project.
- The file is `chmod 600` because it carries operational state; not sensitive per se, but personal to the machine.
