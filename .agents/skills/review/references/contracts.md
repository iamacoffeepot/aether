# Review Result Contracts

Use these contracts for native subagent prompts and parent validation. Return exactly one JSON object with no prose or Markdown fence.

## Finding

Every finding contains:

```json
{
  "id": null,
  "file": "/absolute/path/file.rs",
  "pillar": "correctness",
  "category": "invariant-violation",
  "line": 42,
  "symbol": "Type::method",
  "severity": "high",
  "confidence": "medium",
  "recommendation": "fix",
  "current_form": "brief description of the current code",
  "suggested_form": "specific better behavior or form",
  "rationale": "concrete consequence and why the change is strictly better",
  "direction": null,
  "char_delta": null
}
```

Rules:

- `pillar`: `spec-fidelity`, `correctness`, `test-integrity`, `economy`, or `convention`.
- `severity` and `confidence`: `high`, `medium`, or `low`.
- `recommendation`: `fix`, `remove`, `rewrite`, or `promote-lint`.
- `line`: a positive integer, or `null` only for a whole-change spec finding.
- `id`: `null` from a finder; the parent assigns a stable id before verification.
- `direction`: `over-verbose` or `over-terse` only for economy; otherwise `null`.
- `char_delta`: an integer estimate only for economy; otherwise `null`.

## Spec result

```json
{
  "outOfScope": ["/absolute/path/file.rs"],
  "findings": []
}
```

Spec categories are `over-delivery`, `under-delivery`, `scope-leakage`, and `silent-deviation`. Every spec finding uses the common finding shape.

## Lane result

```json
{
  "lane": "behavior",
  "filesReviewed": ["/absolute/path/file.rs"],
  "findings": [],
  "lintCandidates": [
    {
      "file": "/absolute/path/file.rs",
      "line": 42,
      "symbol": "Type::method",
      "rule": "named mechanical rule or gate-gap",
      "note": "why this belongs in a mechanical gate"
    }
  ],
  "uncertain": []
}
```

`lane` is `behavior` or `quality`. An uncertain row contains `file`, `pillar`, `symbol`, and `reason`.

## Verification result

```json
{
  "verdicts": [
    {
      "findingId": "F0001",
      "finalVerdict": "confirmed",
      "rationale": "why the claim survives or fails the strict bar",
      "evidence": ["path:line or existing test name"]
    }
  ]
}
```

Return one verdict for every submitted id. `finalVerdict` is `confirmed`, `false-positive`, or `uncertain`.

## Challenge result

```json
{
  "finalVerdict": "clean-confirmed",
  "missed": [],
  "rationale": "why the assigned recall-sensitive lenses are clean or uncertain"
}
```

`finalVerdict` is `clean-confirmed`, `missed`, or `uncertain`. Every `missed` item uses the common finding shape and must later receive a parent-assigned id and independent verification.

## Parent rollup

Keep these top-level collections even when empty:

```json
{
  "totals": {},
  "softHolds": [],
  "confirmed": [],
  "lintCandidates": [],
  "spared": [],
  "uncertain": [],
  "coverage": {
    "depth": "deep",
    "base": "origin/main",
    "head": "HEAD",
    "batches": [],
    "skipped": []
  }
}
```
