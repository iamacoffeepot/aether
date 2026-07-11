---
name: review
description: "Run Aether pre-land review with specialist lenses. Integrated review of a PR-bound change is automatic in CI (the review action runs it at the PR's first green) — do not run this inline at the end of implement. Use for backfill audits of existing code, or for a change that never becomes a PR."
---

# Review

Use this Codex skill for the pre-land review workflow described by `.claude/workflows/review.js`.

## Source

- Workflow source: `.claude/workflows/review.js`
- Translation rules: `../_shared/claude-to-codex.md`

Read both before running the workflow.

## Inputs

Prefer explicit inputs from the caller:

- `files`: absolute paths for code lenses.
- `testFiles`: absolute paths for test-integrity review.
- `issue`: optional issue or scope text for spec-fidelity review.
- `diffs`: optional per-file diff hunks.
- `lenses`: optional subset of `spec-fidelity`, `correctness`, `test-integrity`, `economy`, and `convention`.
- `depth`: `gate` selects the light per-PR gate (correctness and spec fidelity, Sonnet verification, no challenge); `deep` is the default full five-pillar review.

If explicit files are absent and the current branch is a PR branch, derive a candidate file set from the diff against `origin/main`. Otherwise ask for files rather than guessing.

## Workflow

1. Scope: if `issue` and `diffs` are present, run the whole-change spec-fidelity pass first. Prune clearly out-of-scope files from later passes.
2. Find: run applicable specialist lenses per file. Use subagents for independent file/lens work when the user or skill run calls for parallel review. In the economy lens, actively check large-file pressure: files around or above 1,000 lines, or diffs adding to already-large coordination files, should get an `economy:file-split` finding when there is a concrete responsibility seam and a named child-module extraction that reduces review burden without moving behavior. Also challenge new public or wire-facing tuple/array fields when their positions carry distinct semantics such as axes, units, or ordering: prefer a named schema type with named fields, while leaving genuinely index-addressed vectors, matrices, colors, and buffers alone.
3. Verify: verify correctness findings even when high confidence. Refute low/medium confidence findings before including them. Challenge clean correctness and test-integrity lenses when useful.
4. Roll up confirmed findings first, ordered by severity, with file/line references. Separate soft holds, advisory findings, lint candidates, uncertain items, and spared/refuted findings.

## Review Bar

Keep findings only when the proposed fix is strictly better, not merely different. Correctness findings must name a concrete bad path or input. Test-integrity findings must identify owned logic that the test fails to exercise. Convention findings must cite `CLAUDE.md`, an ADR, or a repo rule and should feed future lint candidates.

Large file size alone is not a finding. A file-split finding must name the responsibility cohorts, identify what stays in the parent, and propose specific module/file names. Do not flag cohesive large files, generated-like tables, or deliberately broad scenario tests unless the tangled responsibilities are concrete.
