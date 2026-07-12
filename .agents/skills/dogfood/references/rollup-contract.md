# Dogfood Rollup and Evidence Contract

Build the rollup deterministically in the parent. Keep the shape accepted by `scripts/post-dogfood-rollup.mjs`:

```json
{
  "totals": {
    "findings": 0,
    "papercut": 0,
    "missingPrimitive": 0,
    "docGap": 0,
    "blocker": 0,
    "softHolds": 0
  },
  "succeeded": true,
  "buildGreen": null,
  "summary": "Attempt summary",
  "artifact": null,
  "friction": {
    "papercut": [],
    "missing-primitive": [],
    "doc-gap": [],
    "blocker": []
  },
  "softHolds": [],
  "task": {},
  "provenance": {
    "driver": {
      "model": null,
      "surface": "codex-main-thread"
    },
    "phases": {
      "author": {
        "model": null,
        "effort": null,
        "surface": "collaboration.spawn_agent",
        "forkTurns": "none",
        "taskName": null,
        "prompt": null
      },
      "attempt": {
        "model": null,
        "effort": null,
        "surface": "collaboration.spawn_agent",
        "forkTurns": "none",
        "taskName": "dogfood_attempt_<run_id_with_underscores>",
        "prompt": "exact prompt sent"
      },
      "judge": {
        "model": null,
        "effort": null,
        "surface": "collaboration.spawn_agent",
        "forkTurns": "none",
        "taskName": null,
        "prompt": null
      }
    }
  },
  "executionErrors": []
}
```

Use `{ "verdict": "correct|wrong|insufficient-evidence|n-a", "rationale": "..." }` for a judged artifact. Keep `artifact: null` when the approved task expects no visual result.

## Deterministic rules

- Preserve every validated Attempt finding and group by category.
- Derive totals from the grouped arrays; never trust child-supplied totals.
- Add `use-visible-incorrect` to `softHolds` for a wrong artifact.
- Add `blocker` for each high-severity blocker.
- Add `trial-incomplete` when required structured output, solution, engine, or frame evidence is unavailable.
- Add `build-failed`, force `succeeded: false`, and keep `buildGreen: false` when a heavy Attempt does not build.
- Add `engine-cleanup-failed` when a run-owned engine remains.
- Keep insufficient evidence actionable but do not describe it as visible incorrectness.
- Set `succeeded: false` when Attempt failed or the trial could not complete. Do not fabricate friction to explain a harness failure; put that in `executionErrors` and a soft hold.

A soft hold has `kind`, optional `where`, and `detail`.

## Honest provenance

Store the exact prompt sent to each phase. For a skipped phase, keep `prompt: null`. Record the actual dispatch surface and task name. Keep `model` and `effort` null unless the active tool itself reports them; prompt wording, a task name, or a local configuration file is not evidence that a model ran.

## Run directory

Stage only:

- `rollup.json`: the bare rollup object above;
- `judged-frame.png`: only the exact file Judge captured and graded;
- `solution/`: only the heavy Attempt's scratch crate.

Do not create a substitute frame. Persist with `scripts/dogfood-evidence.sh`, then post with `scripts/post-dogfood-rollup.mjs` as directed by the parent skill.
