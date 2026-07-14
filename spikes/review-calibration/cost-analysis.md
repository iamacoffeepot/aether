# Cost analysis — bang for buck by configuration

Computed from per-agent token counts in `results/agents.csv` (finder-main + finder-rerun calls; refute-round1 calls) at 2026-07-14 API sticker prices per MTok — haiku 4.5 $1/$5, sonnet 5 $3/$15 (intro $2/$10 through 2026-08-31), opus 4.8 $5/$25, fable 5 $10/$50; cache reads 0.1× input, ephemeral-5m cache writes 1.25× input. A "column" = one 16-file review pass (the sweep's per-cell workload, a proxy for one mid-size PR's correctness+test-integrity finder fan-out).

## Finder cost × recall

| cell | $/call | $/16-file column | recall | notes |
|---|---|---|---|---|
| **haiku:high** | **$0.046** | **$0.73** | **91.7%** | cheapest AND best haiku; ties sonnet:high recall at ¼ the price |
| haiku:medium | $0.052 | $0.84 | 83.3% | |
| haiku:low | $0.078 | $1.25 | 83.3% | low effort makes haiku *slower and dearer* (more flailing) |
| sonnet:medium | $0.113 | $1.81 | 87.5% | intro pricing: $1.20 |
| **sonnet:high** | **$0.190** | **$3.04** | **91.7%** | intro: $2.03; the only non-fable cell that caught r1 (real missing-guard bug) |
| sonnet:low | $0.146 | $2.33 | 75.0% | |
| sonnet:xhigh | $0.360 | $5.76 | 83.3% | strictly dominated: 1.9× sonnet:high price, worse recall |
| opus:medium | $0.174 | $2.78 | 70.8% | strictly dominated by haiku:medium (3.3× price, −13 pts) |
| opus:high | $0.312 | $4.99 | 75.0% | strictly dominated |
| opus:low | $0.253 | $4.05 | 66.7% | strictly dominated |
| **fable:medium** | **$0.662** | **$10.60** | **100%** | only single-model 100% |

Opus is dominated at every effort — more expensive *and* lower recall than both haiku and sonnet alternatives. Sonnet:xhigh is likewise dominated by sonnet:high. Neither should appear in any configuration.

## Refuter cost (all cells ≈ equally accurate — see refute_final.json)

| cell | $/refute |
|---|---|
| sonnet:medium | $0.21 |
| opus:medium | $0.42 (scrambled-mapping cells averaged: ~$0.25–0.42) |
| sonnet:high | $0.38 |
| opus:high | $0.34 |
| fable:medium | $0.64–0.83 |

Every refuter cell kept 6/6 real findings and killed 6/6 fabrications (interference-corrected), so the cheapest competent refuter wins: **sonnet:medium at ~$0.21**, half the production opus default with no measured quality loss. (Its one deviation was a policy-correct `uncertain` on an untested claim — arguably the *right* answer under the NO_BUILD rule.)

## The ensemble observation

haiku:high and sonnet:high have **disjoint misses** on this dataset: haiku:high missed only r1 (the real missing-guard bug — requires noticing the sibling code path enforces a cap this one lacks); sonnet:high missed only s8 (the pan-law denominator — pure domain math). Running **both** as parallel finders and unioning findings scores **12/12 (100%) at $3.77/column** — fable-level recall at 36% of fable's price. The refute stage already dedups and kills FPs, so union noise is absorbed by existing machinery (measured FP rate: 1 finding in 280 calls).

## Recommended configurations

| config | finder(s) | refuter | $/column + typical refutes | recall | when |
|---|---|---|---|---|---|
| **Default gate (recommended)** | sonnet@high | sonnet@medium | ~$3.5–4 | 91.7% | review.yml deep pass — catches the r1 class (real-world shipped bugs), FP≈0 |
| **Budget** | haiku@high | sonnet@medium | ~$1.2–1.5 | 91.7% | backfill audits, advisory sweeps, high-volume passes; blind spot: cross-path missing-guard bugs |
| **Ensemble (high-stakes)** | haiku@high + sonnet@high | sonnet@medium (opus@medium for soft-holds if desired) | ~$4.5–5 | 100% observed | release-cut reviews, security-adjacent diffs |
| **Ceiling** | fable@medium | any | ~$11–12 | 100% | calibration reruns / arbitration, not routine CI |

Deltas vs today's production defaults (`finderModel: sonnet` at inherited session effort, `verifyModel: opus`):
1. **Pin `effort: 'high'` on finders** — free recall (+4–17 pts vs whatever the session happens to run at) and it prevents an xhigh session from silently degrading the gate.
2. **Drop `verifyModel` to sonnet** — ~50% off the verify stage, no measured loss. Keep opus only if the team wants a different model family double-checking sonnet finders (diversity argument, not an evidence-based one).
3. **Never route finders to opus or xhigh** — both are strictly dominated.

Caveats: single-trial cells carry ±1-item (±8.3%) noise; recall figures are on this 12-bug dataset, not a population estimate; sonnet intro pricing lapses 2026-08-31 (post-intro numbers shown above are sticker). The haiku:high column cost is remarkably low partly because higher effort made haiku *more* token-efficient (fewer flailing re-reads: 3.9k output tokens/call vs 4.8k at low).
