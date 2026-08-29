# Authoring terrain

**Class:** drive. No recompile — start a desktop substrate and connect the
[MCP harness](../mcp-harness.md). This recipe drives already-built components;
`capture_frame` needs the desktop chassis, not the headless default.

Terrain authoring is a transaction, not an undo exercise: create a revisioned
mark, derive a bounded operation from its current geometry, stage it, inspect
the staged terrain, then explicitly commit or discard it. Staging never changes
committed terrain, and a committed preview is pixel-identical to the result of
that commit.

This recipe has two equally valid front doors:

- A person uses `TerrainWorkbench`: mark the terrain in its viewport, set an
  instruction and operator, then use **Stage**, **Preview**, and **Accept** or
  **Discard**.
- An agent drives the same components directly over `send_mail`, with the
  `aether.kit.mark.*`, `aether.kit.terra.*`, and `aether.kit.world.*` kinds.
  There is no bespoke terrain tool surface: the generic harness calls carry
  every operation, and `describe_kinds` is the authoritative source for each
  kind's exact shape.

> **Verify against current code.** The public kinds live in
> `crates/aether-kit-terrain/src/{mark,terra,world}/` and
> `crates/aether-kit-workbench/src/`; the design
> contracts are [ADR-0142](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0142-terrain-mark-identity-and-revisions.md)
> and [ADR-0143](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0143-terrain-proposal-commit-transaction.md). If a
> kind, field, or tool below has changed, update this recipe with the code.

## Load the peer set and retain its identities

Load the exports in this order — the mark, world, and terra actors from the
`aether-kit-terrain` module, the workbench from `aether-kit-workbench`:

1. `aether_kit_terrain@aether.kit.mark`, named exactly `aether.kit.mark` for the
   workbench's overlay refresh.
2. `aether_kit_terrain@aether.kit.world`.
3. `aether_kit_terrain@aether.kit.terra`, configured with the MarkBook's returned
   `mailbox_id` as `TerraConfig.mark_book_mailbox`.
4. `aether_kit_workbench@aether.kit.workbench`, configured with the three returned mailbox
   ids in `WorkbenchConfig { mark_book_mailbox, terra_mailbox, world_mailbox, ... }`
   and a non-overlapping `WorkbenchLayout`.

Use `load_component` and `describe_component` for the live config schemas. The
returned values have two different jobs:

| Value | Use it for |
| --- | --- |
| `LoadResult.mailbox_id` | `TerraConfig` and `WorkbenchConfig` peer fields. |
| `LoadResult.name` | Every MCP task-tool mailbox argument and every generic `send_mail` recipient. |

For example, a load named `marks` normally returns
`aether.component/aether.embedded:marks`. That entire string is the value for
`mark_book_mailbox` and `recipient_name`; neither `aether.kit.mark`, a registry
selector, nor the tagged mailbox id is a substitute. Preserve the exact returned
names below as `<marks-name>`, `<world-name>`, `<terra-name>`, and
`<workbench-name>`.

The workbench creates its `tools`, `viewport`, `console`, and `shell` inline
children on its first tick. Do not send its private intent kind directly: the
root accepts it only from its own panel. Its public read surface is
`aether.kit.workbench.query`.

## The human workbench loop

The workbench turns viewport input into `aether.kit.world.pick_terrain` mail,
then records the returned `WorldPoint` in octimeters. Its tool panel keeps one
operation in flight, so wait for the status to settle before the next action.

1. **Mark.** Choose **Point**, **Path**, or **Area**. Type the label in the
   **Instruction** field, then click the terrain in the viewport. A point mark
   is created on its first hit. For a path, collect at least two hits and click
   **Finish mark**; for an area, collect at least three hits and finish it.
   The workbench creates through `aether.kit.terra.create_mark`, which in turn
   creates a MarkBook record and selects the returned `MarkRef`.
2. **Instruct.** With a selection, changing **Instruction** relabels that
   selection through `aether.kit.terra.relabel_selection`. Choose **Brush** or
   **Automaton**, then set radius and spacing in **octimeters**, material,
   `max_steps`, and `max_subcells`. Brush accepts point, path, and area marks
   (a point becomes a one-point path); Automaton accepts only a point mark.
3. **Stage.** Click **Stage**. The workbench first gets the selected mark and
   requires its id and revision to still match. It then sends
   `aether.kit.world.propose` with either `ProposalOperation::ApplyBrush` or
   `ProposalOperation::RunAutomaton`. `ProposalResult::Staged` supplies a
   session-scoped `ProposalId`, operation result, and `ProposalDigest`.
4. **Preview.** Click **Preview** only after the staged result. The workbench
   sends `aether.kit.world.set_proposal_preview` with
   `proposal_id: Some(ProposalId)`. The preview uses the staged terrain while
   leaving the committed terrain alone.
5. **Validate.** Inspect the visible result and the status/console. Query the
   root with `aether.kit.workbench.query` when you need the cached
   `selection`, `draft`, `proposal`, `busy`, and `failure` values. A rejected
   proposal, stale mark reference, unsupported automaton geometry, or exhausted
   operator budget is a decision point, not a partial commit.
6. **Commit or discard.** Click **Accept** to send
   `aether.kit.world.commit_proposal`, or **Discard** to send
   `aether.kit.world.discard_proposal`. Commit requires a fresh proposal;
   discard also accepts a stale retained proposal. A successful accept clears
   the workbench's staged state.

## Generic `send_mail`: the live component codec

Use generic `send_mail` when you are intentionally driving the wire kinds. It
uses the live kind schema, so `recipient_name` is the exact loaded component
name and `kind_name` is the kind. Keep `replies: "all"` while establishing a
workflow so the correlated result is visible.

The generic codec exposes `MarkId` as its Rust newtype record, `{ "0": 7 }`.
That is correct here. The task-level tools below deliberately adapt the same id
to `{ "value": 7 }`; do not mix those two representations.

Create a path mark. The path is a variable-length collection, but every point
is a named octimeter record rather than an `[x, z]` pair:

```json
{
  "mails": [{
    "engine_id": "<engine-id>",
    "recipient_name": "<marks-name>",
    "kind_name": "aether.kit.mark.create",
    "params": {
      "geometry": {
        "Path": [
          { "x_octimeters": 3968, "z_octimeters": 2048 },
          { "x_octimeters": 4224, "z_octimeters": 2048 }
        ]
      },
      "label": "ridge brush"
    }
  }],
  "replies": "all"
}
```

Retain the `MarkCreateResult::Created.reference`, including its revision. To
stage a brush from that reference, send the proposal wrapper rather than the
immediate `aether.kit.world.apply_brush` kind:

```json
{
  "mails": [{
    "engine_id": "<engine-id>",
    "recipient_name": "<world-name>",
    "kind_name": "aether.kit.world.propose",
    "params": {
      "operation": {
        "ApplyBrush": {
          "request": {
            "source": { "id": { "0": 7 }, "revision": 1 },
            "path": [
              { "x_octimeters": 3968, "z_octimeters": 2048 },
              { "x_octimeters": 4224, "z_octimeters": 2048 }
            ],
            "brush": {
              "radius_octimeters": 128,
              "spacing_octimeters": 256,
              "material": 3
            },
            "budget": { "max_steps": 2, "max_subcells": 4096 }
          }
        }
      }
    }
  }],
  "replies": "all"
}
```

Only continue after `aether.kit.world.proposal_result` is `Staged`. Its
`proposal_id` is a named `{ "value": ... }` record. The next sends select the
preview and then commit that exact id:

```json
{
  "mails": [{
    "engine_id": "<engine-id>",
    "recipient_name": "<world-name>",
    "kind_name": "aether.kit.world.set_proposal_preview",
    "params": { "proposal_id": { "value": 1 } }
  }]
}
```

```json
{
  "mails": [{
    "engine_id": "<engine-id>",
    "recipient_name": "<world-name>",
    "kind_name": "aether.kit.world.commit_proposal",
    "params": { "proposal_id": { "value": 1 } }
  }]
}
```

Use `aether.kit.world.discard_proposal` with the same `proposal_id` to abandon
it, or clear an active preview with
`aether.kit.world.set_proposal_preview { "proposal_id": null }`. All three
requests reply with `aether.kit.world.proposal_result`; inspect its typed
`Rejected` variants rather than treating a missing image change as success.

To inspect the human route without reaching into private child messages, query
the root's public cache:

```json
{
  "mails": [{
    "engine_id": "<engine-id>",
    "recipient_name": "<workbench-name>",
    "kind_name": "aether.kit.workbench.query",
    "params": null
  }]
}
```

## Task-level MCP loop

The retired task adapters used to preflight these sends; driving the kinds
directly, those checks are the agent's own responsibility — reject a stale or
missing mark reference yourself before proposing against it, and require
`Path` geometry for a `source_mark` brush input.

1. **Mark:** send `aether.kit.mark.create` to the MarkBook's loaded lineage
   address and keep the reference from `MarkCreateResult::Created`. Semantic
   selection, rename, move, and deletion are the `aether.kit.terra.*` kinds
   on the TerraEditor.

2. **Instruct and stage:** fetch the source mark with `aether.kit.mark.get`,
   verify the reference is current and the geometry fits the operation, then
   send `aether.kit.world.propose` to the WorldView. A brush wants a path
   source; an automaton wants a point source, converted to a cell seed with
   negative-safe flooring.

3. **Preview and validate:** retain `ProposalResult::Staged.proposal_id` and
   send `aether.kit.world.set_proposal_preview`. Validate both parts of the result:
   `PreviewSet` must name the proposal and carry its digest, while the image
   must show the bounded edit you intended. A digest has named
   `touched_chunks`, `triangle_count`, and optional named meter-space
   `changed_geometry_bounds`; an operator result reports named `steps_run`,
   `subcells_written`, and `touched_chunks` even when it fails at a budget
   boundary.

4. **Commit or discard:** send `aether.kit.world.commit_proposal` (or
   `aether.kit.world.discard_proposal`) with the same proposal id only after
   validation. `StaleProposal`, `UnknownProposal`, `NoTouchedChunks`,
   `StagedProposalLimitReached`, and `ProposalIdExhausted` are ordinary
   domain rejections, not reasons to guess a replacement id.

Kind names above follow the families the components register; take every
exact request and result shape from `describe_kinds` against the live engine
rather than from this page.

## Capture the preview you are validating

`capture_frame` runs its `mails` bundle before reading the PNG. For a loaded
workbench, the current in-tree scenario refreshes the viewport and world at
render stage, then ticks the panel and console. It uses these exact inline
child names, derived by appending the `aether.embedded` lineage:

```json
{
  "engine_id": "<engine-id>",
  "mails": [
    {
      "recipient_name": "<workbench-name>/aether.embedded:viewport",
      "kind_name": "aether.lifecycle.render"
    },
    {
      "recipient_name": "<world-name>",
      "kind_name": "aether.lifecycle.render"
    },
    {
      "recipient_name": "<workbench-name>/aether.embedded:tools",
      "kind_name": "aether.lifecycle.tick"
    },
    {
      "recipient_name": "<workbench-name>/aether.embedded:console",
      "kind_name": "aether.lifecycle.tick"
    }
  ],
  "after_mails": [],
  "checks": [{
    "reduction": "differs_from_background",
    "tolerance": 5,
    "region": { "min_x": 180, "min_y": 0, "max_x": 639, "max_y": 359 }
  }],
  "include_image": true
}
```

Those pixel bounds are the current test fixture's viewport, not a portable
layout. Replace them with the named rectangle from your `WorkbenchLayout`.
`capture_frame` checks run against full-resolution RGBA; use `coverage`,
`centroid`, or `bounding_box` when a more specific visual assertion fits.
`after_mails` is the cleanup bundle when a capture-only state change must be
reversed after readback.

The executable evidence is deliberately in-tree:

- `crates/aether-kit-workbench/tests/terrain_workbench_scenario.rs` drives raw input
  through the real workbench, checks `WorkbenchQueryResult`, then uses
  `SubstrateHarness`, `HarnessOp::capture_with_mails`, `ArtifactGuard`, and
  `aether_harness_substrate_capture::visual::{decode_png, run_checks, target_color_stats}`
  to prove staging is bounded, discard restores the baseline, and accepted
  pixels equal the preview.
- `crates/aether-kit-terrain/tests/proposal_scenario.rs` proves the lower-level
  `aether.kit.world` transaction: staging leaves the committed frame unchanged,
  preview is visibly bounded, commit is pixel-exact with that preview, stale
  peers reject, and replacement drops proposal session state.

Use those scenarios as the validation starting point when changing this
workflow. They are the current names and helpers; do not invent a second
terrain test or visual API.
