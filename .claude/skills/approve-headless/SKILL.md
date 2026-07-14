---
name: approve-headless
description: Headless variant of /approve for a one-shot GitHub Actions runner. Executes ../approve/SKILL.md verbatim for the gate checks, the ADR hard gate, and the approval-tier lookup; overrides only the interaction surface — a refusal and any non-auto tier become ask-and-park, the terminal summary becomes end-turn — per ../headless/protocol.md.
---

# /approve-headless — headless approval wrapper

This wraps `/approve` for a headless agent running one-shot on an ephemeral GitHub Actions runner. It carries no process of its own.

Execute `../approve/SKILL.md` verbatim — every gate check, the freshness gate, the dependency gate, the umbrella-integrity gate, the ADR merge gate, the ADR hard gate, the approval-tier lookup, the `phase:ready` label reconcile, the idempotency rules. Where the original touches the interaction surface, `../headless/protocol.md` governs. An instruction is overridden if and only if it appears in the Overrides table below, cited by the original's anchor; everything else is the original's, unchanged. An improvement to `/approve` is live here the moment it merges, because this wrapper only references it.

Before any process step, run the protocol's [re-entrancy-first](../headless/protocol.md#re-entrancy-first) guard: read the issue's current `phase:*` label (already `phase:ready` means the gate cleared — re-validate and no-op per [`## Idempotency`](../approve/SKILL.md#idempotency)) and check for an unanswered `agent:awaiting-answer` park, then begin the original at the point the observed state implies.

## The authority bound

A headless run advances **only an `auto`-tier issue**. Run [`## ADR hard gate`](../approve/SKILL.md#adr-hard-gate) and then [`## Approval tier`](../approve/SKILL.md#approval-tier) exactly as the original specifies, and route on the result:

- **`auto`** — the policy says this surface needs no reader. Apply [`## Actions on pass`](../approve/SKILL.md#actions-on-pass) and flip the issue to `phase:ready`.
- **`judge`, `human`, or ADR-bearing** — this run has no authority over it. Leave the phase untouched, comment the resolved tier and why, and end the turn. Never flip it.

The dispatcher pre-filters to the `auto` tier so a run is not spent to learn this, but the workflow is dispatchable by anything holding `actions: write`, so the bound is enforced here, at the point of the write, rather than trusting a caller. A tier the wrapper cannot resolve (an unreadable or absent policy file) is not `auto`.

## Overrides

| Original anchor | Interactive behavior | Headless override |
|-----------------|---------------------|-------------------|
| [`## Gate checks`](../approve/SKILL.md#gate-checks) refusal, and the hard refusals in [`## Freshness gate`](../approve/SKILL.md#freshness-gate) / [`## Dependency gate`](../approve/SKILL.md#dependency-gate) | Refuse, list every failing gate, and leave the issue for the user to fix or `/bounce` | [ask-and-park](../headless/protocol.md#ask-and-park) — post the same full failure list in the park shape, apply `agent:awaiting-answer` + `agent:park:approve`, exit 0. The phase label never moves; the issue rests at `phase:plan` |
| [`## Approval tier`](../approve/SKILL.md#approval-tier) `judge` / `human` routing, and [`## ADR hard gate`](../approve/SKILL.md#adr-hard-gate) | The owner reads the issue and runs `/approve` himself; a `judge`-tier issue goes to the shadow judge and still waits on the owner | [end-turn-not-wait](../headless/protocol.md#end-turn-not-wait) — a headless run holds no such authority (see [The authority bound](#the-authority-bound)). Comment the resolved tier, leave `phase:plan`, end the turn. This is a clean, expected terminal state, not a bounce, so it applies no `agent:awaiting-answer` — the issue is not waiting on an *answer*, it is waiting on its reader |
| [`## Actions on pass`](../approve/SKILL.md#actions-on-pass) step 3 "Print a summary to the user" | Print the `✓ #N approved / Phase: Plan → Ready / Next: /implement <N>` summary to the operator's terminal | [end-turn-not-wait](../headless/protocol.md#end-turn-not-wait) — the label swap is the durable record and the tick picks the issue up for `implement` on the next wave. Post the same summary as a comment and end the turn; dispatch nothing |
| [`## Sweep approve`](../approve/SKILL.md#sweep-approve) step 3 "print the approve plan and wait for confirmation" | Enumerate every Plan-complete issue and hold the whole batch behind one operator confirmation | Not reachable from *this* wrapper — its dispatch names exactly one ref, so it is always the single-issue path. Never enumerate the board and never batch from here: a self-driven sweep would escape the dispatcher's discovery and wave width. The sanctioned batched shape is its own task — the tick dispatches [`/approve-sweep-headless`](../approve-sweep-headless/SKILL.md), which owns the enumeration |
| [`## Freshness gate`](../approve/SKILL.md#freshness-gate) Tier A `git fetch origin main` and the `git log`/`git cat-file` reads | Run against the operator's local clone | [checkout-as-isolation](../headless/protocol.md#checkout-as-isolation) — the runner's `$GITHUB_WORKSPACE` checkout is the clone; it is on the default branch with full history, so the gate runs unchanged. Cut no branch and no worktree; `/approve` writes no code |

**Not overridden — `--skip-adr`.** It is an emergency override that requires a human's judgment and a mandatory rationale note, and no dispatch can carry one. A headless run never passes it: an unmerged-ADR issue fails the [ADR merge gate](../approve/SKILL.md#adr-gate-in-detail) and parks, exactly like any other failing gate.

Everything the table does not cite — the gate checks themselves, the freshness and dependency and umbrella-integrity logic, the ADR merge gate, the tier resolution, the phase-label reconcile mechanics, the idempotent re-run, the side-findings hands-off rule, and everything under `## What /approve does NOT do` (it dispatches no implementation, edits no issue body, and never edits `.github/approval-policy.yml`) — is `/approve`'s, verbatim.
