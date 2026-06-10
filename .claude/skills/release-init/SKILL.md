---
name: release-init
description: Bootstrap a new aether release. Copies the release template project (canonical schema with the phase vocabulary in the built-in Status field plus Type/Size/AgentReady/BounceTo/ADR/AuthBudget custom fields, and the server-side added→Backlog / closed→Done workflows), populates .claude/release-state.json with the project ID + field/option ID cache, optionally seeds the project with starter issues. Required before /scope works.
---

# /release-init — release bootstrap skill

Bootstraps the GitHub Project + local config that the other release skills depend on. The per-release project is a **copy of the release template project** — the copy carries the field schema, views, and the two configured workflows (item added → Backlog, issue closed → Done), so phase placement on add and close runs on GitHub's side with no skill writes. Idempotent only when `--reuse` is passed; otherwise creates a fresh project each call.

**Status-as-Phase.** The lifecycle vocabulary (Backlog … Done, Bounced, Stalled) lives in the built-in `Status` field's options, because GitHub's built-in workflows can only set `Status` — never a custom single-select. The built-in field can't be renamed or deleted, so the UI header reads "Status"; everything else (this skill, `release-state.json`, the other skills, chat) keeps calling it **Phase** — the `field_cache` key `"Phase"` simply points at the field named `Status`.

## Invocation

```
/release-init <version>                            copy the template into "aether <version>"
/release-init <version> --reuse <project-number>   adopt an existing project (no creation)
/release-init <version> --import #N,#M,#...        add listed issues to the project
/release-init <version> --owner <owner>            override default owner (iamacoffeepot)
/release-init --init-template                      one-time: create the release template project
```

Combinable: `/release-init 0.4 --reuse 2 --import #297,#694`.

## Preconditions

1. `gh auth status` must include `project` scope. If not, instruct the user to run `gh auth refresh -s project` and abort.
2. The bootstrap script must exist at `scripts/release-project-init.sh` (committed in repo). If missing, abort with a pointer.
3. `.claude/release-state.json` should not already exist (the file is the active-release marker; only one at a time). If it exists, ask the user to confirm overwrite before proceeding.

## Steps

### 0. (One-time) create the template

If no project titled `aether release template` exists for the owner, `/release-init --init-template` runs:

```bash
bash scripts/release-project-init.sh --init-template --owner <owner>
```

The script creates the project, replaces the `Status` field's options with the phase vocabulary (one `updateProjectV2Field` mutation), and creates the other custom fields. The two workflow toggles ("Item added to project" → Backlog, "Item closed" → Done) are **UI-only** — the workflow API is read/delete-only — and the script prints the exact steps. Done once; every release copies them for free (GitHub excludes only auto-add workflows from copies, which this flow doesn't use — `/sketch` adds items itself).

### 1. Create or adopt the project

If `--reuse <num>` was not passed:

```bash
bash scripts/release-project-init.sh <version> --owner <owner>
```

The script locates the template by title and copies it (`gh project copy`) into `aether <version>`. Capture the project number from the script's output. If the script reports the template is missing, run step 0 first.

If `--reuse <num>` was passed, skip creation and use `<num>` directly. Verify the project exists and has the expected fields by running the next step's `field-list` and checking that Status (carrying the phase options), Type, Size, AgentReady, BounceTo, ADR, AuthBudget are present. If any are missing, abort with a message naming the missing fields.

### 2. Query the field cache

```bash
gh project field-list <project-number> --owner <owner> --format json
gh project view <project-number> --owner <owner> --format json
```

Extract:
- The project's GraphQL node ID (from `view`, `.id`).
- For each of Status, Type, Size, AgentReady, BounceTo: the field ID and, for single-select fields, every option's ID. **The field named `Status` is cached under the key `"Phase"`** — the copy regenerates all field/option IDs, so never reuse IDs from the template or a prior release.
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
  },
  "item_cache": {}
}
```

The `"Phase"` entry holds the field named `Status` on the board (see [Status-as-Phase](#release-init--release-bootstrap-skill) above) — the key name is the tooling vocabulary, not the UI name. `item_cache` starts empty; `/sketch` seeds it per filed issue and other skills append on lookup miss.

Write atomically: temp file then rename. Set permissions to user-only (`chmod 600`) since this is operational state, not source.

### 4. Ensure `.gitignore` covers it

Append `.claude/release-state.json` to `.gitignore` if not already present. This file is per-machine operational state, not project source. Do not commit.

### 5. Import issues if `--import` was given

For each issue in the comma-separated list:

```bash
gh project item-add <project-number> --owner <owner> --url https://github.com/<owner>/aether/issues/<number>
```

No Phase write — the copied "item added" workflow sets Backlog server-side. Spot-check the first import landed in Backlog; if it didn't (the workflow toggle was skipped on the template), set the field explicitly and remind the user to fix the template's workflows. Don't set Type/Size/AgentReady — those are `/scope`'s responsibility per-issue. Record each returned item ID in `item_cache`.

### 6. Print summary

```
✓ aether <version> bootstrapped
  Project: https://github.com/users/<owner>/projects/<number>
  Local state: .claude/release-state.json
  Imported: <N> issue(s)

Next:
  1. Open the project URL above; the board view groups by Status (the phase vocabulary).
     Verify both workflows copied: item added → Backlog, item closed → Done.
  2. Add more issues: /sketch (files + board-adds in one step)
  3. Scope an issue: /scope <issue-number>
```

## Failure modes

- **`gh` lacks `project` scope**: abort with the refresh command.
- **Template project missing**: the script exits with a pointer; run `/release-init --init-template`, do the two printed workflow toggles, then re-run.
- **Bootstrap script fails partway**: leave whatever was created on the GH side, report the error, do not write `release-state.json`. The user can `gh project delete` and retry.
- **`field-list` returns fewer fields than expected** (e.g. someone hand-deleted a field, or the Status options weren't replaced on the template): abort and report the missing fields/options by name.
- **`.claude/release-state.json` already exists and `--force` not passed**: ask the user to confirm overwrite before proceeding.
- **`--import` issue doesn't exist or is in a different repo**: skip with a warning comment; continue with the rest.

## What `/release-init` does NOT do

- Configure the board view layout (group-by, sort, sub-grouping) or toggle workflows. Both are UI-only; both live on the template, configured once at `--init-template` time and carried into every copy.
- Add Type/Size/AgentReady values to imported issues — `/scope` handles that.
- Migrate items from an older release project. If you want to move issues from `aether 0.3` to `aether 0.4`, use `gh project item-archive` on the old + `--import` on the new.
- Rename the Status field. GitHub doesn't allow it; the tooling vocabulary "Phase" lives in `release-state.json`'s cache key and everything downstream of it.
- Delete or close old release projects. The user does that manually when they're done with a release.

## Notes on `release-state.json`

- The file is the **active-release marker**. Only one exists at a time per repo. Switching releases means re-running `/release-init <newversion>` (after archiving the old).
- The `field_cache` is invalidated if anyone hand-edits fields/options in the UI. If `/scope` or another skill ever fails with a "field ID not found" error, run `/release-init <version> --reuse <num>` to rebuild the cache against the same project. Rebuilding preserves `item_cache` (item IDs only die with the project itself).
- The file is `chmod 600` because it carries operational state; not sensitive per se, but personal to the machine.
