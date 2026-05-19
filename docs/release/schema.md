# Release project schema

Each aether release lives in its own GitHub Project (`aether 0.4`, `aether 0.5`, …). The schema is created by `release-project-init.sh <version>` and used by future `/release-*` skills.

## Phase (custom single-select field — board's grouping column)

We use a custom `Phase` field rather than the default `Status` because we need to set its options programmatically; `Status`'s options aren't editable via `gh project field-create`. Group the board view by Phase and ignore Status.

| Phase     | Meaning                                                    | Advances by  |
|-----------|------------------------------------------------------------|--------------|
| Backlog   | Not yet picked up for this release                         | User         |
| Define    | Problem framing in progress                                | User + Claude |
| Design    | Tradeoffs / options / ADR drafting                         | User + Claude |
| Plan      | Sequencing, dependencies, sub-issues                       | User + Claude |
| Ready     | Agent-ready; awaiting dispatch                             | Gate: `AgentReady=Yes` |
| Executing | Agent has the issue; PR in flight                          | Auto         |
| Refine    | CI-feedback loop, agent-driven                             | Auto         |
| Done      | PR merged                                                  | Auto         |
| Bounced   | Phase regression — see `BounceTo`                          | User triage  |
| Stalled   | Env/tooling failure, blocks dispatch                       | User triage  |

## Custom fields

| Field        | Type           | Values / shape                                | Notes |
|--------------|----------------|-----------------------------------------------|-------|
| `Phase`      | single-select  | (see above)                                   | Board column |
| `Type`       | single-select  | feat / fix / chore / docs / refactor / ci / test | Mirrors conventional-commit type |
| `Size`       | single-select  | S / M / L                                     | Caps parallelism in Phase C executor |
| `AgentReady` | single-select  | No (default) / Yes                            | Gate to `Ready` |
| `BounceTo`   | single-select  | Plan / Design / Define                        | Only meaningful when `Phase=Bounced` |
| `ADR`        | text           | e.g. "ADR-0072"                               | Non-null in `Design` if architectural |
| `AuthBudget` | text           | freeform                                      | Phase C placeholder (cost/time/retry caps) |

## Native fields used

- `Repository`: auto-populated when issues are added.
- Issue dependencies: GH's native feature, not a custom field. `gh issue edit <n> --add-dependency <m>` and the dependency graph view.

## Phase-transition rules (enforced socially in Phase B; codified in `/release-promote` in Phase C)

```
Backlog  → Define     body has a problem statement
Define   → Design     if multi-PR, umbrella issue exists; if architectural, ADR drafted
Design   → Plan       tradeoffs aired in comments; ADR merged if applicable
Plan     → Ready      dependencies declared, AgentReady=Yes, one concept per issue
Ready    → Executing  agent dispatched (via /delegate or fleet executor)
Executing → Refine    PR opened, CI running
Refine   → Done       CI green, merged
Executing/Refine → Bounced   agent surfaced an upstream-phase issue (BounceTo set)
Any      → Stalled    env/tooling failure (not the issue's fault)
```

## Operations

- **Create release project:** `release-project-init.sh 0.4`
- **Add issue to project:** `gh project item-add <project> --owner iamacoffeepot --url <issue-url>`
- **Set a field on an issue:** `gh project item-edit --id <item-id> --field-id <field-id> --single-select-option-id <option-id>` (or `--text` / `--number` etc.)

Field and option IDs aren't human-readable; future `/release-*` skills will cache them at project-create time so callers can use field names directly.
