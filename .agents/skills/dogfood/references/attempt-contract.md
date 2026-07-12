# Dogfood Attempt Contract

Run Attempt as a fresh consumer. Give it only `medium`, `prompt`, `surfaceUnderTest`, and a `renders` boolean, plus public surface pointers, public documentation root, exact solution path, one parent-provisioned engine id (or null), and this contract. Never give it `expectedArtifact`, the issue, diff, implementation plan, producer reasoning, or private implementation paths.

## Consumer rules

- Start from `docs/guide/SUMMARY.md` and public crate signatures.
- Log a `doc-gap` before reading private implementation or tests to overcome missing public guidance.
- Log friction at the point it occurs. Do not heroically hide an awkward API, missing primitive, surprising default, confusing error, or blocker.
- Write only below the supplied `solution_dir`; keep the repository read-only.
- Do not commit, push, post to GitHub, edit issue state, or inspect git history for implementation clues.
- Use or terminate only the supplied engine id. Never spawn another engine.

For `drive`, write no crate and return `buildGreen: null`, `solutionPath: null`. For `author` and `build-layer`, put the complete scratch crate at the exact supplied solution directory, build it with the appropriate public workflow, and return a boolean `buildGreen` plus the exact `solutionPath`. A failed heavy build requires `buildGreen: false` and `succeeded: false`.

## Rendering and engine handoff

When `renders` is true, leave exactly one run-owned engine alive for Judge and return its id. Immediate-mode draws clear after one frame, so return every one-shot mail needed to reconstruct the judged frame in `replayMails`. Use an empty bundle only when a component redraws continuously.

When `renders` is false, terminate the supplied engine before returning and use `engineId: null` and `replayMails: []`.

## Output

Return exactly one JSON object with no prose or Markdown fence:

```json
{
  "succeeded": true,
  "summary": "What the consumer accomplished and how it went.",
  "engineId": null,
  "replayMails": [],
  "buildGreen": null,
  "findings": [],
  "solutionPath": null
}
```

Each replay mail has:

```json
{
  "recipient_name": "aether.render",
  "kind_name": "aether.draw_triangle",
  "params": {}
}
```

Omit `params` only for a fieldless kind.

A non-null `engineId` must equal the supplied parent-provisioned id and names the one live engine handed to Judge.

Each finding has:

```json
{
  "category": "papercut",
  "severity": "medium",
  "where": "public surface or task step",
  "what": "concrete friction",
  "suggested": "consumer-facing improvement, or an empty string"
}
```

`category` is `papercut`, `missing-primitive`, `doc-gap`, or `blocker`. `severity` is `high`, `medium`, or `low`. An empty finding list means the surface was friction-free, not that reporting was skipped.
